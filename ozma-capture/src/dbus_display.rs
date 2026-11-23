// SPDX-License-Identifier: AGPL-3.0-only
//! QEMU D-Bus display framebuffer capture.
//!
//! QEMU can expose its emulated display as a peer-to-peer D-Bus service
//! (`-display dbus,p2p=yes,addr=unix:path=...`). The host asks QEMU to attach a
//! socket over QMP (`getfd` + `add_client`), authenticates with SASL EXTERNAL,
//! registers a listener with SCM_RIGHTS, and then receives `Scanout`/`Update`
//! pushes at the guest's display refresh rate. This module implements that
//! path as a [`CaptureSource`]:
//!
//! ```text
//! QMP getfd/add_client -> D-Bus p2p display connection
//!   GetAll(Console)     -> width, height, label
//!   RegisterListener(h) -> listener socketpair (SCM_RIGHTS)
//! listener: Scanout/Update/Disable method calls -> owned BGRA frames
//! ```
//!
//! ## Why not `zbus`
//!
//! The QEMU display protocol is a raw peer-to-peer byte stream with unix-FD
//! passing and QEMU's own blocking method-call push model. `zbus` is built
//! around a bus connection and an object server; using it here would add a
//! large dependency tree for a small, fixed message set. This module therefore
//! ships a deliberately small D-Bus wire codec (little-endian, the types QEMU
//! uses) and a [`DBusTransport`] seam so the connection and parser can be
//! tested without QEMU. No `unsafe` code is involved: FD passing uses `nix`
//! SCM_RIGHTS over `tokio::net::UnixStream`.
//!
//! ## Framebuffer handling
//!
//! QEMU sends 32-bit pixman formats. `Scanout` carries the full framebuffer;
//! `Update` carries a damaged rectangle that is applied to the frame currently
//! held by the source; `Disable` drops the framebuffer and produces no frame.
//! Every emitted frame is an owned, tightly packed BGRA buffer, so QEMU cannot
//! mutate a frame that has already entered the pipeline. Strides are repacked
//! row by row; a non-32-bpp pixman format is rejected instead of being
//! mislabelled as BGRA.
//!
//! ## Lifecycle and testing
//!
//! [`DBusDisplayProvider::enumerate`] only checks the configured QMP socket
//! path. [`DBusDisplaySource`] connects lazily on the first
//! [`CaptureSource::next_frame`] call, so a missing or unresponsive QEMU does
//! not crash node startup. The default test suite injects scripted
//! [`DBusTransport`]s through [`DBusDisplaySource::with_transports`]; the real
//! QEMU test is `#[ignore]`d and gated by `OZMA_DBUS_TEST_QMP`.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nix::errno::Errno;
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use ozma_qmp::{QmpClient, QmpConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::provider::CaptureProvider;
use crate::v4l2::{monotonic_pts, next_sequence};

/// Default capture source id.
pub const DBUS_DEFAULT_ID: &str = "dbus0";

/// Default human-readable source name.
pub const DBUS_DEFAULT_NAME: &str = "QEMU D-Bus display";

/// Default console index (`Console_0`).
pub const DBUS_DEFAULT_CONSOLE_INDEX: u32 = 0;

/// Default advertised maximum width.
pub const DBUS_DEFAULT_WIDTH: u32 = 1920;

/// Default advertised maximum height.
pub const DBUS_DEFAULT_HEIGHT: u32 = 1080;

/// Default frame-rate ceiling.
pub const DBUS_DEFAULT_FPS: u32 = 30;

/// Default bound on connecting, authenticating, and waiting for a frame.
pub const DBUS_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest width or height accepted from a `Scanout` message.
///
/// This is a sanity bound against corrupt or hostile messages, not a product
/// limit. It matches the QMP PPM and Looking Glass parsers' bound.
pub const MAX_DBUS_DIMENSION: u32 = 16_384;

/// Largest D-Bus body this codec will read or allocate.
///
/// 512 MiB covers a tightly packed BGRA frame at common VM resolutions with
/// headroom for stride padding, while still refusing an absurd allocation from
/// a malformed length field.
pub const MAX_DBUS_BODY_BYTES: usize = 512 * 1024 * 1024;

/// Object path prefix for QEMU display consoles.
pub const DBUS_CONSOLE_PATH_PREFIX: &str = "/org/qemu/Display1/Console_";

/// `org.qemu.Display1.Console` interface.
pub const DBUS_CONSOLE_INTERFACE: &str = "org.qemu.Display1.Console";

/// `org.qemu.Display1.Listener` interface.
pub const DBUS_LISTENER_INTERFACE: &str = "org.qemu.Display1.Listener";

/// `org.freedesktop.DBus.Properties` interface.
pub const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

/// Name reported to QMP for the display socket descriptor.
pub const DBUS_FD_NAME: &str = "ozma-display";

/// QMP `add_client` protocol name for the D-Bus display.
pub const DBUS_ADD_CLIENT_PROTOCOL: &str = "@dbus-display";

const DBUS_METHOD_CALL: u8 = 1;
const DBUS_METHOD_RETURN: u8 = 2;
const DBUS_ERROR: u8 = 3;

const HEADER_FIELD_PATH: u8 = 1;
const HEADER_FIELD_INTERFACE: u8 = 2;
const HEADER_FIELD_MEMBER: u8 = 3;
const HEADER_FIELD_ERROR_NAME: u8 = 4;
const HEADER_FIELD_REPLY_SERIAL: u8 = 5;
const HEADER_FIELD_SIGNATURE: u8 = 8;
const HEADER_FIELD_UNIX_FDS: u8 = 9;

const MAX_AUTH_LINE_BYTES: usize = 4096;
const MAX_HEADER_FIELDS_BYTES: usize = 1024 * 1024;

/// Typed failures from the QEMU D-Bus display transport and codec.
#[derive(Debug, thiserror::Error)]
pub enum DBusError {
    /// The socket or stream failed.
    #[error("D-Bus I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The peer closed the connection.
    #[error("D-Bus peer closed the connection")]
    Closed,
    /// SASL EXTERNAL negotiation failed, or unix-FD passing was refused.
    #[error("D-Bus authentication failed: {0}")]
    Auth(String),
    /// A message was truncated or structurally invalid.
    #[error("malformed D-Bus message: {0}")]
    Protocol(String),
    /// A valid message used a feature this codec does not implement.
    #[error("unsupported D-Bus feature: {0}")]
    Unsupported(String),
    /// A frame carried invalid dimensions, rectangle, or stride.
    #[error("invalid display geometry: {0}")]
    Geometry(String),
    /// A frame/message exceeded the configured size bound.
    #[error("D-Bus frame is too large: {0}")]
    Oversized(String),
    /// No message arrived at a message boundary within the timeout.
    #[error("no D-Bus message arrived within {0:?}")]
    Idle(Duration),
}

impl DBusError {
    fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    fn geometry(message: impl Into<String>) -> Self {
        Self::Geometry(message.into())
    }

    fn oversized(message: impl Into<String>) -> Self {
        Self::Oversized(message.into())
    }

    /// Convert to a media-pipeline error.
    ///
    /// EOF is a clean end of stream; every other transport/protocol failure is
    /// a typed capture failure.
    pub fn to_media_error(&self, id: &str) -> MediaError {
        match self {
            Self::Closed => MediaError::EndOfStream,
            other => MediaError::capture(id, format!("QEMU D-Bus display: {other}")),
        }
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

fn read_le_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_le_i32(data: &[u8], offset: usize) -> Option<i32> {
    read_le_u32(data, offset).map(|value| value as i32)
}

/// A decoded D-Bus message header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DBusHeader {
    /// Message type (1 method call, 2 method return, 3 error, 4 signal).
    pub message_type: u8,
    /// Message flags.
    pub flags: u8,
    /// Message serial.
    pub serial: u32,
    /// Object path, for method calls.
    pub path: Option<String>,
    /// Interface, for method calls.
    pub interface: Option<String>,
    /// Member name, for method calls.
    pub member: Option<String>,
    /// Error name, for error replies.
    pub error_name: Option<String>,
    /// Serial of the call this message replies to.
    pub reply_serial: Option<u32>,
    /// Body signature.
    pub signature: Option<String>,
    /// Number of unix file descriptors attached to the message.
    pub unix_fds: u32,
}

impl DBusHeader {
    /// True for a method call.
    pub fn is_method_call(&self) -> bool {
        self.message_type == DBUS_METHOD_CALL
    }

    /// True for a method return.
    pub fn is_method_return(&self) -> bool {
        self.message_type == DBUS_METHOD_RETURN
    }
}

/// A decoded D-Bus message with its body bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DBusMessage {
    /// Parsed header fields.
    pub header: DBusHeader,
    /// Raw body bytes.
    pub body: Vec<u8>,
}

/// A D-Bus value subset sufficient for `GetAll` properties and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DBusValue {
    /// Byte (`y`).
    Byte(u8),
    /// Boolean (`b`).
    Bool(bool),
    /// Signed 32-bit integer (`i`/`n`).
    I32(i32),
    /// Unsigned 32-bit integer (`u`/`q`/`h`).
    U32(u32),
    /// Unsigned 64-bit integer (`x`/`t`).
    U64(u64),
    /// String, object path, or signature (`s`/`o`/`g`).
    String(String),
    /// Array of strings (`as`).
    StrArray(Vec<String>),
    /// Byte array (`ay`).
    Bytes(Vec<u8>),
    /// A value type this subset intentionally does not model.
    Other,
}

impl DBusValue {
    fn signature(&self) -> &'static str {
        match self {
            Self::Byte(_) => "y",
            Self::Bool(_) => "b",
            Self::I32(_) => "i",
            Self::U32(_) => "u",
            Self::U64(_) => "x",
            Self::String(_) => "s",
            Self::StrArray(_) => "as",
            Self::Bytes(_) => "ay",
            Self::Other => "v",
        }
    }

    fn as_u32(&self) -> Option<u32> {
        match self {
            Self::U32(value) => Some(*value),
            _ => None,
        }
    }
}

