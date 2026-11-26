// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted DDC/CI display-control surface.
//!
//! DDC/CI uses the I²C DDC/CI address `0x37` (8-bit write address `0x6e`).
//! Every entry must be an exact `BUS:ADDR` pair that is **also** present in the
//! I²C allowlist, and `ADDR` must be `0x37`. Reads are allowed once listed;
//! writes additionally require the `:write` suffix on that exact pair. VCP
//! get/set is restricted to the three bounded standard controls
//! (brightness `0x10`, contrast `0x12`, input source `0x60`). Capabilities are
//! read-only and bounded.
//!
//! The real backend uses safe `i2cdev` reads/writes and the standard DDC/CI
//! framing (source `0x51`, length `0x80|n`, XOR checksum). No bus is scanned,
//! no display is detected, and no write is attempted without `:write`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusLimits, BusState,
    MAX_ERROR_DETAIL, MAX_REQUEST_BODY_BYTES,
};

/// DDC/CI's fixed 7-bit I²C address.
pub const DDC_ADDRESS: u8 = 0x37;
/// 8-bit DDC/CI write address.
const DDC_WRITE_ADDRESS: u8 = 0x6e;
/// 8-bit DDC/CI read address.
const DDC_READ_ADDRESS: u8 = 0x6f;
/// DDC/CI host source byte.
const DDC_HOST_SOURCE: u8 = 0x51;
/// Maximum DDC/CI data fragment accepted.
const DDC_MAX_DATA: usize = 32;
/// Largest capability string returned by one read. A DDC/CI fragment carries
/// at most 32 data bytes; multi-fragment capability strings are not reassembled.
pub const MAX_DDC_CAPABILITIES_BYTES: usize = DDC_MAX_DATA;
/// Standard DDC/CI turnaround pause before reading a response.
const DDC_READ_DELAY: Duration = Duration::from_millis(50);
/// Largest raw response read (2 header + 32 data + checksum).
const DDC_RAW_READ_BYTES: usize = DDC_MAX_DATA + 3;

/// VCP code for brightness.
pub const VCP_BRIGHTNESS: u8 = 0x10;
/// VCP code for contrast.
pub const VCP_CONTRAST: u8 = 0x12;
/// VCP code for input source.
pub const VCP_INPUT_SOURCE: u8 = 0x60;

fn vcp_name(code: u8) -> Option<&'static str> {
    match code {
        VCP_BRIGHTNESS => Some("brightness"),
        VCP_CONTRAST => Some("contrast"),
        VCP_INPUT_SOURCE => Some("input-source"),
        _ => None,
    }
}

/// Exact DDC/CI bus/address allowlist plus the per-pair write opt-in.
#[derive(Clone, Debug, Default)]
pub struct DdcConfig {
    allow: BTreeMap<u16, BTreeMap<u8, bool>>,
}

