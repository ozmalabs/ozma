// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Best-effort PMBus identification over the allowlisted I²C surface.
//!
//! There is no separate probe list and no auto-detection. A request names one
//! exact `BUS:ADDR` that is already in `--i2c-allow`; the node reads only the
//! standard PMBus identification registers (`PMBUS_REVISION`, `MFR_ID`,
//! `MFR_MODEL`, `MFR_REVISION`) and, when present, `VOUT_MODE`, `READ_VOUT`,
//! `READ_IOUT`, and `READ_TEMPERATURE_1`. If the revision register does not
//! answer, the route returns typed `bus.unavailable` instead of guessing.
//!
//! No PMBus write or configuration command is exposed. VOUT is converted only
//! when `VOUT_MODE` reports the linear format; IOUT and temperature are returned
//! as raw words because their exponent is device-specific and is not invented
//! here. SMBus PEC follows the node-wide `--i2c-pec` opt-in.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Serialize;

use super::i2c::I2cReadOp;
use super::{io_failed, json_response, unavailable, BusError, BusState};

/// PMBus `VOUT_MODE`.
pub const PMBUS_VOUT_MODE: u8 = 0x20;
/// PMBus `READ_VOUT`.
pub const PMBUS_READ_VOUT: u8 = 0x8b;
/// PMBus `READ_IOUT`.
pub const PMBUS_READ_IOUT: u8 = 0x8c;
/// PMBus `READ_TEMPERATURE_1`.
pub const PMBUS_READ_TEMPERATURE_1: u8 = 0x8d;
/// PMBus `PMBUS_REVISION`.
pub const PMBUS_REVISION: u8 = 0x98;
/// PMBus `MFR_ID`.
pub const PMBUS_MFR_ID: u8 = 0x99;
/// PMBus `MFR_MODEL`.
pub const PMBUS_MFR_MODEL: u8 = 0x9a;
/// PMBus `MFR_REVISION`.
pub const PMBUS_MFR_REVISION: u8 = 0x9b;
/// Largest manufacturer string emitted.
const MAX_TEXT_CHARS: usize = 64;

/// `GET /buses/pmbus/{bus}/{addr}` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PmbusResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub pmbus: bool,
    /// Raw `PMBUS_REVISION` value; the answer that identified the device.
    pub revision: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfr_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfr_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mfr_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vout_mode: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vout_raw: Option<u16>,
    /// VOUT in volts when `VOUT_MODE` reports the linear format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vout_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iout_raw: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_raw: Option<u16>,
    /// SMBus PEC was enabled for every read.
    pub pec: bool,
}

