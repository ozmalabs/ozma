// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Optional Windows shared-memory and registry bridge for AIDA64 sensor data.
//!
//! AIDA64's native Windows exports are proprietary implementation details:
//! the `AIDA64_SensorValues` shared-memory record layout, its Registry layout,
//! and its WMI classes are not documented by FinalWire as stable public
//! formats. This module does **not** claim to reproduce any of those layouts
//! and does not hard-code AIDA64 field offsets. Instead it publishes the same
//! [`AidaSnapshot`] model used by the AIDA64-compatible HTTP/SSE listener
//! through a small, documented Ozma format for Windows consumers.
//!
//! The portable protocol layer in this file (bounded binary map, Registry
//! value map, and sink trait) compiles on every platform so it can be tested
//! on Linux. The actual named shared-memory and Registry backend is compiled
//! only on Windows and is disabled by default. The existing RemoteSensor
//! HTTP/SSE listener remains the AIDA64-compatible surface.
//!
//! WMI is deliberately not implemented. A WMI provider is not just a writer:
//! it is a COM server plus a MOF class declaration that must be registered
//! with WMI (normally by an installer with appropriate privileges). Shipping
//! that as part of this bridge would require a separate installable provider;
//! writing Registry or shared-memory values does not create WMI classes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use crate::aida64::{AidaSensor, AidaSnapshot, AidaValue};

/// Name of the named shared-memory object created by the Windows backend.
pub const SHARED_MEMORY_NAME: &str = "Local\\Ozma_Aida64_SensorValues";

/// Registry key (under `HKEY_CURRENT_USER`) used by the Windows backend.
pub const REGISTRY_KEY_PATH: &str = "Software\\Ozma\\Aida64\\SensorValues";

/// Magic at the start of the shared-memory header.
pub const SHARED_MEMORY_MAGIC: [u8; 8] = *b"OZMASHRM";

/// Shared-memory header version. The encoded payload has its own version.
pub const SHARED_MEMORY_VERSION: u16 = 1;

/// Size of the shared-memory header; the encoded blob starts after it.
pub const SHARED_MEMORY_HEADER_BYTES: usize = 24;
pub const SHARED_MEMORY_MAGIC_OFFSET: usize = 0;
pub const SHARED_MEMORY_VERSION_OFFSET: usize = 8;
pub const SHARED_MEMORY_FLAGS_OFFSET: usize = 10;
pub const SHARED_MEMORY_SEQUENCE_OFFSET: usize = 12;
pub const SHARED_MEMORY_PAYLOAD_LEN_OFFSET: usize = 16;
pub const SHARED_MEMORY_RESERVED_OFFSET: usize = 20;

/// Magic at the start of the encoded [`AidaSnapshot`] blob.
pub const PROTOCOL_MAGIC: [u8; 8] = *b"OZMAAIDA";

/// Version of the encoded snapshot blob.
pub const PROTOCOL_VERSION: u16 = 1;

/// Header size of the encoded snapshot blob.
pub const PROTOCOL_HEADER_BYTES: usize = 24;

/// Hard upper bound on sensors in one snapshot.
pub const MAX_SENSORS: usize = 4096;

/// Per-field byte limits for the encoded snapshot.
pub const MAX_ID_BYTES: usize = 64;
pub const MAX_LABEL_BYTES: usize = 256;
pub const MAX_UNIT_BYTES: usize = 32;
pub const MAX_KIND_BYTES: usize = 32;
pub const MAX_COLOR_BYTES: usize = 32;
pub const MAX_TEXT_VALUE_BYTES: usize = 512;

/// Hard upper bound on an encoded snapshot, independent of configuration.
pub const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;

/// Default payload limit for the bridge when enabled.
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

/// Configuration lower bound; tiny mappings are not useful.
pub const MIN_MAX_BYTES: usize = 4096;

/// Configuration upper bound; keep a hostile or accidental value bounded.
pub const MAX_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Registry limits. `MAX_REGISTRY_ENTRIES` is five per sensor plus metadata.
pub const MAX_REGISTRY_ENTRIES: usize = MAX_SENSORS * 5 + 3;
pub const MAX_REGISTRY_NAME_BYTES: usize = 128;
pub const MAX_REGISTRY_TEXT_BYTES: usize = 512;

/// Human-readable format marker stored as a Registry string value.
pub const REGISTRY_FORMAT: &str = "ozma-aida64-sensor-map-v1";

