// SPDX-License-Identifier: AGPL-3.0-only
//! Typed USB/IP control wire-protocol codec.
//!
//! This module implements the version-1.1.1 (`0x0111`) USB/IP **control**
//! messages only: `OP_REQ_DEVLIST`/`OP_REP_DEVLIST` and
//! `OP_REQ_IMPORT`/`OP_REP_IMPORT`, plus the device/interface records they
//! carry. Everything is network byte order and every fixed-width string is
//! NUL-padded exactly like the kernel's packed `usbip_usb_device` record.
//!
//! The codec is deliberately strict and bounded:
//!
//! * a frame larger than [`USBIP_MAX_FRAME_BYTES`] (or a caller-supplied
//!   smaller limit) is rejected before allocation;
//! * device counts are bounded by [`USBIP_MAX_DEVICES`] and interface counts
//!   by [`USBIP_MAX_INTERFACES`];
//! * path/bus-id strings must be non-empty, printable ASCII and fit their
//!   fixed buffers with a terminating NUL;
//! * truncated and trailing bytes are distinct typed errors.
//!
//! The USB/IP wire protocol carries no token or key-fingerprint field. Peer
//! authorization therefore lives in [`crate::usbip_server`] and is supplied
//! out-of-band; this codec never sees secrets.
//!
//! [`UsbIpDeviceDescriptor`] models the kernel `usbip_usb_device` record.
//! `num_interfaces` is the wire `bNumInterfaces`; `interfaces` holds the
//! per-interface records that appear **only** in `OP_REP_DEVLIST`.
//! `OP_REP_IMPORT` carries the 312-byte device record without interface
//! records, so an import reply decodes with `interfaces` empty and the
//! declared `num_interfaces` preserved.

use thiserror::Error;

use crate::usbip::validate_bus_id;

/// `OP_REQ_DEVLIST`.
pub const OP_REQ_DEVLIST: u16 = 0x8005;
/// `OP_REP_DEVLIST`.
pub const OP_REP_DEVLIST: u16 = 0x0005;
/// `OP_REQ_IMPORT`.
pub const OP_REQ_IMPORT: u16 = 0x8003;
/// `OP_REP_IMPORT`.
pub const OP_REP_IMPORT: u16 = 0x0003;

/// The only USB/IP protocol version this crate speaks.
pub const USBIP_PROTOCOL_VERSION: u16 = 0x0111;

/// Fixed `path` buffer in `usbip_usb_device`.
pub const USBIP_PATH_BYTES: usize = 256;
/// Fixed `busid` buffer in `usbip_usb_device`.
pub const USBIP_BUSID_BYTES: usize = 32;
/// Maximum interface records per device (`USB_MAXINTERFACES`).
pub const USBIP_MAX_INTERFACES: usize = 32;
/// Maximum devices in one device-list reply.
pub const USBIP_MAX_DEVICES: usize = 256;
/// Encoded `struct op_common` size.
pub const USBIP_OP_HEADER_BYTES: usize = 8;
/// Encoded `struct usbip_usb_device` size (without interface records).
pub const USBIP_DEVICE_BYTES: usize = 312;
/// Encoded `struct usbip_usb_interface` size.
pub const USBIP_INTERFACE_BYTES: usize = 4;
/// Encoded `op_common` + `ndev` device-list prefix.
pub const USBIP_DEVLIST_HEADER_BYTES: usize = USBIP_OP_HEADER_BYTES + 4;
/// Encoded `OP_REQ_IMPORT` frame size.
pub const USBIP_IMPORT_REQUEST_BYTES: usize = USBIP_OP_HEADER_BYTES + USBIP_BUSID_BYTES;
/// Encoded successful `OP_REP_IMPORT` frame size.
pub const USBIP_IMPORT_REPLY_BYTES: usize = USBIP_OP_HEADER_BYTES + USBIP_DEVICE_BYTES;
/// Largest device record including all interface records.
pub const USBIP_MAX_DEVICE_BYTES: usize =
    USBIP_DEVICE_BYTES + USBIP_MAX_INTERFACES * USBIP_INTERFACE_BYTES;
/// Largest control frame this codec will produce or accept.
pub const USBIP_MAX_FRAME_BYTES: usize =
    USBIP_DEVLIST_HEADER_BYTES + USBIP_MAX_DEVICES * USBIP_MAX_DEVICE_BYTES;