/// Parse one complete D-Bus message from `data`.
///
/// Only the little-endian wire format is accepted; QEMU and every mainstream
/// D-Bus implementation emit it. The parser never panics on arbitrary input
/// and never reads past the declared message.
pub fn parse_message(data: &[u8]) -> Result<DBusMessage, DBusError> {
    if data.len() < 16 {
        return Err(DBusError::protocol(format!(
            "header is {} bytes; at least 16 are required",
            data.len()
        )));
    }
    if data[0] != b'l' {
        return Err(DBusError::Unsupported(format!(
            "endianness byte {:?}; only little-endian messages are supported",
            data[0] as char
        )));
    }
    let message_type = data[1];
    if !(1..=4).contains(&message_type) {
        return Err(DBusError::protocol(format!(
            "message type {message_type} is not one of 1..=4"
        )));
    }
    if data[3] != 1 {
        return Err(DBusError::protocol(format!(
            "protocol version {} is not 1",
            data[3]
        )));
    }

    let body_len =
        read_le_u32(data, 4).ok_or_else(|| DBusError::protocol("truncated body length"))? as usize;
    let serial = read_le_u32(data, 8).ok_or_else(|| DBusError::protocol("truncated serial"))?;
    let fields_len = read_le_u32(data, 12)
        .ok_or_else(|| DBusError::protocol("truncated header-field length"))?
        as usize;

    let fields_end = 16usize
        .checked_add(fields_len)
        .ok_or_else(|| DBusError::protocol("header-field length overflows"))?;
    if fields_end > data.len() {
        return Err(DBusError::protocol(format!(
            "header fields end at {fields_end}; message is {} bytes",
            data.len()
        )));
    }
    let body_start = align_up(fields_end, 8);
    let body_end = body_start
        .checked_add(body_len)
        .ok_or_else(|| DBusError::protocol("body length overflows"))?;
    if body_end > data.len() {
        return Err(DBusError::protocol(format!(
            "body ends at {body_end}; message is {} bytes",
            data.len()
        )));
    }

    let mut cursor = Cursor::new(&data[16..fields_end]);
    let mut header = DBusHeader {
        message_type,
        flags: data[2],
        serial,
        path: None,
        interface: None,
        member: None,
        error_name: None,
        reply_serial: None,
        signature: None,
        unix_fds: 0,
    };

    while cursor.remaining() > 0 {
        cursor.align(8)?;
        if cursor.remaining() == 0 {
            break;
        }
        let code = cursor.u8()?;
        let signature = cursor.signature()?;
        let value = parse_variant_value(&mut cursor, &signature)?;
        match code {
            HEADER_FIELD_PATH => header.path = expect_string(value, "path")?,
            HEADER_FIELD_INTERFACE => header.interface = expect_string(value, "interface")?,
            HEADER_FIELD_MEMBER => header.member = expect_string(value, "member")?,
            HEADER_FIELD_ERROR_NAME => header.error_name = expect_string(value, "error name")?,
            HEADER_FIELD_REPLY_SERIAL => {
                header.reply_serial =
                    Some(value.as_u32().ok_or_else(|| {
                        DBusError::protocol("reply serial header field is not a u32")
                    })?)
            }
            HEADER_FIELD_SIGNATURE => header.signature = expect_string(value, "signature")?,
            HEADER_FIELD_UNIX_FDS => {
                header.unix_fds = value
                    .as_u32()
                    .ok_or_else(|| DBusError::protocol("unix-fds header field is not a u32"))?
            }
            _ => {}
        }
    }

    Ok(DBusMessage {
        header,
        body: data[body_start..body_end].to_vec(),
    })
}

fn expect_string(value: DBusValue, field: &str) -> Result<Option<String>, DBusError> {
    match value {
        DBusValue::String(value) => Ok(Some(value)),
        other => Err(DBusError::protocol(format!(
            "{field} header field has type {:?}, expected a string",
            other
        ))),
    }
}

/// Parse a `a{sv}` property dictionary body.
pub fn parse_properties(body: &[u8]) -> Result<BTreeMap<String, DBusValue>, DBusError> {
    let mut cursor = Cursor::new(body);
    let array_len = cursor.array_len()?;
    cursor.align(8)?;
    let end = cursor
        .position()
        .checked_add(array_len)
        .ok_or_else(|| DBusError::protocol("property array length overflows"))?;
    if end > body.len() {
        return Err(DBusError::protocol(format!(
            "property array ends at {end}; body is {} bytes",
            body.len()
        )));
    }

    let mut properties = BTreeMap::new();
    while cursor.position() < end {
        cursor.align(8)?;
        if cursor.position() >= end {
            break;
        }
        let key = cursor.string()?;
        let signature = cursor.signature()?;
        let value = parse_variant_value(&mut cursor, &signature)?;
        if cursor.position() > end {
            return Err(DBusError::protocol(format!(
                "property `{key}` overruns the property array"
            )));
        }
        properties.insert(key, value);
    }
    cursor.seek(end)?;
    Ok(properties)
}

/// Encode a method call with an already encoded body.
pub fn encode_method_call(
    serial: u32,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields = vec![
        Field::Path(path),
        Field::Interface(interface),
        Field::Member(member),
    ];
    if !signature.is_empty() {
        fields.push(Field::Signature(signature));
    }
    encode_message(DBUS_METHOD_CALL, serial, &fields, body)
}

/// Encode a successful method return with an already encoded body.
pub fn encode_method_return(
    serial: u32,
    reply_serial: u32,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields = vec![Field::ReplySerial(reply_serial)];
    if !signature.is_empty() {
        fields.push(Field::Signature(signature));
    }
    encode_message(DBUS_METHOD_RETURN, serial, &fields, body)
}

/// Encode `Console.RegisterListener("h")` with one attached unix descriptor.
pub fn encode_register_listener_call(
    serial: u32,
    console_path: &str,
    unix_fds: Option<u32>,
) -> Vec<u8> {
    let mut body = Encoder::new();
    body.u32(0);
    let mut fields = vec![
        Field::Path(console_path),
        Field::Interface(DBUS_CONSOLE_INTERFACE),
        Field::Member("RegisterListener"),
        Field::Signature("h"),
    ];
    if let Some(count) = unix_fds {
        fields.push(Field::UnixFds(count));
    }
    encode_message(DBUS_METHOD_CALL, serial, &fields, &body.finish())
}

/// Encode a D-Bus string body (signature `s`).
pub fn encode_string_body(value: &str) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.string(value);
    encoder.finish()
}

/// Encode an `a{sv}` property dictionary body.
pub fn encode_properties_body(properties: &[(&str, DBusValue)]) -> Result<Vec<u8>, DBusError> {
    let mut encoder = Encoder::new();
    let length_position = encoder.position();
    encoder.u32(0);
    encoder.align(8);
    let start = encoder.position();
    for (key, value) in properties {
        encoder.align(8);
        encoder.string(key);
        encoder.signature(value.signature());
        encode_variant_value(&mut encoder, value)?;
    }
    let end = encoder.position();
    let length = u32::try_from(end - start)
        .map_err(|_| DBusError::oversized("property dictionary exceeds 4 GiB"))?;
    encoder.patch_u32(length_position, length);
    Ok(encoder.finish())
}