/// Errors from the portable protocol or the optional Windows backend.
#[derive(Debug, thiserror::Error)]
pub enum WindowsBridgeError {
    #[error("encoded AIDA64 map is {size} bytes, exceeding the {max}-byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("encoded AIDA64 map is truncated: need {needed} bytes, have {available}")]
    Truncated { needed: usize, available: usize },
    #[error("encoded AIDA64 map has trailing bytes at offset {offset}")]
    TrailingBytes { offset: usize },
    #[error("invalid AIDA64 map magic")]
    BadMagic,
    #[error("unsupported AIDA64 map version {0}")]
    UnsupportedVersion(u16),
    #[error("unsupported AIDA64 map flags 0x{0:04x}")]
    UnsupportedFlags(u16),
    #[error("unsupported AIDA64 sensor value tag {0}")]
    UnsupportedValueTag(u8),
    #[error("AIDA64 snapshot contains {count} sensors, exceeding the {max} limit")]
    TooManySensors { count: usize, max: usize },
    #[error("duplicate AIDA64 sensor id {0:?}")]
    DuplicateSensorId(String),
    #[error("AIDA64 sensor {id:?} has a non-finite value")]
    NonFiniteValue { id: String },
    #[error("AIDA64 snapshot timestamp is not finite")]
    NonFiniteTimestamp,
    #[error("{field} is {len} bytes, exceeding the {max}-byte limit")]
    StringTooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{field} is not valid UTF-8")]
    InvalidUtf8 { field: &'static str },
    #[error("AIDA64 sensor id {id:?} cannot be used as a Registry value name")]
    InvalidIdentifier { id: String },
    #[error("AIDA64 Registry value {0:?} is duplicated")]
    DuplicateRegistryValue(String),
    #[error("AIDA64 Registry value name {0:?} is invalid")]
    InvalidRegistryName(String),
    #[error("AIDA64 Registry text value {name:?} contains a NUL byte")]
    InvalidRegistryText { name: String },
    #[error("AIDA64 Registry value {name:?} exceeds the {max}-byte limit")]
    RegistryValueTooLong { name: String, max: usize },
    #[error("AIDA64 Registry map contains {count} values, exceeding the {max} limit")]
    TooManyRegistryValues { count: usize, max: usize },
    #[error("Windows shared-memory {operation} failed (GetLastError={code})")]
    SharedMemory { operation: &'static str, code: u32 },
    #[error("Windows Registry {operation} failed (status={code})")]
    Registry { operation: &'static str, code: i32 },
    #[error("no Windows bridge sinks could be opened")]
    NoSinksAvailable,
    #[error("Windows bridge state lock is poisoned")]
    StatePoisoned,
}

/// Feature switches and payload bound for the Windows bridge.
///
/// Both transports default to off. The bridge is intentionally not wired into
/// the existing AIDA64 HTTP listener or the node command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsBridgeConfig {
    pub shared_memory: bool,
    pub registry: bool,
    pub max_bytes: usize,
}

impl Default for WindowsBridgeConfig {
    fn default() -> Self {
        Self {
            shared_memory: false,
            registry: false,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl WindowsBridgeConfig {
    /// Clamp `max_bytes` to the supported range.
    pub fn normalized(self) -> Self {
        Self {
            shared_memory: self.shared_memory,
            registry: self.registry,
            max_bytes: normalize_max_bytes(self.max_bytes),
        }
    }

    /// Whether at least one transport is selected.
    pub fn is_enabled(&self) -> bool {
        self.shared_memory || self.registry
    }
}

fn normalize_max_bytes(max_bytes: usize) -> usize {
    max_bytes.clamp(MIN_MAX_BYTES, MAX_MAX_BYTES)
}

/// Destination for one sensor snapshot.
pub trait SensorSink {
    fn publish(&self, snapshot: &AidaSnapshot) -> Result<(), WindowsBridgeError>;
}

/// In-memory sink used by tests and by callers that want to inspect the
/// portable encoding without touching Windows APIs.
#[derive(Debug)]
pub struct MemorySink {
    max_bytes: usize,
    state: Mutex<MemorySinkState>,
}

#[derive(Debug, Default)]
struct MemorySinkState {
    publishes: u64,
    last_snapshot: Option<AidaSnapshot>,
    last_encoded: Option<Vec<u8>>,
}

impl Default for MemorySink {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BYTES)
    }
}

impl MemorySink {
    /// Create a sink with a payload limit. Unlike [`WindowsBridgeConfig`],
    /// this does not clamp the value, which lets tests exercise small limits.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new(MemorySinkState::default()),
        }
    }

    /// Number of successfully published snapshots.
    pub fn publishes(&self) -> Result<u64, WindowsBridgeError> {
        Ok(self.lock()?.publishes)
    }

    /// Most recently published snapshot, if any.
    pub fn last_snapshot(&self) -> Result<Option<AidaSnapshot>, WindowsBridgeError> {
        Ok(self.lock()?.last_snapshot.clone())
    }

    /// Most recently encoded blob, if any.
    pub fn last_encoded(&self) -> Result<Option<Vec<u8>>, WindowsBridgeError> {
        Ok(self.lock()?.last_encoded.clone())
    }

    fn lock(&self) -> Result<MutexGuard<'_, MemorySinkState>, WindowsBridgeError> {
        self.state
            .lock()
            .map_err(|_| WindowsBridgeError::StatePoisoned)
    }
}

impl SensorSink for MemorySink {
    fn publish(&self, snapshot: &AidaSnapshot) -> Result<(), WindowsBridgeError> {
        let encoded = encode_snapshot(snapshot, self.max_bytes)?;
        let mut state = self.lock()?;
        state.publishes = state.publishes.saturating_add(1);
        state.last_snapshot = Some(snapshot.clone());
        state.last_encoded = Some(encoded);
        Ok(())
    }
}

struct Encoder {
    buf: Vec<u8>,
    max: usize,
}

impl Encoder {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), WindowsBridgeError> {
        let size = self
            .buf
            .len()
            .checked_add(bytes.len())
            .ok_or(WindowsBridgeError::TooLarge {
                size: usize::MAX,
                max: self.max,
            })?;
        if size > self.max {
            return Err(WindowsBridgeError::TooLarge {
                size,
                max: self.max,
            });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), WindowsBridgeError> {
        self.write(&[value])
    }

