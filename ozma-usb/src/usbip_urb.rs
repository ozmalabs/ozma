// SPDX-License-Identifier: AGPL-3.0-only
//! Typed USB/IP URB (bulk data path) wire codec.
//!
//! The control plane (`OP_REQ_DEVLIST`/`OP_REQ_IMPORT`) is implemented in
//! [`crate::usbip_protocol`]. After a successful `OP_REP_IMPORT` the same
//! connection carries the four URB messages described by the kernel's
//! `include/uapi/linux/usbip.h`:
//!
//! | Command | Raw value | Direction | Body |
//! |---|---|---|---|
//! | `USBIP_CMD_SUBMIT` | `0x00000001` | client → server | submit fields + OUT data + ISO descriptors |
//! | `USBIP_CMD_UNLINK` | `0x00000002` | client → server | target `seqnum` |
//! | `USBIP_RET_SUBMIT` | `0x00000003` | server → client | completion fields + IN data + ISO descriptors |
//! | `USBIP_RET_UNLINK` | `0x00000004` | server → client | status |
//!
//! Every message starts with the packed 20-byte `usbip_header_basic`
//! (`command`, `seqnum`, `devid`, `direction`, `ep`) followed by the 28-byte
//! command-specific union, for a 48-byte header. All integers are network byte
//! order.
//!
//! The payload is authentic device data or empty; this codec never invents
//! bytes. Encoders reject a payload whose length does not match the declared
//! `transfer_buffer_length` (OUT submit) or `actual_length` (IN completion) and
//! manually redact data from `Debug`, so a URB payload cannot leak through a
//! status/log line.
//!
//! Bounds are explicit:
//!
//! * transfer/actual lengths are capped by [`USBIP_URB_MAX_TRANSFER_BYTES`],
//!   which reuses the control frame ceiling [`USBIP_MAX_FRAME_BYTES`];
//! * ISO packet counts are capped by [`USBIP_MAX_ISO_PACKETS`] and each
//!   descriptor's `offset + length` is checked against the transfer buffer;
//! * a non-ISO URB must use the kernel sentinel
//!   [`USBIP_URB_NO_ISO_PACKETS`] (`0xffffffff`), with `0` also accepted for
//!   compatibility, and must not carry descriptors;
//! * truncated and trailing bytes, unknown commands, unsupported directions,
//!   and endpoints above 15 are distinct typed errors.

use std::fmt;

use thiserror::Error;

use crate::usbip_protocol::USBIP_MAX_FRAME_BYTES;

/// `USBIP_CMD_SUBMIT`.
pub const USBIP_CMD_SUBMIT: u32 = 0x0000_0001;
/// `USBIP_CMD_UNLINK`.
pub const USBIP_CMD_UNLINK: u32 = 0x0000_0002;
/// `USBIP_RET_SUBMIT`.
pub const USBIP_RET_SUBMIT: u32 = 0x0000_0003;
/// `USBIP_RET_UNLINK`.
pub const USBIP_RET_UNLINK: u32 = 0x0000_0004;

/// `USBIP_DIR_OUT`.
pub const USBIP_DIR_OUT: u32 = 0;
/// `USBIP_DIR_IN`.
pub const USBIP_DIR_IN: u32 = 1;

/// Encoded `usbip_header` size (basic header plus command union).
pub const USBIP_URB_HEADER_BYTES: usize = 48;
/// Encoded `usbip_iso_packet_descriptor` size.
pub const USBIP_ISO_PACKET_BYTES: usize = 16;
/// Maximum ISO descriptors in one URB (strictly bounded).
pub const USBIP_MAX_ISO_PACKETS: usize = 128;
/// Highest valid USB endpoint number carried by a submit header.
pub const USBIP_MAX_ENDPOINT: u32 = 15;
/// `number_of_packets` sentinel for a non-ISO transfer.
pub const USBIP_URB_NO_ISO_PACKETS: u32 = u32::MAX;

/// Largest transfer buffer accepted by the URB codec.
///
/// This deliberately reuses the control frame ceiling so the data path cannot
/// allocate a frame larger than the control plane already permits.
pub const USBIP_URB_MAX_TRANSFER_BYTES: usize = USBIP_MAX_FRAME_BYTES;
/// Largest encoded URB frame including data and ISO descriptors.
pub const USBIP_URB_MAX_FRAME_BYTES: usize = USBIP_URB_HEADER_BYTES
    + USBIP_URB_MAX_TRANSFER_BYTES
    + USBIP_MAX_ISO_PACKETS * USBIP_ISO_PACKET_BYTES;

