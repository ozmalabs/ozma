// SPDX-License-Identifier: AGPL-3.0-only
//! NBD export descriptors and protocol framing.
//!
//! This module defines the bounded oldstyle/newstyle handshake and
//! request/reply frames plus validated export descriptors. The socket server
//! itself lives in [`crate::nbd_server`]; this module stays transport-free so
//! the same framing can be tested with in-memory streams.
//!
//! The oldstyle handshake mirrors `softnode/virtual_media.py`'s server
//! (`NBDMAGIC` + `IHAVEOPT` magic + size + 128 zero/flag bytes). The newstyle
//! handshake and option constants are used by the fixed-newstyle server.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::usbip::validate_host;

/// Default NBD TCP port used by the Python virtual-media server.
pub const NBD_DEFAULT_PORT: u16 = 10_809;
/// NBD magic bytes.
pub const NBD_MAGIC: &[u8; 8] = b"NBDMAGIC";
/// Oldstyle "cliserv" magic: `0x0000420281861253`.
pub const NBD_CLISERV_MAGIC: u64 = 0x0000_4202_8186_1253;
/// Request magic.
pub const NBD_REQUEST_MAGIC: u32 = 0x2560_9513;
/// Reply magic.
pub const NBD_REPLY_MAGIC: u32 = 0x6744_6698;
/// Request header length in bytes.
pub const NBD_REQUEST_HEADER_LEN: usize = 28;
/// Reply header length in bytes.
pub const NBD_REPLY_HEADER_LEN: usize = 16;
/// Oldstyle handshake length: 8 + 8 + 8 + 128.
pub const NBD_HANDSHAKE_LEN: usize = 152;
/// Largest read/write request accepted by the framing model (32 MiB).
pub const NBD_MAX_REQUEST_BYTES: u32 = 32 * 1024 * 1024;

/// Newstyle server magic: `IHAVEOPT`.
pub const NBD_NEWSTYLE_MAGIC: u64 = 0x4948_4156_454f_5054;
/// Newstyle option-reply magic.
pub const NBD_REP_MAGIC: u64 = 0x0003_e889_0455_65a9;
/// Server handshake flag: fixed newstyle negotiation.
pub const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
/// Server handshake flag: the client may omit the 124 zero bytes.
pub const NBD_FLAG_NO_ZEROES: u16 = 1 << 1;
/// Client handshake flag: fixed newstyle negotiation.
pub const NBD_FLAG_C_FIXED_NEWSTYLE: u32 = 1 << 0;
/// Client handshake flag: the client accepts no-zeroes export responses.
pub const NBD_FLAG_C_NO_ZEROES: u32 = 1 << 1;

/// `NBD_OPT_EXPORT_NAME`.
pub const NBD_OPT_EXPORT_NAME: u32 = 1;
/// `NBD_OPT_ABORT`.
pub const NBD_OPT_ABORT: u32 = 2;
/// `NBD_OPT_LIST`.
pub const NBD_OPT_LIST: u32 = 3;
/// `NBD_OPT_STARTTLS` (deliberately unsupported).
pub const NBD_OPT_STARTTLS: u32 = 5;
/// `NBD_OPT_INFO`.
pub const NBD_OPT_INFO: u32 = 6;
/// `NBD_OPT_GO`.
pub const NBD_OPT_GO: u32 = 7;

/// `NBD_REP_ACK`.
pub const NBD_REP_ACK: u32 = 1;
/// `NBD_REP_SERVER`.
pub const NBD_REP_SERVER: u32 = 2;
/// `NBD_REP_INFO`.
pub const NBD_REP_INFO: u32 = 3;
/// `NBD_REP_ERR_UNSUP`.
pub const NBD_REP_ERR_UNSUP: u32 = 0x8000_0001;
/// `NBD_REP_ERR_POLICY`.
pub const NBD_REP_ERR_POLICY: u32 = 0x8000_0002;
/// `NBD_REP_ERR_INVALID`.
pub const NBD_REP_ERR_INVALID: u32 = 0x8000_0003;
/// `NBD_REP_ERR_UNKNOWN`.
pub const NBD_REP_ERR_UNKNOWN: u32 = 0x8000_0006;
/// `NBD_REP_ERR_TOO_BIG`.
pub const NBD_REP_ERR_TOO_BIG: u32 = 0x8000_0009;