    fn u16(&mut self, value: u16) -> Result<(), WindowsBridgeError> {
        self.write(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> Result<(), WindowsBridgeError> {
        self.write(&value.to_le_bytes())
    }

    fn f64(&mut self, value: f64) -> Result<(), WindowsBridgeError> {
        self.write(&value.to_le_bytes())
    }
}

fn write_bounded_string(
    encoder: &mut Encoder,
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), WindowsBridgeError> {
    let len = value.len();
    if len > max {
        return Err(WindowsBridgeError::StringTooLong { field, len, max });
    }
    let len =
        u16::try_from(len).map_err(|_| WindowsBridgeError::StringTooLong { field, len, max })?;
    encoder.u16(len)?;
    encoder.write(value.as_bytes())
}

/// Encode a snapshot into the documented, versioned Ozma binary map.
///
/// Sensors are sorted by id before writing, so the same logical snapshot
/// always produces identical bytes regardless of input order. The encode is
/// bounded by `max_bytes`; non-finite values and duplicate ids are rejected.
pub fn encode_snapshot(
    snapshot: &AidaSnapshot,
    max_bytes: usize,
) -> Result<Vec<u8>, WindowsBridgeError> {
    if snapshot.sensors.len() > MAX_SENSORS {
        return Err(WindowsBridgeError::TooManySensors {
            count: snapshot.sensors.len(),
            max: MAX_SENSORS,
        });
    }
    if !snapshot.timestamp.is_finite() {
        return Err(WindowsBridgeError::NonFiniteTimestamp);
    }

    let mut sensors: Vec<&AidaSensor> = snapshot.sensors.iter().collect();
    sensors.sort_by(|a, b| a.id.as_bytes().cmp(b.id.as_bytes()));
    for pair in sensors.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(WindowsBridgeError::DuplicateSensorId(pair[0].id.clone()));
        }
    }

    let mut encoder = Encoder::new(max_bytes);
    encoder.write(&PROTOCOL_MAGIC)?;
    encoder.u16(PROTOCOL_VERSION)?;
    encoder.u16(0)?;
    let count = u32::try_from(sensors.len()).map_err(|_| WindowsBridgeError::TooManySensors {
        count: sensors.len(),
        max: u32::MAX as usize,
    })?;
    encoder.u32(count)?;
    encoder.f64(snapshot.timestamp)?;

    for sensor in sensors {
        write_bounded_string(&mut encoder, "id", &sensor.id, MAX_ID_BYTES)?;
        write_bounded_string(&mut encoder, "label", &sensor.label, MAX_LABEL_BYTES)?;
        write_bounded_string(&mut encoder, "unit", &sensor.unit, MAX_UNIT_BYTES)?;
        write_bounded_string(&mut encoder, "kind", &sensor.kind, MAX_KIND_BYTES)?;
        write_bounded_string(&mut encoder, "color", &sensor.color, MAX_COLOR_BYTES)?;
        match &sensor.value {
            AidaValue::Number(value) => {
                if !value.is_finite() {
                    return Err(WindowsBridgeError::NonFiniteValue {
                        id: sensor.id.clone(),
                    });
                }
                encoder.u8(0)?;
                encoder.f64(*value)?;
            }
            AidaValue::Text(value) => {
                encoder.u8(1)?;
                write_bounded_string(&mut encoder, "value", value, MAX_TEXT_VALUE_BYTES)?;
            }
        }
    }

    Ok(encoder.buf)
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WindowsBridgeError> {
        let needed = self
            .pos
            .checked_add(len)
            .ok_or(WindowsBridgeError::Truncated {
                needed: usize::MAX,
                available: self.data.len(),
            })?;
        if needed > self.data.len() {
            return Err(WindowsBridgeError::Truncated {
                needed,
                available: self.data.len(),
            });
        }
        let bytes = &self.data[self.pos..needed];
        self.pos = needed;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, WindowsBridgeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WindowsBridgeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, WindowsBridgeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn f64(&mut self) -> Result<f64, WindowsBridgeError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(f64::from_le_bytes(bytes))
    }

    fn string(&mut self, field: &'static str, max: usize) -> Result<String, WindowsBridgeError> {
        let len = self.u16()? as usize;
        if len > max {
            return Err(WindowsBridgeError::StringTooLong { field, len, max });
        }
        let bytes = self.take(len)?;
        let text =
            std::str::from_utf8(bytes).map_err(|_| WindowsBridgeError::InvalidUtf8 { field })?;
        Ok(text.to_owned())
    }
}

/// Decode an encoded snapshot using the hard [`MAX_ENCODED_BYTES`] limit.
pub fn decode_snapshot(data: &[u8]) -> Result<AidaSnapshot, WindowsBridgeError> {
    decode_snapshot_with_limit(data, MAX_ENCODED_BYTES)
}