/// A typed USB/IP URB codec failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UsbIpUrbError {
    /// The frame ended before the header or declared payload was complete.
    #[error("USB/IP URB frame is truncated: need {needed} bytes, have {available}")]
    Truncated {
        /// Required bytes.
        needed: usize,
        /// Available bytes.
        available: usize,
    },

    /// The frame contained bytes after the last field/descriptor.
    #[error("USB/IP URB frame has {actual} bytes; expected exactly {expected}")]
    TrailingBytes {
        /// Expected frame size.
        expected: usize,
        /// Actual frame size.
        actual: usize,
    },

    /// The command word is not one of the four URB commands.
    #[error("USB/IP URB command 0x{command:08x} is unknown")]
    UnknownCommand {
        /// Raw command word.
        command: u32,
    },

    /// Direction is neither `USBIP_DIR_OUT` nor `USBIP_DIR_IN`.
    #[error("USB/IP URB direction {direction} is unsupported (expected 0 or 1)")]
    UnsupportedDirection {
        /// Raw direction word.
        direction: u32,
    },

    /// Endpoint number is outside the USB 4-bit range.
    #[error("USB/IP URB endpoint {ep} is invalid (maximum {limit})")]
    InvalidEndpoint {
        /// Raw endpoint word.
        ep: u32,
        /// Highest accepted endpoint.
        limit: u32,
    },

    /// A signed header field was negative or otherwise unusable.
    #[error("USB/IP URB {field} is invalid: {reason}")]
    InvalidField {
        /// Field name (`transfer_buffer_length`, `actual_length`, ...).
        field: &'static str,
        /// Why the value was rejected.
        reason: String,
    },

    /// A length or count exceeds its configured bound.
    #[error("USB/IP URB {field} is too large: {requested} > {limit}")]
    Oversized {
        /// Field or frame name.
        field: &'static str,
        /// Maximum accepted value.
        limit: usize,
        /// Requested value.
        requested: usize,
    },

    /// The declared ISO packet count does not match the descriptor array.
    #[error("USB/IP URB {field} count mismatch: declared {declared}, actual {actual}")]
    CountMismatch {
        /// Field name.
        field: &'static str,
        /// Declared count.
        declared: usize,
        /// Actual descriptor count.
        actual: usize,
    },

    /// An ISO descriptor points outside the transfer buffer.
    #[error(
        "USB/IP URB ISO packet {index} is out of bounds: offset {offset} + length {length} > {limit}"
    )]
    IsoPacketOutOfBounds {
        /// Descriptor index.
        index: usize,
        /// Declared offset.
        offset: usize,
        /// Declared length.
        length: usize,
        /// Buffer bound.
        limit: usize,
    },

    /// A payload length does not agree with the header.
    #[error("USB/IP URB {field} is inconsistent: expected {expected}, actual {actual}")]
    InconsistentData {
        /// Field/payload name.
        field: &'static str,
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
}

/// USB transfer direction as encoded in the basic header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsbIpDirection {
    /// Host-to-device (`USBIP_DIR_OUT`).
    Out,
    /// Device-to-host (`USBIP_DIR_IN`).
    In,
}

impl UsbIpDirection {
    /// Raw network-order value.
    pub const fn raw(self) -> u32 {
        match self {
            Self::Out => USBIP_DIR_OUT,
            Self::In => USBIP_DIR_IN,
        }
    }

    /// Parse a raw value, rejecting anything else.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            USBIP_DIR_OUT => Some(Self::Out),
            USBIP_DIR_IN => Some(Self::In),
            _ => None,
        }
    }

    /// Stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }
}

/// One of the four URB command words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsbIpUrbCommand {
    /// `USBIP_CMD_SUBMIT`.
    CmdSubmit,
    /// `USBIP_CMD_UNLINK`.
    CmdUnlink,
    /// `USBIP_RET_SUBMIT`.
    RetSubmit,
    /// `USBIP_RET_UNLINK`.
    RetUnlink,
}

impl UsbIpUrbCommand {
    /// Raw command word.
    pub const fn raw(self) -> u32 {
        match self {
            Self::CmdSubmit => USBIP_CMD_SUBMIT,
            Self::CmdUnlink => USBIP_CMD_UNLINK,
            Self::RetSubmit => USBIP_RET_SUBMIT,
            Self::RetUnlink => USBIP_RET_UNLINK,
        }
    }

    /// Parse a raw command word.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            USBIP_CMD_SUBMIT => Some(Self::CmdSubmit),
            USBIP_CMD_UNLINK => Some(Self::CmdUnlink),
            USBIP_RET_SUBMIT => Some(Self::RetSubmit),
            USBIP_RET_UNLINK => Some(Self::RetUnlink),
            _ => None,
        }
    }

    /// Whether this command is a client request.
    pub const fn is_request(self) -> bool {
        matches!(self, Self::CmdSubmit | Self::CmdUnlink)
    }

    /// Whether this command is a server reply.
    pub const fn is_reply(self) -> bool {
        matches!(self, Self::RetSubmit | Self::RetUnlink)
    }

    /// Stable human-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::CmdSubmit => "USBIP_CMD_SUBMIT",
            Self::CmdUnlink => "USBIP_CMD_UNLINK",
            Self::RetSubmit => "USBIP_RET_SUBMIT",
            Self::RetUnlink => "USBIP_RET_UNLINK",
        }
    }
}

/// One `usbip_iso_packet_descriptor` (16 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbIpIsoPacketDescriptor {
    /// Packet offset inside the transfer buffer.
    pub offset: u32,
    /// Requested packet length.
    pub length: u32,
    /// Actually transferred packet length.
    pub actual_length: u32,
    /// Negative errno, or zero on success.
    pub status: i32,
}

impl UsbIpIsoPacketDescriptor {
    /// Construct a descriptor.
    pub const fn new(offset: u32, length: u32, actual_length: u32, status: i32) -> Self {
        Self {
            offset,
            length,
            actual_length,
            status,
        }
    }

    fn encode_into(self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.offset.to_be_bytes());
        bytes.extend_from_slice(&self.length.to_be_bytes());
        bytes.extend_from_slice(&self.actual_length.to_be_bytes());
        bytes.extend_from_slice(&self.status.to_be_bytes());
    }

    fn decode_at(bytes: &[u8], offset: usize) -> Self {
        Self {
            offset: read_u32(bytes, offset),
            length: read_u32(bytes, offset + 4),
            actual_length: read_u32(bytes, offset + 8),
            status: read_i32(bytes, offset + 12),
        }
    }
}