/// `NBD_INFO_EXPORT`.
pub const NBD_INFO_EXPORT: u16 = 0;
/// `NBD_INFO_NAME`.
pub const NBD_INFO_NAME: u16 = 1;
/// `NBD_INFO_DESCRIPTION`.
pub const NBD_INFO_DESCRIPTION: u16 = 2;
/// `NBD_INFO_BLOCK_SIZE`.
pub const NBD_INFO_BLOCK_SIZE: u16 = 3;

/// `NBD_CMD_FLAG_FUA`.
pub const NBD_CMD_FLAG_FUA: u16 = 1 << 0;
/// `NBD_CMD_FLAG_NO_HOLE`.
pub const NBD_CMD_FLAG_NO_HOLE: u16 = 1 << 1;
/// `NBD_CMD_FLAG_DF`.
pub const NBD_CMD_FLAG_DF: u16 = 1 << 2;

/// `NBD_CMD_READ`.
pub const NBD_CMD_READ: u32 = 0;
/// `NBD_CMD_WRITE`.
pub const NBD_CMD_WRITE: u32 = 1;
/// `NBD_CMD_DISC`.
pub const NBD_CMD_DISC: u32 = 2;
/// `NBD_CMD_FLUSH`.
pub const NBD_CMD_FLUSH: u32 = 3;
/// `NBD_CMD_TRIM`.
pub const NBD_CMD_TRIM: u32 = 4;
/// `NBD_CMD_WRITE_ZEROES`.
pub const NBD_CMD_WRITE_ZEROES: u32 = 6;

/// `NBD_FLAG_HAS_FLAGS`.
pub const NBD_FLAG_HAS_FLAGS: u32 = 1 << 0;
/// `NBD_FLAG_READ_ONLY`.
pub const NBD_FLAG_READ_ONLY: u32 = 1 << 1;
/// `NBD_FLAG_SEND_FLUSH`.
pub const NBD_FLAG_SEND_FLUSH: u32 = 1 << 2;
/// `NBD_FLAG_SEND_TRIM`.
pub const NBD_FLAG_SEND_TRIM: u32 = 1 << 5;
/// `NBD_FLAG_SEND_WRITE_ZEROES`.
pub const NBD_FLAG_SEND_WRITE_ZEROES: u32 = 1 << 6;

/// Validated NBD export metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NbdExport {
    /// Export name (for example `ozma-media`).
    pub name: String,
    /// Export size in bytes.
    pub size_bytes: u64,
    /// Minimum/preferred block size in bytes.
    pub block_size: u32,
    /// Whether the export rejects writes.
    pub read_only: bool,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl NbdExport {
    /// A read-only export with a 4 KiB block size.
    pub fn read_only(name: impl Into<String>, size_bytes: u64) -> Self {
        Self {
            name: name.into(),
            size_bytes,
            block_size: 4096,
            read_only: true,
            description: None,
        }
    }

    /// A writable export with a 4 KiB block size.
    pub fn writable(name: impl Into<String>, size_bytes: u64) -> Self {
        Self {
            read_only: false,
            ..Self::read_only(name, size_bytes)
        }
    }

    /// Set the block size.
    pub fn with_block_size(mut self, block_size: u32) -> Self {
        self.block_size = block_size;
        self
    }

    /// Set a description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Validate the export name, size, and block size.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.name.is_empty() || self.name.len() > 255 {
            return Err(invalid_nbd("export name must be 1..=255 bytes"));
        }
        if self.name.chars().any(char::is_control) {
            return Err(invalid_nbd(
                "export name must not contain control characters",
            ));
        }
        if self.size_bytes == 0 {
            return Err(invalid_nbd("export size must be greater than zero"));
        }
        if self.block_size < 512
            || self.block_size > 1_048_576
            || !self.block_size.is_power_of_two()
        {
            return Err(invalid_nbd(
                "block size must be a power of two from 512 to 1048576 bytes",
            ));
        }
        if let Some(description) = &self.description {
            if description.len() > 512 || description.chars().any(char::is_control) {
                return Err(invalid_nbd(
                    "description must be at most 512 printable bytes",
                ));
            }
        }
        Ok(())
    }

    /// Handshake flags advertised to a client.
    pub fn handshake_flags(&self) -> u32 {
        let mut flags = NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_TRIM;
        if self.read_only {
            flags |= NBD_FLAG_READ_ONLY;
        }
        flags
    }

    /// Oldstyle handshake for this export.
    pub fn handshake(&self) -> NbdHandshake {
        NbdHandshake {
            export_size: self.size_bytes,
            flags: self.handshake_flags(),
        }
    }
}