/// Decode an encoded snapshot and reject inputs larger than `max_bytes`.
///
/// Truncation, trailing bytes, bad magic/version/flags, invalid UTF-8,
/// oversized fields, duplicate ids, and non-finite numbers are rejected.
pub fn decode_snapshot_with_limit(
    data: &[u8],
    max_bytes: usize,
) -> Result<AidaSnapshot, WindowsBridgeError> {
    if data.len() > max_bytes {
        return Err(WindowsBridgeError::TooLarge {
            size: data.len(),
            max: max_bytes,
        });
    }

    let mut decoder = Decoder::new(data);
    if decoder.take(PROTOCOL_MAGIC.len())? != PROTOCOL_MAGIC {
        return Err(WindowsBridgeError::BadMagic);
    }
    let version = decoder.u16()?;
    if version != PROTOCOL_VERSION {
        return Err(WindowsBridgeError::UnsupportedVersion(version));
    }
    let flags = decoder.u16()?;
    if flags != 0 {
        return Err(WindowsBridgeError::UnsupportedFlags(flags));
    }
    let count = decoder.u32()? as usize;
    if count > MAX_SENSORS {
        return Err(WindowsBridgeError::TooManySensors {
            count,
            max: MAX_SENSORS,
        });
    }
    let timestamp = decoder.f64()?;
    if !timestamp.is_finite() {
        return Err(WindowsBridgeError::NonFiniteTimestamp);
    }

    let mut sensors = Vec::with_capacity(count);
    let mut seen = BTreeSet::new();
    for _ in 0..count {
        let id = decoder.string("id", MAX_ID_BYTES)?;
        if !seen.insert(id.clone()) {
            return Err(WindowsBridgeError::DuplicateSensorId(id));
        }
        let label = decoder.string("label", MAX_LABEL_BYTES)?;
        let unit = decoder.string("unit", MAX_UNIT_BYTES)?;
        let kind = decoder.string("kind", MAX_KIND_BYTES)?;
        let color = decoder.string("color", MAX_COLOR_BYTES)?;
        let tag = decoder.u8()?;
        let value = match tag {
            0 => {
                let value = decoder.f64()?;
                if !value.is_finite() {
                    return Err(WindowsBridgeError::NonFiniteValue { id });
                }
                AidaValue::Number(value)
            }
            1 => AidaValue::Text(decoder.string("value", MAX_TEXT_VALUE_BYTES)?),
            other => return Err(WindowsBridgeError::UnsupportedValueTag(other)),
        };
        sensors.push(AidaSensor {
            id,
            label,
            value,
            unit,
            kind,
            color,
        });
    }

    if decoder.remaining() != 0 {
        return Err(WindowsBridgeError::TrailingBytes {
            offset: decoder.pos,
        });
    }

    Ok(AidaSnapshot { timestamp, sensors })
}

/// A flat Registry value, either a UTF-8 string or an `f64` number.
#[derive(Clone, Debug, PartialEq)]
pub enum RegistryValue {
    String(String),
    Number(f64),
}

fn insert_registry_value(
    values: &mut BTreeMap<String, RegistryValue>,
    name: &str,
    value: RegistryValue,
) -> Result<(), WindowsBridgeError> {
    if name.is_empty() || name.len() > MAX_REGISTRY_NAME_BYTES || name.contains('\0') {
        return Err(WindowsBridgeError::InvalidRegistryName(name.to_owned()));
    }
    match &value {
        RegistryValue::String(text) => {
            if text.len() > MAX_REGISTRY_TEXT_BYTES {
                return Err(WindowsBridgeError::RegistryValueTooLong {
                    name: name.to_owned(),
                    max: MAX_REGISTRY_TEXT_BYTES,
                });
            }
            if text.contains('\0') {
                return Err(WindowsBridgeError::InvalidRegistryText {
                    name: name.to_owned(),
                });
            }
        }
        RegistryValue::Number(number) => {
            if !number.is_finite() {
                return Err(WindowsBridgeError::NonFiniteValue {
                    id: name.to_owned(),
                });
            }
        }
    }
    if values.insert(name.to_owned(), value).is_some() {
        return Err(WindowsBridgeError::DuplicateRegistryValue(name.to_owned()));
    }
    Ok(())
}

fn validate_registry_identifier(id: &str) -> Result<(), WindowsBridgeError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(WindowsBridgeError::InvalidIdentifier { id: id.to_owned() })
    }
}

/// Build the bounded, deterministic Registry value map.
///
/// Metadata keys start with `@`. Each sensor contributes
/// `sensor.<id>.value` plus `.label`, `.unit`, `.kind`, and `.color`.
/// Numeric values remain `f64`; the Windows backend stores them as
/// `REG_BINARY` little-endian doubles and strings as `REG_SZ`.
pub fn registry_values(
    snapshot: &AidaSnapshot,
) -> Result<BTreeMap<String, RegistryValue>, WindowsBridgeError> {
    if snapshot.sensors.len() > MAX_SENSORS {
        return Err(WindowsBridgeError::TooManySensors {
            count: snapshot.sensors.len(),
            max: MAX_SENSORS,
        });
    }
    if !snapshot.timestamp.is_finite() {
        return Err(WindowsBridgeError::NonFiniteTimestamp);
    }

    let mut values = BTreeMap::new();
    insert_registry_value(
        &mut values,
        "@format",
        RegistryValue::String(REGISTRY_FORMAT.to_owned()),
    )?;
    insert_registry_value(
        &mut values,
        "@timestamp",
        RegistryValue::Number(snapshot.timestamp),
    )?;
    let sensor_count =
        u32::try_from(snapshot.sensors.len()).map_err(|_| WindowsBridgeError::TooManySensors {
            count: snapshot.sensors.len(),
            max: u32::MAX as usize,
        })?;
    insert_registry_value(
        &mut values,
        "@sensor_count",
        RegistryValue::Number(f64::from(sensor_count)),
    )?;

    let mut sensors: Vec<&AidaSensor> = snapshot.sensors.iter().collect();
    sensors.sort_by(|a, b| a.id.as_bytes().cmp(b.id.as_bytes()));
    for sensor in sensors {
        validate_registry_identifier(&sensor.id)?;
        let value = match &sensor.value {
            AidaValue::Number(number) => {
                if !number.is_finite() {
                    return Err(WindowsBridgeError::NonFiniteValue {
                        id: sensor.id.clone(),
                    });
                }
                RegistryValue::Number(*number)
            }
            AidaValue::Text(text) => RegistryValue::String(text.clone()),
        };
        insert_registry_value(&mut values, &format!("sensor.{}.value", sensor.id), value)?;
        insert_registry_value(
            &mut values,
            &format!("sensor.{}.label", sensor.id),
            RegistryValue::String(sensor.label.clone()),
        )?;
        insert_registry_value(
            &mut values,
            &format!("sensor.{}.unit", sensor.id),
            RegistryValue::String(sensor.unit.clone()),
        )?;
        insert_registry_value(
            &mut values,
            &format!("sensor.{}.kind", sensor.id),
            RegistryValue::String(sensor.kind.clone()),
        )?;
        insert_registry_value(
            &mut values,
            &format!("sensor.{}.color", sensor.id),
            RegistryValue::String(sensor.color.clone()),
        )?;
    }

    if values.len() > MAX_REGISTRY_ENTRIES {
        return Err(WindowsBridgeError::TooManyRegistryValues {
            count: values.len(),
            max: MAX_REGISTRY_ENTRIES,
        });
    }
    Ok(values)
}