enum Field<'a> {
    Path(&'a str),
    Interface(&'a str),
    Member(&'a str),
    #[allow(dead_code)]
    ErrorName(&'a str),
    ReplySerial(u32),
    Signature(&'a str),
    UnixFds(u32),
}

fn encode_message(message_type: u8, serial: u32, fields: &[Field<'_>], body: &[u8]) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.buf.extend_from_slice(&[b'l', message_type, 0, 1]);
    let body_length_position = encoder.position();
    encoder.u32(0);
    encoder.u32(serial);
    let fields_length_position = encoder.position();
    encoder.u32(0);
    let fields_start = encoder.position();

    for field in fields {
        encoder.align(8);
        match field {
            Field::Path(value) => {
                encoder.u8(HEADER_FIELD_PATH);
                encoder.signature("o");
                encoder.align(4);
                encoder.string(value);
            }
            Field::Interface(value) => {
                encoder.u8(HEADER_FIELD_INTERFACE);
                encoder.signature("s");
                encoder.align(4);
                encoder.string(value);
            }
            Field::Member(value) => {
                encoder.u8(HEADER_FIELD_MEMBER);
                encoder.signature("s");
                encoder.align(4);
                encoder.string(value);
            }
            Field::ErrorName(value) => {
                encoder.u8(HEADER_FIELD_ERROR_NAME);
                encoder.signature("s");
                encoder.align(4);
                encoder.string(value);
            }
            Field::ReplySerial(value) => {
                encoder.u8(HEADER_FIELD_REPLY_SERIAL);
                encoder.signature("u");
                encoder.align(4);
                encoder.u32(*value);
            }
            Field::Signature(value) => {
                encoder.u8(HEADER_FIELD_SIGNATURE);
                encoder.signature("g");
                encoder.signature(value);
            }
            Field::UnixFds(value) => {
                encoder.u8(HEADER_FIELD_UNIX_FDS);
                encoder.signature("u");
                encoder.align(4);
                encoder.u32(*value);
            }
        }
    }

    let fields_length = encoder.position() - fields_start;
    let fields_length = u32::try_from(fields_length).unwrap_or(u32::MAX);
    encoder.patch_u32(fields_length_position, fields_length);
    encoder.align(8);
    let body_length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    encoder.patch_u32(body_length_position, body_length);
    encoder.buf.extend_from_slice(body);
    encoder.align(8);
    encoder.finish()
}

fn encode_variant_value(encoder: &mut Encoder, value: &DBusValue) -> Result<(), DBusError> {
    match value {
        DBusValue::Byte(value) => encoder.u8(*value),
        DBusValue::Bool(value) => {
            encoder.align(4);
            encoder.u32(u32::from(*value));
        }
        DBusValue::I32(value) => {
            encoder.align(4);
            encoder.u32(*value as u32);
        }
        DBusValue::U32(value) => {
            encoder.align(4);
            encoder.u32(*value);
        }
        DBusValue::U64(value) => {
            encoder.align(8);
            encoder.u64(*value);
        }
        DBusValue::String(value) => {
            encoder.align(4);
            encoder.string(value);
        }
        DBusValue::StrArray(values) => {
            encoder.align(4);
            let length_position = encoder.position();
            encoder.u32(0);
            encoder.align(4);
            let start = encoder.position();
            for value in values {
                encoder.string(value);
            }
            let end = encoder.position();
            let length = u32::try_from(end - start)
                .map_err(|_| DBusError::oversized("string array exceeds 4 GiB"))?;
            encoder.patch_u32(length_position, length);
        }
        DBusValue::Bytes(values) => {
            encoder.align(4);
            let length = u32::try_from(values.len())
                .map_err(|_| DBusError::oversized("byte array exceeds 4 GiB"))?;
            encoder.u32(length);
            encoder.buf.extend_from_slice(values);
        }
        DBusValue::Other => {
            return Err(DBusError::Unsupported(
                "cannot encode an unmodelled D-Bus value".to_owned(),
            ));
        }
    }
    Ok(())
}

struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn position(&self) -> usize {
        self.buf.len()
    }

    fn align(&mut self, alignment: usize) {
        let aligned = align_up(self.buf.len(), alignment);
        self.buf.resize(aligned, 0);
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.align(4);
        let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
        self.u32(length);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
    }

    fn signature(&mut self, value: &str) {
        let length = u8::try_from(value.len()).unwrap_or(u8::MAX);
        self.buf.push(length);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
    }

    fn patch_u32(&mut self, offset: usize, value: u32) {
        if let Some(bytes) = self.buf.get_mut(offset..offset + 4) {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn seek(&mut self, position: usize) -> Result<(), DBusError> {
        if position > self.data.len() {
            return Err(DBusError::protocol(format!(
                "cursor seek to {position} exceeds {} bytes",
                self.data.len()
            )));
        }
        self.pos = position;
        Ok(())
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn align(&mut self, alignment: usize) -> Result<(), DBusError> {
        self.seek(align_up(self.pos, alignment))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DBusError> {
        let end = self
            .pos
            .checked_add(length)
            .ok_or_else(|| DBusError::protocol("length overflows"))?;
        if end > self.data.len() {
            return Err(DBusError::protocol(format!(
                "need {length} bytes at {}; only {} remain",
                self.pos,
                self.remaining()
            )));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DBusError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DBusError> {
        self.align(2)?;
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn i16(&mut self) -> Result<i16, DBusError> {
        self.u16().map(|value| value as i16)
    }

    fn u32(&mut self) -> Result<u32, DBusError> {
        self.align(4)?;
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32, DBusError> {
        self.u32().map(|value| value as i32)
    }

    fn u64(&mut self) -> Result<u64, DBusError> {
        self.align(8)?;
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn f64(&mut self) -> Result<f64, DBusError> {
        self.u64().map(f64::from_bits)
    }

    fn string(&mut self) -> Result<String, DBusError> {
        self.align(4)?;
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        if self.u8()? != 0 {
            return Err(DBusError::protocol("string is not nul-terminated"));
        }
        std::str::from_utf8(bytes)
            .map(|value| value.to_owned())
            .map_err(|error| DBusError::protocol(format!("string is not UTF-8: {error}")))
    }

    fn signature(&mut self) -> Result<String, DBusError> {
        let length = self.u8()? as usize;
        let bytes = self.take(length)?;
        if self.u8()? != 0 {
            return Err(DBusError::protocol("signature is not nul-terminated"));
        }
        std::str::from_utf8(bytes)
            .map(|value| value.to_owned())
            .map_err(|error| DBusError::protocol(format!("signature is not UTF-8: {error}")))
    }

    fn array_len(&mut self) -> Result<usize, DBusError> {
        self.align(4)?;
        self.u32().map(|length| length as usize)
    }
}

fn parse_variant_value(cursor: &mut Cursor<'_>, signature: &str) -> Result<DBusValue, DBusError> {
    match signature {
        "y" => Ok(DBusValue::Byte(cursor.u8()?)),
        "b" => {
            let value = cursor.u32()?;
            match value {
                0 => Ok(DBusValue::Bool(false)),
                1 => Ok(DBusValue::Bool(true)),
                other => Err(DBusError::protocol(format!(
                    "boolean has value {other}, expected 0 or 1"
                ))),
            }
        }
        "n" => Ok(DBusValue::I32(i32::from(cursor.i16()?))),
        "q" => Ok(DBusValue::U32(u32::from(cursor.u16()?))),
        "i" => Ok(DBusValue::I32(cursor.i32()?)),
        "u" | "h" => Ok(DBusValue::U32(cursor.u32()?)),
        "x" | "t" => Ok(DBusValue::U64(cursor.u64()?)),
        "d" => {
            cursor.f64()?;
            Ok(DBusValue::Other)
        }
        "s" | "o" => Ok(DBusValue::String(cursor.string()?)),
        "g" => Ok(DBusValue::String(cursor.signature()?)),
        "as" => {
            let length = cursor.array_len()?;
            cursor.align(4)?;
            let end = cursor
                .position()
                .checked_add(length)
                .ok_or_else(|| DBusError::protocol("string array length overflows"))?;
            let mut values = Vec::new();
            while cursor.position() < end {
                let value = cursor.string()?;
                if cursor.position() > end {
                    return Err(DBusError::protocol(
                        "string array element overruns the array",
                    ));
                }
                values.push(value);
            }
            cursor.seek(end)?;
            Ok(DBusValue::StrArray(values))
        }
        "ay" => {
            let length = cursor.array_len()?;
            let bytes = cursor.take(length)?;
            Ok(DBusValue::Bytes(bytes.to_vec()))
        }
        "v" => {
            let inner = cursor.signature()?;
            parse_variant_value(cursor, &inner)
        }
        _ => {
            skip_value(cursor, signature)?;
            Ok(DBusValue::Other)
        }
    }
}

fn skip_value(cursor: &mut Cursor<'_>, signature: &str) -> Result<(), DBusError> {
    match signature {
        "y" => {
            cursor.take(1)?;
        }
        "b" | "i" | "u" | "h" => {
            cursor.align(4)?;
            cursor.take(4)?;
        }
        "n" | "q" => {
            cursor.align(2)?;
            cursor.take(2)?;
        }
        "x" | "t" | "d" => {
            cursor.align(8)?;
            cursor.take(8)?;
        }
        "s" | "o" => {
            cursor.align(4)?;
            let length = cursor.u32()? as usize;
            cursor.take(length)?;
            cursor.take(1)?;
        }
        "g" => {
            let length = cursor.u8()? as usize;
            cursor.take(length)?;
            cursor.take(1)?;
        }
        "v" => {
            let inner = cursor.signature()?;
            skip_value(cursor, &inner)?;
        }
        other if other.starts_with('a') => {
            let length = cursor.array_len()?;
            cursor.align(alignment_of(&other[1..]))?;
            cursor.take(length)?;
        }
        other if other.starts_with('(') || other.starts_with('{') => {
            cursor.align(8)?;
            let inner = &other[1..other.len().saturating_sub(1)];
            for part in split_signature(inner) {
                skip_value(cursor, &part)?;
            }
        }
        other => {
            return Err(DBusError::Unsupported(format!(
                "D-Bus signature `{other}` cannot be skipped"
            )));
        }
    }
    Ok(())
}

fn alignment_of(signature: &str) -> usize {
    match signature.chars().next() {
        Some('y') | Some('g') | Some('v') => 1,
        Some('n') | Some('q') => 2,
        Some('s') | Some('o') | Some('a') | Some('i') | Some('u') | Some('h') | Some('b') => 4,
        Some('x') | Some('t') | Some('d') | Some('(') | Some('{') => 8,
        _ => 1,
    }
}

fn split_signature(signature: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut chars = signature.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            'a' => {
                let mut part = String::from("a");
                if let Some(next) = chars.next() {
                    part.push(next);
                    if next == '{' || next == '(' {
                        let close = if next == '{' { '}' } else { ')' };
                        for inner in chars.by_ref() {
                            part.push(inner);
                            if inner == close {
                                break;
                            }
                        }
                    }
                }
                parts.push(part);
            }
            '(' | '{' => {
                let close = if character == '{' { '}' } else { ')' };
                let mut part = String::new();
                part.push(character);
                for inner in chars.by_ref() {
                    part.push(inner);
                    if inner == close {
                        break;
                    }
                }
                parts.push(part);
            }
            other => parts.push(other.to_string()),
        }
    }
    parts
}

/// Owned, tightly packed BGRA framebuffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Framebuffer {
    /// Build a framebuffer from a `Scanout` body.
    pub fn from_scanout(body: &[u8], max_width: u32, max_height: u32) -> Result<Self, DBusError> {
        const HEADER: usize = 20;
        if body.len() < HEADER {
            return Err(DBusError::protocol(format!(
                "Scanout body is {} bytes; at least {HEADER} are required",
                body.len()
            )));
        }
        let width = read_le_u32(body, 0).expect("header length was checked");
        let height = read_le_u32(body, 4).expect("header length was checked");
        let stride = read_le_u32(body, 8).expect("header length was checked");
        let format = read_le_u32(body, 12).expect("header length was checked");
        let array_len = read_le_u32(body, 16).expect("header length was checked") as usize;

        validate_frame_geometry(width, height, max_width, max_height)?;
        validate_pixman_format(format)?;

        let row_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| DBusError::oversized("Scanout row length overflows"))?;
        let stride = stride as usize;
        if stride < row_bytes {
            return Err(DBusError::geometry(format!(
                "Scanout stride {stride} is smaller than a {width}-pixel BGRA row ({row_bytes})"
            )));
        }
        let needed = stride
            .checked_mul(height as usize - 1)
            .and_then(|offset| offset.checked_add(row_bytes))
            .ok_or_else(|| DBusError::oversized("Scanout payload length overflows"))?;
        if array_len < needed {
            return Err(DBusError::protocol(format!(
                "Scanout array is {array_len} bytes; {width}x{height} at stride {stride} needs {needed}"
            )));
        }
        if body.len() - HEADER < array_len {
            return Err(DBusError::protocol(format!(
                "Scanout body has {} payload bytes; array declares {array_len}",
                body.len() - HEADER
            )));
        }

        let data = &body[HEADER..HEADER + array_len];
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        if stride == row_bytes {
            pixels.extend_from_slice(&data[..row_bytes * height as usize]);
        } else {
            for row in 0..height as usize {
                let start = row * stride;
                pixels.extend_from_slice(&data[start..start + row_bytes]);
            }
        }

        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// Apply an `Update` damaged rectangle to this framebuffer in place.
    pub fn apply_update(&mut self, body: &[u8]) -> Result<(), DBusError> {
        const HEADER: usize = 28;
        if body.len() < HEADER {
            return Err(DBusError::protocol(format!(
                "Update body is {} bytes; at least {HEADER} are required",
                body.len()
            )));
        }
        let x = read_le_i32(body, 0).expect("header length was checked");
        let y = read_le_i32(body, 4).expect("header length was checked");
        let width = read_le_i32(body, 8).expect("header length was checked");
        let height = read_le_i32(body, 12).expect("header length was checked");
        let stride = read_le_u32(body, 16).expect("header length was checked") as usize;
        let format = read_le_u32(body, 20).expect("header length was checked");
        let array_len = read_le_u32(body, 24).expect("header length was checked") as usize;

        if x < 0 || y < 0 || width <= 0 || height <= 0 {
            return Err(DBusError::geometry(format!(
                "Update rectangle x={x} y={y} w={width} h={height} is not a positive rectangle"
            )));
        }
        let (x, y) = (x as u32, y as u32);
        let (width, height) = (width as u32, height as u32);
        let right = x
            .checked_add(width)
            .ok_or_else(|| DBusError::geometry("Update rectangle x extent overflows"))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| DBusError::geometry("Update rectangle y extent overflows"))?;
        if right > self.width || bottom > self.height {
            return Err(DBusError::geometry(format!(
                "Update rectangle {x},{y} {width}x{height} exceeds the {}x{} framebuffer",
                self.width, self.height
            )));
        }
        validate_pixman_format(format)?;

        let row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| DBusError::oversized("Update row length overflows"))?;
        if stride < row_bytes {
            return Err(DBusError::geometry(format!(
                "Update stride {stride} is smaller than a {width}-pixel BGRA row ({row_bytes})"
            )));
        }
        let needed = stride
            .checked_mul(height as usize - 1)
            .and_then(|offset| offset.checked_add(row_bytes))
            .ok_or_else(|| DBusError::oversized("Update payload length overflows"))?;
        if array_len < needed {
            return Err(DBusError::protocol(format!(
                "Update array is {array_len} bytes; {width}x{height} at stride {stride} needs {needed}"
            )));
        }
        if body.len() - HEADER < array_len {
            return Err(DBusError::protocol(format!(
                "Update body has {} payload bytes; array declares {array_len}",
                body.len() - HEADER
            )));
        }

        let data = &body[HEADER..HEADER + array_len];
        let destination_stride = self.width as usize * 4;
        for row in 0..height as usize {
            let source = row * stride;
            let destination = (y as usize + row) * destination_stride + x as usize * 4;
            self.pixels[destination..destination + row_bytes]
                .copy_from_slice(&data[source..source + row_bytes]);
        }
        Ok(())
    }

    /// Framebuffer width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Framebuffer height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Row stride of the owned buffer (`width * 4`).
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Tightly packed BGRA pixels.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

fn validate_frame_geometry(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> Result<(), DBusError> {
    if width == 0 || height == 0 {
        return Err(DBusError::geometry(format!(
            "frame is {width}x{height}; both dimensions must be non-zero"
        )));
    }
    if width > MAX_DBUS_DIMENSION || height > MAX_DBUS_DIMENSION {
        return Err(DBusError::oversized(format!(
            "frame {width}x{height} exceeds the {MAX_DBUS_DIMENSION}-pixel bound"
        )));
    }
    if width > max_width || height > max_height {
        return Err(DBusError::oversized(format!(
            "frame {width}x{height} exceeds the configured {max_width}x{max_height} limit"
        )));
    }
    Ok(())
}

fn validate_pixman_format(format: u32) -> Result<(), DBusError> {
    let bpp = (format >> 24) & 0xff;
    if bpp != 32 {
        return Err(DBusError::Unsupported(format!(
            "pixman format {format:#010x} has {bpp} bits per pixel; only 32-bit BGRA displays are supported"
        )));
    }
    Ok(())
}

/// A D-Bus byte-stream transport.
///
/// The production implementation is a `tokio` Unix socket; tests provide a
/// scripted in-memory implementation. Keeping this seam narrow is what makes
/// the protocol logic testable without QEMU.
#[async_trait]
pub trait DBusTransport: Send {
    /// Read exactly `buffer.len()` bytes.
    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), DBusError>;

    /// Write the whole message.
    async fn write_all(&mut self, data: &[u8]) -> Result<(), DBusError>;

    /// Write a message with unix file descriptors attached.
    ///
    /// The default implementation reports an unsupported transport, which is
    /// the honest answer for an in-memory fake that did not opt in.
    async fn write_with_fds(&mut self, data: &[u8], fds: &[RawFd]) -> Result<(), DBusError> {
        if fds.is_empty() {
            self.write_all(data).await
        } else {
            Err(DBusError::Unsupported(
                "this transport cannot attach unix file descriptors".to_owned(),
            ))
        }
    }

    /// Close the transport. Idempotent.
    async fn shutdown(&mut self) -> Result<(), DBusError> {
        Ok(())
    }
}

/// A peer-to-peer D-Bus connection over a Unix socket.
#[derive(Debug)]
pub struct UnixDBusTransport {
    stream: UnixStream,
}

impl UnixDBusTransport {
    /// Wrap an already-connected stream.
    pub fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Borrow the underlying stream.
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

#[async_trait]
impl DBusTransport for UnixDBusTransport {
    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), DBusError> {
        match self.stream.read_exact(buffer).await {
            Ok(_) => Ok(()),
            Err(error) => Err(map_io_error(error)),
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), DBusError> {
        self.stream.write_all(data).await.map_err(map_io_error)
    }

    async fn write_with_fds(&mut self, data: &[u8], fds: &[RawFd]) -> Result<(), DBusError> {
        let iov = [io::IoSlice::new(data)];
        let cmsgs = [ControlMessage::ScmRights(fds)];
        loop {
            match sendmsg::<()>(
                self.stream.as_raw_fd(),
                &iov,
                &cmsgs,
                MsgFlags::MSG_NOSIGNAL,
                None,
            ) {
                Ok(sent) if sent == data.len() => return Ok(()),
                Ok(sent) => {
                    return Err(DBusError::Io(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!("short sendmsg: wrote {sent} of {} bytes", data.len()),
                    )))
                }
                Err(Errno::EAGAIN) => {
                    self.stream.writable().await.map_err(map_io_error)?;
                }
                Err(error) => {
                    return Err(DBusError::Io(io::Error::from_raw_os_error(error as i32)))
                }
            }
        }
    }

    async fn shutdown(&mut self) -> Result<(), DBusError> {
        let _ = self.stream.shutdown().await;
        Ok(())
    }
}

fn map_io_error(error: io::Error) -> DBusError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => DBusError::Closed,
        _ => DBusError::Io(error),
    }
}

/// Read one D-Bus message from `transport`.
///
/// `idle_timeout` bounds only the wait for the first byte, which keeps a
/// timeout cancellation-safe: either a whole message has started (and is then
/// read to completion) or nothing was consumed.
pub async fn read_message(
    transport: &mut dyn DBusTransport,
    max_body: usize,
    idle_timeout: Option<Duration>,
) -> Result<DBusMessage, DBusError> {
    let mut first = [0u8; 1];
    if let Some(timeout) = idle_timeout {
        match tokio::time::timeout(timeout, transport.read_exact(&mut first)).await {
            Ok(result) => result?,
            Err(_) => return Err(DBusError::Idle(timeout)),
        }
    } else {
        transport.read_exact(&mut first).await?;
    }
    if first[0] != b'l' {
        return Err(DBusError::Unsupported(format!(
            "endianness byte {:?}; only little-endian messages are supported",
            first[0] as char
        )));
    }

    let mut fixed = [0u8; 12];
    fixed[0] = first[0];
    transport.read_exact(&mut fixed[1..]).await?;
    let body_len = u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]) as usize;
    if body_len > max_body {
        return Err(DBusError::oversized(format!(
            "declared body is {body_len} bytes; the configured limit is {max_body}"
        )));
    }

    let mut fields_length = [0u8; 4];
    transport.read_exact(&mut fields_length).await?;
    let fields_len = u32::from_le_bytes(fields_length) as usize;
    if fields_len > MAX_HEADER_FIELDS_BYTES {
        return Err(DBusError::oversized(format!(
            "header fields are {fields_len} bytes"
        )));
    }
    let padded_fields = align_up(fields_len, 8);

    let mut raw = Vec::with_capacity(align_up(16 + padded_fields + body_len, 8));
    raw.extend_from_slice(&fixed);
    raw.extend_from_slice(&fields_length);
    let mut fields = vec![0u8; padded_fields];
    transport.read_exact(&mut fields).await?;
    raw.extend_from_slice(&fields);
    // The header (fixed fields + field array) is already 8-aligned because
    // `padded_fields` is a multiple of 8 and the prefix is 16 bytes.
    let body_padding = align_up(body_len, 8) - body_len;
    raw.reserve(body_len + body_padding);
    let start = raw.len();
    raw.resize(start + body_len, 0);
    transport.read_exact(&mut raw[start..]).await?;
    if body_padding > 0 {
        let mut padding = vec![0u8; body_padding];
        transport.read_exact(&mut padding).await?;
    }

    parse_message(&raw)
}

async fn read_auth_line(transport: &mut dyn DBusTransport) -> Result<String, DBusError> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        transport.read_exact(&mut byte).await?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
        if line.len() > MAX_AUTH_LINE_BYTES {
            return Err(DBusError::Auth(format!(
                "authentication response exceeded {MAX_AUTH_LINE_BYTES} bytes"
            )));
        }
    }
    let content = &line[..line.len() - 2];
    std::str::from_utf8(content)
        .map(|line| line.to_owned())
        .map_err(|error| DBusError::Auth(format!("authentication response is not UTF-8: {error}")))
}

async fn authenticate(transport: &mut dyn DBusTransport) -> Result<(), DBusError> {
    let uid = nix::unistd::getuid().as_raw().to_string();
    let hex: String = uid.bytes().map(|byte| format!("{byte:02x}")).collect();
    transport
        .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
        .await?;
    let response = read_auth_line(transport).await?;
    if !response.starts_with("OK") {
        return Err(DBusError::Auth(format!(
            "SASL EXTERNAL was not accepted: {response}"
        )));
    }

    transport.write_all(b"NEGOTIATE_UNIX_FD\r\n").await?;
    let response = read_auth_line(transport).await?;
    if !response.starts_with("AGREE_UNIX_FD") {
        return Err(DBusError::Auth(format!(
            "peer refused unix file-descriptor passing: {response}"
        )));
    }

    transport.write_all(b"BEGIN\r\n").await?;
    Ok(())
}

/// Configuration for [`DBusDisplayProvider`] and [`DBusDisplaySource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DBusDisplayConfig {
    /// QEMU control QMP Unix socket.
    pub qmp_socket: PathBuf,
    /// QEMU console index (`0` selects `Console_0`).
    pub console_index: u32,
    /// Stable capture source id.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Largest width this source will accept from a `Scanout`.
    pub width: u32,
    /// Largest height this source will accept from a `Scanout`.
    pub height: u32,
    /// Frame-rate ceiling advertised in the capability.
    pub fps: u32,
    /// Bound on connecting, authenticating, and waiting for a frame.
    pub connect_timeout: Duration,
}

impl Default for DBusDisplayConfig {
    fn default() -> Self {
        Self {
            qmp_socket: PathBuf::from("/run/ozma/qmp0.sock"),
            console_index: DBUS_DEFAULT_CONSOLE_INDEX,
            id: DBUS_DEFAULT_ID.to_owned(),
            name: DBUS_DEFAULT_NAME.to_owned(),
            width: DBUS_DEFAULT_WIDTH,
            height: DBUS_DEFAULT_HEIGHT,
            fps: DBUS_DEFAULT_FPS,
            connect_timeout: DBUS_DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

impl DBusDisplayConfig {
    /// Configuration for a QMP socket with the default identity and limits.
    pub fn new(qmp_socket: impl Into<PathBuf>) -> Self {
        Self {
            qmp_socket: qmp_socket.into(),
            ..Self::default()
        }
    }

    /// Select a console index (`0` is `Console_0`).
    pub fn with_console_index(mut self, console_index: u32) -> Self {
        self.console_index = console_index;
        self
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Override the accepted frame dimensions.
    pub fn with_limits(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the frame-rate ceiling.
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    /// Override the connect/frame wait bound.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Object path of the selected console.
    pub fn console_path(&self) -> String {
        format!("{DBUS_CONSOLE_PATH_PREFIX}{}", self.console_index)
    }

    /// Largest body accepted from the display listener.
    fn max_message_body(&self) -> usize {
        let pixels = u64::from(self.width)
            .saturating_mul(u64::from(self.height))
            .saturating_mul(4)
            .saturating_add(4 * 1024 * 1024);
        usize::try_from(pixels)
            .unwrap_or(MAX_DBUS_BODY_BYTES)
            .min(MAX_DBUS_BODY_BYTES)
    }

    fn validate(&self) -> Result<(), ApiError> {
        if self.qmp_socket.as_os_str().is_empty() {
            return Err(ApiError::invalid(
                "The D-Bus display source requires a non-empty QMP socket path.",
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The D-Bus display source requires a non-empty id.",
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(ApiError::invalid(
                "D-Bus display limits must have non-zero width and height.",
            ));
        }
        if self.width > MAX_DBUS_DIMENSION || self.height > MAX_DBUS_DIMENSION {
            return Err(ApiError::invalid(format!(
                "D-Bus display limits {}x{} exceed the {MAX_DBUS_DIMENSION}-pixel bound.",
                self.width, self.height
            )));
        }
        if self.fps == 0 {
            return Err(ApiError::invalid("D-Bus display fps must be at least 1."));
        }
        if self.connect_timeout.is_zero() {
            return Err(ApiError::invalid(
                "D-Bus display connect_timeout must be non-zero.",
            ));
        }
        Ok(())
    }
}

/// A capture provider for QEMU's D-Bus display interface.
#[derive(Clone, Debug)]
pub struct DBusDisplayProvider {
    config: DBusDisplayConfig,
}

impl DBusDisplayProvider {
    /// Create a provider for `config`.
    pub fn new(config: DBusDisplayConfig) -> Self {
        Self { config }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &DBusDisplayConfig {
        &self.config
    }

    /// The capability advertised when the QMP socket path exists.
    ///
    /// This never connects to QEMU. A missing socket path yields `None`
    /// rather than an error, matching the QMP display provider.
    pub fn capability(&self) -> Option<CaptureCapability> {
        if self.config.validate().is_err() || !self.config.qmp_socket.exists() {
            return None;
        }
        Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: CaptureKind::Dbus,
            formats: vec![PixelFormat::Bgra.as_str().to_owned()],
            max_width: self.config.width,
            max_height: self.config.height,
            max_fps: self.config.fps.max(1),
            compressed: false,
            hdr: false,
            has_audio: false,
        })
    }
}

impl CaptureProvider for DBusDisplayProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::Dbus
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        self.config.validate()?;
        Ok(self.capability().into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::Dbus || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!(
                    "D-Bus display source `{}` is not available from this provider.",
                    capability.id
                ),
            )
            .with_details(capability.id.clone()));
        }
        self.config.validate()?;
        if !self.config.qmp_socket.exists() {
            return Err(missing_socket_error(&self.config.qmp_socket));
        }
        Ok(Box::new(DBusDisplaySource::new(self.config.clone())?))
    }
}

/// A [`CaptureSource`] fed by QEMU's D-Bus display listener.
pub struct DBusDisplaySource {
    config: DBusDisplayConfig,
    display: Option<Box<dyn DBusTransport>>,
    listener: Option<Box<dyn DBusTransport>>,
    listener_peer: Option<StdUnixStream>,
    framebuffer: Option<Framebuffer>,
    format: FrameFormat,
    origin: Instant,
    serial: u32,
    initialized: bool,
    stopped: bool,
    eof: bool,
    last_sequence: Option<u64>,
    last_pts: Option<u64>,
}

impl fmt::Debug for DBusDisplaySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DBusDisplaySource")
            .field("config", &self.config)
            .field("has_display", &self.display.is_some())
            .field("has_listener", &self.listener.is_some())
            .field("initialized", &self.initialized)
            .field("stopped", &self.stopped)
            .field("eof", &self.eof)
            .field(
                "framebuffer",
                &self.framebuffer.as_ref().map(|fb| (fb.width, fb.height)),
            )
            .finish()
    }
}

impl DBusDisplaySource {
    /// Build a source without connecting.
    ///
    /// The QMP socket, auth, and listener are only opened on the first
    /// [`CaptureSource::next_frame`] (or an explicit [`Self::connect`]).
    pub fn new(config: DBusDisplayConfig) -> Result<Self, ApiError> {
        config.validate()?;
        Ok(Self {
            config,
            display: None,
            listener: None,
            listener_peer: None,
            framebuffer: None,
            format: FrameFormat::new(PixelFormat::Bgra, 0, 0),
            origin: Instant::now(),
            serial: 0,
            initialized: false,
            stopped: false,
            eof: false,
            last_sequence: None,
            last_pts: None,
        })
    }

    /// Build a source over injected transports, for tests and embedding.
    ///
    /// The transports are still authenticated and driven through the normal
    /// handshake when [`Self::connect`] runs, so this exercises the real
    /// protocol logic without QMP or QEMU.
    pub fn with_transports(
        config: DBusDisplayConfig,
        display: Box<dyn DBusTransport>,
        listener: Box<dyn DBusTransport>,
    ) -> Result<Self, ApiError> {
        let mut source = Self::new(config)?;
        source.display = Some(display);
        source.listener = Some(listener);
        Ok(source)
    }

    /// QMP socket this source uses.
    pub fn socket_path(&self) -> &Path {
        &self.config.qmp_socket
    }

    /// Object path of the selected console.
    pub fn console_path(&self) -> String {
        self.config.console_path()
    }

    /// Frame interval derived from the configured fps.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_micros((1_000_000 / u64::from(self.config.fps.max(1))).max(1))
    }

    /// True after [`CaptureSource::stop`].
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// True after the listener reached EOF or a terminal protocol failure.
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// True once the handshake and listener registration completed.
    pub fn is_connected(&self) -> bool {
        self.initialized
    }

    /// Establish the QMP handoff, D-Bus auth, and listener registration.
    ///
    /// Idempotent. Bounded by [`DBusDisplayConfig::connect_timeout`].
    pub async fn connect(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Err(MediaError::EndOfStream);
        }
        if self.initialized {
            return Ok(());
        }
        if self.display.is_none() {
            self.establish_real().await?;
        }

        let id = self.config.id.clone();
        let timeout = self.config.connect_timeout;
        match tokio::time::timeout(timeout, self.initialize()).await {
            Ok(result) => result,
            Err(_) => Err(MediaError::capture(
                &id,
                format!(
                    "timed out after {timeout:?} establishing the QEMU D-Bus display connection"
                ),
            )),
        }
    }

    async fn establish_real(&mut self) -> Result<(), MediaError> {
        let id = self.config.id.clone();
        if !self.config.qmp_socket.exists() {
            return Err(MediaError::from(missing_socket_error(
                &self.config.qmp_socket,
            )));
        }

        let qmp_config = QmpConfig::new(self.config.qmp_socket.clone())
            .with_connect_timeout(self.config.connect_timeout);
        let client = QmpClient::connect(qmp_config)
            .await
            .map_err(|error| MediaError::capture(&id, format!("QMP connect failed: {error}")))?;

        let (display_ours, display_theirs) = StdUnixStream::pair().map_err(|error| {
            MediaError::capture(&id, format!("cannot create display socketpair: {error}"))
        })?;
        // A plain std socketpair is blocking, like the Python client's. Only
        // our end is switched to non-blocking for tokio; QEMU's duplicate keeps
        // the blocking file-status flags it expects.
        if let Err(error) = client.getfd(DBUS_FD_NAME, display_theirs.as_raw_fd()).await {
            client.disconnect().await;
            return Err(MediaError::capture(
                &id,
                format!("QMP getfd for `{DBUS_FD_NAME}` failed: {error}"),
            ));
        }
        drop(display_theirs);
        if let Err(error) = client
            .add_client(DBUS_ADD_CLIENT_PROTOCOL, DBUS_FD_NAME)
            .await
        {
            client.disconnect().await;
            return Err(MediaError::capture(
                &id,
                format!(
                    "QMP add_client `{DBUS_ADD_CLIENT_PROTOCOL}` failed: {error} (is QEMU running with -display dbus,p2p=yes?)"
                ),
            ));
        }
        client.disconnect().await;

        let (listener_ours, listener_theirs) = StdUnixStream::pair().map_err(|error| {
            MediaError::capture(&id, format!("cannot create listener socketpair: {error}"))
        })?;
        let display_ours = into_tokio_stream(display_ours).map_err(|error| {
            MediaError::capture(&id, format!("cannot adopt display socket: {error}"))
        })?;
        let listener_ours = into_tokio_stream(listener_ours).map_err(|error| {
            MediaError::capture(&id, format!("cannot adopt listener socket: {error}"))
        })?;

        self.display = Some(Box::new(UnixDBusTransport::new(display_ours)));
        self.listener = Some(Box::new(UnixDBusTransport::new(listener_ours)));
        self.listener_peer = Some(listener_theirs);
        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), MediaError> {
        let mut display = self.display.take().ok_or_else(|| {
            MediaError::capture(&self.config.id, "display transport was not created")
        })?;
        let mut listener = self.listener.take().ok_or_else(|| {
            MediaError::capture(&self.config.id, "listener transport was not created")
        })?;

        let result = self
            .initialize_with(display.as_mut(), listener.as_mut())
            .await;

        self.display = Some(display);
        self.listener = Some(listener);
        result?;
        self.initialized = true;
        Ok(())
    }

    async fn initialize_with(
        &mut self,
        display: &mut dyn DBusTransport,
        listener: &mut dyn DBusTransport,
    ) -> Result<(), MediaError> {
        let id = self.config.id.clone();
        authenticate(display)
            .await
            .map_err(|error| error.to_media_error(&id))?;
        let (width, height) = self
            .read_console_properties(display)
            .await
            .map_err(|error| error.to_media_error(&id))?;
        self.format = FrameFormat::new(PixelFormat::Bgra, width, height);

        let serial = self.next_serial();
        let fd = self.listener_peer.as_ref().map(|stream| stream.as_raw_fd());
        let message =
            encode_register_listener_call(serial, &self.config.console_path(), fd.map(|_| 1));
        let fds: Vec<RawFd> = fd.into_iter().collect();
        display
            .write_with_fds(&message, &fds)
            .await
            .map_err(|error| error.to_media_error(&id))?;
        let reply = read_message(
            display,
            self.config.max_message_body(),
            Some(self.config.connect_timeout),
        )
        .await
        .map_err(|error| error.to_media_error(&id))?;
        if reply.header.message_type == DBUS_ERROR {
            return Err(MediaError::capture(
                &id,
                format!(
                    "QEMU rejected RegisterListener: {}",
                    reply
                        .header
                        .error_name
                        .or(reply.header.member)
                        .unwrap_or_else(|| "unknown D-Bus error".to_owned())
                ),
            ));
        }

        authenticate(listener)
            .await
            .map_err(|error| error.to_media_error(&id))?;
        self.reply_to_listener_getall(listener).await
    }

    async fn read_console_properties(
        &mut self,
        display: &mut dyn DBusTransport,
    ) -> Result<(u32, u32), DBusError> {
        let serial = self.next_serial();
        let body = encode_string_body(DBUS_CONSOLE_INTERFACE);
        let message = encode_method_call(
            serial,
            &self.config.console_path(),
            DBUS_PROPERTIES_INTERFACE,
            "GetAll",
            "s",
            &body,
        );
        display.write_all(&message).await?;

        let reply = read_message(
            display,
            self.config.max_message_body(),
            Some(self.config.connect_timeout),
        )
        .await?;
        if reply.header.message_type == DBUS_ERROR {
            return Err(DBusError::Protocol(format!(
                "GetAll({DBUS_CONSOLE_INTERFACE}) returned an error: {}",
                reply
                    .header
                    .error_name
                    .unwrap_or_else(|| "unknown error".to_owned())
            )));
        }
        if !reply.header.is_method_return() {
            return Err(DBusError::protocol(format!(
                "GetAll expected a method return, got message type {}",
                reply.header.message_type
            )));
        }
        if let Some(reply_serial) = reply.header.reply_serial {
            if reply_serial != serial {
                return Err(DBusError::protocol(format!(
                    "GetAll reply serial {reply_serial} does not match request {serial}"
                )));
            }
        }

        let properties = parse_properties(&reply.body)?;
        let width = properties
            .get("Width")
            .and_then(DBusValue::as_u32)
            .ok_or_else(|| DBusError::geometry("console GetAll has no uint32 `Width`"))?;
        let height = properties
            .get("Height")
            .and_then(DBusValue::as_u32)
            .ok_or_else(|| DBusError::geometry("console GetAll has no uint32 `Height`"))?;
        // QEMU can report 0x0 before the guest sets a video mode; the first
        // Scanout carries the real geometry and is validated there. Only
        // reject a console that is already larger than the configured bound.
        if width > self.config.width || height > self.config.height {
            return Err(DBusError::oversized(format!(
                "console is {width}x{height}, above the configured {}x{} limit",
                self.config.width, self.config.height
            )));
        }
        if width > MAX_DBUS_DIMENSION || height > MAX_DBUS_DIMENSION {
            return Err(DBusError::oversized(format!(
                "console is {width}x{height}, above the {MAX_DBUS_DIMENSION}-pixel bound"
            )));
        }

        if let Some(DBusValue::String(label)) = properties.get("Label") {
            tracing::debug!(id = %self.config.id, label, width, height, "QEMU D-Bus console");
        }
        Ok((width, height))
    }

    async fn reply_to_listener_getall(
        &mut self,
        listener: &mut dyn DBusTransport,
    ) -> Result<(), MediaError> {
        let id = self.config.id.clone();
        let message = read_message(
            listener,
            self.config.max_message_body(),
            Some(self.config.connect_timeout),
        )
        .await
        .map_err(|error| error.to_media_error(&id))?;
        if !message.header.is_method_call() {
            return Err(MediaError::capture(
                &id,
                format!(
                    "listener expected GetAll, got message type {}",
                    message.header.message_type
                ),
            ));
        }
        let reply = self
            .build_method_reply(&message)
            .map_err(|error| error.to_media_error(&id))?;
        listener
            .write_all(&reply)
            .await
            .map_err(|error| error.to_media_error(&id))
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.serial = 1;
        }
        self.serial
    }

    fn build_method_reply(&mut self, message: &DBusMessage) -> Result<Vec<u8>, DBusError> {
        let serial = self.next_serial();
        let reply_serial = message.header.serial;
        if message.header.member.as_deref() == Some("GetAll")
            && message.header.interface.as_deref() == Some(DBUS_PROPERTIES_INTERFACE)
        {
            let body = encode_properties_body(&[(
                "Interfaces",
                DBusValue::StrArray(vec![DBUS_LISTENER_INTERFACE.to_owned()]),
            )])?;
            Ok(encode_method_return(serial, reply_serial, "a{sv}", &body))
        } else {
            Ok(encode_method_return(serial, reply_serial, "", &[]))
        }
    }

    fn make_frame(&mut self) -> Result<Frame, MediaError> {
        let framebuffer = self.framebuffer.as_ref().ok_or_else(|| {
            MediaError::capture(&self.config.id, "no framebuffer has been received")
        })?;
        let format = FrameFormat::new(PixelFormat::Bgra, framebuffer.width, framebuffer.height);
        let pts = monotonic_pts(elapsed_micros(self.origin), self.last_pts);
        let sequence = next_sequence(self.last_sequence);
        let frame = Frame::with_data(format, pts, sequence, true, framebuffer.pixels.clone())?;
        self.last_sequence = Some(sequence);
        self.last_pts = Some(pts);
        self.format = format;
        Ok(frame)
    }

    async fn read_until_frame(&mut self) -> Result<Option<Frame>, MediaError> {
        loop {
            let read = {
                let id = self.config.id.clone();
                let max_body = self.config.max_message_body();
                let timeout = self.config.connect_timeout;
                let Some(listener) = self.listener.as_mut() else {
                    return Err(MediaError::capture(id, "listener transport is closed"));
                };
                read_message(listener.as_mut(), max_body, Some(timeout)).await
            };

            let message = match read {
                Ok(message) => message,
                Err(DBusError::Idle(_)) => return Ok(None),
                Err(error) => {
                    // A framing/transport failure means the byte stream can no
                    // longer be trusted; later reads report end of stream.
                    self.eof = true;
                    return Err(error.to_media_error(&self.config.id));
                }
            };

            if !message.header.is_method_call() {
                continue;
            }

            let reply = self
                .build_method_reply(&message)
                .map_err(|error| error.to_media_error(&self.config.id))?;
            let write = {
                let Some(listener) = self.listener.as_mut() else {
                    return Err(MediaError::capture(
                        &self.config.id,
                        "listener transport is closed",
                    ));
                };
                listener.write_all(&reply).await
            };
            if let Err(error) = write {
                self.eof = true;
                return Err(error.to_media_error(&self.config.id));
            }

            match message.header.member.as_deref() {
                Some("Scanout") => {
                    let framebuffer = Framebuffer::from_scanout(
                        &message.body,
                        self.config.width,
                        self.config.height,
                    )
                    .map_err(|error| error.to_media_error(&self.config.id))?;
                    self.framebuffer = Some(framebuffer);
                    return self.make_frame().map(Some);
                }
                Some("Update") => {
                    {
                        let framebuffer = self.framebuffer.as_mut().ok_or_else(|| {
                            MediaError::capture(
                                &self.config.id,
                                "QEMU sent Update before any Scanout",
                            )
                        })?;
                        framebuffer
                            .apply_update(&message.body)
                            .map_err(|error| error.to_media_error(&self.config.id))?;
                    }
                    return self.make_frame().map(Some);
                }
                Some("Disable") => {
                    self.framebuffer = None;
                    self.format = FrameFormat::new(PixelFormat::Bgra, 0, 0);
                }
                _ => {}
            }
        }
    }
}