/// An NBD export plus its listener endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NbdExportDescriptor {
    /// Export metadata.
    pub export: NbdExport,
    /// TCP host; may be empty when `unix_socket` is set.
    #[serde(default)]
    pub host: String,
    /// TCP port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Optional Unix socket path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<PathBuf>,
}

fn default_port() -> u16 {
    NBD_DEFAULT_PORT
}

impl NbdExportDescriptor {
    /// A loopback TCP descriptor.
    pub fn tcp(export: NbdExport) -> Self {
        Self {
            export,
            host: "127.0.0.1".to_owned(),
            port: NBD_DEFAULT_PORT,
            unix_socket: None,
        }
    }

    /// A Unix-socket descriptor.
    pub fn unix(export: NbdExport, path: impl Into<PathBuf>) -> Self {
        Self {
            export,
            host: String::new(),
            port: NBD_DEFAULT_PORT,
            unix_socket: Some(path.into()),
        }
    }

    /// Validate the export and endpoint.
    pub fn validate(&self) -> Result<(), UsbError> {
        self.export.validate()?;
        if self.port == 0 {
            return Err(invalid_nbd("port must be in 1..=65535"));
        }
        let has_socket = self
            .unix_socket
            .as_ref()
            .is_some_and(|path| !path.as_os_str().is_empty());
        if self.host.is_empty() && !has_socket {
            return Err(invalid_nbd(
                "descriptor needs a TCP host or a Unix socket path",
            ));
        }
        if !self.host.is_empty() {
            validate_host(&self.host)?;
        }
        if let Some(path) = &self.unix_socket {
            if path.as_os_str().is_empty()
                || !path.is_absolute()
                || path.as_os_str().to_string_lossy().contains('\0')
            {
                return Err(invalid_nbd(
                    "Unix socket path must be a non-empty absolute path",
                ));
            }
        }
        Ok(())
    }

    /// The export is writable only when both the descriptor and export allow
    /// writes; the framing model never writes by itself.
    pub fn is_read_only(&self) -> bool {
        self.export.read_only
    }
}

/// Oldstyle NBD handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbdHandshake {
    /// Export size in bytes.
    pub export_size: u64,
    /// Handshake flags.
    pub flags: u32,
}

impl NbdHandshake {
    /// Encode the 152-byte oldstyle handshake.
    pub fn encode(&self) -> [u8; NBD_HANDSHAKE_LEN] {
        let mut bytes = [0u8; NBD_HANDSHAKE_LEN];
        bytes[..8].copy_from_slice(NBD_MAGIC);
        bytes[8..16].copy_from_slice(&NBD_CLISERV_MAGIC.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.export_size.to_be_bytes());
        bytes[24..28].copy_from_slice(&self.flags.to_be_bytes());
        bytes
    }

    /// Decode and validate an oldstyle handshake.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbError> {
        if bytes.len() < NBD_HANDSHAKE_LEN {
            return Err(nbd_protocol_error("truncated handshake"));
        }
        if &bytes[..8] != NBD_MAGIC {
            return Err(nbd_protocol_error("bad NBD magic"));
        }
        let magic = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
        if magic != NBD_CLISERV_MAGIC {
            return Err(nbd_protocol_error("bad cliserv magic"));
        }
        let export_size = u64::from_be_bytes(bytes[16..24].try_into().unwrap_or([0; 8]));
        let flags = u32::from_be_bytes(bytes[24..28].try_into().unwrap_or([0; 4]));
        if export_size == 0 {
            return Err(nbd_protocol_error("zero export size"));
        }
        Ok(Self { export_size, flags })
    }
}

/// NBD command types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NbdCommand {
    /// Read data.
    Read,
    /// Write data.
    Write,
    /// Disconnect.
    Disconnect,
    /// Flush caches.
    Flush,
    /// Trim a range.
    Trim,
    /// Zero a range.
    WriteZeroes,
    /// Unknown command number.
    Unknown(u32),
}

impl NbdCommand {
    /// Raw command number.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Read => NBD_CMD_READ,
            Self::Write => NBD_CMD_WRITE,
            Self::Disconnect => NBD_CMD_DISC,
            Self::Flush => NBD_CMD_FLUSH,
            Self::Trim => NBD_CMD_TRIM,
            Self::WriteZeroes => NBD_CMD_WRITE_ZEROES,
            Self::Unknown(value) => value,
        }
    }
}

/// Parsed request header without write payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbdRequestHeader {
    /// Command flags (`NBD_CMD_FLAG_*`).
    pub flags: u16,
    /// Command.
    pub command: NbdCommand,
    /// Client handle echoed in the reply.
    pub handle: u64,
    /// Byte offset.
    pub offset: u64,
    /// Requested data length.
    pub length: u32,
}