/// Windows-only named shared-memory and Registry writers.
///
/// All hazard operations are confined to this module.
#[cfg(windows)]
mod windows_backend {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use std::sync::atomic::{fence, AtomicU32, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use winapi::shared::minwindef::{DWORD, HKEY};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::memoryapi::FILE_MAP_ALL_ACCESS;
    use winapi::um::memoryapi::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile};
    use winapi::um::winnt::{
        KEY_SET_VALUE, PAGE_READWRITE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use winapi::um::winreg::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY_CURRENT_USER};

    use super::*;
    use crate::aida64::build_snapshot;
    use crate::hardware::HardwareState;

    /// Poll period used by [`start_bridge`]. Deliberately bounded and not
    /// user-tunable from the AIDA64 HTTP listener.
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn last_error() -> u32 {
        // Only called immediately after a failed Win32 call.
        unsafe { GetLastError() }
    }

    fn shared_memory_error(operation: &'static str) -> WindowsBridgeError {
        WindowsBridgeError::SharedMemory {
            operation,
            code: last_error(),
        }
    }

    fn registry_error(operation: &'static str, code: i32) -> WindowsBridgeError {
        WindowsBridgeError::Registry { operation, code }
    }

    /// Writer for the documented `Local\Ozma_Aida64_SensorValues` mapping.
    pub struct SharedMemorySink {
        max_bytes: usize,
        mapping: winapi::um::winnt::HANDLE,
        view: *mut u8,
        view_len: usize,
    }