/// A typed USB/IP control-frame failure.
///
/// These are deliberately finer-grained than the generic
/// [`crate::error::UsbError::Protocol`] variant so callers and tests can
/// distinguish a wrong version from a wrong opcode, a short frame, a frame
/// that is too large, or a bad string record.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UsbIpProtocolError {
    /// The frame ended before a complete record could be read.
    #[error("USB/IP frame is truncated: need {needed} bytes, have {available}")]
    Truncated {
        /// Required bytes.
        needed: usize,
        /// Available bytes.
        available: usize,
    },

    /// The frame contained bytes after the last complete record.
    #[error("USB/IP frame has {actual} bytes; expected exactly {expected}")]
    TrailingBytes {
        /// Expected frame size.
        expected: usize,
        /// Actual frame size.
        actual: usize,
    },

    /// The peer announced a different USB/IP version.
    #[error("USB/IP version 0x{version:04x} is unsupported (expected 0x{expected:04x})")]
    UnsupportedVersion {
        /// Version found in the frame.
        version: u16,
        /// Version supported by this crate.
        expected: u16,
    },

    /// The opcode is not one of the four control opcodes.
    #[error("USB/IP opcode 0x{code:04x} is unknown")]
    UnknownOpcode {
        /// Raw opcode.
        code: u16,
    },

    /// The opcode is known but not valid in this position.
    #[error("USB/IP expected {expected} opcode, got {actual}")]
    UnexpectedOpcode {
        /// Human-readable expected opcode class.
        expected: &'static str,
        /// Human-readable received opcode.
        actual: &'static str,
    },

    /// The status code is not a USB/IP `ST_*` value.
    #[error("USB/IP status 0x{status:08x} is unknown")]
    UnknownStatus {
        /// Raw status value.
        status: u32,
    },

    /// A count, string, or complete frame exceeds its configured bound.
    #[error("USB/IP {field} is too large: {requested} > {limit}")]
    Oversized {
        /// Field or frame name.
        field: &'static str,
        /// Maximum accepted value.
        limit: usize,
        /// Requested value.
        requested: usize,
    },

    /// A fixed-width string is empty, too long, or not printable ASCII.
    #[error("USB/IP {field} is invalid: {reason}")]
    InvalidString {
        /// Field name (`path`, `busid`).
        field: &'static str,
        /// Why the value was rejected.
        reason: String,
    },

    /// A declared count does not match the records that follow.
    #[error("USB/IP {field} count mismatch: declared {declared}, actual {actual}")]
    CountMismatch {
        /// Field name.
        field: &'static str,
        /// Declared count.
        declared: usize,
        /// Actual record count.
        actual: usize,
    },

    /// A header status is not compatible with the frame body.
    #[error("USB/IP status {status:?} is inconsistent with {context}")]
    InconsistentStatus {
        /// Status found in the header.
        status: UsbIpStatus,
        /// What the frame expects.
        context: &'static str,
    },
}

/// A USB/IP control opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsbIpOpCode {
    /// `OP_REQ_DEVLIST` (`0x8005`).
    ReqDevList,
    /// `OP_REP_DEVLIST` (`0x0005`).
    RepDevList,
    /// `OP_REQ_IMPORT` (`0x8003`).
    ReqImport,
    /// `OP_REP_IMPORT` (`0x0003`).
    RepImport,
}

impl UsbIpOpCode {
    /// Raw network-order code.
    pub const fn raw(self) -> u16 {
        match self {
            Self::ReqDevList => OP_REQ_DEVLIST,
            Self::RepDevList => OP_REP_DEVLIST,
            Self::ReqImport => OP_REQ_IMPORT,
            Self::RepImport => OP_REP_IMPORT,
        }
    }

    /// Parse a raw code, rejecting unknown opcodes.
    pub const fn from_raw(code: u16) -> Option<Self> {
        match code {
            OP_REQ_DEVLIST => Some(Self::ReqDevList),
            OP_REP_DEVLIST => Some(Self::RepDevList),
            OP_REQ_IMPORT => Some(Self::ReqImport),
            OP_REP_IMPORT => Some(Self::RepImport),
            _ => None,
        }
    }

    /// Whether this is a client request opcode.
    pub const fn is_request(self) -> bool {
        matches!(self, Self::ReqDevList | Self::ReqImport)
    }

    /// The matching reply opcode for a request.
    pub const fn reply(self) -> Option<Self> {
        match self {
            Self::ReqDevList => Some(Self::RepDevList),
            Self::ReqImport => Some(Self::RepImport),
            Self::RepDevList | Self::RepImport => None,
        }
    }

    /// Stable human-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReqDevList => "OP_REQ_DEVLIST",
            Self::RepDevList => "OP_REP_DEVLIST",
            Self::ReqImport => "OP_REQ_IMPORT",
            Self::RepImport => "OP_REP_IMPORT",
        }
    }
}