impl DdcConfig {
    /// Parse comma/space separated `BUS:ADDR[:write]` entries.
    ///
    /// `BUS` and `ADDR` accept decimal or `0x` hex; `ADDR` must be
    /// [`DDC_ADDRESS`]. Duplicate entries collapse and `:write` is sticky.
    pub fn from_entries<I, S>(entries: I) -> Result<Self, BusError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut config = Self::default();
        for entry in entries {
            for token in entry
                .as_ref()
                .split(|character: char| character == ',' || character.is_whitespace())
            {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                config.add_entry(token)?;
            }
        }
        config.validate()?;
        Ok(config)
    }

    fn add_entry(&mut self, token: &str) -> Result<(), BusError> {
        let parts: Vec<&str> = token.split(':').collect();
        if !matches!(parts.len(), 2 | 3) {
            return Err(invalid_request(format!(
                "invalid --ddc-allow entry {token:?}; expected BUS:ADDR[:write]"
            )));
        }
        let bus = super::i2c::parse_bus(parts[0])?;
        let address = super::i2c::parse_address(parts[1])?;
        if address != DDC_ADDRESS {
            return Err(invalid_request(format!(
                "--ddc-allow address 0x{address:02x} is not the DDC/CI address 0x{DDC_ADDRESS:02x}"
            )));
        }
        let write = match parts.get(2).map(|value| value.trim()) {
            None | Some("") => false,
            Some("write") => true,
            Some(other) => {
                return Err(invalid_request(format!(
                    "invalid DDC option {other:?} in {token:?}; expected write"
                )))
            }
        };
        *self
            .allow
            .entry(bus)
            .or_default()
            .entry(address)
            .or_default() |= write;
        Ok(())
    }

    /// Validate the absolute entry ceiling.
    pub fn validate(&self) -> Result<(), BusError> {
        if self.total_addresses() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "DDC allowlist contains {} addresses, above the {} absolute bound",
                self.total_addresses(),
                super::MAX_DEVICES
            )));
        }
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Exact bus/address allowlist check before any open.
    pub fn allows(&self, bus: u16, address: u8) -> bool {
        self.allow
            .get(&bus)
            .is_some_and(|addresses| addresses.contains_key(&address))
    }

    /// True only when this exact pair was listed with `:write`.
    pub fn write_allowed(&self, bus: u16, address: u8) -> bool {
        self.allow
            .get(&bus)
            .and_then(|addresses| addresses.get(&address))
            .copied()
            .unwrap_or(false)
    }

    pub fn bus_numbers(&self) -> impl Iterator<Item = u16> + '_ {
        self.allow.keys().copied()
    }

    pub fn addresses(&self, bus: u16) -> Option<&BTreeMap<u8, bool>> {
        self.allow.get(&bus)
    }

    pub fn total_addresses(&self) -> usize {
        self.allow.values().map(BTreeMap::len).sum()
    }

    pub fn write_addresses(&self) -> usize {
        self.allow
            .values()
            .flat_map(|addresses| addresses.values())
            .filter(|write| **write)
            .count()
    }
}

/// Parse and bound a route VCP code, accepting only the three supported codes.
pub fn parse_vcp_code(raw: &str) -> Result<u8, BusError> {
    let raw = raw.trim();
    let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16)
    } else {
        raw.parse::<u8>()
    };
    let code = parsed.map_err(|_| {
        invalid_request(format!(
            "invalid VCP code {raw:?}; expected 0x10, 0x12, or 0x60"
        ))
    })?;
    if vcp_name(code).is_none() {
        return Err(invalid_request(format!(
            "VCP code 0x{code:02x} is not supported; only brightness 0x10, contrast 0x12, and input source 0x60 are allowed"
        )));
    }
    Ok(code)
}

/// One bounded VCP reading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VcpReading {
    pub code: u8,
    pub result_code: u8,
    /// 0 = set parameter, 1 = momentary.
    pub vcp_type: u8,
    pub maximum: u16,
    pub current: u16,
    /// Exact reply data bytes, lower-case hex.
    pub raw: String,
}

/// Backend abstraction. Production uses [`RealDdcBackend`]; tests inject a
/// deterministic fake so allowlist/write gating is observable first.
pub trait DdcBackend: Send + Sync {
    fn get_vcp(&self, bus: u16, address: u8, code: u8) -> Result<VcpReading, BusError>;
    fn set_vcp(&self, bus: u16, address: u8, code: u8, value: u16) -> Result<(), BusError>;
    fn capabilities(
        &self,
        bus: u16,
        address: u8,
        max_bytes: usize,
    ) -> Result<(String, bool), BusError>;
}

/// Real Linux DDC/CI backend using safe `i2cdev` file reads/writes.
pub struct RealDdcBackend;

