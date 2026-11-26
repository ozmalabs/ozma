// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted I²C surface.
//!
//! The allowlist is a set of exact `BUS:ADDR` pairs. Every read/write checks the
//! pair **before** opening `/dev/i2c-BUS`; detection checks the bus before
//! running `i2cdetect`. Reserved and broadcast addresses (`0x00..=0x07` and
//! `0x78..=0x7f`) are rejected at configuration and request time, so there is no
//! general-call write and no 10-bit/reserved target. The node never probes a bus
//! that is absent from `--i2c-allow`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{
    bounded_timeout, invalid_request, io_failed, json_response, parse_hex_bytes, unavailable,
    BusError, BusLimits, BusState, MAX_ERROR_DETAIL, MAX_REQUEST_BODY_BYTES,
};

/// Lowest non-reserved 7-bit I²C target address accepted by this surface.
pub const MIN_I2C_ADDRESS: u8 = 0x08;
/// Highest non-reserved 7-bit I²C target address accepted by this surface.
pub const MAX_I2C_ADDRESS: u8 = 0x77;
/// Default `i2cdetect` deadline.
pub const DEFAULT_DETECT_TIMEOUT_MS: u64 = 2_000;
/// Smallest accepted `i2cdetect` deadline.
pub const MIN_DETECT_TIMEOUT_MS: u64 = 100;
/// Largest accepted `i2cdetect` deadline.
pub const MAX_DETECT_TIMEOUT_MS: u64 = 10_000;
/// Bounded `i2cdetect` stdout read.
const MAX_DETECT_OUTPUT_BYTES: usize = 64 * 1024;

/// True when `address` is an ordinary 7-bit user target.
///
/// `0x00` is the SMBus general call/broadcast address. `0x01..=0x07` are
/// reserved (CBUS, other bus types, SMBus host, etc.) and `0x78..=0x7f` are
/// reserved for 10-bit addressing. Writing any of these is never permitted.
pub fn is_user_address(address: u8) -> bool {
    (MIN_I2C_ADDRESS..=MAX_I2C_ADDRESS).contains(&address)
}

/// Exact I²C bus/address allowlist.
#[derive(Clone, Debug)]
pub struct I2cConfig {
    allow: BTreeMap<u16, BTreeSet<u8>>,
    /// Opt-in SMBus Packet Error Checking for every transaction.
    pec: bool,
    /// `i2cdetect` deadline. The subprocess is killed when it expires.
    pub detect_timeout: Duration,
}

impl Default for I2cConfig {
    fn default() -> Self {
        Self {
            allow: BTreeMap::new(),
            pec: false,
            detect_timeout: Duration::from_millis(DEFAULT_DETECT_TIMEOUT_MS),
        }
    }
}

impl I2cConfig {
    /// Parse comma/space separated `BUS:ADDR` entries.
    ///
    /// Each entry is `bus:address`. Both fields accept decimal or `0x` hex.
    /// Duplicate pairs are collapsed.
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
        let Some((bus, address)) = token.split_once(':') else {
            return Err(invalid_request(format!(
                "invalid --i2c-allow entry {token:?}; expected BUS:ADDR"
            )));
        };
        if address.contains(':') {
            return Err(invalid_request(format!(
                "invalid --i2c-allow entry {token:?}; expected exactly one ':'"
            )));
        }
        let bus = parse_bus(bus.trim())?;
        let address = parse_address_value(address.trim())?;
        if !is_user_address(address) {
            return Err(invalid_request(format!(
                "--i2c-allow address 0x{address:02x} is reserved or broadcast; use 0x{MIN_I2C_ADDRESS:02x}..=0x{MAX_I2C_ADDRESS:02x}"
            )));
        }
        self.allow.entry(bus).or_default().insert(address);
        Ok(())
    }

    /// Validate the configured deadline and absolute entry ceiling.
    pub fn validate(&self) -> Result<(), BusError> {
        bounded_timeout(
            self.detect_timeout.as_millis() as u64,
            MIN_DETECT_TIMEOUT_MS,
            MAX_DETECT_TIMEOUT_MS,
            "I2C detect",
        )?;
        if self.total_addresses() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "I2C allowlist contains {} addresses, above the {} absolute bound",
                self.total_addresses(),
                super::MAX_DEVICES
            )));
        }
        Ok(())
    }

    /// True when at least one exact pair is allowlisted.
    pub fn is_configured(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Opt in/out of SMBus PEC. PEC is a per-transaction kernel option; no
    /// address is added or probed by enabling it.
    pub fn with_pec(mut self, pec: bool) -> Self {
        self.pec = pec;
        self
    }

    /// True when SMBus PEC was explicitly requested with `--i2c-pec`.
    pub fn pec_enabled(&self) -> bool {
        self.pec
    }

    /// Exact allowlist check used before any open or CLI invocation.
    pub fn allows(&self, bus: u16, address: u8) -> bool {
        self.allow
            .get(&bus)
            .is_some_and(|addresses| addresses.contains(&address))
    }

    /// True when the bus has at least one allowlisted address.
    pub fn allows_bus(&self, bus: u16) -> bool {
        self.allow
            .get(&bus)
            .is_some_and(|addresses| !addresses.is_empty())
    }

    /// Allowlisted addresses for one bus.
    pub fn addresses(&self, bus: u16) -> Option<&BTreeSet<u8>> {
        self.allow.get(&bus)
    }

    /// Allowlisted bus numbers in ascending order.
    pub fn bus_numbers(&self) -> impl Iterator<Item = u16> + '_ {
        self.allow.keys().copied()
    }

    /// Number of exact pairs across all buses.
    pub fn total_addresses(&self) -> usize {
        self.allow.values().map(BTreeSet::len).sum()
    }
}

/// Validate a route/config bus number. Accepts decimal or `0x` hex.
pub fn parse_bus(raw: &str) -> Result<u16, BusError> {
    let raw = raw.trim();
    let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        raw.parse::<u16>()
    };
    parsed.map_err(|_| {
        invalid_request(format!(
            "invalid I2C bus {raw:?}; expected 0..=65535 decimal or 0xHEX"
        ))
    })
}