/// A USB/IP `ST_*` status code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UsbIpStatus {
    /// `ST_OK`.
    #[default]
    Ok,
    /// `ST_NA` (not available).
    Na,
    /// `ST_DEV_BUSY`.
    DevBusy,
    /// `ST_DEV_ERR`.
    DevErr,
    /// `ST_NODEV`.
    NoDev,
    /// `ST_ERROR`.
    Error,
}

impl UsbIpStatus {
    /// Raw network-order status value.
    pub const fn raw(self) -> u32 {
        match self {
            Self::Ok => 0x00,
            Self::Na => 0x01,
            Self::DevBusy => 0x02,
            Self::DevErr => 0x03,
            Self::NoDev => 0x04,
            Self::Error => 0x05,
        }
    }

    /// Parse a raw status, rejecting unknown values.
    pub const fn from_raw(status: u32) -> Option<Self> {
        match status {
            0x00 => Some(Self::Ok),
            0x01 => Some(Self::Na),
            0x02 => Some(Self::DevBusy),
            0x03 => Some(Self::DevErr),
            0x04 => Some(Self::NoDev),
            0x05 => Some(Self::Error),
            _ => None,
        }
    }

    /// Stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Na => "not-available",
            Self::DevBusy => "device-busy",
            Self::DevErr => "device-error",
            Self::NoDev => "no-device",
            Self::Error => "error",
        }
    }
}

/// The 8-byte `struct op_common` control header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpOpHeader {
    /// Protocol version (`0x0111` for a valid frame).
    pub version: u16,
    /// Request or reply opcode.
    pub opcode: UsbIpOpCode,
    /// Status (`Ok` on requests).
    pub status: UsbIpStatus,
}

impl UsbIpOpHeader {
    /// A request header for the configured protocol version.
    pub const fn request(opcode: UsbIpOpCode) -> Self {
        Self {
            version: USBIP_PROTOCOL_VERSION,
            opcode,
            status: UsbIpStatus::Ok,
        }
    }

    /// A reply header for the configured protocol version.
    pub const fn reply(opcode: UsbIpOpCode, status: UsbIpStatus) -> Self {
        Self {
            version: USBIP_PROTOCOL_VERSION,
            opcode,
            status,
        }
    }

    /// Encode network byte order.
    pub fn encode(self) -> [u8; USBIP_OP_HEADER_BYTES] {
        let mut bytes = [0u8; USBIP_OP_HEADER_BYTES];
        bytes[0..2].copy_from_slice(&self.version.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.opcode.raw().to_be_bytes());
        bytes[4..8].copy_from_slice(&self.status.raw().to_be_bytes());
        bytes
    }

    /// Decode exactly eight bytes, validating version, opcode, and status.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() < USBIP_OP_HEADER_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_OP_HEADER_BYTES,
                available: bytes.len(),
            });
        }
        if bytes.len() > USBIP_OP_HEADER_BYTES {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: USBIP_OP_HEADER_BYTES,
                actual: bytes.len(),
            });
        }
        let version = u16::from_be_bytes([bytes[0], bytes[1]]);
        if version != USBIP_PROTOCOL_VERSION {
            return Err(UsbIpProtocolError::UnsupportedVersion {
                version,
                expected: USBIP_PROTOCOL_VERSION,
            });
        }
        let raw_code = u16::from_be_bytes([bytes[2], bytes[3]]);
        let opcode = UsbIpOpCode::from_raw(raw_code)
            .ok_or(UsbIpProtocolError::UnknownOpcode { code: raw_code })?;
        let raw_status = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let status = UsbIpStatus::from_raw(raw_status)
            .ok_or(UsbIpProtocolError::UnknownStatus { status: raw_status })?;
        if opcode.is_request() && status != UsbIpStatus::Ok {
            return Err(UsbIpProtocolError::InconsistentStatus {
                status,
                context: "a request header",
            });
        }
        Ok(Self {
            version,
            opcode,
            status,
        })
    }
}

/// One `struct usbip_usb_interface` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpInterface {
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
}

impl UsbIpInterface {
    /// Construct an interface record.
    pub const fn new(class: u8, subclass: u8, protocol: u8) -> Self {
        Self {
            class,
            subclass,
            protocol,
        }
    }
}

/// A `struct usbip_usb_device` record plus its DEVLIST interface records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpDeviceDescriptor {
    /// Sysfs path (`/sys/devices/...`).
    pub path: String,
    /// USB/IP bus id (`1-1`, `1-1.2`).
    pub bus_id: String,
    /// Bus number.
    pub busnum: u32,
    /// Device number.
    pub devnum: u32,
    /// `USB_SPEED_*` value (1 = low, 2 = full, 3 = high, 5 = super).
    pub speed: u32,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `bcdDevice`.
    pub bcd_device: u16,
    /// `bDeviceClass`.
    pub device_class: u8,
    /// `bDeviceSubClass`.
    pub device_subclass: u8,
    /// `bDeviceProtocol`.
    pub device_protocol: u8,
    /// `bConfigurationValue`.
    pub configuration_value: u8,
    /// `bNumConfigurations`.
    pub num_configurations: u8,
    /// `bNumInterfaces` (the declared count).
    pub num_interfaces: u8,
    /// DEVLIST interface records; empty in an import reply.
    pub interfaces: Vec<UsbIpInterface>,
}