impl DdcBackend for RealDdcBackend {
    fn get_vcp(&self, bus: u16, address: u8, code: u8) -> Result<VcpReading, BusError> {
        #[cfg(target_os = "linux")]
        {
            get_vcp_real(bus, address, code)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, address, code);
            Err(unsupported())
        }
    }

    fn set_vcp(&self, bus: u16, address: u8, code: u8, value: u16) -> Result<(), BusError> {
        #[cfg(target_os = "linux")]
        {
            set_vcp_real(bus, address, code, value)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, address, code, value);
            Err(unsupported())
        }
    }

    fn capabilities(
        &self,
        bus: u16,
        address: u8,
        max_bytes: usize,
    ) -> Result<(String, bool), BusError> {
        #[cfg(target_os = "linux")]
        {
            capabilities_real(bus, address, max_bytes)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, address, max_bytes);
            Err(unsupported())
        }
    }
}

#[allow(dead_code)]
fn unsupported() -> BusError {
    BusError::Unsupported {
        detail: "DDC/CI is only implemented on Linux i2c-dev".to_owned(),
    }
}

/// Build the payload (after the I²C address byte) for one DDC/CI request.
fn build_request(data: &[u8]) -> Vec<u8> {
    let length = 0x80 | (data.len() as u8 & 0x7f);
    let mut packet = Vec::with_capacity(data.len() + 3);
    packet.push(DDC_HOST_SOURCE);
    packet.push(length);
    packet.extend_from_slice(data);
    let checksum = packet
        .iter()
        .fold(DDC_WRITE_ADDRESS, |checksum, byte| checksum ^ byte);
    packet.push(checksum);
    packet
}

/// Parse a DDC/CI reply and return the data bytes including the reply opcode.
fn parse_reply(raw: &[u8], expected_opcode: u8) -> Result<Vec<u8>, BusError> {
    if raw.len() < 3 {
        return Err(io_failed(format!(
            "DDC/CI reply is only {} bytes",
            raw.len()
        )));
    }
    let length = (raw[1] & 0x7f) as usize;
    if length == 0 || length > DDC_MAX_DATA {
        return Err(io_failed(format!(
            "DDC/CI reply length {length} is outside 1..={DDC_MAX_DATA}"
        )));
    }
    if raw.len() < 2 + length + 1 {
        return Err(io_failed(format!(
            "DDC/CI reply is truncated: need {} bytes, got {}",
            2 + length + 1,
            raw.len()
        )));
    }
    let data = &raw[2..2 + length];
    let expected = raw[2 + length];
    let primary = data
        .iter()
        .fold(0x50u8 ^ raw[0] ^ raw[1], |checksum, byte| checksum ^ byte);
    let alternate = data.iter().fold(
        DDC_READ_ADDRESS ^ DDC_HOST_SOURCE ^ raw[1],
        |checksum, byte| checksum ^ byte,
    );
    if expected != primary && expected != alternate {
        return Err(io_failed(format!(
            "DDC/CI reply checksum mismatch: expected 0x{expected:02x}, computed 0x{primary:02x}"
        )));
    }
    if data[0] != expected_opcode {
        return Err(io_failed(format!(
            "DDC/CI reply opcode 0x{:02x} does not match 0x{expected_opcode:02x}",
            data[0]
        )));
    }
    Ok(data.to_vec())
}

fn parse_vcp_reply(data: &[u8], code: u8) -> Result<VcpReading, BusError> {
    if data.len() < 8 {
        return Err(io_failed(format!(
            "DDC/CI get-VCP reply has {} data bytes; expected 8",
            data.len()
        )));
    }
    let result_code = data[1];
    if result_code != 0 {
        return Err(BusError::Unsupported {
            detail: format!(
                "display rejected get-VCP 0x{code:02x} with result code 0x{result_code:02x}"
            ),
        });
    }
    if data[2] != code {
        return Err(io_failed(format!(
            "DDC/CI get-VCP reply code 0x{:02x} does not match 0x{code:02x}",
            data[2]
        )));
    }
    Ok(VcpReading {
        code,
        result_code,
        vcp_type: data[3],
        maximum: u16::from_be_bytes([data[4], data[5]]),
        current: u16::from_be_bytes([data[6], data[7]]),
        raw: super::encode_hex(data),
    })
}