#[async_trait]
impl CaptureSource for DBusDisplaySource {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped || self.eof {
            return Err(MediaError::EndOfStream);
        }
        if !self.initialized {
            self.connect().await?;
        }

        match self.read_until_frame().await {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => {
                // A bounded idle wait re-emits the latest complete framebuffer
                // so a static guest screen does not look like a dead source.
                if self.framebuffer.is_some() {
                    self.make_frame()
                } else {
                    Err(MediaError::capture(
                        &self.config.id,
                        format!(
                            "no D-Bus display frame arrived within {:?}",
                            self.config.connect_timeout
                        ),
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.eof = true;
        self.framebuffer = None;
        self.listener_peer = None;
        if let Some(mut listener) = self.listener.take() {
            let _ = listener.shutdown().await;
        }
        if let Some(mut display) = self.display.take() {
            let _ = display.shutdown().await;
        }
        Ok(())
    }
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn into_tokio_stream(stream: StdUnixStream) -> io::Result<UnixStream> {
    stream.set_nonblocking(true)?;
    UnixStream::from_std(stream)
}

fn missing_socket_error(path: &Path) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!("QMP socket `{}` does not exist.", path.display()),
    )
    .with_details(path.display().to_string())
    .with_remediation("Start the VM with a QMP socket at the configured path, then retry.")
    .retryable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const PIXMAN_X8R8G8B8: u32 = 0x2000_0000;

    #[derive(Clone, Default)]
    struct WriteLog {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        fd_writes: Arc<AtomicUsize>,
    }

    impl WriteLog {
        fn messages(&self) -> Vec<DBusMessage> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .filter(|write| write.first() == Some(&b'l'))
                .filter_map(|write| parse_message(write).ok())
                .collect()
        }

        fn fds(&self) -> usize {
            self.fd_writes.load(Ordering::Relaxed)
        }
    }

    struct FakeTransport {
        read: Vec<u8>,
        pos: usize,
        log: WriteLog,
        idle_when_empty: bool,
    }

    impl FakeTransport {
        fn new(read: Vec<u8>) -> Self {
            Self {
                read,
                pos: 0,
                log: WriteLog::default(),
                idle_when_empty: false,
            }
        }

        fn with_log(read: Vec<u8>, log: WriteLog) -> Self {
            Self {
                read,
                pos: 0,
                log,
                idle_when_empty: false,
            }
        }

        fn idle_when_empty(mut self) -> Self {
            self.idle_when_empty = true;
            self
        }
    }

    #[async_trait]
    impl DBusTransport for FakeTransport {
        async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), DBusError> {
            if self.pos + buffer.len() > self.read.len() {
                return if self.idle_when_empty {
                    Err(DBusError::Idle(Duration::from_millis(1)))
                } else {
                    Err(DBusError::Closed)
                };
            }
            let end = self.pos + buffer.len();
            buffer.copy_from_slice(&self.read[self.pos..end]);
            self.pos = end;
            Ok(())
        }

        async fn write_all(&mut self, data: &[u8]) -> Result<(), DBusError> {
            self.log.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }

        async fn write_with_fds(&mut self, data: &[u8], fds: &[RawFd]) -> Result<(), DBusError> {
            self.log.fd_writes.fetch_add(fds.len(), Ordering::Relaxed);
            self.write_all(data).await
        }
    }

    fn auth_bytes() -> Vec<u8> {
        b"OK 0123456789abcdef0123456789abcdef\r\nAGREE_UNIX_FD\r\n".to_vec()
    }

    fn display_script(width: u32, height: u32) -> Vec<u8> {
        let mut script = auth_bytes();
        let properties = encode_properties_body(&[
            ("Width", DBusValue::U32(width)),
            ("Height", DBusValue::U32(height)),
            ("Label", DBusValue::String("Primary".to_owned())),
        ])
        .unwrap();
        script.extend(encode_method_return(1, 1, "a{sv}", &properties));
        script.extend(encode_method_return(2, 2, "", &[]));
        script
    }

    fn listener_script(console_path: &str) -> Vec<u8> {
        let mut script = auth_bytes();
        script.extend(encode_method_call(
            1,
            console_path,
            DBUS_PROPERTIES_INTERFACE,
            "GetAll",
            "s",
            &encode_string_body(DBUS_LISTENER_INTERFACE),
        ));
        script
    }

    fn scanout_body(width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&width.to_le_bytes());
        body.extend_from_slice(&height.to_le_bytes());
        body.extend_from_slice(&stride.to_le_bytes());
        body.extend_from_slice(&format.to_le_bytes());
        body.extend_from_slice(&u32::try_from(pixels.len()).unwrap().to_le_bytes());
        body.extend_from_slice(pixels);
        body
    }