impl UsbIpDeviceDescriptor {
    /// An otherwise-empty descriptor with the required strings.
    pub fn new(path: impl Into<String>, bus_id: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            bus_id: bus_id.into(),
            busnum: 0,
            devnum: 0,
            speed: 0,
            vendor_id: 0,
            product_id: 0,
            bcd_device: 0,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 0,
            num_configurations: 0,
            num_interfaces: 0,
            interfaces: Vec::new(),
        }
    }

    /// Set `idVendor`/`idProduct`.
    pub fn with_ids(mut self, vendor_id: u16, product_id: u16) -> Self {
        self.vendor_id = vendor_id;
        self.product_id = product_id;
        self
    }

    /// Set `busnum`/`devnum`.
    pub fn with_numbers(mut self, busnum: u32, devnum: u32) -> Self {
        self.busnum = busnum;
        self.devnum = devnum;
        self
    }

    /// Set the USB speed value.
    pub fn with_speed(mut self, speed: u32) -> Self {
        self.speed = speed;
        self
    }

    /// Set `bcdDevice`.
    pub fn with_bcd_device(mut self, bcd_device: u16) -> Self {
        self.bcd_device = bcd_device;
        self
    }

    /// Set the device class triple.
    pub fn with_device_class(mut self, class: u8, subclass: u8, protocol: u8) -> Self {
        self.device_class = class;
        self.device_subclass = subclass;
        self.device_protocol = protocol;
        self
    }

    /// Set `bConfigurationValue`/`bNumConfigurations`.
    pub fn with_configuration(mut self, value: u8, count: u8) -> Self {
        self.configuration_value = value;
        self.num_configurations = count;
        self
    }

    /// Replace the interface records and the declared `bNumInterfaces`.
    pub fn with_interfaces(mut self, interfaces: Vec<UsbIpInterface>) -> Self {
        self.num_interfaces = interfaces.len().min(u8::MAX as usize) as u8;
        self.interfaces = interfaces;
        self
    }

    /// Whether the descriptor shape is encodable.
    pub fn validate(&self) -> Result<(), UsbIpProtocolError> {
        validate_text("path", &self.path, USBIP_PATH_BYTES)?;
        validate_text("busid", &self.bus_id, USBIP_BUSID_BYTES)?;
        validate_bus_id(&self.bus_id).map_err(|_| UsbIpProtocolError::InvalidString {
            field: "busid",
            reason: "not a valid USB/IP bus id".to_owned(),
        })?;
        if self.interfaces.len() > USBIP_MAX_INTERFACES {
            return Err(UsbIpProtocolError::Oversized {
                field: "interfaces",
                limit: USBIP_MAX_INTERFACES,
                requested: self.interfaces.len(),
            });
        }
        if self.num_interfaces as usize > USBIP_MAX_INTERFACES {
            return Err(UsbIpProtocolError::Oversized {
                field: "bNumInterfaces",
                limit: USBIP_MAX_INTERFACES,
                requested: self.num_interfaces as usize,
            });
        }
        Ok(())
    }

    /// Encoded size of the device record without interface records.
    pub const fn base_len(&self) -> usize {
        USBIP_DEVICE_BYTES
    }

    /// Encoded DEVLIST size (device record plus interface records).
    pub const fn devlist_len(&self) -> usize {
        USBIP_DEVICE_BYTES + self.num_interfaces as usize * USBIP_INTERFACE_BYTES
    }

    /// Encode the device plus its interface records for `OP_REP_DEVLIST`.
    pub fn encode_devlist(&self) -> Result<Vec<u8>, UsbIpProtocolError> {
        self.validate()?;
        if self.interfaces.len() != self.num_interfaces as usize {
            return Err(UsbIpProtocolError::CountMismatch {
                field: "interfaces",
                declared: self.num_interfaces as usize,
                actual: self.interfaces.len(),
            });
        }
        let mut bytes = Vec::with_capacity(self.devlist_len());
        self.encode_base(&mut bytes);
        for interface in &self.interfaces {
            bytes.push(interface.class);
            bytes.push(interface.subclass);
            bytes.push(interface.protocol);
            bytes.push(0);
        }
        Ok(bytes)
    }

    /// Encode only the 312-byte record for `OP_REP_IMPORT`.
    pub fn encode_import(&self) -> Result<Vec<u8>, UsbIpProtocolError> {
        self.validate()?;
        if self.interfaces.len() > self.num_interfaces as usize {
            return Err(UsbIpProtocolError::CountMismatch {
                field: "interfaces",
                declared: self.num_interfaces as usize,
                actual: self.interfaces.len(),
            });
        }
        let mut bytes = Vec::with_capacity(USBIP_DEVICE_BYTES);
        self.encode_base(&mut bytes);
        Ok(bytes)
    }

    /// Decode an exact DEVLIST device record (including interfaces).
    pub fn decode_devlist(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        let (device, used) = Self::decode_devlist_at(bytes, 0)?;
        if used != bytes.len() {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: used,
                actual: bytes.len(),
            });
        }
        Ok(device)
    }

    /// Decode an exact import-reply device record (no interface records).
    pub fn decode_import(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() < USBIP_DEVICE_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_DEVICE_BYTES,
                available: bytes.len(),
            });
        }
        if bytes.len() > USBIP_DEVICE_BYTES {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: USBIP_DEVICE_BYTES,
                actual: bytes.len(),
            });
        }
        Self::decode_base(bytes)
    }

    fn encode_base(&self, bytes: &mut Vec<u8>) {
        encode_fixed_string_unchecked("path", &self.path, USBIP_PATH_BYTES, bytes);
        encode_fixed_string_unchecked("busid", &self.bus_id, USBIP_BUSID_BYTES, bytes);
        bytes.extend_from_slice(&self.busnum.to_be_bytes());
        bytes.extend_from_slice(&self.devnum.to_be_bytes());
        bytes.extend_from_slice(&self.speed.to_be_bytes());
        bytes.extend_from_slice(&self.vendor_id.to_be_bytes());
        bytes.extend_from_slice(&self.product_id.to_be_bytes());
        bytes.extend_from_slice(&self.bcd_device.to_be_bytes());
        bytes.push(self.device_class);
        bytes.push(self.device_subclass);
        bytes.push(self.device_protocol);
        bytes.push(self.configuration_value);
        bytes.push(self.num_configurations);
        bytes.push(self.num_interfaces);
    }

    fn decode_base(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() < USBIP_DEVICE_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_DEVICE_BYTES,
                available: bytes.len(),
            });
        }
        let path = decode_fixed_string("path", &bytes[0..USBIP_PATH_BYTES])?;
        let bus_id = decode_fixed_string(
            "busid",
            &bytes[USBIP_PATH_BYTES..USBIP_PATH_BYTES + USBIP_BUSID_BYTES],
        )?;
        validate_bus_id(&bus_id).map_err(|_| UsbIpProtocolError::InvalidString {
            field: "busid",
            reason: "not a valid USB/IP bus id".to_owned(),
        })?;
        let mut cursor = USBIP_PATH_BYTES + USBIP_BUSID_BYTES;
        let busnum = read_u32(bytes, &mut cursor);
        let devnum = read_u32(bytes, &mut cursor);
        let speed = read_u32(bytes, &mut cursor);
        let vendor_id = read_u16(bytes, &mut cursor);
        let product_id = read_u16(bytes, &mut cursor);
        let bcd_device = read_u16(bytes, &mut cursor);
        let device_class = bytes[cursor];
        let device_subclass = bytes[cursor + 1];
        let device_protocol = bytes[cursor + 2];
        let configuration_value = bytes[cursor + 3];
        let num_configurations = bytes[cursor + 4];
        let num_interfaces = bytes[cursor + 5];
        if num_interfaces as usize > USBIP_MAX_INTERFACES {
            return Err(UsbIpProtocolError::Oversized {
                field: "bNumInterfaces",
                limit: USBIP_MAX_INTERFACES,
                requested: num_interfaces as usize,
            });
        }
        Ok(Self {
            path,
            bus_id,
            busnum,
            devnum,
            speed,
            vendor_id,
            product_id,
            bcd_device,
            device_class,
            device_subclass,
            device_protocol,
            configuration_value,
            num_configurations,
            num_interfaces,
            interfaces: Vec::new(),
        })
    }

    fn decode_devlist_at(bytes: &[u8], offset: usize) -> Result<(Self, usize), UsbIpProtocolError> {
        let end = offset
            .checked_add(USBIP_DEVICE_BYTES)
            .ok_or(UsbIpProtocolError::Oversized {
                field: "offset",
                limit: USBIP_MAX_FRAME_BYTES,
                requested: usize::MAX,
            })?;
        if bytes.len() < end {
            return Err(UsbIpProtocolError::Truncated {
                needed: end,
                available: bytes.len(),
            });
        }
        let mut device = Self::decode_base(&bytes[offset..end])?;
        let interface_bytes = device.num_interfaces as usize * USBIP_INTERFACE_BYTES;
        let next = end
            .checked_add(interface_bytes)
            .ok_or(UsbIpProtocolError::Oversized {
                field: "interfaces",
                limit: USBIP_MAX_INTERFACES,
                requested: device.num_interfaces as usize,
            })?;
        if bytes.len() < next {
            return Err(UsbIpProtocolError::Truncated {
                needed: next,
                available: bytes.len(),
            });
        }
        let mut interface_cursor = end;
        for _ in 0..device.num_interfaces {
            device.interfaces.push(UsbIpInterface {
                class: bytes[interface_cursor],
                subclass: bytes[interface_cursor + 1],
                protocol: bytes[interface_cursor + 2],
            });
            interface_cursor += USBIP_INTERFACE_BYTES;
        }
        Ok((device, next))
    }
}