fn parse_capabilities_reply(data: &[u8], max_bytes: usize) -> (String, bool) {
    // Data starts with 0xE3 and a two-byte offset, followed by bounded ASCII.
    let body = if data.len() > 3 { &data[3..] } else { &[][..] };
    let truncated = body.len() > max_bytes;
    let text: String = body
        .iter()
        .take(max_bytes)
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' || *byte == b'\n' || *byte == b'\r' {
                *byte as char
            } else {
                '.'
            }
        })
        .collect();
    (text, truncated)
}

#[cfg(target_os = "linux")]
fn open_ddc(bus: u16, address: u8) -> Result<i2cdev::linux::LinuxI2CDevice, BusError> {
    use i2cdev::linux::LinuxI2CDevice;

    if address != DDC_ADDRESS {
        return Err(invalid_request(format!(
            "DDC/CI address must be 0x{DDC_ADDRESS:02x}"
        )));
    }
    let path = super::i2c::device_path(bus);
    if !Path::new(&path).exists() {
        return Err(unavailable(format!(
            "I2C bus {bus} device {path} is not present"
        )));
    }
    LinuxI2CDevice::new(&path, address as u16)
        .map_err(|error| map_ddc_error(&format!("opening {path} address 0x{address:02x}"), error))
}

#[cfg(target_os = "linux")]
fn read_reply(
    device: &mut i2cdev::linux::LinuxI2CDevice,
    length: usize,
    context: &str,
) -> Result<Vec<u8>, BusError> {
    use i2cdev::core::I2CDevice;
    std::thread::sleep(DDC_READ_DELAY);
    let mut buffer = vec![0u8; length];
    device
        .read(&mut buffer)
        .map_err(|error| map_ddc_error(context, error))?;
    Ok(buffer)
}

#[cfg(target_os = "linux")]
fn get_vcp_real(bus: u16, address: u8, code: u8) -> Result<VcpReading, BusError> {
    use i2cdev::core::I2CDevice;

    let mut device = open_ddc(bus, address)?;
    let request = build_request(&[0x01, code]);
    device
        .write(&request)
        .map_err(|error| map_ddc_error("writing get-VCP request", error))?;
    let raw = read_reply(&mut device, 11, "reading get-VCP reply")?;
    let data = parse_reply(&raw, 0x02)?;
    parse_vcp_reply(&data, code)
}

#[cfg(target_os = "linux")]
fn set_vcp_real(bus: u16, address: u8, code: u8, value: u16) -> Result<(), BusError> {
    use i2cdev::core::I2CDevice;

    let mut device = open_ddc(bus, address)?;
    let [high, low] = value.to_be_bytes();
    let request = build_request(&[0x03, code, high, low]);
    device
        .write(&request)
        .map_err(|error| map_ddc_error("writing set-VCP request", error))
}

#[cfg(target_os = "linux")]
fn capabilities_real(bus: u16, address: u8, max_bytes: usize) -> Result<(String, bool), BusError> {
    use i2cdev::core::I2CDevice;

    let mut device = open_ddc(bus, address)?;
    let request = build_request(&[0xf3]);
    device
        .write(&request)
        .map_err(|error| map_ddc_error("writing capabilities request", error))?;
    let raw = read_reply(
        &mut device,
        DDC_RAW_READ_BYTES,
        "reading capabilities reply",
    )?;
    let data = parse_reply(&raw, 0xe3)?;
    Ok(parse_capabilities_reply(&data, max_bytes))
}

#[cfg(target_os = "linux")]
fn map_ddc_error(context: &str, error: i2cdev::linux::LinuxI2CError) -> BusError {
    let error = std::io::Error::from(error);
    let detail = format!("{context}: {error}");
    let detail: String = detail.chars().take(MAX_ERROR_DETAIL).collect();
    if error.kind() == std::io::ErrorKind::TimedOut || error.raw_os_error() == Some(libc::ETIMEDOUT)
    {
        BusError::Timeout { detail }
    } else if error.kind() == std::io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(libc::ENXIO) | Some(libc::ENODEV))
    {
        unavailable(detail)
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        BusError::PermissionDenied { detail }
    } else {
        io_failed(detail)
    }
}