fn sanitize_text(bytes: &[u8]) -> Option<String> {
    let text: String = bytes
        .iter()
        .copied()
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
        .map(char::from)
        .take(MAX_TEXT_CHARS)
        .collect();
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Convert a PMBus linear-format word to volts. Only the linear VOUT mode
/// (`VOUT_MODE` bits 7:5 == 0) is converted; direct/VID/coefficient modes are
/// left raw because their interpretation needs device-specific coefficients.
fn linear_vout_volts(raw: u16, mode: u8) -> Option<f64> {
    if mode & 0xe0 != 0 {
        return None;
    }
    let exponent = (((mode & 0x1f) as i8) << 3) >> 3;
    Some((raw as i16 as f64) * 2f64.powi(i32::from(exponent)))
}

impl BusState {
    fn pmbus_read_byte(&self, bus: u16, address: u8, register: u8) -> Result<u8, BusError> {
        let pec = self.inner.config.i2c.pec_enabled();
        let data = self.inner.i2c.read(
            bus,
            address,
            &I2cReadOp::Byte {
                register: Some(register),
            },
            pec,
        )?;
        data.first().copied().ok_or_else(|| {
            io_failed(format!(
                "I2C backend returned no byte for PMBus register 0x{register:02x}"
            ))
        })
    }

    fn pmbus_read_word(&self, bus: u16, address: u8, register: u8) -> Result<u16, BusError> {
        let pec = self.inner.config.i2c.pec_enabled();
        let data = self
            .inner
            .i2c
            .read(bus, address, &I2cReadOp::Word { register }, pec)?;
        if data.len() < 2 {
            return Err(io_failed(format!(
                "I2C backend returned {} bytes for PMBus word 0x{register:02x}",
                data.len()
            )));
        }
        Ok(u16::from_le_bytes([data[0], data[1]]))
    }

    fn pmbus_read_block(&self, bus: u16, address: u8, register: u8) -> Result<Vec<u8>, BusError> {
        let pec = self.inner.config.i2c.pec_enabled();
        self.inner.i2c.read(
            bus,
            address,
            &I2cReadOp::SmbusBlock {
                register,
                length: self.inner.config.max_block_bytes,
            },
            pec,
        )
    }

    pub(super) fn pmbus_identify(
        &self,
        bus_raw: &str,
        address_raw: &str,
    ) -> Result<PmbusResponse, BusError> {
        if !self.inner.config.i2c.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let bus = super::i2c::parse_bus(bus_raw)?;
        let address = super::i2c::parse_address(address_raw)?;
        if !self.inner.config.i2c.allows(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "PMBus bus {bus} address 0x{address:02x} is not in --i2c-allow; add --i2c-allow {bus}:0x{address:02x}"
                ),
            });
        }
        let pec = self.inner.config.i2c.pec_enabled();
        // The revision register is the identification transaction. A NAK or
        // other failure is reported as unavailable rather than inventing a
        // PMBus identity; permission and platform errors keep their own codes.
        let revision = match self.pmbus_read_byte(bus, address, PMBUS_REVISION) {
            Ok(value) => value,
            Err(
                error @ (BusError::PermissionDenied { .. }
                | BusError::Unsupported { .. }
                | BusError::Timeout { .. }),
            ) => return Err(error),
            Err(error) => {
                return Err(unavailable(format!(
                    "PMBus identification at bus {bus} address 0x{address:02x} failed reading 0x{PMBUS_REVISION:02x}: {error}"
                )))
            }
        };

        let mfr_id = self
            .pmbus_read_block(bus, address, PMBUS_MFR_ID)
            .ok()
            .and_then(|bytes| sanitize_text(&bytes));
        let mfr_model = self
            .pmbus_read_block(bus, address, PMBUS_MFR_MODEL)
            .ok()
            .and_then(|bytes| sanitize_text(&bytes));
        let mfr_revision = self
            .pmbus_read_block(bus, address, PMBUS_MFR_REVISION)
            .ok()
            .and_then(|bytes| sanitize_text(&bytes));
        let vout_mode = self.pmbus_read_byte(bus, address, PMBUS_VOUT_MODE).ok();
        let vout_raw = self.pmbus_read_word(bus, address, PMBUS_READ_VOUT).ok();
        let vout_volts = vout_mode
            .zip(vout_raw)
            .and_then(|(mode, raw)| linear_vout_volts(raw, mode));
        let iout_raw = self.pmbus_read_word(bus, address, PMBUS_READ_IOUT).ok();
        let temperature_raw = self
            .pmbus_read_word(bus, address, PMBUS_READ_TEMPERATURE_1)
            .ok();

        Ok(PmbusResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            pmbus: true,
            revision,
            mfr_id,
            mfr_model,
            mfr_revision,
            vout_mode,
            vout_raw,
            vout_volts,
            iout_raw,
            temperature_raw,
            pec,
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/pmbus/:bus/:addr", get(identify_handler))
        .with_state(state)
}