/// An `OP_REQ_DEVLIST` request (header only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpDevListRequest {
    /// Request header.
    pub header: UsbIpOpHeader,
}

impl Default for UsbIpDevListRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl UsbIpDevListRequest {
    /// A version-negotiated request header.
    pub const fn new() -> Self {
        Self {
            header: UsbIpOpHeader::request(UsbIpOpCode::ReqDevList),
        }
    }

    /// Encode the eight-byte request.
    pub fn encode(&self) -> Result<[u8; USBIP_OP_HEADER_BYTES], UsbIpProtocolError> {
        validate_header(&self.header, UsbIpOpCode::ReqDevList)?;
        Ok(self.header.encode())
    }

    /// Decode an exact eight-byte request.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        let header = UsbIpOpHeader::decode(bytes)?;
        validate_header(&header, UsbIpOpCode::ReqDevList)?;
        Ok(Self { header })
    }
}

/// An `OP_REP_DEVLIST` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpDevListReply {
    /// Reply header.
    pub header: UsbIpOpHeader,
    /// Exportable devices. Empty on an error reply.
    pub devices: Vec<UsbIpDeviceDescriptor>,
}

impl UsbIpDevListReply {
    /// A successful device-list reply.
    pub fn new(devices: Vec<UsbIpDeviceDescriptor>) -> Self {
        Self {
            header: UsbIpOpHeader::reply(UsbIpOpCode::RepDevList, UsbIpStatus::Ok),
            devices,
        }
    }