/// Validate an I²C address string. Accepts decimal or `0x` hex and rejects
/// reserved/broadcast addresses with `bus.invalid-request`.
pub fn parse_address(raw: &str) -> Result<u8, BusError> {
    let raw = raw.trim();
    let address = parse_address_value(raw)?;
    if !is_user_address(address) {
        return Err(invalid_request(format!(
            "I2C address 0x{address:02x} is reserved or broadcast; allowed range is 0x{MIN_I2C_ADDRESS:02x}..=0x{MAX_I2C_ADDRESS:02x}"
        )));
    }
    Ok(address)
}

fn parse_address_value(raw: &str) -> Result<u8, BusError> {
    let raw = raw.trim();
    let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u8::from_str_radix(hex, 16)
    } else {
        raw.parse::<u8>()
    };
    parsed.map_err(|_| {
        invalid_request(format!(
            "invalid I2C address {raw:?}; expected 0..=255 decimal or 0xHEX"
        ))
    })
}

/// `/dev/i2c-N` path for a validated numeric bus.
pub(crate) fn device_path(bus: u16) -> String {
    format!("/dev/i2c-{bus}")
}

/// I²C operation selector.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum I2cOp {
    Byte,
    Word,
    /// SMBus I²C block transfer with an explicit length.
    Block,
    /// SMBus block transfer with a device-supplied count byte. This is the
    /// only block form that supports PEC on the wire.
    #[serde(rename = "smbus-block", alias = "smbus_block")]
    SmbusBlock,
}

impl I2cOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::Word => "word",
            Self::Block => "block",
            Self::SmbusBlock => "smbus-block",
        }
    }
}

/// `POST /buses/i2c/{bus}/{addr}/read` body.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct I2cReadRequest {
    pub op: I2cOp,
    /// SMBus command/register byte. Required for `word`/`block`, optional for
    /// `byte` (absent means the no-register `read_byte` transaction).
    #[serde(default)]
    pub register: Option<u8>,
    /// Block length in bytes. Required for `block`; an optional upper bound
    /// for `smbus-block`, rejected otherwise.
    #[serde(default)]
    pub length: Option<usize>,
}

/// `POST /buses/i2c/{bus}/{addr}/write` body.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct I2cWriteRequest {
    pub op: I2cOp,
    /// SMBus command/register byte. Required for `word`/`block`, optional for
    /// `byte` (absent means the no-register `write_byte` transaction).
    #[serde(default)]
    pub register: Option<u8>,
    /// Byte/word value. `byte` accepts 0..=255; `word` accepts 0..=65535.
    #[serde(default)]
    pub value: Option<u32>,
    /// Hex data for `block` writes, bounded by `max_block_bytes`.
    #[serde(default)]
    pub data: Option<String>,
}

/// Normalized read operation handed to a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum I2cReadOp {
    Byte { register: Option<u8> },
    Word { register: u8 },
    Block { register: u8, length: usize },
    SmbusBlock { register: u8, length: usize },
}

impl I2cReadOp {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Byte { .. } => "byte",
            Self::Word { .. } => "word",
            Self::Block { .. } => "block",
            Self::SmbusBlock { .. } => "smbus-block",
        }
    }

    pub fn register(&self) -> Option<u8> {
        match self {
            Self::Byte { register } => *register,
            Self::Word { register }
            | Self::Block { register, .. }
            | Self::SmbusBlock { register, .. } => Some(*register),
        }
    }
}

/// Normalized write operation handed to a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum I2cWriteOp {
    Byte { register: Option<u8>, value: u8 },
    Word { register: u8, value: u16 },
    Block { register: u8, data: Vec<u8> },
    SmbusBlock { register: u8, data: Vec<u8> },
}

impl I2cWriteOp {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Byte { .. } => "byte",
            Self::Word { .. } => "word",
            Self::Block { .. } => "block",
            Self::SmbusBlock { .. } => "smbus-block",
        }
    }

    pub fn register(&self) -> Option<u8> {
        match self {
            Self::Byte { register, .. } => *register,
            Self::Word { register, .. }
            | Self::Block { register, .. }
            | Self::SmbusBlock { register, .. } => Some(*register),
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            Self::Byte { .. } => 1,
            Self::Word { .. } => 2,
            Self::Block { data, .. } | Self::SmbusBlock { data, .. } => data.len(),
        }
    }
}

impl I2cReadRequest {
    fn into_op(self, max_block_bytes: usize) -> Result<I2cReadOp, BusError> {
        match self.op {
            I2cOp::Byte => {
                if self.length.is_some() {
                    return Err(invalid_request(
                        "byte reads reject `length`; use `op: block` for a bounded block read",
                    ));
                }
                Ok(I2cReadOp::Byte {
                    register: self.register,
                })
            }
            I2cOp::Word => {
                if self.length.is_some() {
                    return Err(invalid_request("word reads reject `length`"));
                }
                let register = self.register.ok_or_else(|| {
                    invalid_request("word reads require a `register` command byte")
                })?;
                Ok(I2cReadOp::Word { register })
            }
            I2cOp::Block => {
                let register = self.register.ok_or_else(|| {
                    invalid_request("block reads require a `register` command byte")
                })?;
                let length = self
                    .length
                    .ok_or_else(|| invalid_request("block reads require `length`"))?;
                if !(1..=max_block_bytes).contains(&length) {
                    return Err(invalid_request(format!(
                        "block length must be between 1 and {max_block_bytes} bytes"
                    )));
                }
                Ok(I2cReadOp::Block { register, length })
            }
            I2cOp::SmbusBlock => {
                let register = self.register.ok_or_else(|| {
                    invalid_request("smbus-block reads require a `register` command byte")
                })?;
                let length = self.length.unwrap_or(max_block_bytes);
                if !(1..=max_block_bytes).contains(&length) {
                    return Err(invalid_request(format!(
                        "smbus-block length must be between 1 and {max_block_bytes} bytes"
                    )));
                }
                Ok(I2cReadOp::SmbusBlock { register, length })
            }
        }
    }
}