    impl SharedMemorySink {
        /// Create (or open) the named mapping. Returns a typed error instead
        /// of panicking when mapping creation is denied or fails.
        pub fn new(name: &str, max_bytes: usize) -> Result<Self, WindowsBridgeError> {
            let max_bytes = normalize_max_bytes(max_bytes);
            let view_len = SHARED_MEMORY_HEADER_BYTES.checked_add(max_bytes).ok_or(
                WindowsBridgeError::TooLarge {
                    size: usize::MAX,
                    max: MAX_MAX_BYTES,
                },
            )?;
            let wide_name = wide(name);
            let size = view_len as u64;
            let mapping = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    ptr::null_mut(),
                    PAGE_READWRITE,
                    (size >> 32) as DWORD,
                    (size & 0xffff_ffff) as DWORD,
                    wide_name.as_ptr(),
                )
            };
            if mapping.is_null() {
                return Err(shared_memory_error("CreateFileMappingW"));
            }
            let view =
                unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, view_len) as *mut u8 };
            if view.is_null() {
                let error = shared_memory_error("MapViewOfFile");
                unsafe {
                    CloseHandle(mapping);
                }
                return Err(error);
            }

            let sink = Self {
                max_bytes,
                mapping,
                view,
                view_len,
            };
            sink.write_header()?;
            Ok(sink)
        }

        fn write_header(&self) -> Result<(), WindowsBridgeError> {
            if self.view.is_null() || self.view_len < SHARED_MEMORY_HEADER_BYTES {
                return Err(WindowsBridgeError::SharedMemory {
                    operation: "initialize header",
                    code: 0,
                });
            }
            unsafe {
                ptr::write_bytes(self.view, 0, SHARED_MEMORY_HEADER_BYTES);
                ptr::copy_nonoverlapping(
                    SHARED_MEMORY_MAGIC.as_ptr(),
                    self.view.add(SHARED_MEMORY_MAGIC_OFFSET),
                    SHARED_MEMORY_MAGIC.len(),
                );
                ptr::write_unaligned(
                    self.view.add(SHARED_MEMORY_VERSION_OFFSET) as *mut u16,
                    SHARED_MEMORY_VERSION,
                );
                ptr::write_unaligned(self.view.add(SHARED_MEMORY_FLAGS_OFFSET) as *mut u16, 0);
                ptr::write_unaligned(self.view.add(SHARED_MEMORY_SEQUENCE_OFFSET) as *mut u32, 0);
                ptr::write_unaligned(
                    self.view.add(SHARED_MEMORY_PAYLOAD_LEN_OFFSET) as *mut u32,
                    0,
                );
                ptr::write_unaligned(self.view.add(SHARED_MEMORY_RESERVED_OFFSET) as *mut u32, 0);
            }
            Ok(())
        }

        fn sequence(&self) -> &AtomicU32 {
            // The page-aligned mapping plus a 4-byte-aligned offset guarantees
            // alignment for AtomicU32.
            unsafe { &*(self.view.add(SHARED_MEMORY_SEQUENCE_OFFSET) as *const AtomicU32) }
        }
    }

    impl SensorSink for SharedMemorySink {
        fn publish(&self, snapshot: &AidaSnapshot) -> Result<(), WindowsBridgeError> {
            let payload = encode_snapshot(snapshot, self.max_bytes)?;
            if payload.len() > self.max_bytes {
                return Err(WindowsBridgeError::TooLarge {
                    size: payload.len(),
                    max: self.max_bytes,
                });
            }
            let payload_len =
                u32::try_from(payload.len()).map_err(|_| WindowsBridgeError::TooLarge {
                    size: payload.len(),
                    max: self.max_bytes,
                })?;
            if self.view.is_null() || payload.len() > self.view_len - SHARED_MEMORY_HEADER_BYTES {
                return Err(WindowsBridgeError::SharedMemory {
                    operation: "publish",
                    code: 0,
                });
            }

            let sequence = self.sequence();
            let current = sequence.load(Ordering::Acquire);
            let odd = current.wrapping_add(1) | 1;
            sequence.store(odd, Ordering::Release);
            unsafe {
                ptr::write_volatile(
                    self.view.add(SHARED_MEMORY_PAYLOAD_LEN_OFFSET) as *mut u32,
                    payload_len,
                );
                ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    self.view.add(SHARED_MEMORY_HEADER_BYTES),
                    payload.len(),
                );
            }
            fence(Ordering::Release);
            sequence.store(odd.wrapping_add(1), Ordering::Release);
            Ok(())
        }
    }

    impl Drop for SharedMemorySink {
        fn drop(&mut self) {
            unsafe {
                if !self.view.is_null() {
                    UnmapViewOfFile(self.view as *const _);
                }
                if !self.mapping.is_null() {
                    CloseHandle(self.mapping);
                }
            }
        }
    }

    // The sink is used from one bridge task at a time. The mapping is not
    // exposed as Sync because concurrent writers would race on the payload.
    unsafe impl Send for SharedMemorySink {}

    /// Writer for `HKCU\Software\Ozma\Aida64\SensorValues`.
    pub struct RegistrySink {
        key: HKEY,
    }

    impl RegistrySink {
        /// Create the Ozma Registry key. Uses `HKEY_CURRENT_USER`, so no
        /// administrator rights are required.
        pub fn open() -> Result<Self, WindowsBridgeError> {
            let subkey = wide(REGISTRY_KEY_PATH);
            let mut key: HKEY = ptr::null_mut();
            let mut disposition: DWORD = 0;
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    ptr::null_mut(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    ptr::null_mut(),
                    &mut key,
                    &mut disposition,
                )
            };
            if status != 0 {
                return Err(registry_error("RegCreateKeyExW", status));
            }
            Ok(Self { key })
        }

        fn set_value(&self, name: &str, value: &RegistryValue) -> Result<(), WindowsBridgeError> {
            let wide_name = wide(name);
            match value {
                RegistryValue::String(text) => {
                    if text.contains('\0') {
                        return Err(WindowsBridgeError::InvalidRegistryText {
                            name: name.to_owned(),
                        });
                    }
                    let data: Vec<u16> = OsStr::new(text).encode_wide().chain(Some(0)).collect();
                    let bytes = data.len().checked_mul(2).ok_or_else(|| {
                        WindowsBridgeError::RegistryValueTooLong {
                            name: name.to_owned(),
                            max: MAX_REGISTRY_TEXT_BYTES,
                        }
                    })?;
                    let bytes = u32::try_from(bytes).map_err(|_| {
                        WindowsBridgeError::RegistryValueTooLong {
                            name: name.to_owned(),
                            max: MAX_REGISTRY_TEXT_BYTES,
                        }
                    })?;
                    let status = unsafe {
                        RegSetValueExW(
                            self.key,
                            wide_name.as_ptr(),
                            0,
                            REG_SZ,
                            data.as_ptr() as *const u8,
                            bytes,
                        )
                    };
                    if status != 0 {
                        return Err(registry_error("RegSetValueExW", status));
                    }
                }
                RegistryValue::Number(number) => {
                    if !number.is_finite() {
                        return Err(WindowsBridgeError::NonFiniteValue {
                            id: name.to_owned(),
                        });
                    }
                    let data = number.to_le_bytes();
                    let status = unsafe {
                        RegSetValueExW(
                            self.key,
                            wide_name.as_ptr(),
                            0,
                            REG_BINARY,
                            data.as_ptr(),
                            data.len() as DWORD,
                        )
                    };
                    if status != 0 {
                        return Err(registry_error("RegSetValueExW", status));
                    }
                }
            }
            Ok(())
        }
    }

    impl SensorSink for RegistrySink {
        fn publish(&self, snapshot: &AidaSnapshot) -> Result<(), WindowsBridgeError> {
            let values = registry_values(snapshot)?;
            for (name, value) in &values {
                self.set_value(name, value)?;
            }
            Ok(())
        }
    }

    impl Drop for RegistrySink {
        fn drop(&mut self) {
            if !self.key.is_null() {
                unsafe {
                    RegCloseKey(self.key);
                }
            }
        }
    }

    unsafe impl Send for RegistrySink {}

    /// Poll the hardware snapshot and publish it to the enabled sinks until
    /// `shutdown` is cancelled.
    ///
    /// This is not called by the existing AIDA64 listener or node startup.
    /// Sink creation and publish failures are logged and returned as typed
    /// errors; they never panic. A sink that cannot be opened is skipped so a
    /// Registry permission problem does not prevent the shared-memory writer.
    pub async fn start_bridge(
        hardware: HardwareState,
        shutdown: CancellationToken,
        config: WindowsBridgeConfig,
    ) -> Result<(), WindowsBridgeError> {
        let config = config.normalized();
        let mut sinks: Vec<Box<dyn SensorSink>> = Vec::new();

        if config.shared_memory {
            match SharedMemorySink::new(SHARED_MEMORY_NAME, config.max_bytes) {
                Ok(sink) => sinks.push(Box::new(sink)),
                Err(error) => {
                    tracing::warn!(error = %error, "could not open AIDA64 shared-memory sink");
                }
            }
        }
        if config.registry {
            match RegistrySink::open() {
                Ok(sink) => sinks.push(Box::new(sink)),
                Err(error) => {
                    tracing::warn!(error = %error, "could not open AIDA64 Registry sink");
                }
            }
        }
        if sinks.is_empty() {
            return Err(WindowsBridgeError::NoSinksAvailable);
        }

        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match hardware.snapshot(false).await {
                Ok(hardware) => {
                    let snapshot = build_snapshot(&hardware);
                    for sink in &sinks {
                        if let Err(error) = sink.publish(&snapshot) {
                            tracing::warn!(error = %error, "AIDA64 Windows bridge publish failed");
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(error = %error, "AIDA64 Windows bridge hardware snapshot failed");
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(DEFAULT_POLL_INTERVAL) => {}
            }
        }
    }
}

#[cfg(windows)]
pub use windows_backend::{start_bridge, RegistrySink, SharedMemorySink, DEFAULT_POLL_INTERVAL};

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(id: &str, value: AidaValue) -> AidaSensor {
        AidaSensor {
            id: id.to_owned(),
            label: format!("{id} label"),
            value,
            unit: "C".to_owned(),
            kind: "temperature".to_owned(),
            color: "#ffffff".to_owned(),
        }
    }

    fn snapshot(sensors: Vec<AidaSensor>) -> AidaSnapshot {
        AidaSnapshot {
            timestamp: 1_700_000_000.25,
            sensors,
        }
    }

    fn assert_snapshot_eq(left: &AidaSnapshot, right: &AidaSnapshot) {
        assert_eq!(left.timestamp.to_bits(), right.timestamp.to_bits());
        assert_eq!(left.sensors.len(), right.sensors.len());
        let mut left_sensors: Vec<_> = left.sensors.iter().collect();
        let mut right_sensors: Vec<_> = right.sensors.iter().collect();
        left_sensors.sort_by(|a, b| a.id.as_bytes().cmp(b.id.as_bytes()));
        right_sensors.sort_by(|a, b| a.id.as_bytes().cmp(b.id.as_bytes()));
        for (left, right) in left_sensors.iter().zip(right_sensors.iter()) {
            assert_eq!(left.id, right.id);
            assert_eq!(left.label, right.label);
            assert_eq!(left.unit, right.unit);
            assert_eq!(left.kind, right.kind);
            assert_eq!(left.color, right.color);
            match (&left.value, &right.value) {
                (AidaValue::Number(left), AidaValue::Number(right)) => {
                    assert_eq!(left.to_bits(), right.to_bits());
                }
                (AidaValue::Text(left), AidaValue::Text(right)) => assert_eq!(left, right),
                _ => panic!("sensor value kind changed"),
            }
        }
    }

    #[test]
    fn encodes_and_decodes_round_trip() {
        let original = snapshot(vec![
            sensor("TCPU", AidaValue::Number(62.5)),
            sensor("SDATE", AidaValue::Text("2026-11-26".to_owned())),
        ]);
        let encoded = encode_snapshot(&original, MAX_ENCODED_BYTES).unwrap();
        assert_eq!(&encoded[..PROTOCOL_MAGIC.len()], PROTOCOL_MAGIC.as_slice());
        let decoded = decode_snapshot(&encoded).unwrap();
        assert_snapshot_eq(&original, &decoded);
    }

    #[test]
    fn rejects_truncated_input_at_every_boundary() {
        let original = snapshot(vec![
            sensor("TCPU", AidaValue::Number(62.5)),
            sensor("SDATE", AidaValue::Text("2026-11-26".to_owned())),
        ]);
        let encoded = encode_snapshot(&original, MAX_ENCODED_BYTES).unwrap();
        for length in 0..encoded.len() {
            let error = decode_snapshot(&encoded[..length]).unwrap_err();
            assert!(
                matches!(error, WindowsBridgeError::Truncated { .. }),
                "length {length} produced {error:?}"
            );
        }
    }

    #[test]
    fn rejects_oversize_inputs_and_limits() {
        let original = snapshot(vec![sensor("TCPU", AidaValue::Number(62.5))]);
        let encoded = encode_snapshot(&original, MAX_ENCODED_BYTES).unwrap();
        let error = decode_snapshot_with_limit(&encoded, encoded.len() - 1).unwrap_err();
        assert!(matches!(error, WindowsBridgeError::TooLarge { .. }));

        let error = encode_snapshot(&original, encoded.len() - 1).unwrap_err();
        assert!(matches!(error, WindowsBridgeError::TooLarge { .. }));
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let original = snapshot(vec![sensor("TCPU", AidaValue::Number(62.5))]);
        let encoded = encode_snapshot(&original, MAX_ENCODED_BYTES).unwrap();

        let mut bad_magic = encoded.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            decode_snapshot(&bad_magic).unwrap_err(),
            WindowsBridgeError::BadMagic
        ));

        let mut bad_version = encoded;
        bad_version[PROTOCOL_MAGIC.len()..PROTOCOL_MAGIC.len() + 2]
            .copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        assert!(matches!(
            decode_snapshot(&bad_version).unwrap_err(),
            WindowsBridgeError::UnsupportedVersion(_)
        ));
    }

    #[test]
    fn handles_non_finite_values() {
        let nan = snapshot(vec![sensor("TCPU", AidaValue::Number(f64::NAN))]);
        assert!(matches!(
            encode_snapshot(&nan, MAX_ENCODED_BYTES).unwrap_err(),
            WindowsBridgeError::NonFiniteValue { .. }
        ));

        let infinite = snapshot(vec![sensor("TCPU", AidaValue::Number(f64::INFINITY))]);
        assert!(matches!(
            encode_snapshot(&infinite, MAX_ENCODED_BYTES).unwrap_err(),
            WindowsBridgeError::NonFiniteValue { .. }
        ));

        let mut encoded = encode_snapshot(
            &snapshot(vec![sensor("TCPU", AidaValue::Number(1.0))]),
            MAX_ENCODED_BYTES,
        )
        .unwrap();
        let value_offset = encoded.len() - 8;
        encoded[value_offset..].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(matches!(
            decode_snapshot(&encoded).unwrap_err(),
            WindowsBridgeError::NonFiniteValue { ref id } if id == "TCPU"
        ));
    }

    #[test]
    fn encoding_is_deterministic_and_sorted() {
        let left = snapshot(vec![
            sensor("B", AidaValue::Number(2.0)),
            sensor("A", AidaValue::Number(1.0)),
        ]);
        let right = snapshot(vec![
            sensor("A", AidaValue::Number(1.0)),
            sensor("B", AidaValue::Number(2.0)),
        ]);
        let left_bytes = encode_snapshot(&left, MAX_ENCODED_BYTES).unwrap();
        let right_bytes = encode_snapshot(&right, MAX_ENCODED_BYTES).unwrap();
        assert_eq!(left_bytes, right_bytes);

        let left_values = registry_values(&left).unwrap();
        let right_values = registry_values(&right).unwrap();
        assert_eq!(left_values, right_values);
        let keys: Vec<_> = left_values.keys().cloned().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn registry_map_is_bounded_and_validated() {
        let values = registry_values(&snapshot(vec![
            sensor("TCPU", AidaValue::Number(62.5)),
            sensor("SDATE", AidaValue::Text("2026-11-26".to_owned())),
        ]))
        .unwrap();
        assert!(values.len() <= MAX_REGISTRY_ENTRIES);
        assert_eq!(
            values.get("@format"),
            Some(&RegistryValue::String(REGISTRY_FORMAT.to_owned()))
        );
        assert!(values.contains_key("@timestamp"));
        assert!(values.contains_key("@sensor_count"));
        assert!(values.contains_key("sensor.TCPU.value"));
        for (name, value) in &values {
            assert!(!name.is_empty());
            assert!(name.len() <= MAX_REGISTRY_NAME_BYTES);
            match value {
                RegistryValue::String(text) => {
                    assert!(text.len() <= MAX_REGISTRY_TEXT_BYTES);
                }
                RegistryValue::Number(number) => assert!(number.is_finite()),
            }
        }

        let invalid = snapshot(vec![sensor("bad/id", AidaValue::Number(1.0))]);
        assert!(matches!(
            registry_values(&invalid).unwrap_err(),
            WindowsBridgeError::InvalidIdentifier { .. }
        ));

        let too_long = snapshot(vec![sensor(
            "TCPU",
            AidaValue::Text("x".repeat(MAX_REGISTRY_TEXT_BYTES + 1)),
        )]);
        assert!(matches!(
            registry_values(&too_long).unwrap_err(),
            WindowsBridgeError::RegistryValueTooLong { .. }
        ));
    }

    #[test]
    fn fake_sink_publishes_and_records() {
        let sink = MemorySink::new(MAX_ENCODED_BYTES);
        assert_eq!(sink.publishes().unwrap(), 0);
        let original = snapshot(vec![
            sensor("TCPU", AidaValue::Number(62.5)),
            sensor("SDATE", AidaValue::Text("2026-11-26".to_owned())),
        ]);
        sink.publish(&original).unwrap();
        assert_eq!(sink.publishes().unwrap(), 1);
        assert_snapshot_eq(&sink.last_snapshot().unwrap().unwrap(), &original);
        let encoded = sink.last_encoded().unwrap().unwrap();
        assert_snapshot_eq(&decode_snapshot(&encoded).unwrap(), &original);
    }

    #[test]
    fn failed_fake_sink_publish_does_not_count() {
        let sink = MemorySink::new(8);
        let original = snapshot(vec![sensor("TCPU", AidaValue::Number(62.5))]);
        assert!(matches!(
            sink.publish(&original).unwrap_err(),
            WindowsBridgeError::TooLarge { .. }
        ));
        assert_eq!(sink.publishes().unwrap(), 0);
        assert!(sink.last_snapshot().unwrap().is_none());
    }

    #[test]
    fn config_defaults_to_off_and_clamps_limits() {
        let default = WindowsBridgeConfig::default();
        assert!(!default.shared_memory);
        assert!(!default.registry);
        assert!(!default.is_enabled());
        assert_eq!(default.max_bytes, DEFAULT_MAX_BYTES);

        let too_small = WindowsBridgeConfig {
            shared_memory: true,
            registry: true,
            max_bytes: 0,
        }
        .normalized();
        assert!(too_small.is_enabled());
        assert_eq!(too_small.max_bytes, MIN_MAX_BYTES);

        let too_large = WindowsBridgeConfig {
            shared_memory: false,
            registry: true,
            max_bytes: usize::MAX,
        }
        .normalized();
        assert_eq!(too_large.max_bytes, MAX_MAX_BYTES);
    }

    #[test]
    fn ffi_is_confined_to_windows_backend() {
        let source = include_str!("aida64_windows.rs");
        let marker = "#[cfg(windows)]\nmod windows_backend {";
        let forbidden = concat!("un", "safe");
        let (portable, rest) = source
            .split_once(marker)
            .expect("Windows backend module marker missing");
        assert!(
            !portable.contains(forbidden),
            "FFI code must be confined to the Windows backend"
        );
        let (_, tests) = rest
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("test module marker missing");
        assert!(
            !tests.contains(forbidden),
            "FFI code must not appear in portable tests"
        );
    }
}