    /// An error device-list reply with no devices.
    pub fn error(status: UsbIpStatus) -> Self {
        Self {
            header: UsbIpOpHeader::reply(UsbIpOpCode::RepDevList, status),
            devices: Vec::new(),
        }
    }

    /// Encode the complete reply.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpProtocolError> {
        validate_header(&self.header, UsbIpOpCode::RepDevList)?;
        if self.header.status != UsbIpStatus::Ok && !self.devices.is_empty() {
            return Err(UsbIpProtocolError::InconsistentStatus {
                status: self.header.status,
                context: "a device-list error reply",
            });
        }
        if self.devices.len() > USBIP_MAX_DEVICES {
            return Err(UsbIpProtocolError::Oversized {
                field: "ndev",
                limit: USBIP_MAX_DEVICES,
                requested: self.devices.len(),
            });
        }
        let mut bytes = Vec::with_capacity(USBIP_DEVLIST_HEADER_BYTES);
        bytes.extend_from_slice(&self.header.encode());
        bytes.extend_from_slice(&(self.devices.len() as u32).to_be_bytes());
        for device in &self.devices {
            bytes.extend_from_slice(&device.encode_devlist()?);
        }
        if bytes.len() > USBIP_MAX_FRAME_BYTES {
            return Err(UsbIpProtocolError::Oversized {
                field: "frame",
                limit: USBIP_MAX_FRAME_BYTES,
                requested: bytes.len(),
            });
        }
        Ok(bytes)
    }

    /// Decode a complete reply with the codec-wide frame bound.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        Self::decode_bounded(bytes, USBIP_MAX_FRAME_BYTES)
    }

    /// Decode a complete reply, rejecting frames larger than `max_frame_bytes`.
    pub fn decode_bounded(
        bytes: &[u8],
        max_frame_bytes: usize,
    ) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() > max_frame_bytes {
            return Err(UsbIpProtocolError::Oversized {
                field: "frame",
                limit: max_frame_bytes,
                requested: bytes.len(),
            });
        }
        if bytes.len() < USBIP_DEVLIST_HEADER_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_DEVLIST_HEADER_BYTES,
                available: bytes.len(),
            });
        }
        let header = UsbIpOpHeader::decode(&bytes[0..USBIP_OP_HEADER_BYTES])?;
        validate_header(&header, UsbIpOpCode::RepDevList)?;
        let ndev = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        if ndev > USBIP_MAX_DEVICES {
            return Err(UsbIpProtocolError::Oversized {
                field: "ndev",
                limit: USBIP_MAX_DEVICES,
                requested: ndev,
            });
        }
        if header.status != UsbIpStatus::Ok {
            if ndev != 0 {
                return Err(UsbIpProtocolError::InconsistentStatus {
                    status: header.status,
                    context: "a device-list error reply with devices",
                });
            }
            if bytes.len() != USBIP_DEVLIST_HEADER_BYTES {
                return Err(UsbIpProtocolError::TrailingBytes {
                    expected: USBIP_DEVLIST_HEADER_BYTES,
                    actual: bytes.len(),
                });
            }
            return Ok(Self {
                header,
                devices: Vec::new(),
            });
        }
        let mut devices = Vec::with_capacity(ndev);
        let mut offset = USBIP_DEVLIST_HEADER_BYTES;
        for _ in 0..ndev {
            let (device, next) = UsbIpDeviceDescriptor::decode_devlist_at(bytes, offset)?;
            offset = next;
            devices.push(device);
        }
        if offset != bytes.len() {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: offset,
                actual: bytes.len(),
            });
        }
        Ok(Self { header, devices })
    }
}