impl I2cWriteRequest {
    fn into_op(self, max_block_bytes: usize) -> Result<I2cWriteOp, BusError> {
        match self.op {
            I2cOp::Byte => {
                if self.data.is_some() {
                    return Err(invalid_request("byte writes reject `data`; use `value`"));
                }
                let value = self
                    .value
                    .ok_or_else(|| invalid_request("byte writes require `value`"))?;
                let value = u8::try_from(value)
                    .map_err(|_| invalid_request("byte write value must be between 0 and 255"))?;
                Ok(I2cWriteOp::Byte {
                    register: self.register,
                    value,
                })
            }
            I2cOp::Word => {
                if self.data.is_some() {
                    return Err(invalid_request("word writes reject `data`; use `value`"));
                }
                let register = self.register.ok_or_else(|| {
                    invalid_request("word writes require a `register` command byte")
                })?;
                let value = self
                    .value
                    .ok_or_else(|| invalid_request("word writes require `value`"))?;
                let value = u16::try_from(value)
                    .map_err(|_| invalid_request("word write value must be between 0 and 65535"))?;
                Ok(I2cWriteOp::Word { register, value })
            }
            I2cOp::Block => {
                if self.value.is_some() {
                    return Err(invalid_request("block writes reject `value`; use `data`"));
                }
                let register = self.register.ok_or_else(|| {
                    invalid_request("block writes require a `register` command byte")
                })?;
                let data = self
                    .data
                    .ok_or_else(|| invalid_request("block writes require `data` as hex"))?;
                let data = parse_hex_bytes(&data, max_block_bytes)?;
                if data.is_empty() {
                    return Err(invalid_request(
                        "block writes require at least one data byte",
                    ));
                }
                Ok(I2cWriteOp::Block { register, data })
            }
            I2cOp::SmbusBlock => {
                if self.value.is_some() {
                    return Err(invalid_request(
                        "smbus-block writes reject `value`; use `data`",
                    ));
                }
                let register = self.register.ok_or_else(|| {
                    invalid_request("smbus-block writes require a `register` command byte")
                })?;
                let data = self
                    .data
                    .ok_or_else(|| invalid_request("smbus-block writes require `data` as hex"))?;
                let data = parse_hex_bytes(&data, max_block_bytes)?;
                if data.is_empty() {
                    return Err(invalid_request(
                        "smbus-block writes require at least one data byte",
                    ));
                }
                Ok(I2cWriteOp::SmbusBlock { register, data })
            }
        }
    }
}

/// Backend abstraction. Production uses [`RealI2cBackend`]; tests inject a fake
/// so allowlist enforcement can be observed before any backend call.
pub trait I2cBackend: Send + Sync {
    /// Run a bounded read-only quick scan and return detected user addresses.
    fn detect(&self, bus: u16, timeout: Duration) -> Result<Vec<u8>, BusError>;
    /// Perform a normalized SMBus read. `pec` is the operator's explicit
    /// `--i2c-pec` opt-in and is passed through to the kernel SMBus ioctl.
    fn read(&self, bus: u16, address: u8, op: &I2cReadOp, pec: bool) -> Result<Vec<u8>, BusError>;
    /// Perform a normalized SMBus write. `pec` behaves as for [`Self::read`].
    fn write(&self, bus: u16, address: u8, op: &I2cWriteOp, pec: bool) -> Result<(), BusError>;
}

/// Real Linux `i2c-dev` backend using the safe `i2cdev` wrapper.
pub struct RealI2cBackend;

impl I2cBackend for RealI2cBackend {
    fn detect(&self, bus: u16, timeout: Duration) -> Result<Vec<u8>, BusError> {
        #[cfg(target_os = "linux")]
        {
            detect_real(bus, timeout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, timeout);
            Err(BusError::Unsupported {
                detail: "I2C detection is only implemented on Linux i2c-dev".to_owned(),
            })
        }
    }

    fn read(&self, bus: u16, address: u8, op: &I2cReadOp, pec: bool) -> Result<Vec<u8>, BusError> {
        #[cfg(target_os = "linux")]
        {
            read_real(bus, address, op, pec)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, address, op, pec);
            Err(BusError::Unsupported {
                detail: "I2C is only implemented on Linux i2c-dev".to_owned(),
            })
        }
    }

    fn write(&self, bus: u16, address: u8, op: &I2cWriteOp, pec: bool) -> Result<(), BusError> {
        #[cfg(target_os = "linux")]
        {
            write_real(bus, address, op, pec)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (bus, address, op, pec);
            Err(BusError::Unsupported {
                detail: "I2C is only implemented on Linux i2c-dev".to_owned(),
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn open_real(bus: u16, address: u8, pec: bool) -> Result<i2cdev::linux::LinuxI2CDevice, BusError> {
    use i2cdev::linux::LinuxI2CDevice;

    let path = device_path(bus);
    let context = format!("{path} address 0x{address:02x}");
    let mut device = LinuxI2CDevice::new(&path, address as u16)
        .map_err(|error| map_i2c_error(&format!("opening {context}"), error))?;
    // The kernel accepts I2C_PEC even on adapters that cannot implement it.
    // `--i2c-pec` is an explicit assertion that PEC is on the wire, so verify
    // the adapter functionality first and fail typed instead of silently
    // degrading to a non-PEC transaction. `i2cdetect -F` only queries adapter
    // capabilities; it does not address any device.
    if pec && !pec_supported(bus)? {
        return Err(BusError::Unsupported {
            detail: format!(
                "{path} does not advertise SMBus PEC; remove --i2c-pec for this adapter"
            ),
        });
    }
    device
        .set_smbus_pec(pec)
        .map_err(|error| map_i2c_error(&format!("{context} set PEC={pec}"), error))?;
    Ok(device)
}

#[cfg(target_os = "linux")]
fn pec_supported(bus: u16) -> Result<bool, BusError> {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<BTreeMap<u16, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(value) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&bus)
        .copied()
    {
        return Ok(value);
    }

    let mut child = Command::new("i2cdetect")
        .arg("-F")
        .arg(bus.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => BusError::Unsupported {
                detail: "i2cdetect was not found on PATH; install i2c-tools to verify --i2c-pec"
                    .to_owned(),
            },
            _ => io_failed(format!(
                "could not start i2cdetect -F on bus {bus}: {error}"
            )),
        })?;
    let deadline = Instant::now() + Duration::from_millis(DEFAULT_DETECT_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BusError::Timeout {
                        detail: format!(
                            "i2cdetect -F on bus {bus} exceeded {DEFAULT_DETECT_TIMEOUT_MS} ms"
                        ),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io_failed(format!(
                    "could not wait for i2cdetect -F on bus {bus}: {error}"
                )));
            }
        }
    };
    let mut stdout = String::new();
    if let Some(pipe) = child.stdout.take() {
        use std::io::Read as _;
        let _ = pipe
            .take(MAX_DETECT_OUTPUT_BYTES as u64)
            .read_to_string(&mut stdout);
    }
    if !status.success() {
        return Err(BusError::Unsupported {
            detail: format!("i2cdetect -F on bus {bus} exited with {status}"),
        });
    }
    let supported = stdout.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("smbus pec")
            && line
                .split_whitespace()
                .last()
                .is_some_and(|value| value.eq_ignore_ascii_case("yes"))
    });
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(bus, supported);
    Ok(supported)
}