/// A `USBIP_CMD_SUBMIT` request.
///
/// `data` is the OUT payload sent by the client. An IN submit carries no
/// payload on the wire; the endpoint supplies the response bytes in the
/// completion.
#[derive(Clone, PartialEq, Eq)]
pub struct UsbIpCmdSubmit {
    /// Sequence number paired with the completion.
    pub seqnum: u32,
    /// Remote device id (`(busnum << 16) | devnum`).
    pub devid: u32,
    /// Transfer direction.
    pub direction: UsbIpDirection,
    /// Endpoint number (0..=15).
    pub ep: u32,
    /// `URB.transfer_flags`.
    pub transfer_flags: u32,
    /// Declared transfer buffer length.
    pub transfer_buffer_length: u32,
    /// ISO start frame (zero for non-ISO).
    pub start_frame: i32,
    /// ISO packet count, or [`USBIP_URB_NO_ISO_PACKETS`].
    pub number_of_packets: u32,
    /// Endpoint interval.
    pub interval: i32,
    /// Eight setup bytes for control transfers (zero otherwise).
    pub setup: [u8; 8],
    /// OUT payload bytes; empty for IN.
    pub data: Vec<u8>,
    /// ISO packet descriptors.
    pub iso_packets: Vec<UsbIpIsoPacketDescriptor>,
}

impl fmt::Debug for UsbIpCmdSubmit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsbIpCmdSubmit")
            .field("seqnum", &self.seqnum)
            .field("devid", &self.devid)
            .field("direction", &self.direction)
            .field("ep", &self.ep)
            .field("transfer_flags", &self.transfer_flags)
            .field("transfer_buffer_length", &self.transfer_buffer_length)
            .field("start_frame", &self.start_frame)
            .field("number_of_packets", &self.number_of_packets)
            .field("interval", &self.interval)
            .field("setup", &self.setup)
            .field("data_len", &self.data.len())
            .field("iso_packets", &self.iso_packets.len())
            .finish()
    }
}

impl UsbIpCmdSubmit {
    /// An otherwise-zero submit for `seqnum`/`devid`.
    pub fn new(seqnum: u32, devid: u32, direction: UsbIpDirection, ep: u32) -> Self {
        Self {
            seqnum,
            devid,
            direction,
            ep,
            transfer_flags: 0,
            transfer_buffer_length: 0,
            start_frame: 0,
            number_of_packets: USBIP_URB_NO_ISO_PACKETS,
            interval: 0,
            setup: [0; 8],
            data: Vec::new(),
            iso_packets: Vec::new(),
        }
    }

    /// A bulk/interrupt OUT submit carrying exactly `data`.
    pub fn out(seqnum: u32, devid: u32, ep: u32, data: Vec<u8>) -> Self {
        let length = data.len().min(u32::MAX as usize) as u32;
        let mut submit = Self::new(seqnum, devid, UsbIpDirection::Out, ep);
        submit.transfer_buffer_length = length;
        submit.data = data;
        submit
    }

    /// A bulk/interrupt IN submit requesting at most `length` bytes.
    pub fn in_request(seqnum: u32, devid: u32, ep: u32, length: u32) -> Self {
        let mut submit = Self::new(seqnum, devid, UsbIpDirection::In, ep);
        submit.transfer_buffer_length = length;
        submit
    }

    /// Number of ISO descriptors implied by `number_of_packets`.
    pub fn iso_packet_count(&self) -> usize {
        iso_count(self.number_of_packets)
    }

    /// Exact encoded frame size.
    pub fn frame_len(&self) -> usize {
        let data_len = match self.direction {
            UsbIpDirection::Out => self.transfer_buffer_length as usize,
            UsbIpDirection::In => 0,
        };
        USBIP_URB_HEADER_BYTES + data_len + self.iso_packets.len() * USBIP_ISO_PACKET_BYTES
    }