/// An `OP_REQ_IMPORT` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpImportRequest {
    /// Request header.
    pub header: UsbIpOpHeader,
    /// Requested bus id.
    pub bus_id: String,
}

impl UsbIpImportRequest {
    /// A version-negotiated import request.
    pub fn new(bus_id: impl Into<String>) -> Self {
        Self {
            header: UsbIpOpHeader::request(UsbIpOpCode::ReqImport),
            bus_id: bus_id.into(),
        }
    }

    /// Encode the fixed 40-byte request.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpProtocolError> {
        validate_header(&self.header, UsbIpOpCode::ReqImport)?;
        validate_bus_id(&self.bus_id).map_err(|_| UsbIpProtocolError::InvalidString {
            field: "busid",
            reason: "not a valid USB/IP bus id".to_owned(),
        })?;
        let mut bytes = Vec::with_capacity(USBIP_IMPORT_REQUEST_BYTES);
        bytes.extend_from_slice(&self.header.encode());
        encode_fixed_string("busid", &self.bus_id, USBIP_BUSID_BYTES, &mut bytes)?;
        Ok(bytes)
    }

    /// Decode an exact 40-byte request.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() < USBIP_IMPORT_REQUEST_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_IMPORT_REQUEST_BYTES,
                available: bytes.len(),
            });
        }
        if bytes.len() > USBIP_IMPORT_REQUEST_BYTES {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: USBIP_IMPORT_REQUEST_BYTES,
                actual: bytes.len(),
            });
        }
        let header = UsbIpOpHeader::decode(&bytes[0..USBIP_OP_HEADER_BYTES])?;
        validate_header(&header, UsbIpOpCode::ReqImport)?;
        let bus_id = decode_fixed_string(
            "busid",
            &bytes[USBIP_OP_HEADER_BYTES..USBIP_IMPORT_REQUEST_BYTES],
        )?;
        validate_bus_id(&bus_id).map_err(|_| UsbIpProtocolError::InvalidString {
            field: "busid",
            reason: "not a valid USB/IP bus id".to_owned(),
        })?;
        Ok(Self { header, bus_id })
    }
}

/// An `OP_REP_IMPORT` reply.
///
/// A successful reply carries exactly the 312-byte device record and nothing
/// else. An error reply carries only the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpImportReply {
    /// Reply header.
    pub header: UsbIpOpHeader,
    /// Imported device on success; `None` on any error status.
    pub device: Option<UsbIpDeviceDescriptor>,
}

impl UsbIpImportReply {
    /// A successful reply for the attached device.
    pub fn new(device: UsbIpDeviceDescriptor) -> Self {
        Self {
            header: UsbIpOpHeader::reply(UsbIpOpCode::RepImport, UsbIpStatus::Ok),
            device: Some(device),
        }
    }

    /// An error reply.
    pub fn error(status: UsbIpStatus) -> Self {
        Self {
            header: UsbIpOpHeader::reply(UsbIpOpCode::RepImport, status),
            device: None,
        }
    }