#[cfg(target_os = "linux")]
fn read_real(bus: u16, address: u8, op: &I2cReadOp, pec: bool) -> Result<Vec<u8>, BusError> {
    use i2cdev::core::I2CDevice;

    let path = device_path(bus);
    let context = format!("{path} address 0x{address:02x}");
    let mut device = open_real(bus, address, pec)?;
    match op {
        I2cReadOp::Byte {
            register: Some(register),
        } => device
            .smbus_read_byte_data(*register)
            .map(|value| vec![value])
            .map_err(|error| map_i2c_error(&format!("{context} read byte"), error)),
        I2cReadOp::Byte { register: None } => device
            .smbus_read_byte()
            .map(|value| vec![value])
            .map_err(|error| map_i2c_error(&format!("{context} read byte"), error)),
        I2cReadOp::Word { register } => device
            .smbus_read_word_data(*register)
            .map(|value| value.to_le_bytes().to_vec())
            .map_err(|error| map_i2c_error(&format!("{context} read word"), error)),
        I2cReadOp::Block { register, length } => device
            .smbus_read_i2c_block_data(*register, *length as u8)
            .map_err(|error| map_i2c_error(&format!("{context} read block"), error)),
        I2cReadOp::SmbusBlock { register, length } => device
            .smbus_read_block_data(*register)
            .map(|mut data| {
                data.truncate(*length);
                data
            })
            .map_err(|error| map_i2c_error(&format!("{context} read smbus-block"), error)),
    }
}

#[cfg(target_os = "linux")]
fn write_real(bus: u16, address: u8, op: &I2cWriteOp, pec: bool) -> Result<(), BusError> {
    use i2cdev::core::I2CDevice;

    let path = device_path(bus);
    let context = format!("{path} address 0x{address:02x}");
    let mut device = open_real(bus, address, pec)?;
    let result = match op {
        I2cWriteOp::Byte {
            register: Some(register),
            value,
        } => device.smbus_write_byte_data(*register, *value),
        I2cWriteOp::Byte {
            register: None,
            value,
        } => device.smbus_write_byte(*value),
        I2cWriteOp::Word { register, value } => device.smbus_write_word_data(*register, *value),
        I2cWriteOp::Block { register, data } => device.smbus_write_i2c_block_data(*register, data),
        I2cWriteOp::SmbusBlock { register, data } => device.smbus_write_block_data(*register, data),
    };
    result.map_err(|error| map_i2c_error(&format!("{context} write"), error))
}

#[cfg(target_os = "linux")]
fn map_i2c_error(context: &str, error: i2cdev::linux::LinuxI2CError) -> BusError {
    let error = std::io::Error::from(error);
    let detail = format!("{context}: {error}");
    if error.kind() == std::io::ErrorKind::TimedOut || error.raw_os_error() == Some(libc::ETIMEDOUT)
    {
        BusError::Timeout {
            detail: bounded_detail(&detail),
        }
    } else if error.kind() == std::io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(libc::ENXIO) | Some(libc::ENODEV))
    {
        unavailable(detail)
    } else if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
        BusError::Unsupported { detail }
    } else {
        io_failed(detail)
    }
}

#[cfg(target_os = "linux")]
fn detect_real(bus: u16, timeout: Duration) -> Result<Vec<u8>, BusError> {
    let device = device_path(bus);
    if !Path::new(&device).exists() {
        return Err(unavailable(format!(
            "I2C bus {bus} device {device} is not present"
        )));
    }
    let mut child = Command::new("i2cdetect")
        .arg("-y")
        .arg("-r")
        .arg(bus.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => BusError::Unsupported {
                detail: "i2cdetect was not found on PATH; install i2c-tools".to_owned(),
            },
            _ => io_failed(format!("could not start i2cdetect on bus {bus}: {error}")),
        })?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BusError::Timeout {
                        detail: format!(
                            "i2cdetect on bus {bus} exceeded {} ms",
                            timeout.as_millis()
                        ),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io_failed(format!(
                    "could not wait for i2cdetect on bus {bus}: {error}"
                )));
            }
        }
    };

    let mut stdout = String::new();
    if let Some(pipe) = child.stdout.take() {
        use std::io::Read as _;
        let _ = pipe
            .take(MAX_DETECT_OUTPUT_BYTES as u64)
            .read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(pipe) = child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.take(2 * 1024).read_to_string(&mut stderr);
    }
    if !status.success() {
        let detail = if stderr.trim().is_empty() {
            format!("i2cdetect on bus {bus} exited with {status}")
        } else {
            format!("i2cdetect on bus {bus} failed: {}", stderr.trim())
        };
        return Err(io_failed(detail));
    }
    Ok(parse_detect_output(&stdout))
}

/// Parse the address cells from `i2cdetect -y -r` output.
fn parse_detect_output(text: &str) -> Vec<u8> {
    let mut found = BTreeSet::new();
    for line in text.lines() {
        let Some((_row, rest)) = line.split_once(':') else {
            continue;
        };
        for token in rest.split_whitespace() {
            let token = token.strip_prefix("0x").unwrap_or(token);
            if token.len() != 2 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(address) = u8::from_str_radix(token, 16) else {
                continue;
            };
            if is_user_address(address) {
                found.insert(address);
            }
        }
    }
    found.into_iter().collect()
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

/// One bus in `GET /buses/i2c`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct I2cBusInfo {
    pub bus: u16,
    pub device: String,
    pub available: bool,
    pub addresses: Vec<u8>,
}

/// `GET /buses/i2c` response. This only reads configuration and stats device
/// nodes; it does not scan any bus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct I2cListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub buses: Vec<I2cBusInfo>,
    pub limits: BusLimits,
}

/// `GET /buses/i2c/{bus}/detect` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct I2cDetectResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub device: String,
    /// The `/dev/i2c-N` node exists.
    pub available: bool,
    /// A bounded quick scan ran.
    pub scanned: bool,
    /// Allowlisted addresses detected by the scan.
    pub addresses: Vec<u8>,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
}