    fn update_body(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        format: u32,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.extend_from_slice(&width.to_le_bytes());
        body.extend_from_slice(&height.to_le_bytes());
        body.extend_from_slice(&stride.to_le_bytes());
        body.extend_from_slice(&format.to_le_bytes());
        body.extend_from_slice(&u32::try_from(pixels.len()).unwrap().to_le_bytes());
        body.extend_from_slice(pixels);
        body
    }

    fn listener_message(serial: u32, member: &str, signature: &str, body: &[u8]) -> Vec<u8> {
        encode_method_call(
            serial,
            "/org/qemu/Display1/Console_0",
            DBUS_LISTENER_INTERFACE,
            member,
            signature,
            body,
        )
    }

    fn test_config() -> DBusDisplayConfig {
        DBusDisplayConfig {
            qmp_socket: PathBuf::from("/run/ozma/test-qmp.sock"),
            console_index: 0,
            id: "dbus-test".to_owned(),
            name: "Test D-Bus display".to_owned(),
            width: 64,
            height: 64,
            fps: 30,
            connect_timeout: Duration::from_millis(250),
        }
    }

    fn source_with(
        config: DBusDisplayConfig,
        display: Vec<u8>,
        listener: Vec<u8>,
    ) -> DBusDisplaySource {
        DBusDisplaySource::with_transports(
            config,
            Box::new(FakeTransport::new(display)),
            Box::new(FakeTransport::new(listener)),
        )
        .unwrap()
    }