/// One allowlisted target in `GET /buses/ddc`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcAddressInfo {
    pub bus: u16,
    pub address: u8,
    pub device: String,
    pub available: bool,
    pub write_allowed: bool,
}

/// `GET /buses/ddc` response. It only reads configuration and stats bus nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub addresses: Vec<DdcAddressInfo>,
    pub limits: BusLimits,
}

/// `GET /buses/ddc/{bus}/{addr}` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcDetailResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub device: String,
    pub available: bool,
    pub write_allowed: bool,
    pub vcp_codes: Vec<u8>,
}

/// `GET /buses/ddc/{bus}/{addr}/vcp/{code}` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcVcpResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub code: u8,
    pub name: &'static str,
    pub result_code: u8,
    #[serde(rename = "type")]
    pub vcp_type: u8,
    pub maximum: u16,
    pub current: u16,
    pub raw: String,
    pub encoding: &'static str,
}

/// `POST /buses/ddc/{bus}/{addr}/vcp/{code}/set` body.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DdcSetRequest {
    /// VCP value, 0–65535.
    pub value: u16,
}

/// `POST /buses/ddc/{bus}/{addr}/vcp/{code}/set` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcSetResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub code: u8,
    pub name: &'static str,
    pub value: u16,
}

/// `GET /buses/ddc/{bus}/{addr}/capabilities` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DdcCapabilitiesResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub bytes: usize,
    pub truncated: bool,
    pub text: String,
}