    /// Validate lengths, direction, endpoint, payload, and ISO descriptors.
    pub fn validate(&self) -> Result<(), UsbIpUrbError> {
        validate_submit(self, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Encode one complete `USBIP_CMD_SUBMIT` frame.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpUrbError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(self.frame_len());
        encode_basic(
            &mut bytes,
            UsbIpUrbCommand::CmdSubmit,
            self.seqnum,
            self.devid,
            self.direction,
            self.ep,
        );
        bytes.extend_from_slice(&self.transfer_flags.to_be_bytes());
        bytes.extend_from_slice(&self.transfer_buffer_length.to_be_bytes());
        bytes.extend_from_slice(&self.start_frame.to_be_bytes());
        bytes.extend_from_slice(&self.number_of_packets.to_be_bytes());
        bytes.extend_from_slice(&self.interval.to_be_bytes());
        bytes.extend_from_slice(&self.setup);
        if self.direction == UsbIpDirection::Out {
            bytes.extend_from_slice(&self.data);
        }
        for packet in &self.iso_packets {
            packet.encode_into(&mut bytes);
        }
        Ok(bytes)
    }

    /// Decode an exact `USBIP_CMD_SUBMIT` frame with the codec-wide bound.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpUrbError> {
        Self::decode_bounded(bytes, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Decode an exact `USBIP_CMD_SUBMIT` frame with a caller-supplied transfer
    /// ceiling.
    pub fn decode_bounded(bytes: &[u8], max_transfer: usize) -> Result<Self, UsbIpUrbError> {
        match decode_frame(bytes, max_transfer, None)? {
            UsbIpUrb::CmdSubmit(submit) => Ok(submit),
            _ => unreachable!("decoded command was checked against CMD_SUBMIT"),
        }
    }
}

/// A `USBIP_RET_SUBMIT` completion.
///
/// `data` is the IN payload produced by the endpoint; an OUT completion carries
/// no payload even when `actual_length` reports the number of bytes consumed.
#[derive(Clone, PartialEq, Eq)]
pub struct UsbIpRetSubmit {
    /// Sequence number copied from the request.
    pub seqnum: u32,
    /// Device id copied from the request.
    pub devid: u32,
    /// Direction copied from the request (needed to delimit IN payload).
    pub direction: UsbIpDirection,
    /// Endpoint number copied from the request.
    pub ep: u32,
    /// URB status (zero or a negative errno).
    pub status: i32,
    /// Number of transferred bytes.
    pub actual_length: u32,
    /// ISO start frame.
    pub start_frame: i32,
    /// ISO packet count, or [`USBIP_URB_NO_ISO_PACKETS`].
    pub number_of_packets: u32,
    /// URB error count.
    pub error_count: i32,
    /// IN payload bytes; empty for OUT.
    pub data: Vec<u8>,
    /// ISO packet descriptors.
    pub iso_packets: Vec<UsbIpIsoPacketDescriptor>,
}

impl fmt::Debug for UsbIpRetSubmit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsbIpRetSubmit")
            .field("seqnum", &self.seqnum)
            .field("devid", &self.devid)
            .field("direction", &self.direction)
            .field("ep", &self.ep)
            .field("status", &self.status)
            .field("actual_length", &self.actual_length)
            .field("start_frame", &self.start_frame)
            .field("number_of_packets", &self.number_of_packets)
            .field("error_count", &self.error_count)
            .field("data_len", &self.data.len())
            .field("iso_packets", &self.iso_packets.len())
            .finish()
    }
}

impl UsbIpRetSubmit {
    /// A successful completion for `seqnum`/`devid` carrying real IN bytes.
    pub fn new(seqnum: u32, devid: u32, direction: UsbIpDirection, ep: u32, data: Vec<u8>) -> Self {
        let actual_length = data.len().min(u32::MAX as usize) as u32;
        Self {
            seqnum,
            devid,
            direction,
            ep,
            status: 0,
            actual_length,
            start_frame: 0,
            number_of_packets: USBIP_URB_NO_ISO_PACKETS,
            error_count: 0,
            data,
            iso_packets: Vec::new(),
        }
    }

    /// An error completion with no payload.
    pub fn error(seqnum: u32, devid: u32, status: i32) -> Self {
        Self {
            seqnum,
            devid,
            direction: UsbIpDirection::Out,
            ep: 0,
            status,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: USBIP_URB_NO_ISO_PACKETS,
            error_count: 0,
            data: Vec::new(),
            iso_packets: Vec::new(),
        }
    }

    /// Number of ISO descriptors implied by `number_of_packets`.
    pub fn iso_packet_count(&self) -> usize {
        iso_count(self.number_of_packets)
    }

    /// Exact encoded frame size.
    pub fn frame_len(&self) -> usize {
        let data_len = match self.direction {
            UsbIpDirection::In => self.actual_length as usize,
            UsbIpDirection::Out => 0,
        };
        USBIP_URB_HEADER_BYTES + data_len + self.iso_packets.len() * USBIP_ISO_PACKET_BYTES
    }

    /// Validate lengths, direction, endpoint, payload, and ISO descriptors.
    pub fn validate(&self) -> Result<(), UsbIpUrbError> {
        validate_completion(self, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Encode one complete `USBIP_RET_SUBMIT` frame.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpUrbError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(self.frame_len());
        encode_basic(
            &mut bytes,
            UsbIpUrbCommand::RetSubmit,
            self.seqnum,
            self.devid,
            self.direction,
            self.ep,
        );
        bytes.extend_from_slice(&self.status.to_be_bytes());
        bytes.extend_from_slice(&self.actual_length.to_be_bytes());
        bytes.extend_from_slice(&self.start_frame.to_be_bytes());
        bytes.extend_from_slice(&self.number_of_packets.to_be_bytes());
        bytes.extend_from_slice(&self.error_count.to_be_bytes());
        bytes.extend_from_slice(&[0; 8]);
        if self.direction == UsbIpDirection::In {
            bytes.extend_from_slice(&self.data);
        }
        for packet in &self.iso_packets {
            packet.encode_into(&mut bytes);
        }
        Ok(bytes)
    }

    /// Decode an exact `USBIP_RET_SUBMIT` frame, using the encoded direction to
    /// delimit the payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpUrbError> {
        Self::decode_bounded(bytes, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Decode boundedly, using the encoded direction to delimit the payload.
    pub fn decode_bounded(bytes: &[u8], max_transfer: usize) -> Result<Self, UsbIpUrbError> {
        match decode_frame(bytes, max_transfer, None)? {
            UsbIpUrb::RetSubmit(submit) => Ok(submit),
            _ => unreachable!("decoded command was checked against RET_SUBMIT"),
        }
    }

    /// Decode a kernel-style completion whose basic-header `direction` is zero
    /// even for IN payloads.
    ///
    /// USB/IP servers conventionally write `direction = 0` in replies; the
    /// client knows the original URB direction. A standalone decoder therefore
    /// accepts that direction explicitly.
    pub fn decode_with_direction(
        bytes: &[u8],
        direction: UsbIpDirection,
    ) -> Result<Self, UsbIpUrbError> {
        Self::decode_bounded_with_direction(bytes, USBIP_URB_MAX_TRANSFER_BYTES, direction)
    }

    /// Bounded variant of [`Self::decode_with_direction`].
    pub fn decode_bounded_with_direction(
        bytes: &[u8],
        max_transfer: usize,
        direction: UsbIpDirection,
    ) -> Result<Self, UsbIpUrbError> {
        match decode_frame(bytes, max_transfer, Some(direction))? {
            UsbIpUrb::RetSubmit(submit) => Ok(submit),
            _ => unreachable!("decoded command was checked against RET_SUBMIT"),
        }
    }
}

/// A `USBIP_CMD_UNLINK` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpCmdUnlink {
    /// Sequence number of the unlink request itself.
    pub seqnum: u32,
    /// Device id.
    pub devid: u32,
    /// Sequence number of the submit to cancel.
    pub unlink_seqnum: u32,
}

impl UsbIpCmdUnlink {
    /// Construct an unlink request.
    pub const fn new(seqnum: u32, devid: u32, unlink_seqnum: u32) -> Self {
        Self {
            seqnum,
            devid,
            unlink_seqnum,
        }
    }