impl NbdRequestHeader {
    /// Decode a 28-byte request header.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbError> {
        if bytes.len() < NBD_REQUEST_HEADER_LEN {
            return Err(nbd_protocol_error("truncated request header"));
        }
        let magic = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
        if magic != NBD_REQUEST_MAGIC {
            return Err(nbd_protocol_error("bad request magic"));
        }
        // Newstyle request headers carry 16-bit flags followed by a 16-bit
        // command. Oldstyle/legacy senders use a 32-bit command with flags=0.
        let flags = u16::from_be_bytes(bytes[4..6].try_into().unwrap_or([0; 2]));
        let raw_command = u16::from_be_bytes(bytes[6..8].try_into().unwrap_or([0; 2])) as u32;
        let command = match raw_command {
            NBD_CMD_READ => NbdCommand::Read,
            NBD_CMD_WRITE => NbdCommand::Write,
            NBD_CMD_DISC => NbdCommand::Disconnect,
            NBD_CMD_FLUSH => NbdCommand::Flush,
            NBD_CMD_TRIM => NbdCommand::Trim,
            NBD_CMD_WRITE_ZEROES => NbdCommand::WriteZeroes,
            other => NbdCommand::Unknown(other),
        };
        let handle = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
        let offset = u64::from_be_bytes(bytes[16..24].try_into().unwrap_or([0; 8]));
        let length = u32::from_be_bytes(bytes[24..28].try_into().unwrap_or([0; 4]));
        if matches!(
            command,
            NbdCommand::Read | NbdCommand::Write | NbdCommand::Trim | NbdCommand::WriteZeroes
        ) && length > NBD_MAX_REQUEST_BYTES
        {
            return Err(nbd_protocol_error(format!(
                "request length {length} exceeds {NBD_MAX_REQUEST_BYTES}"
            )));
        }
        Ok(Self {
            flags,
            command,
            handle,
            offset,
            length,
        })
    }

    /// Encode this header.
    pub fn encode(&self) -> [u8; NBD_REQUEST_HEADER_LEN] {
        let mut bytes = [0u8; NBD_REQUEST_HEADER_LEN];
        bytes[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        bytes[4..6].copy_from_slice(&self.flags.to_be_bytes());
        // Commands are 16-bit in newstyle framing; oldstyle values above
        // `u16::MAX` are not valid protocol commands anyway.
        bytes[6..8].copy_from_slice(&(self.command.as_u32() as u16).to_be_bytes());
        bytes[8..16].copy_from_slice(&self.handle.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.offset.to_be_bytes());
        bytes[24..28].copy_from_slice(&self.length.to_be_bytes());
        bytes
    }
}

/// A decoded NBD request, including write data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NbdRequest {
    /// Read `length` bytes.
    Read {
        /// Client handle.
        handle: u64,
        /// Byte offset.
        offset: u64,
        /// Length.
        length: u32,
    },
    /// Write data at `offset`.
    Write {
        /// Client handle.
        handle: u64,
        /// Byte offset.
        offset: u64,
        /// Payload; length must match the header.
        data: Vec<u8>,
    },
    /// Disconnect.
    Disconnect {
        /// Client handle.
        handle: u64,
    },
    /// Flush.
    Flush {
        /// Client handle.
        handle: u64,
    },
    /// Trim a range.
    Trim {
        /// Client handle.
        handle: u64,
        /// Byte offset.
        offset: u64,
        /// Length.
        length: u32,
    },
    /// Zero-fill a range.
    WriteZeroes {
        /// Client handle.
        handle: u64,
        /// Byte offset.
        offset: u64,
        /// Length.
        length: u32,
    },
}

impl NbdRequest {
    /// Decode a header plus the optional write payload.
    pub fn decode(header: &[u8], payload: &[u8]) -> Result<Self, UsbError> {
        let header = NbdRequestHeader::decode(header)?;
        match header.command {
            NbdCommand::Read => Ok(Self::Read {
                handle: header.handle,
                offset: header.offset,
                length: header.length,
            }),
            NbdCommand::Write => {
                if payload.len() != header.length as usize {
                    return Err(nbd_protocol_error(
                        "write payload length does not match the request header",
                    ));
                }
                Ok(Self::Write {
                    handle: header.handle,
                    offset: header.offset,
                    data: payload.to_vec(),
                })
            }
            NbdCommand::Disconnect => Ok(Self::Disconnect {
                handle: header.handle,
            }),
            NbdCommand::Flush => Ok(Self::Flush {
                handle: header.handle,
            }),
            NbdCommand::Trim => Ok(Self::Trim {
                handle: header.handle,
                offset: header.offset,
                length: header.length,
            }),
            NbdCommand::WriteZeroes => Ok(Self::WriteZeroes {
                handle: header.handle,
                offset: header.offset,
                length: header.length,
            }),
            NbdCommand::Unknown(value) => Err(nbd_protocol_error(format!(
                "unsupported NBD command {value}"
            ))),
        }
    }