async fn identify_handler(
    State(state): State<BusState>,
    AxumPath((bus, address)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.pmbus_identify(&bus, &address)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "PMBus identify task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

fn error_response(error: BusError) -> Response {
    axum::response::IntoResponse::into_response(error)
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(super::MAX_ERROR_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::can::RealCanBackend;
    use crate::buses::gpio::RealGpioBackend;
    use crate::buses::i2c::{I2cBackend, I2cReadOp, I2cWriteOp};
    use crate::buses::spi::RealSpiBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Read(u16, u8, I2cReadOp, bool),
    }

    #[derive(Default)]
    struct FakeInner {
        calls: Vec<Call>,
        reads: VecDeque<Result<Vec<u8>, BusError>>,
    }

    #[derive(Clone, Default)]
    struct FakeI2c {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl FakeI2c {
        fn queue(&self, result: Result<Vec<u8>, BusError>) {
            self.inner.lock().unwrap().reads.push_back(result);
        }

        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }
    }

    impl I2cBackend for FakeI2c {
        fn detect(&self, _bus: u16, _timeout: Duration) -> Result<Vec<u8>, BusError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            bus: u16,
            address: u8,
            op: &I2cReadOp,
            pec: bool,
        ) -> Result<Vec<u8>, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Read(bus, address, op.clone(), pec));
            inner.reads.pop_front().unwrap_or_else(|| {
                Err(BusError::Unavailable {
                    detail: "no queued PMBus reply".to_owned(),
                })
            })
        }

        fn write(
            &self,
            _bus: u16,
            _address: u8,
            _op: &I2cWriteOp,
            _pec: bool,
        ) -> Result<(), BusError> {
            panic!("PMBus identification must never write")
        }
    }

    fn config(pec: bool) -> BusConfig {
        BusConfig::new()
            .with_i2c_allow(&["1:0x40".to_owned()])
            .unwrap()
            .with_i2c_pec(pec)
    }

    fn state(config: BusConfig, fake: FakeI2c) -> BusState {
        BusState::with_all_backends(
            config,
            Arc::new(fake),
            Arc::new(RealSpiBackend),
            Arc::new(RealGpioBackend::default()),
            Arc::new(RealCanBackend::default()),
        )
        .expect("valid state")
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn linear_vout_conversion_is_bounded() {
        // VOUT_MODE 0x14: linear, exponent -12. 0x3000 * 2^-12 = 3.0 V.
        assert_eq!(linear_vout_volts(0x3000, 0x14), Some(3.0));
        // A non-linear mode is never guessed.
        assert_eq!(linear_vout_volts(0x3000, 0x80), None);
    }

    #[tokio::test]
    async fn identification_reads_only_allowlisted_pmbus_registers() {
        let fake = FakeI2c::default();
        fake.queue(Ok(vec![0x12]));
        fake.queue(Ok(b"ACME".to_vec()));
        fake.queue(Ok(b"PSU1".to_vec()));
        fake.queue(Ok(b"1.0".to_vec()));
        fake.queue(Ok(vec![0x14]));
        fake.queue(Ok(vec![0x00, 0x30]));
        fake.queue(Ok(vec![0x64, 0x00]));
        fake.queue(Ok(vec![0xe8, 0x03]));
        let app = router(state(config(true), fake.clone()));

        let (status, body) = get(&app, "/buses/pmbus/1/0x40").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pmbus"], true);
        assert_eq!(body["bus"], 1);
        assert_eq!(body["address"], 64);
        assert_eq!(body["revision"], 0x12);
        assert_eq!(body["mfr_id"], "ACME");
        assert_eq!(body["mfr_model"], "PSU1");
        assert_eq!(body["mfr_revision"], "1.0");
        assert_eq!(body["vout_mode"], 0x14);
        assert_eq!(body["vout_raw"], 0x3000);
        assert_eq!(body["vout_volts"], 3.0);
        assert_eq!(body["iout_raw"], 100);
        assert_eq!(body["temperature_raw"], 1000);
        assert_eq!(body["pec"], true);

        let calls = fake.calls();
        assert_eq!(calls.len(), 8);
        assert_eq!(
            calls[0],
            Call::Read(
                1,
                0x40,
                I2cReadOp::Byte {
                    register: Some(PMBUS_REVISION)
                },
                true
            )
        );
        assert_eq!(
            calls[1],
            Call::Read(
                1,
                0x40,
                I2cReadOp::SmbusBlock {
                    register: PMBUS_MFR_ID,
                    length: 32
                },
                true
            )
        );
        assert_eq!(
            calls[5],
            Call::Read(
                1,
                0x40,
                I2cReadOp::Word {
                    register: PMBUS_READ_VOUT
                },
                true
            )
        );
    }

    #[tokio::test]
    async fn non_pmbus_unlisted_and_unconfigured_are_typed() {
        let fake = FakeI2c::default();
        fake.queue(Err(BusError::Unavailable {
            detail: "NAK".to_owned(),
        }));
        let app = router(state(config(false), fake.clone()));

        let (status, body) = get(&app, "/buses/pmbus/1/0x40").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        let (status, body) = get(&app, "/buses/pmbus/1/0x41").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = get(&app, "/buses/pmbus/1/0x00").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");

        let app = router(state(BusConfig::default(), FakeI2c::default()));
        let (status, body) = get(&app, "/buses/pmbus/1/0x40").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
    }
}