    /// Encode the fixed 48-byte frame (unused direction/endpoint/padding zero).
    pub fn encode(&self) -> Result<[u8; USBIP_URB_HEADER_BYTES], UsbIpUrbError> {
        let mut bytes = [0u8; USBIP_URB_HEADER_BYTES];
        write_basic(
            &mut bytes,
            UsbIpUrbCommand::CmdUnlink,
            self.seqnum,
            self.devid,
            UsbIpDirection::Out,
            0,
        );
        bytes[20..24].copy_from_slice(&self.unlink_seqnum.to_be_bytes());
        Ok(bytes)
    }

    /// Decode an exact 48-byte frame.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpUrbError> {
        match decode_frame(bytes, USBIP_URB_MAX_TRANSFER_BYTES, None)? {
            UsbIpUrb::CmdUnlink(unlink) => Ok(unlink),
            _ => unreachable!("decoded command was checked against CMD_UNLINK"),
        }
    }
}

/// A `USBIP_RET_UNLINK` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpRetUnlink {
    /// Sequence number copied from the unlink request.
    pub seqnum: u32,
    /// Device id copied from the unlink request.
    pub devid: u32,
    /// Negative errno (`-ECONNRESET` after cancellation, `0` when the submit
    /// already completed) or another endpoint-supplied status.
    pub status: i32,
}

impl UsbIpRetUnlink {
    /// Construct an unlink reply.
    pub const fn new(seqnum: u32, devid: u32, status: i32) -> Self {
        Self {
            seqnum,
            devid,
            status,
        }
    }

    /// Successful cancellation status (`-ECONNRESET`).
    pub const fn cancelled(seqnum: u32, devid: u32) -> Self {
        Self::new(seqnum, devid, USBIP_URB_UNLINK_CANCELLED)
    }

    /// The submit already completed before the unlink arrived.
    pub const fn not_found(seqnum: u32, devid: u32) -> Self {
        Self::new(seqnum, devid, USBIP_URB_UNLINK_NOT_FOUND)
    }

    /// Encode the fixed 48-byte frame (unused direction/endpoint/padding zero).
    pub fn encode(&self) -> Result<[u8; USBIP_URB_HEADER_BYTES], UsbIpUrbError> {
        let mut bytes = [0u8; USBIP_URB_HEADER_BYTES];
        write_basic(
            &mut bytes,
            UsbIpUrbCommand::RetUnlink,
            self.seqnum,
            self.devid,
            UsbIpDirection::Out,
            0,
        );
        bytes[20..24].copy_from_slice(&self.status.to_be_bytes());
        Ok(bytes)
    }

    /// Decode an exact 48-byte frame.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpUrbError> {
        match decode_frame(bytes, USBIP_URB_MAX_TRANSFER_BYTES, None)? {
            UsbIpUrb::RetUnlink(unlink) => Ok(unlink),
            _ => unreachable!("decoded command was checked against RET_UNLINK"),
        }
    }
}

/// `-ECONNRESET`, the kernel's successful `USBIP_RET_UNLINK` status.
pub const USBIP_URB_UNLINK_CANCELLED: i32 = -104;
/// Status used when the target submit already completed.
pub const USBIP_URB_UNLINK_NOT_FOUND: i32 = 0;

/// A decoded URB of any command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbIpUrb {
    /// `USBIP_CMD_SUBMIT`.
    CmdSubmit(UsbIpCmdSubmit),
    /// `USBIP_RET_SUBMIT`.
    RetSubmit(UsbIpRetSubmit),
    /// `USBIP_CMD_UNLINK`.
    CmdUnlink(UsbIpCmdUnlink),
    /// `USBIP_RET_UNLINK`.
    RetUnlink(UsbIpRetUnlink),
}

impl UsbIpUrb {
    /// Command carried by this URB.
    pub fn command(&self) -> UsbIpUrbCommand {
        match self {
            Self::CmdSubmit(_) => UsbIpUrbCommand::CmdSubmit,
            Self::RetSubmit(_) => UsbIpUrbCommand::RetSubmit,
            Self::CmdUnlink(_) => UsbIpUrbCommand::CmdUnlink,
            Self::RetUnlink(_) => UsbIpUrbCommand::RetUnlink,
        }
    }