impl BusState {
    fn ddc_allow(&self, bus_raw: &str, address_raw: &str) -> Result<(u16, u8), BusError> {
        if !self.inner.config.ddc.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let bus = super::i2c::parse_bus(bus_raw)?;
        let address = super::i2c::parse_address(address_raw)?;
        if !self.inner.config.ddc.allows(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "DDC/CI bus {bus} address 0x{address:02x} is not allowlisted; add --ddc-allow {bus}:0x{address:02x}"
                ),
            });
        }
        if !self.inner.config.i2c.allows(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "DDC/CI bus {bus} address 0x{address:02x} is not present in --i2c-allow; add --i2c-allow {bus}:0x{address:02x}"
                ),
            });
        }
        Ok((bus, address))
    }

    pub(super) fn ddc_list(&self) -> Result<DdcListResponse, BusError> {
        if !self.inner.config.ddc.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut addresses = Vec::new();
        for bus in self.inner.config.ddc.bus_numbers() {
            for (address, write_allowed) in
                self.inner.config.ddc.addresses(bus).into_iter().flatten()
            {
                let device = super::i2c::device_path(bus);
                addresses.push(DdcAddressInfo {
                    bus,
                    address: *address,
                    available: Path::new(&device).exists(),
                    device,
                    write_allowed: *write_allowed,
                });
            }
        }
        let matched = addresses.len();
        let truncated = matched > limit;
        addresses.truncate(limit);
        Ok(DdcListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: addresses.len(),
            limit,
            truncated,
            addresses,
            limits: self.limits(),
        })
    }

    pub(super) fn ddc_detail(
        &self,
        bus_raw: &str,
        address_raw: &str,
    ) -> Result<DdcDetailResponse, BusError> {
        let (bus, address) = self.ddc_allow(bus_raw, address_raw)?;
        let device = super::i2c::device_path(bus);
        Ok(DdcDetailResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            available: Path::new(&device).exists(),
            device,
            write_allowed: self.inner.config.ddc.write_allowed(bus, address),
            vcp_codes: vec![VCP_BRIGHTNESS, VCP_CONTRAST, VCP_INPUT_SOURCE],
        })
    }

    pub(super) fn ddc_get_vcp(
        &self,
        bus_raw: &str,
        address_raw: &str,
        code_raw: &str,
    ) -> Result<DdcVcpResponse, BusError> {
        let (bus, address) = self.ddc_allow(bus_raw, address_raw)?;
        let code = parse_vcp_code(code_raw)?;
        let reading = self.inner.ddc.get_vcp(bus, address, code)?;
        let name = vcp_name(code).expect("validated VCP code");
        Ok(DdcVcpResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            code,
            name,
            result_code: reading.result_code,
            vcp_type: reading.vcp_type,
            maximum: reading.maximum,
            current: reading.current,
            raw: reading.raw,
            encoding: "hex",
        })
    }

    pub(super) fn ddc_set_vcp(
        &self,
        bus_raw: &str,
        address_raw: &str,
        code_raw: &str,
        request: DdcSetRequest,
    ) -> Result<DdcSetResponse, BusError> {
        let (bus, address) = self.ddc_allow(bus_raw, address_raw)?;
        let code = parse_vcp_code(code_raw)?;
        if !self.inner.config.ddc.write_allowed(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "DDC/CI bus {bus} address 0x{address:02x} is read-only; add :write to --ddc-allow {bus}:0x{address:02x}:write"
                ),
            });
        }
        self.inner.ddc.set_vcp(bus, address, code, request.value)?;
        let name = vcp_name(code).expect("validated VCP code");
        Ok(DdcSetResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            code,
            name,
            value: request.value,
        })
    }

    pub(super) fn ddc_capabilities(
        &self,
        bus_raw: &str,
        address_raw: &str,
    ) -> Result<DdcCapabilitiesResponse, BusError> {
        let (bus, address) = self.ddc_allow(bus_raw, address_raw)?;
        let max_bytes = self
            .inner
            .config
            .max_transfer_bytes
            .min(MAX_DDC_CAPABILITIES_BYTES);
        let (text, truncated) = self.inner.ddc.capabilities(bus, address, max_bytes)?;
        Ok(DdcCapabilitiesResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            bytes: text.len(),
            truncated,
            text,
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/ddc", get(list_handler))
        .route("/buses/ddc/:bus/:addr", get(detail_handler))
        .route("/buses/ddc/:bus/:addr/vcp/:code", get(vcp_handler))
        .route(
            "/buses/ddc/:bus/:addr/vcp/:code/set",
            post(set_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .route(
            "/buses/ddc/:bus/:addr/capabilities",
            get(capabilities_handler),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.ddc_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "DDC list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn detail_handler(
    State(state): State<BusState>,
    AxumPath((bus, address)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.ddc_detail(&bus, &address)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "DDC detail task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn vcp_handler(
    State(state): State<BusState>,
    AxumPath((bus, address, code)): AxumPath<(String, String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.ddc_get_vcp(&bus, &address, &code)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "DDC get-VCP task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn set_handler(
    State(state): State<BusState>,
    AxumPath((bus, address, code)): AxumPath<(String, String, String)>,
    body: Result<axum::Json<DdcSetRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.ddc_set_vcp(&bus, &address, &code, request))
        .await
    {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "DDC set-VCP task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn capabilities_handler(
    State(state): State<BusState>,
    AxumPath((bus, address)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.ddc_capabilities(&bus, &address)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "DDC capabilities task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

fn rejection_response(rejection: JsonRejection) -> Response {
    let status = rejection.status();
    let detail = if status == StatusCode::PAYLOAD_TOO_LARGE {
        format!("request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte bound")
    } else {
        format!("invalid JSON body: {}", rejection.body_text())
    };
    error_response(invalid_request(detail))
}

fn error_response(error: BusError) -> Response {
    axum::response::IntoResponse::into_response(error)
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::can::RealCanBackend;
    use crate::buses::gpio::RealGpioBackend;
    use crate::buses::i2c::RealI2cBackend;
    use crate::buses::spi::RealSpiBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Get(u16, u8, u8),
        Set(u16, u8, u8, u16),
        Capabilities(u16, u8, usize),
    }

    #[derive(Default)]
    struct FakeInner {
        calls: Vec<Call>,
        gets: VecDeque<Result<VcpReading, BusError>>,
        sets: VecDeque<Result<(), BusError>>,
        capabilities: VecDeque<Result<(String, bool), BusError>>,
    }

    #[derive(Clone, Default)]
    struct FakeDdc {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl FakeDdc {
        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }

        fn readings(&self) -> VcpReading {
            VcpReading {
                code: VCP_BRIGHTNESS,
                result_code: 0,
                vcp_type: 0,
                maximum: 100,
                current: 42,
                raw: "020010000064002a".to_owned(),
            }
        }
    }

    impl DdcBackend for FakeDdc {
        fn get_vcp(&self, bus: u16, address: u8, code: u8) -> Result<VcpReading, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Get(bus, address, code));
            inner
                .gets
                .pop_front()
                .unwrap_or_else(|| Ok(self.readings()))
        }

        fn set_vcp(&self, bus: u16, address: u8, code: u8, value: u16) -> Result<(), BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Set(bus, address, code, value));
            inner.sets.pop_front().unwrap_or(Ok(()))
        }

        fn capabilities(
            &self,
            bus: u16,
            address: u8,
            max_bytes: usize,
        ) -> Result<(String, bool), BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner
                .calls
                .push(Call::Capabilities(bus, address, max_bytes));
            inner
                .capabilities
                .pop_front()
                .unwrap_or_else(|| Ok(("(prot(monitor)type(lcd)vcp(10 12 60))".to_owned(), false)))
        }
    }

    fn config(ddc: &[&str]) -> BusConfig {
        BusConfig::new()
            .with_i2c_allow(&["1:0x37".to_owned()])
            .unwrap()
            .with_ddc_allow(
                &ddc.iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid DDC allowlist")
    }

    fn state(config: BusConfig, fake: FakeDdc) -> BusState {
        BusState::with_all_backends_and_ddc(
            config,
            Arc::new(RealI2cBackend),
            Arc::new(RealSpiBackend),
            Arc::new(RealGpioBackend::default()),
            Arc::new(RealCanBackend::default()),
            Arc::new(fake),
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

    async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn allowlist_requires_ddc_address_and_i2c_membership() {
        let config = DdcConfig::from_entries(["1:0x37, 2:55:write", "1:0x37:write"]).unwrap();
        assert!(config.allows(1, DDC_ADDRESS));
        assert!(config.write_allowed(1, DDC_ADDRESS));
        assert!(config.allows(2, 55));
        assert!(config.write_allowed(2, 55));
        assert_eq!(config.total_addresses(), 2);
        assert_eq!(config.write_addresses(), 2);

        for bad in [
            "1",
            "1:",
            ":0x37",
            "1:0x50",
            "1:0x37:wibble",
            "1:0x37:write:extra",
        ] {
            assert!(DdcConfig::from_entries([bad]).is_err(), "{bad}");
        }

        // DDC can only narrow the I2C allowlist.
        let missing = BusConfig::new()
            .with_ddc_allow(&["1:0x37".to_owned()])
            .unwrap();
        assert!(matches!(
            missing.validate(),
            Err(BusError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn vcp_codes_are_restricted_and_parsed() {
        for raw in ["0x10", "16", "0x12", "0x60"] {
            assert!(parse_vcp_code(raw).is_ok(), "{raw}");
        }
        for bad in ["0x11", "0x0", "0x100", "-1", "brightness"] {
            assert!(parse_vcp_code(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ddc_request_and_reply_framing_is_checked() {
        assert_eq!(
            build_request(&[0x01, VCP_BRIGHTNESS]),
            vec![0x51, 0x82, 0x01, 0x10, 0xac]
        );

        let data = [0x02u8, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x2a];
        let length = 0x80 | data.len() as u8;
        let checksum = data
            .iter()
            .fold(0x50u8 ^ 0x6e ^ length, |checksum, byte| checksum ^ byte);
        let mut raw = vec![0x6e, length];
        raw.extend_from_slice(&data);
        raw.push(checksum);
        let parsed = parse_reply(&raw, 0x02).unwrap();
        assert_eq!(parsed, data);
        let reading = parse_vcp_reply(&parsed, VCP_BRIGHTNESS).unwrap();
        assert_eq!(reading.maximum, 100);
        assert_eq!(reading.current, 42);

        let mut broken = raw.clone();
        *broken.last_mut().unwrap() ^= 0xff;
        assert!(parse_reply(&broken, 0x02).is_err());

        let mut wrong_opcode = raw;
        wrong_opcode[2] = 0x03;
        assert!(parse_reply(&wrong_opcode, 0x02).is_err());
    }

    #[tokio::test]
    async fn get_capabilities_and_listing_are_bounded() {
        let fake = FakeDdc::default();
        let app = router(state(config(&["1:0x37"]), fake.clone()));

        let (status, body) = get(&app, "/buses/ddc").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 1);
        assert_eq!(body["addresses"][0]["address"], 55);
        assert_eq!(body["addresses"][0]["write_allowed"], false);
        assert!(fake.calls().is_empty(), "listing must not open the bus");

        let (status, body) = get(&app, "/buses/ddc/1/0x37").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["write_allowed"], false);
        assert_eq!(body["vcp_codes"], json!([16, 18, 96]));

        let (status, body) = get(&app, "/buses/ddc/1/0x37/vcp/0x10").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "brightness");
        assert_eq!(body["maximum"], 100);
        assert_eq!(body["current"], 42);
        assert_eq!(body["encoding"], "hex");

        let (status, body) = get(&app, "/buses/ddc/1/0x37/capabilities").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["truncated"], false);
        assert!(body["text"].as_str().unwrap().contains("vcp(10 12 60)"));

        assert_eq!(
            fake.calls(),
            vec![
                Call::Get(1, DDC_ADDRESS, VCP_BRIGHTNESS),
                Call::Capabilities(1, DDC_ADDRESS, MAX_DDC_CAPABILITIES_BYTES)
            ]
        );

        let (status, body) = get(&app, "/buses/ddc/1/0x37/vcp/0x11").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");
    }

    #[tokio::test]
    async fn writes_require_explicit_write_suffix_and_exact_allowlist() {
        let fake = FakeDdc::default();
        let app = router(state(config(&["1:0x37"]), fake.clone()));

        let (status, body) =
            post(&app, "/buses/ddc/1/0x37/vcp/0x10/set", json!({"value": 70})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert!(fake.calls().is_empty(), "read-only pair must not write");

        let fake = FakeDdc::default();
        let app = router(state(config(&["1:0x37:write"]), fake.clone()));
        let (status, body) =
            post(&app, "/buses/ddc/1/0x37/vcp/0x10/set", json!({"value": 70})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], 70);
        assert_eq!(body["name"], "brightness");
        assert_eq!(
            fake.calls(),
            vec![Call::Set(1, DDC_ADDRESS, VCP_BRIGHTNESS, 70)]
        );

        let (status, body) = post(
            &app,
            "/buses/ddc/1/0x37/vcp/0x12/set",
            json!({"value": 70000}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");

        let (status, body) = post(
            &app,
            "/buses/ddc/1/0x37/vcp/0x60/set",
            json!({"value": 0x11}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "input-source");

        for uri in [
            "/buses/ddc/2/0x37/vcp/0x10/set",
            "/buses/ddc/1/0x50/vcp/0x10/set",
        ] {
            let (status, response) = post(&app, uri, json!({"value": 1})).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(response["code"], "bus.not-allowed", "{uri}");
        }
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed_and_does_not_open() {
        let fake = FakeDdc::default();
        let app = router(state(BusConfig::default(), fake.clone()));
        for uri in [
            "/buses/ddc",
            "/buses/ddc/1/0x37",
            "/buses/ddc/1/0x37/vcp/0x10",
            "/buses/ddc/1/0x37/capabilities",
        ] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }
        let (status, body) =
            post(&app, "/buses/ddc/1/0x37/vcp/0x10/set", json!({"value": 1})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert!(fake.calls().is_empty());
    }
}