    #[test]
    fn config_validation_rejects_invalid_values() {
        let mut config = test_config();
        config.id.clear();
        assert!(DBusDisplaySource::new(config).is_err());

        let mut config = test_config();
        config.width = 0;
        assert!(DBusDisplaySource::new(config).is_err());

        let mut config = test_config();
        config.height = MAX_DBUS_DIMENSION + 1;
        assert!(DBusDisplaySource::new(config).is_err());

        let mut config = test_config();
        config.fps = 0;
        assert!(DBusDisplaySource::new(config).is_err());

        let mut config = test_config();
        config.connect_timeout = Duration::ZERO;
        assert!(DBusDisplaySource::new(config).is_err());

        let mut config = test_config();
        config.qmp_socket = PathBuf::new();
        assert!(DBusDisplaySource::new(config).is_err());

        assert!(DBusDisplaySource::new(test_config()).is_ok());
    }

    #[test]
    fn defaults_match_the_vm_profile_shape() {
        let config = DBusDisplayConfig::default();
        assert_eq!(config.console_index, 0);
        assert_eq!(config.id, "dbus0");
        assert_eq!(config.name, "QEMU D-Bus display");
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.fps, 30);
        assert_eq!(config.console_path(), "/org/qemu/Display1/Console_0");
        assert_eq!(
            DBusDisplayConfig::default()
                .with_console_index(2)
                .console_path(),
            "/org/qemu/Display1/Console_2"
        );
    }

    #[test]
    fn enumerate_is_side_effect_free_and_shape_is_bgra() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("qmp.sock");
        std::fs::write(&socket, b"placeholder").unwrap();

        let config = DBusDisplayConfig::new(&socket)
            .with_limits(800, 600)
            .with_fps(24)
            .with_identity("dbus-x", "Secondary display")
            .with_console_index(1);
        let provider = DBusDisplayProvider::new(config.clone());
        assert_eq!(provider.kind(), CaptureKind::Dbus);

        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let capability = &capabilities[0];
        assert_eq!(capability.kind, CaptureKind::Dbus);
        assert_eq!(capability.id, "dbus-x");
        assert_eq!(capability.name, "Secondary display");
        assert_eq!(capability.formats, vec!["bgra".to_owned()]);
        assert!(!capability.compressed);
        assert!(!capability.hdr);
        assert!(!capability.has_audio);
        assert_eq!((capability.max_width, capability.max_height), (800, 600));
        assert_eq!(capability.max_fps, 24);

        // Registration and open never connect to QEMU.
        let registry = CaptureRegistry::new().with_dbus_display(config.clone());
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans[0].kind, CaptureKind::Dbus);
        assert_eq!(scans[0].capabilities.len(), 1);

        let mut wrong_kind = capability.clone();
        wrong_kind.kind = CaptureKind::Qmp;
        assert_eq!(
            provider.open(&wrong_kind).err().unwrap().code,
            codes::NOT_FOUND
        );

        let mut wrong_id = capability.clone();
        wrong_id.id = "other".to_owned();
        assert_eq!(
            provider.open(&wrong_id).err().unwrap().code,
            codes::NOT_FOUND
        );

        assert!(provider.open(capability).is_ok());

        let missing = DBusDisplayConfig::new(directory.path().join("absent.sock"));
        assert!(DBusDisplayProvider::new(missing)
            .enumerate()
            .unwrap()
            .is_empty());

        // D-Bus capture is never part of the implicit hardware registries.
        assert!(CaptureRegistry::system()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::Dbus));
        assert!(CaptureRegistry::system_with_pipewire()
            .scan()
            .iter()
            .all(|scan| scan.kind != CaptureKind::Dbus));
    }

    #[test]
    fn method_call_and_return_headers_round_trip() {
        let body = encode_string_body("org.qemu.Display1.Console");
        let encoded = encode_method_call(
            7,
            "/org/qemu/Display1/Console_0",
            DBUS_PROPERTIES_INTERFACE,
            "GetAll",
            "s",
            &body,
        );
        let message = parse_message(&encoded).unwrap();
        assert_eq!(message.header.message_type, DBUS_METHOD_CALL);
        assert!(message.header.is_method_call());
        assert_eq!(message.header.serial, 7);
        assert_eq!(
            message.header.path.as_deref(),
            Some("/org/qemu/Display1/Console_0")
        );
        assert_eq!(
            message.header.interface.as_deref(),
            Some(DBUS_PROPERTIES_INTERFACE)
        );
        assert_eq!(message.header.member.as_deref(), Some("GetAll"));
        assert_eq!(message.header.signature.as_deref(), Some("s"));
        assert_eq!(message.body, body);

        let reply = encode_method_return(9, 7, "", &[]);
        let message = parse_message(&reply).unwrap();
        assert!(message.header.is_method_return());
        assert_eq!(message.header.serial, 9);
        assert_eq!(message.header.reply_serial, Some(7));
        assert!(message.body.is_empty());
    }

    #[test]
    fn properties_round_trip_including_variant_arrays() {
        let body = encode_properties_body(&[
            ("Width", DBusValue::U32(1920)),
            ("Height", DBusValue::U32(1080)),
            ("Label", DBusValue::String("Primary".to_owned())),
            ("Head", DBusValue::U32(0)),
            (
                "Interfaces",
                DBusValue::StrArray(vec![
                    DBUS_CONSOLE_INTERFACE.to_owned(),
                    DBUS_LISTENER_INTERFACE.to_owned(),
                ]),
            ),
            ("Raw", DBusValue::Bytes(vec![1, 2, 3, 4])),
        ])
        .unwrap();
        let properties = parse_properties(&body).unwrap();
        assert_eq!(
            properties.get("Width").and_then(DBusValue::as_u32),
            Some(1920)
        );
        assert_eq!(
            properties.get("Height").and_then(DBusValue::as_u32),
            Some(1080)
        );
        assert_eq!(
            properties.get("Label"),
            Some(&DBusValue::String("Primary".to_owned()))
        );
        assert_eq!(
            properties.get("Interfaces"),
            Some(&DBusValue::StrArray(vec![
                DBUS_CONSOLE_INTERFACE.to_owned(),
                DBUS_LISTENER_INTERFACE.to_owned()
            ]))
        );
        assert_eq!(
            properties.get("Raw"),
            Some(&DBusValue::Bytes(vec![1, 2, 3, 4]))
        );
    }

    #[test]
    fn scanout_builds_a_packed_bgra_framebuffer() {
        let pixels: Vec<u8> = (0..16).collect();
        let body = scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &pixels);
        let framebuffer = Framebuffer::from_scanout(&body, 64, 64).unwrap();
        assert_eq!(framebuffer.width(), 2);
        assert_eq!(framebuffer.height(), 2);
        assert_eq!(framebuffer.stride(), 8);
        assert_eq!(framebuffer.pixels(), pixels.as_slice());
    }

    #[test]
    fn scanout_repacks_a_padded_stride() {
        let mut pixels = Vec::new();
        for row in 0..2u8 {
            for column in 0..2u8 {
                let value = row * 10 + column;
                pixels.extend_from_slice(&[value, value + 1, value + 2, value + 3]);
            }
            pixels.extend_from_slice(&[0xaa; 4]);
        }
        assert_eq!(pixels.len(), 24);
        let body = scanout_body(2, 2, 12, PIXMAN_X8R8G8B8, &pixels);
        let framebuffer = Framebuffer::from_scanout(&body, 64, 64).unwrap();
        assert_eq!(
            framebuffer.pixels(),
            &[0, 1, 2, 3, 1, 2, 3, 4, 10, 11, 12, 13, 11, 12, 13, 14]
        );
    }

    #[test]
    fn update_applies_only_the_damaged_rectangle() {
        let initial = vec![0u8; 4 * 4 * 4];
        let body = scanout_body(4, 4, 16, PIXMAN_X8R8G8B8, &initial);
        let mut framebuffer = Framebuffer::from_scanout(&body, 64, 64).unwrap();

        let mut damaged = Vec::new();
        for row in 0..2u8 {
            for column in 0..2u8 {
                let value = 100 + row * 10 + column;
                damaged.extend_from_slice(&[value; 4]);
            }
        }
        framebuffer
            .apply_update(&update_body(1, 1, 2, 2, 8, PIXMAN_X8R8G8B8, &damaged))
            .unwrap();

        for row in 0..4usize {
            for column in 0..4usize {
                let offset = (row * 4 + column) * 4;
                let expected = if (1..3).contains(&row) && (1..3).contains(&column) {
                    let value = 100 + (row - 1) as u8 * 10 + (column - 1) as u8;
                    vec![value; 4]
                } else {
                    vec![0; 4]
                };
                assert_eq!(
                    &framebuffer.pixels()[offset..offset + 4],
                    expected.as_slice(),
                    "pixel {column},{row}"
                );
            }
        }
    }

    #[test]
    fn geometry_and_format_errors_are_typed() {
        let pixels = vec![0u8; 16];
        let body = scanout_body(0, 2, 8, PIXMAN_X8R8G8B8, &pixels);
        assert!(matches!(
            Framebuffer::from_scanout(&body, 64, 64),
            Err(DBusError::Geometry(_))
        ));

        let body = scanout_body(2, 2, 8, 0x0000_0000, &pixels);
        assert!(matches!(
            Framebuffer::from_scanout(&body, 64, 64),
            Err(DBusError::Unsupported(_))
        ));

        let body = scanout_body(8, 8, 32, PIXMAN_X8R8G8B8, &vec![0u8; 256]);
        assert!(matches!(
            Framebuffer::from_scanout(&body, 4, 4),
            Err(DBusError::Oversized(_))
        ));

        let body = scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &pixels[..4]);
        assert!(matches!(
            Framebuffer::from_scanout(&body, 64, 64),
            Err(DBusError::Protocol(_))
        ));

        let initial = vec![0u8; 4 * 4 * 4];
        let body = scanout_body(4, 4, 16, PIXMAN_X8R8G8B8, &initial);
        let mut framebuffer = Framebuffer::from_scanout(&body, 64, 64).unwrap();
        let error = framebuffer
            .apply_update(&update_body(3, 3, 2, 2, 8, PIXMAN_X8R8G8B8, &vec![0u8; 16]))
            .unwrap_err();
        assert!(matches!(error, DBusError::Geometry(_)));
    }

    #[test]
    fn short_and_malformed_messages_do_not_panic() {
        assert!(matches!(parse_message(&[]), Err(DBusError::Protocol(_))));
        assert!(matches!(
            parse_message(&[b'l', 1, 0, 1, 0, 0]),
            Err(DBusError::Protocol(_))
        ));

        let mut message = encode_method_return(1, 1, "", &[]);
        message.truncate(20);
        assert!(matches!(
            parse_message(&message),
            Err(DBusError::Protocol(_))
        ));

        let mut message = encode_method_return(1, 1, "", &[]);
        message[0] = b'B';
        assert!(matches!(
            parse_message(&message),
            Err(DBusError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn read_message_reports_short_reads_and_idle_waits() {
        let mut transport = FakeTransport::new(vec![b'l']);
        assert!(matches!(
            read_message(&mut transport, 1024, None).await,
            Err(DBusError::Closed)
        ));

        let mut transport = FakeTransport::new(Vec::new()).idle_when_empty();
        assert!(matches!(
            read_message(&mut transport, 1024, Some(Duration::from_millis(10))).await,
            Err(DBusError::Idle(_))
        ));
    }

    #[tokio::test]
    async fn authentication_failures_are_typed() {
        let mut transport = FakeTransport::new(b"REJECTED EXTERNAL\r\n".to_vec());
        let error = authenticate(&mut transport).await.unwrap_err();
        assert!(matches!(error, DBusError::Auth(_)));
        assert!(error.to_string().contains("authentication"));

        let mut transport = FakeTransport::new(b"OK guid\r\nERROR nope\r\n".to_vec());
        let error = authenticate(&mut transport).await.unwrap_err();
        assert!(matches!(error, DBusError::Auth(_)));
        assert!(error.to_string().contains("refused"));
    }

    #[tokio::test]
    async fn source_negotiates_and_emits_scanout_then_update() {
        let config = test_config();
        let console = config.console_path();
        let first_pixels: Vec<u8> = (0..16).collect();

        let display_log = WriteLog::default();
        let listener_log = WriteLog::default();

        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &first_pixels),
        ));
        listener.extend(listener_message(
            3,
            "Update",
            "iiiiuuay",
            &update_body(0, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[9, 8, 7, 6]),
        ));

        let mut source = DBusDisplaySource::with_transports(
            config,
            Box::new(FakeTransport::with_log(
                display_script(2, 2),
                display_log.clone(),
            )),
            Box::new(FakeTransport::with_log(listener, listener_log.clone())),
        )
        .unwrap();

        let first = source.next_frame().await.unwrap();
        assert_eq!(first.sequence(), 0);
        assert_eq!(first.format(), FrameFormat::new(PixelFormat::Bgra, 2, 2));
        assert_eq!(first.data(), first_pixels.as_slice());
        assert!(source.is_connected());

        let second = source.next_frame().await.unwrap();
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() > first.pts_micros());
        let mut expected = first_pixels.clone();
        expected[..4].copy_from_slice(&[9, 8, 7, 6]);
        assert_eq!(second.data(), expected.as_slice());

        // GetAll was sent with the Console interface, RegisterListener carried
        // one fd, and every listener call got a reply.
        let display_writes = display_log.messages();
        let getall = display_writes
            .iter()
            .find(|message| message.header.member.as_deref() == Some("GetAll"))
            .expect("GetAll was sent");
        assert_eq!(
            getall.header.interface.as_deref(),
            Some(DBUS_PROPERTIES_INTERFACE)
        );
        let register = display_writes
            .iter()
            .find(|message| message.header.member.as_deref() == Some("RegisterListener"))
            .expect("RegisterListener was sent");
        assert_eq!(register.header.signature.as_deref(), Some("h"));
        // Injected transports do not own a real listener descriptor; the
        // encoder still marks the FD field when a descriptor is attached.
        assert_eq!(register.header.unix_fds, 0);
        assert_eq!(display_log.fds(), 0);
        let with_fd = encode_register_listener_call(11, &console, Some(1));
        assert_eq!(parse_message(&with_fd).unwrap().header.unix_fds, 1);

        let listener_writes = listener_log.messages();
        let getall_reply = listener_writes
            .iter()
            .find(|message| message.header.reply_serial == Some(1))
            .expect("listener GetAll received a reply");
        assert_eq!(getall_reply.header.signature.as_deref(), Some("a{sv}"));
        let interfaces = parse_properties(&getall_reply.body).unwrap();
        assert_eq!(
            interfaces.get("Interfaces"),
            Some(&DBusValue::StrArray(vec![
                DBUS_LISTENER_INTERFACE.to_owned()
            ]))
        );
        let scanout_reply = listener_writes
            .iter()
            .find(|message| message.header.reply_serial == Some(2))
            .expect("Scanout received a reply");
        assert!(scanout_reply.header.is_method_return());
        assert!(listener_writes
            .iter()
            .any(|message| message.header.reply_serial == Some(3)));
    }

    #[tokio::test]
    async fn disable_drops_the_framebuffer_without_emitting_a_frame() {
        let config = test_config();
        let console = config.console_path();
        let first_pixels = vec![1u8; 16];
        let second_pixels = vec![2u8; 16];

        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &first_pixels),
        ));
        listener.extend(listener_message(3, "Disable", "", &[]));
        listener.extend(listener_message(
            4,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &second_pixels),
        ));

        let mut source = source_with(config, display_script(2, 2), listener);
        let first = source.next_frame().await.unwrap();
        assert_eq!(first.data(), first_pixels.as_slice());
        let second = source.next_frame().await.unwrap();
        assert_eq!(second.sequence(), 1);
        assert_eq!(second.data(), second_pixels.as_slice());
    }

    #[tokio::test]
    async fn update_before_scanout_is_a_typed_error() {
        let config = test_config();
        let console = config.console_path();
        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Update",
            "iiiiuuay",
            &update_body(0, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[1, 2, 3, 4]),
        ));

        let mut source = source_with(config, display_script(2, 2), listener);
        let error = source.next_frame().await.unwrap_err();
        assert!(matches!(error, MediaError::Capture { .. }));
        assert!(error.to_string().contains("Update before any Scanout"));
    }

    #[tokio::test]
    async fn auth_failure_surfaces_from_next_frame() {
        let config = test_config();
        let mut source = source_with(
            config,
            b"REJECTED EXTERNAL\r\n".to_vec(),
            listener_script("/org/qemu/Display1/Console_0"),
        );
        let error = source.next_frame().await.unwrap_err();
        assert!(matches!(error, MediaError::Capture { .. }));
        assert!(error.to_string().contains("authentication"));
    }

    #[tokio::test]
    async fn idle_listener_re_emits_the_current_framebuffer() {
        let config = test_config();
        let console = config.console_path();
        let pixels = vec![5u8; 16];
        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &pixels),
        ));

        let mut source = DBusDisplaySource::with_transports(
            config,
            Box::new(FakeTransport::new(display_script(2, 2))),
            Box::new(FakeTransport::new(listener).idle_when_empty()),
        )
        .unwrap();

        let first = source.next_frame().await.unwrap();
        let second = source.next_frame().await.unwrap();
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() > first.pts_micros());
        assert_eq!(second.data(), first.data());
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_later_reads_are_eos() {
        let config = test_config();
        let console = config.console_path();
        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &vec![7u8; 16]),
        ));

        let mut source = source_with(config, display_script(2, 2), listener);
        source.next_frame().await.unwrap();
        source.stop().await.unwrap();
        assert!(source.is_stopped());
        assert!(source.is_eof());
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        source.stop().await.unwrap();
    }

    #[tokio::test]
    async fn listener_eof_is_end_of_stream_not_a_panic() {
        let config = test_config();
        let console = config.console_path();
        let mut listener = listener_script(&console);
        listener.extend(listener_message(
            2,
            "Scanout",
            "uuuuaay",
            &scanout_body(2, 2, 8, PIXMAN_X8R8G8B8, &vec![7u8; 16]),
        ));

        let mut source = source_with(config, display_script(2, 2), listener);
        source.next_frame().await.unwrap();
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        assert!(source.is_eof());
    }

    #[tokio::test]
    #[ignore = "requires a real QEMU with -display dbus,p2p=yes; set OZMA_DBUS_TEST_QMP"]
    async fn real_qemu_dbus_display_pushes_a_frame() {
        let Ok(qmp_socket) = std::env::var("OZMA_DBUS_TEST_QMP") else {
            return;
        };
        let config = DBusDisplayConfig::new(qmp_socket)
            .with_limits(4096, 2160)
            .with_connect_timeout(Duration::from_secs(15));
        let mut source = DBusDisplaySource::new(config).unwrap();
        let frame = source.next_frame().await.unwrap();
        assert_eq!(frame.format().pixel_format, PixelFormat::Bgra);
        assert!(!frame.data().is_empty());
        source.stop().await.unwrap();
    }
}