    /// Encode the request (header, plus write payload when present).
    pub fn encode(&self) -> Vec<u8> {
        let header = match self {
            Self::Read {
                handle,
                offset,
                length,
            } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::Read,
                handle: *handle,
                offset: *offset,
                length: *length,
            },
            Self::Write {
                handle,
                offset,
                data,
            } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::Write,
                handle: *handle,
                offset: *offset,
                length: data.len() as u32,
            },
            Self::Disconnect { handle } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::Disconnect,
                handle: *handle,
                offset: 0,
                length: 0,
            },
            Self::Flush { handle } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::Flush,
                handle: *handle,
                offset: 0,
                length: 0,
            },
            Self::Trim {
                handle,
                offset,
                length,
            } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::Trim,
                handle: *handle,
                offset: *offset,
                length: *length,
            },
            Self::WriteZeroes {
                handle,
                offset,
                length,
            } => NbdRequestHeader {
                flags: 0,
                command: NbdCommand::WriteZeroes,
                handle: *handle,
                offset: *offset,
                length: *length,
            },
        };
        let mut bytes = header.encode().to_vec();
        if let Self::Write { data, .. } = self {
            bytes.extend_from_slice(data);
        }
        bytes
    }

    /// Client handle.
    pub fn handle(&self) -> u64 {
        match self {
            Self::Read { handle, .. }
            | Self::Write { handle, .. }
            | Self::Disconnect { handle }
            | Self::Flush { handle }
            | Self::Trim { handle, .. }
            | Self::WriteZeroes { handle, .. } => *handle,
        }
    }
}

/// NBD reply header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbdReply {
    /// Zero on success, otherwise a POSIX errno.
    pub error: u32,
    /// Client handle.
    pub handle: u64,
}

impl NbdReply {
    /// Successful reply.
    pub fn success(handle: u64) -> Self {
        Self { error: 0, handle }
    }

    /// Error reply.
    pub fn error(handle: u64, error: u32) -> Self {
        Self { error, handle }
    }

    /// Encode the 16-byte reply header.
    pub fn encode(&self) -> [u8; NBD_REPLY_HEADER_LEN] {
        let mut bytes = [0u8; NBD_REPLY_HEADER_LEN];
        bytes[0..4].copy_from_slice(&NBD_REPLY_MAGIC.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.error.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.handle.to_be_bytes());
        bytes
    }

    /// Decode a reply header.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbError> {
        if bytes.len() < NBD_REPLY_HEADER_LEN {
            return Err(nbd_protocol_error("truncated reply header"));
        }
        let magic = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
        if magic != NBD_REPLY_MAGIC {
            return Err(nbd_protocol_error("bad reply magic"));
        }
        Ok(Self {
            error: u32::from_be_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            handle: u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8])),
        })
    }

    /// Whether this reply reports success.
    pub fn is_success(&self) -> bool {
        self.error == 0
    }
}

/// Validate an NBD export descriptor without opening a socket.
pub fn validate_nbd_descriptor(descriptor: &NbdExportDescriptor) -> Result<(), UsbError> {
    descriptor.validate()
}

/// A descriptor that serves a read-only file-sized export. The path is
/// recorded for a future userspace server; this crate never opens it.
pub fn read_only_file_export(
    name: impl Into<String>,
    path: impl AsRef<Path>,
    size_bytes: u64,
) -> Result<NbdExportDescriptor, UsbError> {
    let descriptor = NbdExportDescriptor::tcp(
        NbdExport::read_only(name, size_bytes)
            .with_description(path.as_ref().display().to_string()),
    );
    descriptor.validate()?;
    Ok(descriptor)
}

fn invalid_nbd(reason: impl Into<String>) -> UsbError {
    UsbError::InvalidMedia {
        format: "nbd",
        path: PathBuf::from("<nbd>"),
        reason: reason.into(),
    }
}

fn nbd_protocol_error(reason: impl Into<String>) -> UsbError {
    UsbError::Protocol {
        protocol: "nbd",
        reason: reason.into(),
    }
}