    /// Encode the complete reply.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpProtocolError> {
        validate_header(&self.header, UsbIpOpCode::RepImport)?;
        if self.header.status == UsbIpStatus::Ok {
            let Some(device) = &self.device else {
                return Err(UsbIpProtocolError::InconsistentStatus {
                    status: self.header.status,
                    context: "a successful import reply without a device",
                });
            };
            let mut bytes = Vec::with_capacity(USBIP_IMPORT_REPLY_BYTES);
            bytes.extend_from_slice(&self.header.encode());
            bytes.extend_from_slice(&device.encode_import()?);
            return Ok(bytes);
        }
        if self.device.is_some() {
            return Err(UsbIpProtocolError::InconsistentStatus {
                status: self.header.status,
                context: "an import error reply with a device",
            });
        }
        Ok(self.header.encode().to_vec())
    }

    /// Decode a complete reply.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpProtocolError> {
        if bytes.len() < USBIP_OP_HEADER_BYTES {
            return Err(UsbIpProtocolError::Truncated {
                needed: USBIP_OP_HEADER_BYTES,
                available: bytes.len(),
            });
        }
        let header = UsbIpOpHeader::decode(&bytes[0..USBIP_OP_HEADER_BYTES])?;
        validate_header(&header, UsbIpOpCode::RepImport)?;
        if header.status == UsbIpStatus::Ok {
            if bytes.len() < USBIP_IMPORT_REPLY_BYTES {
                return Err(UsbIpProtocolError::Truncated {
                    needed: USBIP_IMPORT_REPLY_BYTES,
                    available: bytes.len(),
                });
            }
            if bytes.len() > USBIP_IMPORT_REPLY_BYTES {
                return Err(UsbIpProtocolError::TrailingBytes {
                    expected: USBIP_IMPORT_REPLY_BYTES,
                    actual: bytes.len(),
                });
            }
            let device = UsbIpDeviceDescriptor::decode_import(
                &bytes[USBIP_OP_HEADER_BYTES..USBIP_IMPORT_REPLY_BYTES],
            )?;
            return Ok(Self {
                header,
                device: Some(device),
            });
        }
        if bytes.len() != USBIP_OP_HEADER_BYTES {
            return Err(UsbIpProtocolError::TrailingBytes {
                expected: USBIP_OP_HEADER_BYTES,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            header,
            device: None,
        })
    }
}

fn validate_header(
    header: &UsbIpOpHeader,
    expected: UsbIpOpCode,
) -> Result<(), UsbIpProtocolError> {
    if header.version != USBIP_PROTOCOL_VERSION {
        return Err(UsbIpProtocolError::UnsupportedVersion {
            version: header.version,
            expected: USBIP_PROTOCOL_VERSION,
        });
    }
    if header.opcode != expected {
        return Err(UsbIpProtocolError::UnexpectedOpcode {
            expected: expected.name(),
            actual: header.opcode.name(),
        });
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    capacity: usize,
) -> Result<(), UsbIpProtocolError> {
    if value.is_empty() {
        return Err(UsbIpProtocolError::InvalidString {
            field,
            reason: "must not be empty".to_owned(),
        });
    }
    if value.len() >= capacity {
        return Err(UsbIpProtocolError::Oversized {
            field,
            limit: capacity - 1,
            requested: value.len(),
        });
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(UsbIpProtocolError::InvalidString {
            field,
            reason: "must be printable ASCII without NUL".to_owned(),
        });
    }
    Ok(())
}

fn encode_fixed_string(
    field: &'static str,
    value: &str,
    capacity: usize,
    bytes: &mut Vec<u8>,
) -> Result<(), UsbIpProtocolError> {
    validate_text(field, value, capacity)?;
    encode_fixed_string_unchecked(field, value, capacity, bytes);
    Ok(())
}

fn encode_fixed_string_unchecked(
    _field: &'static str,
    value: &str,
    capacity: usize,
    bytes: &mut Vec<u8>,
) {
    let start = bytes.len();
    bytes.extend_from_slice(value.as_bytes());
    bytes.resize(start + capacity, 0);
}

fn decode_fixed_string(field: &'static str, bytes: &[u8]) -> Result<String, UsbIpProtocolError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let value = &bytes[..end];
    if value.is_empty() {
        return Err(UsbIpProtocolError::InvalidString {
            field,
            reason: "must not be empty".to_owned(),
        });
    }
    if !value.iter().all(|byte| byte.is_ascii_graphic()) {
        return Err(UsbIpProtocolError::InvalidString {
            field,
            reason: "must be printable ASCII without NUL".to_owned(),
        });
    }
    String::from_utf8(value.to_vec()).map_err(|_| UsbIpProtocolError::InvalidString {
        field,
        reason: "must be valid UTF-8/ASCII".to_owned(),
    })
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> u32 {
    let value = u32::from_be_bytes([
        bytes[*cursor],
        bytes[*cursor + 1],
        bytes[*cursor + 2],
        bytes[*cursor + 3],
    ]);
    *cursor += 4;
    value
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> u16 {
    let value = u16::from_be_bytes([bytes[*cursor], bytes[*cursor + 1]]);
    *cursor += 2;
    value
}