/// `POST /buses/i2c/{bus}/{addr}/read` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct I2cReadResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register: Option<u8>,
    pub bytes: usize,
    /// Assembled value for `byte`/`word` reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u32>,
    /// Exact returned bytes, lower-case hex.
    pub data: String,
    pub encoding: &'static str,
    /// SMBus PEC was enabled for this transaction (`--i2c-pec`).
    pub pec: bool,
}

/// `POST /buses/i2c/{bus}/{addr}/write` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct I2cWriteResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub bus: u16,
    pub address: u8,
    pub op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register: Option<u8>,
    pub bytes: usize,
    /// SMBus PEC was enabled for this transaction (`--i2c-pec`).
    pub pec: bool,
}

impl BusState {
    pub(super) fn i2c_list(&self) -> Result<I2cListResponse, BusError> {
        if !self.inner.config.i2c.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut buses: Vec<I2cBusInfo> = self
            .inner
            .config
            .i2c
            .bus_numbers()
            .map(|bus| {
                let device = device_path(bus);
                I2cBusInfo {
                    bus,
                    available: Path::new(&device).exists(),
                    device,
                    addresses: self
                        .inner
                        .config
                        .i2c
                        .addresses(bus)
                        .map(|addresses| addresses.iter().copied().collect())
                        .unwrap_or_default(),
                }
            })
            .collect();
        let matched = buses.len();
        let truncated = matched > limit;
        buses.truncate(limit);
        Ok(I2cListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: buses.len(),
            limit,
            truncated,
            buses,
            limits: self.limits(),
        })
    }

    pub(super) fn i2c_detect(&self, bus_raw: &str) -> Result<I2cDetectResponse, BusError> {
        if !self.inner.config.i2c.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let bus = parse_bus(bus_raw)?;
        if !self.inner.config.i2c.allows_bus(bus) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "I2C bus {bus} has no allowlisted addresses; add --i2c-allow {bus}:0xADDR"
                ),
            });
        }
        let device = device_path(bus);
        let timeout = self.inner.config.i2c.detect_timeout;
        let detected = self.inner.i2c.detect(bus, timeout)?;
        let allowed = self
            .inner
            .config
            .i2c
            .addresses(bus)
            .cloned()
            .unwrap_or_default();
        let mut addresses: Vec<u8> = detected
            .into_iter()
            .filter(|address| allowed.contains(address))
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        let limit = self.inner.config.max_devices;
        let truncated = addresses.len() > limit;
        addresses.truncate(limit);
        Ok(I2cDetectResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            available: Path::new(&device).exists(),
            scanned: true,
            count: addresses.len(),
            addresses,
            limit,
            truncated,
            device,
        })
    }

    pub(super) fn i2c_read(
        &self,
        bus_raw: &str,
        address_raw: &str,
        request: I2cReadRequest,
    ) -> Result<I2cReadResponse, BusError> {
        if !self.inner.config.i2c.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let bus = parse_bus(bus_raw)?;
        let address = parse_address(address_raw)?;
        if !self.inner.config.i2c.allows(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "I2C bus {bus} address 0x{address:02x} is not allowlisted; add --i2c-allow {bus}:0x{address:02x}"
                ),
            });
        }
        let op = request.into_op(self.inner.config.max_block_bytes)?;
        let pec = self.inner.config.i2c.pec_enabled();
        let mut data = self.inner.i2c.read(bus, address, &op, pec)?;
        match &op {
            I2cReadOp::Byte { .. } if data.len() != 1 => {
                return Err(io_failed(format!(
                    "I2C backend returned {} bytes for a byte read",
                    data.len()
                )))
            }
            I2cReadOp::Word { .. } if data.len() != 2 => {
                return Err(io_failed(format!(
                    "I2C backend returned {} bytes for a word read",
                    data.len()
                )))
            }
            _ => {}
        }
        if let I2cReadOp::Block { length, .. } | I2cReadOp::SmbusBlock { length, .. } = &op {
            data.truncate(*length);
        }
        let value = match &op {
            I2cReadOp::Byte { .. } => data.first().map(|byte| u32::from(*byte)),
            I2cReadOp::Word { .. } => {
                let bytes: [u8; 2] = [
                    data.first().copied().unwrap_or(0),
                    data.get(1).copied().unwrap_or(0),
                ];
                Some(u32::from(u16::from_le_bytes(bytes)))
            }
            I2cReadOp::Block { .. } | I2cReadOp::SmbusBlock { .. } => None,
        };
        Ok(I2cReadResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            op: op.op_name(),
            register: op.register(),
            bytes: data.len(),
            value,
            data: super::encode_hex(&data),
            encoding: "hex",
            pec,
        })
    }

    pub(super) fn i2c_write(
        &self,
        bus_raw: &str,
        address_raw: &str,
        request: I2cWriteRequest,
    ) -> Result<I2cWriteResponse, BusError> {
        if !self.inner.config.i2c.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let bus = parse_bus(bus_raw)?;
        let address = parse_address(address_raw)?;
        if !self.inner.config.i2c.allows(bus, address) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "I2C bus {bus} address 0x{address:02x} is not allowlisted; add --i2c-allow {bus}:0x{address:02x}"
                ),
            });
        }
        let op = request.into_op(self.inner.config.max_block_bytes)?;
        let pec = self.inner.config.i2c.pec_enabled();
        self.inner.i2c.write(bus, address, &op, pec)?;
        Ok(I2cWriteResponse {
            ok: true,
            supported: true,
            configured: true,
            bus,
            address,
            op: op.op_name(),
            register: op.register(),
            bytes: op.byte_len(),
            pec,
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/i2c", get(list_handler))
        .route("/buses/i2c/:bus/detect", get(detect_handler))
        .route(
            "/buses/i2c/:bus/:addr/read",
            post(read_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .route(
            "/buses/i2c/:bus/:addr/write",
            post(write_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.i2c_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "I2C list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn detect_handler(
    State(state): State<BusState>,
    AxumPath(bus): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.i2c_detect(&bus)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "I2C detect task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn read_handler(
    State(state): State<BusState>,
    AxumPath((bus, address)): AxumPath<(String, String)>,
    body: Result<axum::Json<I2cReadRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.i2c_read(&bus, &address, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "I2C read task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn write_handler(
    State(state): State<BusState>,
    AxumPath((bus, address)): AxumPath<(String, String)>,
    body: Result<axum::Json<I2cWriteRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.i2c_write(&bus, &address, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "I2C write task failed: {}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::spi::RealSpiBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Detect(u16),
        Read(u16, u8, I2cReadOp, bool),
        Write(u16, u8, I2cWriteOp, bool),
    }

    #[derive(Default)]
    struct FakeInner {
        calls: Vec<Call>,
        detected: Vec<u8>,
        detect_error: Option<BusError>,
        reads: VecDeque<Result<Vec<u8>, BusError>>,
        writes: VecDeque<Result<(), BusError>>,
    }

    #[derive(Clone, Default)]
    struct FakeI2c {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl FakeI2c {
        fn with_detected(detected: Vec<u8>) -> Self {
            let fake = Self::default();
            fake.inner.lock().unwrap().detected = detected;
            fake
        }

        fn queue_read(&self, result: Result<Vec<u8>, BusError>) {
            self.inner.lock().unwrap().reads.push_back(result);
        }

        fn queue_write(&self, result: Result<(), BusError>) {
            self.inner.lock().unwrap().writes.push_back(result);
        }

        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }
    }

    impl I2cBackend for FakeI2c {
        fn detect(&self, bus: u16, _timeout: Duration) -> Result<Vec<u8>, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Detect(bus));
            if let Some(error) = inner.detect_error.clone() {
                return Err(error);
            }
            Ok(inner.detected.clone())
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
            inner.reads.pop_front().unwrap_or_else(|| Ok(vec![0x5a]))
        }

        fn write(&self, bus: u16, address: u8, op: &I2cWriteOp, pec: bool) -> Result<(), BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Write(bus, address, op.clone(), pec));
            inner.writes.pop_front().unwrap_or(Ok(()))
        }
    }

    fn config() -> BusConfig {
        BusConfig::new()
            .with_i2c_allow(&["1:0x50, 1:0x52".to_owned(), "2:80".to_owned()])
            .expect("valid allowlist")
    }

    fn state(config: BusConfig, fake: FakeI2c) -> BusState {
        BusState::with_backends(config, Arc::new(fake), Arc::new(RealSpiBackend))
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
    fn allowlist_parsing_is_exact_and_rejects_reserved_addresses() {
        let config =
            I2cConfig::from_entries(["1:0x50,1:0x52", "2:80", "0x03:0x51", "1:0x50"]).unwrap();
        assert!(config.allows(1, 0x50));
        assert!(config.allows(1, 0x52));
        assert!(config.allows(2, 80));
        assert!(config.allows(3, 0x51));
        assert_eq!(config.total_addresses(), 4);
        assert!(!config.allows(1, 0x51));
        assert!(!config.allows_bus(4));

        for bad in [
            "1",
            "1:",
            ":0x50",
            "1:0x00",
            "1:0x02",
            "1:0x07",
            "1:0x78",
            "1:0x7f",
            "1:0x100",
            "65536:0x50",
            "1:0x50:0x51",
        ] {
            assert!(
                I2cConfig::from_entries([bad]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn route_ids_are_parsed_without_defaulting() {
        assert_eq!(parse_bus("1").unwrap(), 1);
        assert_eq!(parse_bus("0x10").unwrap(), 16);
        assert!(parse_bus("-1").is_err());
        assert!(parse_bus("bus").is_err());
        assert_eq!(parse_address("0x50").unwrap(), 0x50);
        assert_eq!(parse_address("80").unwrap(), 80);
        for reserved in ["0x00", "0x07", "0x78", "0x7f", "127"] {
            assert!(parse_address(reserved).is_err(), "reserved {reserved}");
        }
    }

    #[test]
    fn detect_output_is_parsed_and_uu_cells_are_ignored() {
        let output = "\
     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f
00:          -- -- -- -- -- -- -- -- -- -- -- -- --
10: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
20: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
30: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
40: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
50: 50 UU -- -- -- 55 -- -- -- -- -- -- -- -- -- --
60: -- -- -- -- -- -- -- -- -- -- -- -- -- -- -- --
70: -- -- -- -- -- -- -- 77
";
        assert_eq!(parse_detect_output(output), vec![0x50, 0x55, 0x77]);
    }

    #[tokio::test]
    async fn list_only_reads_configuration_and_never_probes() {
        let fake = FakeI2c::with_detected(vec![0x50]);
        let app = router(state(config(), fake.clone()));
        let (status, body) = get(&app, "/buses/i2c").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        let buses = body["buses"].as_array().unwrap();
        assert_eq!(buses[0]["bus"], 1);
        assert_eq!(buses[0]["device"], "/dev/i2c-1");
        assert_eq!(buses[0]["addresses"], json!([80, 82]));
        assert_eq!(buses[1]["bus"], 2);
        assert_eq!(buses[1]["addresses"], json!([80]));
        assert!(fake.calls().is_empty(), "listing must not scan buses");
    }

    #[tokio::test]
    async fn detect_requires_bus_allowlist_and_filters_addresses() {
        let fake = FakeI2c::with_detected(vec![0x50, 0x51, 0x52, 0x77]);
        let app = router(state(config(), fake.clone()));

        let (status, body) = get(&app, "/buses/i2c/1/detect").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scanned"], true);
        assert_eq!(body["addresses"], json!([80, 82]));
        assert_eq!(body["count"], 2);
        assert_eq!(fake.calls(), vec![Call::Detect(1)]);

        let (status, body) = get(&app, "/buses/i2c/9/detect").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert_eq!(fake.calls(), vec![Call::Detect(1)]);

        let (status, body) = get(&app, "/buses/i2c/nope/detect").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");
        assert_eq!(fake.calls(), vec![Call::Detect(1)]);
    }

    #[tokio::test]
    async fn read_ops_are_normalized_and_bounded() {
        let fake = FakeI2c::default();
        fake.queue_read(Ok(vec![0x12]));
        fake.queue_read(Ok(vec![0x34, 0x12]));
        fake.queue_read(Ok(vec![1, 2, 3]));
        let app = router(state(config(), fake.clone()));

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/read",
            json!({"op": "byte", "register": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["op"], "byte");
        assert_eq!(body["value"], 0x12);
        assert_eq!(body["data"], "12");
        assert_eq!(body["bytes"], 1);

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/read",
            json!({"op": "word", "register": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], 0x1234);
        assert_eq!(body["data"], "3412");

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x52/read",
            json!({"op": "block", "register": 0, "length": 3}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"], "010203");
        assert_eq!(body["bytes"], 3);
        assert_eq!(body["register"], 0);

        let calls = fake.calls();
        assert_eq!(
            calls[0],
            Call::Read(1, 0x50, I2cReadOp::Byte { register: Some(0) }, false)
        );
        assert_eq!(
            calls[1],
            Call::Read(1, 0x50, I2cReadOp::Word { register: 0 }, false)
        );
        assert_eq!(
            calls[2],
            Call::Read(
                1,
                0x52,
                I2cReadOp::Block {
                    register: 0,
                    length: 3
                },
                false
            )
        );
    }

    #[tokio::test]
    async fn write_ops_are_exact_and_never_reach_unlisted_addresses() {
        let fake = FakeI2c::default();
        let app = router(state(config(), fake.clone()));

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/write",
            json!({"op": "byte", "register": 1, "value": 255}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bytes"], 1);

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x52/write",
            json!({"op": "word", "register": 2, "value": 4660}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bytes"], 2);

        let (status, body) = post(
            &app,
            "/buses/i2c/2/80/write",
            json!({"op": "block", "register": 3, "data": "de ad be ef"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bytes"], 4);

        let calls = fake.calls();
        assert_eq!(
            calls[0],
            Call::Write(
                1,
                0x50,
                I2cWriteOp::Byte {
                    register: Some(1),
                    value: 255
                },
                false
            )
        );
        assert_eq!(
            calls[1],
            Call::Write(
                1,
                0x52,
                I2cWriteOp::Word {
                    register: 2,
                    value: 4660
                },
                false
            )
        );
        assert_eq!(
            calls[2],
            Call::Write(
                2,
                80,
                I2cWriteOp::Block {
                    register: 3,
                    data: vec![0xde, 0xad, 0xbe, 0xef]
                },
                false
            )
        );

        let before = fake.calls().len();
        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x51/write",
            json!({"op": "byte", "register": 0, "value": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x00/write",
            json!({"op": "byte", "value": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");
        assert_eq!(
            fake.calls().len(),
            before,
            "rejected writes must not call the backend"
        );
    }

    #[tokio::test]
    async fn malformed_read_and_write_bodies_are_typed() {
        let fake = FakeI2c::default();
        let app = router(state(config(), fake.clone()));

        for body in [
            json!({"op": "word"}),
            json!({"op": "byte", "register": 0, "length": 1}),
            json!({"op": "block", "register": 0}),
            json!({"op": "block", "register": 0, "length": 0}),
            json!({"op": "block", "register": 0, "length": 33}),
            json!({"op": "nonsense"}),
            json!({"op": "byte", "register": 0, "unknown": true}),
        ] {
            let (status, body) = post(&app, "/buses/i2c/1/0x50/read", body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "read body should be rejected"
            );
            assert_eq!(body["code"], "bus.invalid-request");
        }

        for body in [
            json!({"op": "byte"}),
            json!({"op": "byte", "value": 256}),
            json!({"op": "byte", "value": 1, "data": "00"}),
            json!({"op": "word", "value": 1}),
            json!({"op": "word", "register": 0, "value": 65536}),
            json!({"op": "block", "register": 0}),
            json!({"op": "block", "register": 0, "data": ""}),
            json!({"op": "block", "register": 0, "data": "00".repeat(33)}),
            json!({"op": "block", "register": 0, "value": 1, "data": "00"}),
            json!({"op": "byte", "value": 1, "extra": 2}),
        ] {
            let (status, body) = post(&app, "/buses/i2c/1/0x50/write", body).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "write body should be rejected"
            );
            assert_eq!(body["code"], "bus.invalid-request");
        }
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn backend_failures_map_to_typed_codes() {
        let fake = FakeI2c::default();
        fake.queue_read(Err(BusError::Timeout {
            detail: "device did not answer".to_owned(),
        }));
        fake.queue_write(Err(BusError::IoFailed {
            detail: "NAK".to_owned(),
        }));
        fake.inner.lock().unwrap().detect_error = Some(BusError::Unsupported {
            detail: "i2cdetect missing".to_owned(),
        });
        let app = router(state(config(), fake));

        let (status, body) = post(&app, "/buses/i2c/1/0x50/read", json!({"op": "byte"})).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["code"], "bus.timeout");

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/write",
            json!({"op": "byte", "value": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "bus.io-failed");

        let (status, body) = get(&app, "/buses/i2c/1/detect").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "bus.unsupported");
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed_and_does_not_open_anything() {
        let fake = FakeI2c::default();
        let app = router(state(BusConfig::default(), fake.clone()));
        for (method, uri) in [("GET", "/buses/i2c"), ("GET", "/buses/i2c/1/detect")] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], "bus.not-configured");
            assert_eq!(body["configured"], false);
        }
        let (status, body) = post(&app, "/buses/i2c/1/0x50/read", json!({"op": "byte"})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn smbus_block_and_pec_are_bounded_and_explicit() {
        let fake = FakeI2c::default();
        fake.queue_read(Ok(vec![1, 2, 3, 4, 5]));
        let app = router(state(config(), fake.clone()));

        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/read",
            json!({"op": "smbus-block", "register": 0x20, "length": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["op"], "smbus-block");
        assert_eq!(body["data"], "01020304");
        assert_eq!(body["bytes"], 4);
        assert_eq!(body["pec"], false);
        assert_eq!(
            fake.calls(),
            vec![Call::Read(
                1,
                0x50,
                I2cReadOp::SmbusBlock {
                    register: 0x20,
                    length: 4
                },
                false
            )]
        );

        let fake = FakeI2c::default();
        let app = router(state(config(), fake.clone()));
        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x52/write",
            json!({"op": "smbus-block", "register": 0x21, "data": "aa bb"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["op"], "smbus-block");
        assert_eq!(body["bytes"], 2);
        assert_eq!(
            fake.calls(),
            vec![Call::Write(
                1,
                0x52,
                I2cWriteOp::SmbusBlock {
                    register: 0x21,
                    data: vec![0xaa, 0xbb]
                },
                false
            )]
        );

        let fake = FakeI2c::default();
        let pec_config = config().with_i2c_pec(true);
        let app = router(state(pec_config, fake.clone()));
        let (status, body) = post(
            &app,
            "/buses/i2c/1/0x50/read",
            json!({"op": "byte", "register": 0x10}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pec"], true);
        assert_eq!(
            fake.calls(),
            vec![Call::Read(
                1,
                0x50,
                I2cReadOp::Byte {
                    register: Some(0x10)
                },
                true
            )]
        );

        let fake = FakeI2c::default();
        let app = router(state(config(), fake.clone()));
        for body in [
            json!({"op": "smbus-block"}),
            json!({"op": "smbus-block", "register": 0, "length": 0}),
            json!({"op": "smbus-block", "register": 0, "length": 33}),
        ] {
            let (status, response) = post(&app, "/buses/i2c/1/0x50/read", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["code"], "bus.invalid-request");
        }
        assert!(fake.calls().is_empty());
    }

    /// Live evidence against the Linux `i2c-stub` virtual adapter.
    ///
    /// This test is ignored by default and requires an explicitly created stub
    /// bus; it never scans or selects a physical bus. Typical invocation:
    ///
    /// ```text
    /// sudo modprobe i2c-stub chip_addr=0x50
    /// # find the new adapter whose /sys/.../name is "SMBus stub driver"
    /// sudo env OZMA_LIVE_I2C_BUS=22 cargo test -p ozma-node --lib \
    ///     buses::i2c::tests::live_i2c_stub_real_backend -- --ignored --nocapture
    /// sudo modprobe -r i2c_stub
    /// ```
    /// Live typed evidence that `i2c-stub` advertises neither SMBus block nor
    /// PEC. The code path is exercised by the fake tests; this test proves the
    /// real backend surfaces `bus.unsupported` rather than silently dropping
    /// PEC or performing a different transaction.
    ///
    /// ```text
    /// sudo modprobe i2c-stub chip_addr=0x50
    /// sudo env OZMA_LIVE_I2C_BUS=22 cargo test -p ozma-node --lib \
    ///     buses::i2c::tests::live_i2c_stub_pec_and_smbus_block_are_typed_unsupported \
    ///     -- --ignored --nocapture
    /// sudo modprobe -r i2c_stub
    /// ```
    #[test]
    #[ignore = "requires a live Linux i2c-stub bus; set OZMA_LIVE_I2C_BUS"]
    fn live_i2c_stub_pec_and_smbus_block_are_typed_unsupported() {
        let bus: u16 = std::env::var("OZMA_LIVE_I2C_BUS")
            .expect("set OZMA_LIVE_I2C_BUS to the i2c-stub adapter number")
            .parse()
            .expect("numeric OZMA_LIVE_I2C_BUS");
        let allow = format!("{bus}:0x50");
        let config = BusConfig::new()
            .with_i2c_allow(&[allow])
            .expect("valid allowlist")
            .with_i2c_pec(true);
        let state = BusState::new(config).expect("valid state");
        let bus = bus.to_string();

        let error = state
            .i2c_read(
                &bus,
                "0x50",
                I2cReadRequest {
                    op: I2cOp::Byte,
                    register: Some(0x10),
                    length: None,
                },
            )
            .expect_err("i2c-stub does not advertise SMBus PEC");
        assert!(
            matches!(error, BusError::Unsupported { .. }),
            "expected typed unsupported for PEC, got {error:?}"
        );

        let config = BusConfig::new()
            .with_i2c_allow(&[format!("{bus}:0x50")])
            .expect("valid allowlist");
        let state = BusState::new(config).expect("valid state");
        let error = state
            .i2c_read(
                &bus,
                "0x50",
                I2cReadRequest {
                    op: I2cOp::SmbusBlock,
                    register: Some(0x10),
                    length: Some(4),
                },
            )
            .expect_err("i2c-stub does not advertise SMBus block reads");
        assert!(
            matches!(error, BusError::Unsupported { .. }),
            "expected typed unsupported for SMBus block, got {error:?}"
        );
    }

    #[test]
    #[ignore = "requires a live Linux i2c-stub bus; set OZMA_LIVE_I2C_BUS"]
    fn live_i2c_stub_real_backend_read_write_detect() {
        let bus: u16 = std::env::var("OZMA_LIVE_I2C_BUS")
            .expect("set OZMA_LIVE_I2C_BUS to the i2c-stub adapter number")
            .parse()
            .expect("numeric OZMA_LIVE_I2C_BUS");
        let address = parse_address(
            &std::env::var("OZMA_LIVE_I2C_ADDR").unwrap_or_else(|_| "0x50".to_owned()),
        )
        .expect("valid OZMA_LIVE_I2C_ADDR");
        let allow = format!("{bus}:0x{address:02x}");
        let config = BusConfig::new()
            .with_i2c_allow(&[allow])
            .expect("valid allowlist");
        let state = BusState::new(config).expect("valid state");
        let bus = bus.to_string();
        let address = format!("0x{address:02x}");

        let write = state
            .i2c_write(
                &bus,
                &address,
                I2cWriteRequest {
                    op: I2cOp::Byte,
                    register: Some(0x10),
                    value: Some(0x5a),
                    data: None,
                },
            )
            .expect("byte write");
        assert_eq!(write.bytes, 1);
        let read = state
            .i2c_read(
                &bus,
                &address,
                I2cReadRequest {
                    op: I2cOp::Byte,
                    register: Some(0x10),
                    length: None,
                },
            )
            .expect("byte read");
        assert_eq!(read.value, Some(0x5a));
        assert_eq!(read.data, "5a");

        state
            .i2c_write(
                &bus,
                &address,
                I2cWriteRequest {
                    op: I2cOp::Word,
                    register: Some(0x11),
                    value: Some(0x1234),
                    data: None,
                },
            )
            .expect("word write");
        let read = state
            .i2c_read(
                &bus,
                &address,
                I2cReadRequest {
                    op: I2cOp::Word,
                    register: Some(0x11),
                    length: None,
                },
            )
            .expect("word read");
        assert_eq!(read.value, Some(0x1234));

        state
            .i2c_write(
                &bus,
                &address,
                I2cWriteRequest {
                    op: I2cOp::Block,
                    register: Some(0x12),
                    value: None,
                    data: Some("01 02 03".to_owned()),
                },
            )
            .expect("block write");
        let read = state
            .i2c_read(
                &bus,
                &address,
                I2cReadRequest {
                    op: I2cOp::Block,
                    register: Some(0x12),
                    length: Some(3),
                },
            )
            .expect("block read");
        assert_eq!(read.data, "010203");

        let detected = state.i2c_detect(&bus).expect("detect");
        assert!(detected.scanned);
        assert_eq!(detected.addresses, vec![0x50]);
    }
}