    /// Request/completion sequence number.
    pub fn seqnum(&self) -> u32 {
        match self {
            Self::CmdSubmit(submit) => submit.seqnum,
            Self::RetSubmit(submit) => submit.seqnum,
            Self::CmdUnlink(unlink) => unlink.seqnum,
            Self::RetUnlink(unlink) => unlink.seqnum,
        }
    }

    /// Device id.
    pub fn devid(&self) -> u32 {
        match self {
            Self::CmdSubmit(submit) => submit.devid,
            Self::RetSubmit(submit) => submit.devid,
            Self::CmdUnlink(unlink) => unlink.devid,
            Self::RetUnlink(unlink) => unlink.devid,
        }
    }

    /// Encode this URB as one complete frame.
    pub fn encode(&self) -> Result<Vec<u8>, UsbIpUrbError> {
        match self {
            Self::CmdSubmit(submit) => submit.encode(),
            Self::RetSubmit(submit) => submit.encode(),
            Self::CmdUnlink(unlink) => Ok(unlink.encode()?.to_vec()),
            Self::RetUnlink(unlink) => Ok(unlink.encode()?.to_vec()),
        }
    }

    /// Decode one exact frame with the codec-wide bound.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbIpUrbError> {
        Self::decode_bounded(bytes, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Decode one exact frame with a caller-supplied transfer ceiling.
    pub fn decode_bounded(bytes: &[u8], max_transfer: usize) -> Result<Self, UsbIpUrbError> {
        decode_frame(bytes, max_transfer, None)
    }

    /// Compute the exact frame length from a 48-byte URB header.
    ///
    /// This lets a stream session read a bounded body before allocating it.
    /// It validates the command, direction, endpoint, transfer length, and ISO
    /// count, but does not read any payload.
    pub fn frame_len_from_header(header: &[u8]) -> Result<usize, UsbIpUrbError> {
        Self::frame_len_from_header_bounded(header, USBIP_URB_MAX_TRANSFER_BYTES)
    }

    /// Bounded variant of [`Self::frame_len_from_header`].
    pub fn frame_len_from_header_bounded(
        header: &[u8],
        max_transfer: usize,
    ) -> Result<usize, UsbIpUrbError> {
        Ok(plan_frame(header, max_transfer, None)?.frame_len)
    }
}

fn iso_count(declared: u32) -> usize {
    if declared == 0 || declared == USBIP_URB_NO_ISO_PACKETS {
        0
    } else {
        declared as usize
    }
}

fn validate_endpoint(ep: u32) -> Result<(), UsbIpUrbError> {
    if ep > USBIP_MAX_ENDPOINT {
        Err(UsbIpUrbError::InvalidEndpoint {
            ep,
            limit: USBIP_MAX_ENDPOINT,
        })
    } else {
        Ok(())
    }
}

fn validate_signed_length(field: &'static str, raw: u32) -> Result<usize, UsbIpUrbError> {
    let signed = raw as i32;
    if signed < 0 {
        return Err(UsbIpUrbError::InvalidField {
            field,
            reason: format!("negative value {signed}"),
        });
    }
    Ok(raw as usize)
}

fn validate_iso_descriptors(
    descriptors: &[UsbIpIsoPacketDescriptor],
    declared: u32,
    buffer_limit: usize,
) -> Result<usize, UsbIpUrbError> {
    let declared = iso_count(declared);
    if declared > USBIP_MAX_ISO_PACKETS {
        return Err(UsbIpUrbError::Oversized {
            field: "number_of_packets",
            limit: USBIP_MAX_ISO_PACKETS,
            requested: declared,
        });
    }
    if declared != descriptors.len() {
        return Err(UsbIpUrbError::CountMismatch {
            field: "iso_packets",
            declared,
            actual: descriptors.len(),
        });
    }
    for (index, packet) in descriptors.iter().enumerate() {
        let offset = packet.offset as usize;
        let length = packet.length as usize;
        let actual_length = packet.actual_length as usize;
        let end = offset
            .checked_add(length)
            .ok_or(UsbIpUrbError::IsoPacketOutOfBounds {
                index,
                offset,
                length,
                limit: buffer_limit,
            })?;
        if end > buffer_limit {
            return Err(UsbIpUrbError::IsoPacketOutOfBounds {
                index,
                offset,
                length,
                limit: buffer_limit,
            });
        }
        if actual_length > length {
            return Err(UsbIpUrbError::InvalidField {
                field: "iso_packets.actual_length",
                reason: format!(
                    "packet {index} actual_length {actual_length} exceeds length {length}"
                ),
            });
        }
    }
    Ok(declared)
}

fn validate_submit(submit: &UsbIpCmdSubmit, max_transfer: usize) -> Result<(), UsbIpUrbError> {
    validate_endpoint(submit.ep)?;
    let length = validate_signed_length("transfer_buffer_length", submit.transfer_buffer_length)?;
    if length > max_transfer {
        return Err(UsbIpUrbError::Oversized {
            field: "transfer_buffer_length",
            limit: max_transfer,
            requested: length,
        });
    }
    match submit.direction {
        UsbIpDirection::Out => {
            if submit.data.len() != length {
                return Err(UsbIpUrbError::InconsistentData {
                    field: "OUT payload",
                    expected: length,
                    actual: submit.data.len(),
                });
            }
        }
        UsbIpDirection::In => {
            if !submit.data.is_empty() {
                return Err(UsbIpUrbError::InconsistentData {
                    field: "IN payload",
                    expected: 0,
                    actual: submit.data.len(),
                });
            }
        }
    }
    validate_iso_descriptors(&submit.iso_packets, submit.number_of_packets, length)?;
    Ok(())
}

fn validate_completion(submit: &UsbIpRetSubmit, max_transfer: usize) -> Result<(), UsbIpUrbError> {
    validate_endpoint(submit.ep)?;
    let actual_length = validate_signed_length("actual_length", submit.actual_length)?;
    if actual_length > max_transfer {
        return Err(UsbIpUrbError::Oversized {
            field: "actual_length",
            limit: max_transfer,
            requested: actual_length,
        });
    }
    match submit.direction {
        UsbIpDirection::In => {
            if submit.data.len() != actual_length {
                return Err(UsbIpUrbError::InconsistentData {
                    field: "IN payload",
                    expected: actual_length,
                    actual: submit.data.len(),
                });
            }
        }
        UsbIpDirection::Out => {
            if !submit.data.is_empty() {
                return Err(UsbIpUrbError::InconsistentData {
                    field: "OUT payload",
                    expected: 0,
                    actual: submit.data.len(),
                });
            }
        }
    }
    validate_iso_descriptors(&submit.iso_packets, submit.number_of_packets, max_transfer)?;
    Ok(())
}

fn encode_basic(
    bytes: &mut Vec<u8>,
    command: UsbIpUrbCommand,
    seqnum: u32,
    devid: u32,
    direction: UsbIpDirection,
    ep: u32,
) {
    bytes.extend_from_slice(&command.raw().to_be_bytes());
    bytes.extend_from_slice(&seqnum.to_be_bytes());
    bytes.extend_from_slice(&devid.to_be_bytes());
    bytes.extend_from_slice(&direction.raw().to_be_bytes());
    bytes.extend_from_slice(&ep.to_be_bytes());
}

fn write_basic(
    bytes: &mut [u8; USBIP_URB_HEADER_BYTES],
    command: UsbIpUrbCommand,
    seqnum: u32,
    devid: u32,
    direction: UsbIpDirection,
    ep: u32,
) {
    bytes[0..4].copy_from_slice(&command.raw().to_be_bytes());
    bytes[4..8].copy_from_slice(&seqnum.to_be_bytes());
    bytes[8..12].copy_from_slice(&devid.to_be_bytes());
    bytes[12..16].copy_from_slice(&direction.raw().to_be_bytes());
    bytes[16..20].copy_from_slice(&ep.to_be_bytes());
}

struct FramePlan {
    command: UsbIpUrbCommand,
    seqnum: u32,
    devid: u32,
    direction: UsbIpDirection,
    ep: u32,
    transfer_flags: u32,
    transfer_buffer_length: u32,
    start_frame: i32,
    number_of_packets: u32,
    interval: i32,
    setup: [u8; 8],
    status: i32,
    actual_length: u32,
    error_count: i32,
    unlink_seqnum: u32,
    data_len: usize,
    iso_count: usize,
    frame_len: usize,
}

fn plan_frame(
    header: &[u8],
    max_transfer: usize,
    direction_override: Option<UsbIpDirection>,
) -> Result<FramePlan, UsbIpUrbError> {
    if header.len() < USBIP_URB_HEADER_BYTES {
        return Err(UsbIpUrbError::Truncated {
            needed: USBIP_URB_HEADER_BYTES,
            available: header.len(),
        });
    }
    let raw_command = read_u32(header, 0);
    let command = UsbIpUrbCommand::from_raw(raw_command).ok_or(UsbIpUrbError::UnknownCommand {
        command: raw_command,
    })?;
    let seqnum = read_u32(header, 4);
    let devid = read_u32(header, 8);
    let raw_direction = read_u32(header, 12);
    let header_direction =
        UsbIpDirection::from_raw(raw_direction).ok_or(UsbIpUrbError::UnsupportedDirection {
            direction: raw_direction,
        })?;
    let ep = read_u32(header, 16);
    validate_endpoint(ep)?;

    let mut plan = FramePlan {
        command,
        seqnum,
        devid,
        direction: header_direction,
        ep,
        transfer_flags: 0,
        transfer_buffer_length: 0,
        start_frame: 0,
        number_of_packets: 0,
        interval: 0,
        setup: [0; 8],
        status: 0,
        actual_length: 0,
        error_count: 0,
        unlink_seqnum: 0,
        data_len: 0,
        iso_count: 0,
        frame_len: USBIP_URB_HEADER_BYTES,
    };

    match command {
        UsbIpUrbCommand::CmdSubmit => {
            plan.transfer_flags = read_u32(header, 20);
            plan.transfer_buffer_length = read_u32(header, 24);
            let transfer_length =
                validate_signed_length("transfer_buffer_length", plan.transfer_buffer_length)?;
            if transfer_length > max_transfer {
                return Err(UsbIpUrbError::Oversized {
                    field: "transfer_buffer_length",
                    limit: max_transfer,
                    requested: transfer_length,
                });
            }
            plan.start_frame = read_i32(header, 28);
            plan.number_of_packets = read_u32(header, 32);
            plan.interval = read_i32(header, 36);
            plan.setup.copy_from_slice(&header[40..48]);
            plan.iso_count = iso_count(plan.number_of_packets);
            if plan.iso_count > USBIP_MAX_ISO_PACKETS {
                return Err(UsbIpUrbError::Oversized {
                    field: "number_of_packets",
                    limit: USBIP_MAX_ISO_PACKETS,
                    requested: plan.iso_count,
                });
            }
            plan.data_len = match plan.direction {
                UsbIpDirection::Out => transfer_length,
                UsbIpDirection::In => 0,
            };
            plan.frame_len = frame_len(plan.data_len, plan.iso_count)?;
        }
        UsbIpUrbCommand::RetSubmit => {
            plan.direction = direction_override.unwrap_or(header_direction);
            plan.status = read_i32(header, 20);
            plan.actual_length = read_u32(header, 24);
            let actual_length = validate_signed_length("actual_length", plan.actual_length)?;
            if actual_length > max_transfer {
                return Err(UsbIpUrbError::Oversized {
                    field: "actual_length",
                    limit: max_transfer,
                    requested: actual_length,
                });
            }
            plan.start_frame = read_i32(header, 28);
            plan.number_of_packets = read_u32(header, 32);
            plan.error_count = read_i32(header, 36);
            plan.iso_count = iso_count(plan.number_of_packets);
            if plan.iso_count > USBIP_MAX_ISO_PACKETS {
                return Err(UsbIpUrbError::Oversized {
                    field: "number_of_packets",
                    limit: USBIP_MAX_ISO_PACKETS,
                    requested: plan.iso_count,
                });
            }
            plan.data_len = match plan.direction {
                UsbIpDirection::In => actual_length,
                UsbIpDirection::Out => 0,
            };
            plan.frame_len = frame_len(plan.data_len, plan.iso_count)?;
        }
        UsbIpUrbCommand::CmdUnlink => {
            plan.unlink_seqnum = read_u32(header, 20);
            plan.frame_len = USBIP_URB_HEADER_BYTES;
        }
        UsbIpUrbCommand::RetUnlink => {
            plan.status = read_i32(header, 20);
            plan.frame_len = USBIP_URB_HEADER_BYTES;
        }
    }

    Ok(plan)
}

fn frame_len(data_len: usize, iso_count: usize) -> Result<usize, UsbIpUrbError> {
    let iso_bytes =
        iso_count
            .checked_mul(USBIP_ISO_PACKET_BYTES)
            .ok_or(UsbIpUrbError::Oversized {
                field: "iso_packets",
                limit: USBIP_MAX_ISO_PACKETS,
                requested: iso_count,
            })?;
    USBIP_URB_HEADER_BYTES
        .checked_add(data_len)
        .and_then(|length| length.checked_add(iso_bytes))
        .ok_or(UsbIpUrbError::Oversized {
            field: "frame",
            limit: USBIP_URB_MAX_FRAME_BYTES,
            requested: data_len,
        })
}

fn decode_frame(
    bytes: &[u8],
    max_transfer: usize,
    direction_override: Option<UsbIpDirection>,
) -> Result<UsbIpUrb, UsbIpUrbError> {
    let plan = plan_frame(bytes, max_transfer, direction_override)?;
    if bytes.len() < plan.frame_len {
        return Err(UsbIpUrbError::Truncated {
            needed: plan.frame_len,
            available: bytes.len(),
        });
    }
    if bytes.len() > plan.frame_len {
        return Err(UsbIpUrbError::TrailingBytes {
            expected: plan.frame_len,
            actual: bytes.len(),
        });
    }

    let data_start = USBIP_URB_HEADER_BYTES;
    let data_end = data_start + plan.data_len;
    let iso_start = data_end;
    let mut descriptors = Vec::with_capacity(plan.iso_count);
    for index in 0..plan.iso_count {
        let start = iso_start + index * USBIP_ISO_PACKET_BYTES;
        descriptors.push(UsbIpIsoPacketDescriptor::decode_at(bytes, start));
    }

    match plan.command {
        UsbIpUrbCommand::CmdSubmit => {
            let iso_bound = plan.transfer_buffer_length as usize;
            validate_iso_descriptors(&descriptors, plan.number_of_packets, iso_bound)?;
            Ok(UsbIpUrb::CmdSubmit(UsbIpCmdSubmit {
                seqnum: plan.seqnum,
                devid: plan.devid,
                direction: plan.direction,
                ep: plan.ep,
                transfer_flags: plan.transfer_flags,
                transfer_buffer_length: plan.transfer_buffer_length,
                start_frame: plan.start_frame,
                number_of_packets: plan.number_of_packets,
                interval: plan.interval,
                setup: plan.setup,
                data: bytes[data_start..data_end].to_vec(),
                iso_packets: descriptors,
            }))
        }
        UsbIpUrbCommand::RetSubmit => {
            validate_iso_descriptors(&descriptors, plan.number_of_packets, max_transfer)?;
            Ok(UsbIpUrb::RetSubmit(UsbIpRetSubmit {
                seqnum: plan.seqnum,
                devid: plan.devid,
                direction: plan.direction,
                ep: plan.ep,
                status: plan.status,
                actual_length: plan.actual_length,
                start_frame: plan.start_frame,
                number_of_packets: plan.number_of_packets,
                error_count: plan.error_count,
                data: bytes[data_start..data_end].to_vec(),
                iso_packets: descriptors,
            }))
        }
        UsbIpUrbCommand::CmdUnlink => Ok(UsbIpUrb::CmdUnlink(UsbIpCmdUnlink {
            seqnum: plan.seqnum,
            devid: plan.devid,
            unlink_seqnum: plan.unlink_seqnum,
        })),
        UsbIpUrbCommand::RetUnlink => Ok(UsbIpUrb::RetUnlink(UsbIpRetUnlink {
            seqnum: plan.seqnum,
            devid: plan.devid,
            status: plan.status,
        })),
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    read_u32(bytes, offset) as i32
}
