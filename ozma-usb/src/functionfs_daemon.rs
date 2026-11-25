// SPDX-License-Identifier: AGPL-3.0-only
//! Safe FunctionFS daemon core.
//!
//! FunctionFS is unusual among composite functions: the kernel does not take
//! descriptors from ConfigFS. A userspace daemon mounts the function, writes a
//! descriptor blob and string table to `ep0`, waits for `FUNCTIONFS_ENABLE`,
//! opens the `ep<N>` files, and then services endpoint traffic and control
//! requests.
//!
//! This module implements that contract over injectable `ep0` and endpoint I/O.
//! It never mounts FunctionFS, never opens a real device by default, and never
//! reports a live daemon unless both `ep0` writes succeeded and the function was
//! enabled. Tests use [`LoopbackFunctionFsDaemon`] with in-memory endpoints; the
//! [`FileFunctionFsDaemon`] path is for an explicitly supplied mount and for
//! ignored/environment-gated kernel tests.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::functionfs::{
    FunctionFsConfig, FunctionFsDaemon, FunctionFsDaemonStatus, FunctionFsEndpoint,
    FunctionFsStatus, MAX_FUNCTIONFS_DESCRIPTOR_BYTES, MAX_FUNCTIONFS_ENDPOINTS,
};
use crate::functionfs_lifecycle::{
    FunctionFsDeviceState, FunctionFsDeviceStatus, FunctionFsStateMachine,
};

/// FunctionFS v2 descriptor blob magic.
pub const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;

/// FunctionFS string table magic.
pub const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;

/// Full-speed descriptor set present.
pub const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
/// High-speed descriptor set present.
pub const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
/// Super-speed descriptor set present.
pub const FUNCTIONFS_HAS_SS_DESC: u32 = 4;
/// Microsoft OS descriptor set present.
pub const FUNCTIONFS_HAS_MS_OS_DESC: u32 = 8;

/// Maximum bytes accepted for one `ep0` write.
pub const MAX_FUNCTIONFS_EP0_WRITE_BYTES: usize = 8 * 1024;

/// Maximum bytes moved in one endpoint transfer.
pub const MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES: usize = 64 * 1024;

/// Size of one encoded `usb_functionfs_event`.
pub const FUNCTIONFS_EVENT_BYTES: usize = 12;

/// One function-scoped descriptor set and the FunctionFS `ep0` encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsDescriptors {
    fs: Vec<u8>,
    hs: Vec<u8>,
    os: Vec<Vec<u8>>,
    strings: Vec<u8>,
    fs_count: u32,
    hs_count: u32,
}

impl FunctionFsDescriptors {
    /// Build FS/HS function descriptors and strings from a validated model.
    ///
    /// The full-speed and high-speed sets intentionally use the same endpoint
    /// shapes. Device and configuration descriptors are excluded because the
    /// composite gadget owns them; only interface, class, and endpoint
    /// descriptors are legal on FunctionFS `ep0`.
    pub fn from_config(config: &FunctionFsConfig) -> Result<Self, UsbError> {
        config.validate()?;
        let standard = config.descriptors();
        let hs = standard.function_descriptors().to_vec();
        let fs = full_speed_function_descriptors(standard.function_descriptors())?;
        let fs_count = count_descriptors(&fs)?;
        let hs_count = count_descriptors(&hs)?;
        let os = config
            .os_descriptors
            .iter()
            .map(|descriptor| descriptor.encode())
            .collect::<Result<Vec<_>, _>>()?;
        let strings = encode_strings(config)?;
        let descriptors = Self {
            fs,
            hs,
            os,
            strings,
            fs_count,
            hs_count,
        };
        descriptors.validate()?;
        Ok(descriptors)
    }

    /// Full-speed function descriptors (interface, class, endpoints).
    pub fn full_speed(&self) -> &[u8] {
        &self.fs
    }

    /// High-speed function descriptors.
    pub fn high_speed(&self) -> &[u8] {
        &self.hs
    }

    /// Encoded Microsoft OS descriptors.
    pub fn os(&self) -> &[Vec<u8>] {
        &self.os
    }

    /// Descriptor blob flags.
    pub fn flags(&self) -> u32 {
        let mut flags = FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC;
        if !self.os.is_empty() {
            flags |= FUNCTIONFS_HAS_MS_OS_DESC;
        }
        flags
    }

    /// Number of full-speed descriptors (not bytes).
    pub fn full_speed_count(&self) -> u32 {
        self.fs_count
    }

    /// Number of high-speed descriptors (not bytes).
    pub fn high_speed_count(&self) -> u32 {
        self.hs_count
    }

    /// Number of Microsoft OS descriptor blocks.
    pub fn os_count(&self) -> u32 {
        self.os.len() as u32
    }

    /// Encoded FunctionFS string table (magic 2, one `0x0409` language table).
    pub fn strings(&self) -> &[u8] {
        &self.strings
    }

    /// Alias for [`Self::strings`].
    pub fn strings_blob(&self) -> &[u8] {
        &self.strings
    }

    /// Alias for [`Self::descriptor_blob`].
    pub fn blob(&self) -> Result<Vec<u8>, UsbError> {
        self.descriptor_blob()
    }

    /// Validate header counts, descriptor boundaries, and total size.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.fs.is_empty() || self.hs.is_empty() {
            return Err(UsbError::InvalidFunctionFsDescriptor {
                reason: "both full-speed and high-speed descriptor sets are required".to_owned(),
            });
        }
        let parsed = parse_descriptor_blob(&self.descriptor_blob()?)?;
        if parsed.fs_count != self.fs_count || parsed.hs_count != self.hs_count {
            return Err(UsbError::InvalidFunctionFsDescriptor {
                reason: "descriptor counts do not match the encoded blob".to_owned(),
            });
        }
        if parsed.os_count != self.os_count() {
            return Err(UsbError::InvalidFunctionFsDescriptor {
                reason: "OS descriptor count does not match the encoded blob".to_owned(),
            });
        }
        parse_string_blob(&self.strings)?;
        Ok(())
    }

    /// Encode the v2 descriptor blob written first to `ep0`.
    pub fn descriptor_blob(&self) -> Result<Vec<u8>, UsbError> {
        let flags = self.flags();
        let mut count_words = 0usize;
        if flags & FUNCTIONFS_HAS_FS_DESC != 0 {
            count_words += 1;
        }
        if flags & FUNCTIONFS_HAS_HS_DESC != 0 {
            count_words += 1;
        }
        if flags & FUNCTIONFS_HAS_SS_DESC != 0 {
            count_words += 1;
        }
        if flags & FUNCTIONFS_HAS_MS_OS_DESC != 0 {
            count_words += 1;
        }

        let os_bytes: usize = self.os.iter().map(Vec::len).sum();
        let total = 12 + count_words * 4 + self.fs.len() + self.hs.len() + os_bytes;
        if total > MAX_FUNCTIONFS_DESCRIPTOR_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "descriptor blob",
                limit: MAX_FUNCTIONFS_DESCRIPTOR_BYTES,
                requested: total,
            });
        }
        let mut blob = Vec::with_capacity(total);
        blob.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
        blob.extend_from_slice(&(total as u32).to_le_bytes());
        blob.extend_from_slice(&flags.to_le_bytes());
        if flags & FUNCTIONFS_HAS_FS_DESC != 0 {
            blob.extend_from_slice(&self.fs_count.to_le_bytes());
        }
        if flags & FUNCTIONFS_HAS_HS_DESC != 0 {
            blob.extend_from_slice(&self.hs_count.to_le_bytes());
        }
        if flags & FUNCTIONFS_HAS_SS_DESC != 0 {
            blob.extend_from_slice(&0u32.to_le_bytes());
        }
        if flags & FUNCTIONFS_HAS_MS_OS_DESC != 0 {
            blob.extend_from_slice(&self.os_count().to_le_bytes());
        }
        blob.extend_from_slice(&self.fs);
        blob.extend_from_slice(&self.hs);
        for descriptor in &self.os {
            blob.extend_from_slice(descriptor);
        }
        Ok(blob)
    }

    /// Write the descriptor blob and then the string table to one writer.
    ///
    /// This is the injectable/writer form used by tests and custom transports.
    /// Each call contains one complete FunctionFS state-machine write.
    pub fn write_to(&self, writer: &mut dyn Write) -> Result<FunctionFsWriteReport, UsbError> {
        let blob = self.descriptor_blob()?;
        writer
            .write_all(&blob)
            .map_err(|source| ep0_error("write", PathBuf::from("<writer>"), source))?;
        writer
            .write_all(&self.strings)
            .map_err(|source| ep0_error("write", PathBuf::from("<writer>"), source))?;
        writer
            .flush()
            .map_err(|source| ep0_error("flush", PathBuf::from("<writer>"), source))?;
        Ok(FunctionFsWriteReport {
            descriptor_bytes: blob.len(),
            string_bytes: self.strings.len(),
        })
    }

    /// Write the descriptor blob and string table through an `ep0` transport.
    pub fn write_to_ep0(
        &self,
        ep0: &mut dyn FunctionFsEp0Io,
    ) -> Result<FunctionFsWriteReport, UsbError> {
        let blob = self.descriptor_blob()?;
        ep0.write_ep0(&blob)?;
        ep0.write_ep0(&self.strings)?;
        Ok(FunctionFsWriteReport {
            descriptor_bytes: blob.len(),
            string_bytes: self.strings.len(),
        })
    }
}

/// Bytes written to FunctionFS `ep0` for one function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsWriteReport {
    /// Descriptor blob bytes.
    pub descriptor_bytes: usize,
    /// String table bytes.
    pub string_bytes: usize,
}

impl FunctionFsWriteReport {
    /// Total bytes written.
    pub fn total(&self) -> usize {
        self.descriptor_bytes + self.string_bytes
    }
}

/// Parsed FunctionFS v2 descriptor blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFunctionFsDescriptors {
    /// Flags from the header.
    pub flags: u32,
    /// Full-speed descriptor count.
    pub fs_count: u32,
    /// High-speed descriptor count.
    pub hs_count: u32,
    /// Super-speed descriptor count.
    pub ss_count: u32,
    /// Microsoft OS descriptor count.
    pub os_count: u32,
    /// Full-speed descriptor bytes.
    pub fs: Vec<u8>,
    /// High-speed descriptor bytes.
    pub hs: Vec<u8>,
    /// Super-speed descriptor bytes.
    pub ss: Vec<u8>,
    /// Microsoft OS descriptor bytes.
    pub os: Vec<u8>,
}

/// Parse and validate a FunctionFS v2 descriptor blob.
///
/// FunctionFS reads little-endian count fields with unaligned loads, so this
/// parser treats the blob as a byte stream but requires every `length` and
/// `bLength` field to line up exactly. Unknown flags, reserved gadget
/// descriptors, trailing bytes, and truncated descriptors are rejected with
/// [`UsbError::InvalidFunctionFsDescriptor`].
pub fn parse_descriptor_blob(bytes: &[u8]) -> Result<ParsedFunctionFsDescriptors, UsbError> {
    if bytes.len() < 12 {
        return Err(invalid_descriptor("v2 header is shorter than 12 bytes"));
    }
    if bytes.len() > MAX_FUNCTIONFS_DESCRIPTOR_BYTES {
        return Err(UsbError::FunctionFsBuffer {
            context: "descriptor blob",
            limit: MAX_FUNCTIONFS_DESCRIPTOR_BYTES,
            requested: bytes.len(),
        });
    }
    let magic = read_u32(bytes, 0)?;
    if magic != FUNCTIONFS_DESCRIPTORS_MAGIC_V2 {
        return Err(invalid_descriptor(format!(
            "expected v2 magic {FUNCTIONFS_DESCRIPTORS_MAGIC_V2}, found {magic}"
        )));
    }
    let length = read_u32(bytes, 4)? as usize;
    if length != bytes.len() {
        return Err(invalid_descriptor(format!(
            "header length {length} does not match {} encoded bytes",
            bytes.len()
        )));
    }
    let flags = read_u32(bytes, 8)?;
    let known = FUNCTIONFS_HAS_FS_DESC
        | FUNCTIONFS_HAS_HS_DESC
        | FUNCTIONFS_HAS_SS_DESC
        | FUNCTIONFS_HAS_MS_OS_DESC;
    if flags & !known != 0 {
        return Err(invalid_descriptor(format!(
            "unsupported descriptor flags 0x{:08x}",
            flags & !known
        )));
    }
    if flags & (FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC) == 0 {
        return Err(invalid_descriptor(
            "at least one of FS or HS descriptors is required",
        ));
    }

    let mut offset = 12usize;
    let mut read_count = |present: bool| -> Result<u32, UsbError> {
        if present {
            let value = read_u32(bytes, offset)?;
            offset += 4;
            Ok(value)
        } else {
            Ok(0)
        }
    };
    let fs_count = read_count(flags & FUNCTIONFS_HAS_FS_DESC != 0)?;
    let hs_count = read_count(flags & FUNCTIONFS_HAS_HS_DESC != 0)?;
    let ss_count = read_count(flags & FUNCTIONFS_HAS_SS_DESC != 0)?;
    let os_count = read_count(flags & FUNCTIONFS_HAS_MS_OS_DESC != 0)?;

    let fs_len = descriptor_set_len(bytes, offset, fs_count, "full-speed")?;
    let fs = bytes[offset..offset + fs_len].to_vec();
    offset += fs_len;
    let hs_len = descriptor_set_len(bytes, offset, hs_count, "high-speed")?;
    let hs = bytes[offset..offset + hs_len].to_vec();
    offset += hs_len;
    let ss_len = descriptor_set_len(bytes, offset, ss_count, "super-speed")?;
    let ss = bytes[offset..offset + ss_len].to_vec();
    offset += ss_len;

    if offset > bytes.len() {
        return Err(invalid_descriptor("descriptor payload is truncated"));
    }
    let os = bytes[offset..].to_vec();
    validate_os_descriptors(&os, os_count)?;

    if ss_count > 0 && ss.is_empty() {
        return Err(invalid_descriptor(
            "super-speed count is non-zero but empty",
        ));
    }
    Ok(ParsedFunctionFsDescriptors {
        flags,
        fs_count,
        hs_count,
        ss_count,
        os_count,
        fs,
        hs,
        ss,
        os,
    })
}

/// Parse and validate a FunctionFS string table.
pub fn parse_string_blob(bytes: &[u8]) -> Result<Vec<String>, UsbError> {
    if bytes.len() < 16 {
        return Err(invalid_descriptor("string table is shorter than 16 bytes"));
    }
    let magic = read_u32(bytes, 0)?;
    if magic != FUNCTIONFS_STRINGS_MAGIC {
        return Err(invalid_descriptor(format!(
            "expected string magic {FUNCTIONFS_STRINGS_MAGIC}, found {magic}"
        )));
    }
    let length = read_u32(bytes, 4)? as usize;
    if length != bytes.len() {
        return Err(invalid_descriptor(format!(
            "string length {length} does not match {} encoded bytes",
            bytes.len()
        )));
    }
    let str_count = read_u32(bytes, 8)? as usize;
    let lang_count = read_u32(bytes, 12)? as usize;
    if (str_count == 0) != (lang_count == 0) {
        return Err(invalid_descriptor(
            "string and language counts must both be zero or both non-zero",
        ));
    }
    if str_count > 256 || lang_count > 16 {
        return Err(invalid_descriptor(
            "string table counts exceed FunctionFS limits",
        ));
    }
    let mut offset = 16usize;
    let mut strings = Vec::new();
    for _ in 0..lang_count {
        if offset + 2 > bytes.len() {
            return Err(invalid_descriptor("string table language is truncated"));
        }
        let _language = read_u16(bytes, offset)?;
        offset += 2;
        for _ in 0..str_count {
            let rest = &bytes[offset..];
            let end = rest
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| invalid_descriptor("string table entry is not NUL terminated"))?;
            let value = std::str::from_utf8(&rest[..end])
                .map_err(|_| invalid_descriptor("string table entries must be valid UTF-8"))?;
            strings.push(value.to_owned());
            offset += end + 1;
        }
    }
    if offset != bytes.len() {
        return Err(invalid_descriptor("string table has trailing bytes"));
    }
    Ok(strings)
}

/// A FunctionFS mount directory with `ep0` and `ep<N>` children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsMount {
    root: PathBuf,
}

impl FunctionFsMount {
    /// A mount at an explicit path (usually `/dev/ffs-<instance>`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The conventional `/dev/ffs-<instance>` mount for a ConfigFS instance.
    pub fn for_instance(instance: &str) -> Self {
        Self::new(format!("/dev/ffs-{instance}"))
    }

    /// Mount root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `ep0` path.
    pub fn ep0_path(&self) -> PathBuf {
        self.root.join("ep0")
    }

    /// `ep<N>` path by one-based descriptor order (not USB endpoint address).
    pub fn endpoint_path(&self, index: u8) -> Result<PathBuf, UsbError> {
        if !(1..=MAX_FUNCTIONFS_ENDPOINTS as u8).contains(&index) {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.endpoint.index",
                reason: format!("endpoint index must be 1..={MAX_FUNCTIONFS_ENDPOINTS}"),
            });
        }
        Ok(self.root.join(format!("ep{index}")))
    }

    /// Validate that the mount directory and `ep0` entry exist.
    ///
    /// Regular files are accepted so tests can use a temporary directory. On a
    /// real FunctionFS mount `ep0` is a character device; neither is opened
    /// here.
    pub fn validate(&self) -> Result<(), UsbError> {
        let metadata = fs::metadata(&self.root).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MissingFunctionFsMount(self.root.clone())
            } else {
                UsbError::FunctionFsEp0Io {
                    operation: "stat mount",
                    path: self.root.clone(),
                    source,
                }
            }
        })?;
        if !metadata.is_dir() {
            return Err(UsbError::MissingFunctionFsMount(self.root.clone()));
        }
        let ep0 = self.ep0_path();
        let metadata = fs::metadata(&ep0).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MissingFunctionFsEp0(ep0.clone())
            } else {
                UsbError::FunctionFsEp0Io {
                    operation: "stat",
                    path: ep0.clone(),
                    source,
                }
            }
        })?;
        if !is_ep0_entry(&metadata) {
            return Err(UsbError::MissingFunctionFsEp0(ep0));
        }
        Ok(())
    }

    /// Open `ep0`, write the descriptor blob, then write the string table.
    pub fn write_descriptors(
        &self,
        descriptors: &FunctionFsDescriptors,
    ) -> Result<FunctionFsWriteReport, UsbError> {
        self.validate()?;
        let mut ep0 = FileEp0Io::open(&self.ep0_path())?;
        descriptors.write_to_ep0(&mut ep0)
    }
}

fn is_ep0_entry(metadata: &fs::Metadata) -> bool {
    if metadata.is_file() {
        return true;
    }
    #[cfg(unix)]
    {
        metadata.file_type().is_char_device() || metadata.file_type().is_fifo()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Injectable FunctionFS `ep0` transport.
pub trait FunctionFsEp0Io {
    /// Write one complete descriptor or string-table chunk.
    fn write_ep0(&mut self, data: &[u8]) -> Result<(), UsbError>;

    /// Read one or more encoded `usb_functionfs_event` values.
    fn read_ep0_event(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError>;

    /// Test/back-end downcast hook.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Test/back-end shared downcast hook.
    fn as_any_ref(&self) -> &dyn Any;
}

/// In-memory `ep0` used by the loopback daemon and tests.
#[derive(Debug, Default)]
pub struct MemoryEp0 {
    writes: Vec<Vec<u8>>,
    events: VecDeque<FunctionFsEvent>,
}

impl MemoryEp0 {
    /// Empty in-memory `ep0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one decoded event for [`FunctionFsDaemonCore::handle_next_event`].
    pub fn push_event(&mut self, event: FunctionFsEvent) {
        self.events.push_back(event);
    }

    /// Every complete `ep0` write, in order.
    pub fn writes(&self) -> &[Vec<u8>] {
        &self.writes
    }

    /// Remove and return all recorded writes.
    pub fn take_writes(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.writes)
    }
}

impl FunctionFsEp0Io for MemoryEp0 {
    fn write_ep0(&mut self, data: &[u8]) -> Result<(), UsbError> {
        if data.len() > MAX_FUNCTIONFS_EP0_WRITE_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "ep0 write",
                limit: MAX_FUNCTIONFS_EP0_WRITE_BYTES,
                requested: data.len(),
            });
        }
        self.writes.push(data.to_vec());
        Ok(())
    }

    fn read_ep0_event(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        if buffer.len() < FUNCTIONFS_EVENT_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "ep0 event buffer",
                limit: FUNCTIONFS_EVENT_BYTES,
                requested: buffer.len(),
            });
        }
        let Some(event) = self.events.pop_front() else {
            return Ok(0);
        };
        let encoded = event.encode();
        buffer[..FUNCTIONFS_EVENT_BYTES].copy_from_slice(&encoded);
        Ok(FUNCTIONFS_EVENT_BYTES)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// File-backed `ep0` for an explicitly supplied mount.
#[derive(Debug)]
pub struct FileEp0Io {
    path: PathBuf,
    file: File,
}

impl FileEp0Io {
    /// Open `ep0` read/write without mounting anything.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, UsbError> {
        let path = path.into();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    UsbError::MissingFunctionFsEp0(path.clone())
                } else {
                    ep0_error("open", path.clone(), source)
                }
            })?;
        Ok(Self { path, file })
    }
}

impl FunctionFsEp0Io for FileEp0Io {
    fn write_ep0(&mut self, data: &[u8]) -> Result<(), UsbError> {
        if data.len() > MAX_FUNCTIONFS_EP0_WRITE_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "ep0 write",
                limit: MAX_FUNCTIONFS_EP0_WRITE_BYTES,
                requested: data.len(),
            });
        }
        let written = self
            .file
            .write(data)
            .map_err(|source| ep0_error("write", self.path.clone(), source))?;
        if written != data.len() {
            return Err(ep0_error(
                "write",
                self.path.clone(),
                io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short ep0 write: {written} of {} bytes", data.len()),
                ),
            ));
        }
        Ok(())
    }

    fn read_ep0_event(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        self.file
            .read(buffer)
            .map_err(|source| ep0_error("read", self.path.clone(), source))
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// One FunctionFS data endpoint.
pub trait FunctionFsEndpointIo {
    /// Endpoint descriptor represented by this handle.
    fn endpoint(&self) -> &FunctionFsEndpoint;

    /// Read host-to-device data from an OUT endpoint.
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError>;

    /// Write device-to-host data to an IN endpoint.
    fn write(&mut self, data: &[u8]) -> Result<usize, UsbError>;

    /// Flush pending device-to-host data.
    fn flush(&mut self) -> Result<(), UsbError> {
        Ok(())
    }

    /// Test/back-end downcast hook.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// In-memory endpoint with separate host-to-device and device-to-host queues.
#[derive(Debug)]
pub struct LoopbackEndpoint {
    endpoint: FunctionFsEndpoint,
    host_to_device: VecDeque<u8>,
    device_to_host: VecDeque<u8>,
}

impl LoopbackEndpoint {
    /// Empty loopback endpoint for a descriptor.
    pub fn new(endpoint: FunctionFsEndpoint) -> Self {
        Self {
            endpoint,
            host_to_device: VecDeque::new(),
            device_to_host: VecDeque::new(),
        }
    }

    /// Queue bytes as if the host had written to an OUT endpoint.
    pub fn push_host_data(&mut self, data: &[u8]) -> Result<(), UsbError> {
        let total = self.host_to_device.len() + data.len();
        if total > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "loopback OUT queue",
                limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
                requested: total,
            });
        }
        self.host_to_device.extend(data);
        Ok(())
    }

    /// Remove bytes the daemon wrote toward the host.
    pub fn take_device_data(&mut self) -> Vec<u8> {
        self.device_to_host.drain(..).collect()
    }

    /// Bytes waiting for the daemon to read.
    pub fn pending_host_bytes(&self) -> usize {
        self.host_to_device.len()
    }

    /// Bytes waiting for the host to read.
    pub fn pending_device_bytes(&self) -> usize {
        self.device_to_host.len()
    }
}

impl FunctionFsEndpointIo for LoopbackEndpoint {
    fn endpoint(&self) -> &FunctionFsEndpoint {
        &self.endpoint
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        let count = buffer.len().min(self.host_to_device.len());
        for slot in &mut buffer[..count] {
            *slot = self
                .host_to_device
                .pop_front()
                .expect("length was checked against the queue");
        }
        Ok(count)
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, UsbError> {
        if data.len() > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "loopback IN write",
                limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
                requested: data.len(),
            });
        }
        let total = self.device_to_host.len() + data.len();
        if total > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "loopback IN queue",
                limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
                requested: total,
            });
        }
        self.device_to_host.extend(data);
        Ok(data.len())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// File-backed data endpoint for an explicitly supplied mount.
#[derive(Debug)]
pub struct FileEndpointIo {
    endpoint: FunctionFsEndpoint,
    path: PathBuf,
    file: File,
}

impl FileEndpointIo {
    /// Open one `ep<N>` file read/write.
    pub fn open(endpoint: FunctionFsEndpoint, path: impl Into<PathBuf>) -> Result<Self, UsbError> {
        let path = path.into();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    UsbError::MissingFunctionFsEndpoint(path.clone())
                } else {
                    endpoint_error(endpoint.address, "open", source)
                }
            })?;
        Ok(Self {
            endpoint,
            path,
            file,
        })
    }

    /// Path backing this endpoint.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl FunctionFsEndpointIo for FileEndpointIo {
    fn endpoint(&self) -> &FunctionFsEndpoint {
        &self.endpoint
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        if buffer.len() > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "endpoint read buffer",
                limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
                requested: buffer.len(),
            });
        }
        self.file
            .read(buffer)
            .map_err(|source| endpoint_error(self.endpoint.address, "read", source))
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, UsbError> {
        if data.len() > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
            return Err(UsbError::FunctionFsBuffer {
                context: "endpoint write",
                limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
                requested: data.len(),
            });
        }
        let written = self
            .file
            .write(data)
            .map_err(|source| endpoint_error(self.endpoint.address, "write", source))?;
        if written != data.len() {
            return Err(endpoint_error(
                self.endpoint.address,
                "write",
                io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short endpoint write: {written} of {} bytes", data.len()),
                ),
            ));
        }
        Ok(written)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.file
            .flush()
            .map_err(|source| endpoint_error(self.endpoint.address, "flush", source))
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Opens endpoint files when FunctionFS enables the function.
pub trait FunctionFsEndpointOpener {
    /// Open endpoint `index` (one-based descriptor order).
    fn open(
        &mut self,
        index: u8,
        endpoint: &FunctionFsEndpoint,
    ) -> Result<Box<dyn FunctionFsEndpointIo>, UsbError>;
}

/// Creates in-memory loopback endpoints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopbackEndpointOpener;

impl FunctionFsEndpointOpener for LoopbackEndpointOpener {
    fn open(
        &mut self,
        _index: u8,
        endpoint: &FunctionFsEndpoint,
    ) -> Result<Box<dyn FunctionFsEndpointIo>, UsbError> {
        Ok(Box::new(LoopbackEndpoint::new(endpoint.clone())))
    }
}

/// Opens `ep<N>` files under an explicitly supplied mount.
#[derive(Clone, Debug)]
pub struct FileEndpointOpener {
    mount: FunctionFsMount,
}

impl FileEndpointOpener {
    /// Opener for one mount.
    pub fn new(mount: FunctionFsMount) -> Self {
        Self { mount }
    }
}

impl FunctionFsEndpointOpener for FileEndpointOpener {
    fn open(
        &mut self,
        index: u8,
        endpoint: &FunctionFsEndpoint,
    ) -> Result<Box<dyn FunctionFsEndpointIo>, UsbError> {
        let path = self.mount.endpoint_path(index)?;
        Ok(Box::new(FileEndpointIo::open(endpoint.clone(), path)?))
    }
}

/// USB direction for daemon read/write loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsDirection {
    /// Host-to-device (OUT endpoint; daemon reads).
    Out,
    /// Device-to-host (IN endpoint; daemon writes).
    In,
}

impl FunctionFsDirection {
    /// Stable lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }
}

/// One decoded `usb_ctrlrequest` received on `ep0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionFsControlRequest {
    /// `bmRequestType`.
    pub bm_request_type: u8,
    /// `bRequest`.
    pub b_request: u8,
    /// `wValue`.
    pub w_value: u16,
    /// `wIndex`.
    pub w_index: u16,
    /// `wLength`.
    pub w_length: u16,
}

impl FunctionFsControlRequest {
    /// Decode eight setup bytes.
    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        Self {
            bm_request_type: bytes[0],
            b_request: bytes[1],
            w_value: u16::from_le_bytes([bytes[2], bytes[3]]),
            w_index: u16::from_le_bytes([bytes[4], bytes[5]]),
            w_length: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }

    /// Encode the eight setup bytes.
    pub fn to_bytes(self) -> [u8; 8] {
        let [value_low, value_high] = self.w_value.to_le_bytes();
        let [index_low, index_high] = self.w_index.to_le_bytes();
        let [length_low, length_high] = self.w_length.to_le_bytes();
        [
            self.bm_request_type,
            self.b_request,
            value_low,
            value_high,
            index_low,
            index_high,
            length_low,
            length_high,
        ]
    }

    /// Data direction.
    pub fn direction(self) -> FunctionFsDirection {
        if self.bm_request_type & 0x80 != 0 {
            FunctionFsDirection::In
        } else {
            FunctionFsDirection::Out
        }
    }

    /// Request type bits (`0` standard, `1` class, `2` vendor).
    pub fn request_type(self) -> u8 {
        (self.bm_request_type >> 5) & 0x03
    }

    /// Recipient bits (`0` device, `1` interface, `2` endpoint).
    pub fn recipient(self) -> u8 {
        self.bm_request_type & 0x1f
    }

    /// `GET_DESCRIPTOR` descriptor type (`wValue` high byte).
    pub fn descriptor_type(self) -> u8 {
        (self.w_value >> 8) as u8
    }

    /// `GET_DESCRIPTOR` descriptor index (`wValue` low byte).
    pub fn descriptor_index(self) -> u8 {
        self.w_value as u8
    }

    /// Whether this is a standard request.
    pub fn is_standard(self) -> bool {
        self.request_type() == 0
    }
}

/// Result of one handled control request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionFsControlResponse {
    /// Device-to-host payload, already truncated to `wLength`.
    pub data: Vec<u8>,
    /// New configuration value for `SET_CONFIGURATION`.
    pub configured: Option<u8>,
}

impl FunctionFsControlResponse {
    /// An empty acknowledgement.
    pub fn ack() -> Self {
        Self::default()
    }
}

/// Handle a standard FunctionFS control request.
///
/// FunctionFS class/vendor requests are deliberately returned as typed
/// `unsupported-functionfs-control` errors: a vendor protocol handler is not
/// part of this crate yet. Standard `GET_DESCRIPTOR` (device, configuration,
/// string) and `SET_CONFIGURATION` are implemented, along with
/// `GET_CONFIGURATION`, `GET/SET_INTERFACE`, `GET_STATUS`, and
/// `SET/CLEAR_FEATURE`.
pub fn handle_control_request(
    config: &FunctionFsConfig,
    request: FunctionFsControlRequest,
    configured: bool,
    alternate_setting: u8,
) -> Result<FunctionFsControlResponse, UsbError> {
    if !request.is_standard() {
        return Err(unsupported_control(
            request,
            "only standard device/interface requests are modelled",
        ));
    }
    match request.b_request {
        0 => {
            // GET_STATUS
            if request.direction() != FunctionFsDirection::In {
                return Err(unsupported_control(request, "GET_STATUS must be IN"));
            }
            Ok(FunctionFsControlResponse {
                data: vec![0, 0],
                configured: None,
            })
        }
        1 | 3 => {
            // CLEAR_FEATURE / SET_FEATURE: acknowledge endpoint halt and
            // remote wakeup without changing modelled state.
            if request.direction() != FunctionFsDirection::Out {
                return Err(unsupported_control(request, "feature requests must be OUT"));
            }
            Ok(FunctionFsControlResponse::ack())
        }
        6 => {
            // GET_DESCRIPTOR
            if request.direction() != FunctionFsDirection::In {
                return Err(unsupported_control(request, "GET_DESCRIPTOR must be IN"));
            }
            let data = match request.descriptor_type() {
                0x01 => config.descriptors().device,
                0x02 => config.descriptors().configuration,
                0x03 => {
                    let strings = config.strings.descriptors();
                    let index = request.descriptor_index() as usize;
                    strings.get(index).cloned().ok_or_else(|| {
                        unsupported_control(
                            request,
                            format!("string descriptor index {index} is not declared"),
                        )
                    })?
                }
                other => {
                    return Err(unsupported_control(
                        request,
                        format!("descriptor type 0x{other:02x} is not modelled"),
                    ));
                }
            };
            if data.len() > MAX_FUNCTIONFS_EP0_WRITE_BYTES {
                return Err(UsbError::FunctionFsBuffer {
                    context: "control response",
                    limit: MAX_FUNCTIONFS_EP0_WRITE_BYTES,
                    requested: data.len(),
                });
            }
            let limit = request.w_length as usize;
            let mut data = data;
            data.truncate(limit.min(data.len()));
            Ok(FunctionFsControlResponse {
                data,
                configured: None,
            })
        }
        8 => {
            // GET_CONFIGURATION
            if request.direction() != FunctionFsDirection::In {
                return Err(unsupported_control(request, "GET_CONFIGURATION must be IN"));
            }
            Ok(FunctionFsControlResponse {
                data: vec![u8::from(configured)],
                configured: None,
            })
        }
        9 => {
            // SET_CONFIGURATION
            if request.direction() != FunctionFsDirection::Out {
                return Err(unsupported_control(
                    request,
                    "SET_CONFIGURATION must be OUT",
                ));
            }
            let value = request.w_value;
            if value > 1 {
                return Err(unsupported_control(
                    request,
                    format!("configuration value {value} is not modelled"),
                ));
            }
            Ok(FunctionFsControlResponse {
                data: Vec::new(),
                configured: Some(value as u8),
            })
        }
        10 => {
            // GET_INTERFACE
            if request.direction() != FunctionFsDirection::In {
                return Err(unsupported_control(request, "GET_INTERFACE must be IN"));
            }
            Ok(FunctionFsControlResponse {
                data: vec![alternate_setting],
                configured: None,
            })
        }
        11 => {
            // SET_INTERFACE
            if request.direction() != FunctionFsDirection::Out {
                return Err(unsupported_control(request, "SET_INTERFACE must be OUT"));
            }
            if request.w_index != 0 || request.w_value != 0 {
                return Err(unsupported_control(
                    request,
                    "only interface 0 alternate setting 0 is modelled",
                ));
            }
            Ok(FunctionFsControlResponse::ack())
        }
        _ => Err(unsupported_control(
            request,
            "standard request is not modelled",
        )),
    }
}

/// One decoded FunctionFS event type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsEventType {
    /// Function bound to a gadget configuration.
    Bind,
    /// Function unbound.
    Unbind,
    /// Function enabled by the UDC/host.
    Enable,
    /// Function disabled.
    Disable,
    /// Control request delivered on `ep0`.
    Setup,
    /// Bus suspend.
    Suspend,
    /// Bus resume.
    Resume,
}

impl FunctionFsEventType {
    /// Decode the kernel event byte.
    pub fn from_u8(value: u8) -> Result<Self, UsbError> {
        match value {
            0 => Ok(Self::Bind),
            1 => Ok(Self::Unbind),
            2 => Ok(Self::Enable),
            3 => Ok(Self::Disable),
            4 => Ok(Self::Setup),
            5 => Ok(Self::Suspend),
            6 => Ok(Self::Resume),
            other => Err(UsbError::InvalidFunctionFsState {
                reason: format!("unknown FunctionFS event type {other}"),
            }),
        }
    }

    /// Stable lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Unbind => "unbind",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Setup => "setup",
            Self::Suspend => "suspend",
            Self::Resume => "resume",
        }
    }
}

/// One decoded `usb_functionfs_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionFsEvent {
    /// Bind event.
    Bind,
    /// Unbind event.
    Unbind,
    /// Enable event.
    Enable,
    /// Disable event.
    Disable,
    /// Setup event with the decoded control request.
    Setup(FunctionFsControlRequest),
    /// Suspend event.
    Suspend,
    /// Resume event.
    Resume,
}

impl FunctionFsEvent {
    /// Event type.
    pub fn event_type(&self) -> FunctionFsEventType {
        match self {
            Self::Bind => FunctionFsEventType::Bind,
            Self::Unbind => FunctionFsEventType::Unbind,
            Self::Enable => FunctionFsEventType::Enable,
            Self::Disable => FunctionFsEventType::Disable,
            Self::Setup(_) => FunctionFsEventType::Setup,
            Self::Suspend => FunctionFsEventType::Suspend,
            Self::Resume => FunctionFsEventType::Resume,
        }
    }

    /// Encode a 12-byte `usb_functionfs_event`.
    pub fn encode(&self) -> [u8; FUNCTIONFS_EVENT_BYTES] {
        let mut bytes = [0u8; FUNCTIONFS_EVENT_BYTES];
        if let Self::Setup(request) = self {
            bytes[..8].copy_from_slice(&request.to_bytes());
        }
        bytes[8] = match self.event_type() {
            FunctionFsEventType::Bind => 0,
            FunctionFsEventType::Unbind => 1,
            FunctionFsEventType::Enable => 2,
            FunctionFsEventType::Disable => 3,
            FunctionFsEventType::Setup => 4,
            FunctionFsEventType::Suspend => 5,
            FunctionFsEventType::Resume => 6,
        };
        bytes
    }

    /// Decode a complete `usb_functionfs_event`.
    pub fn decode(bytes: &[u8]) -> Result<Self, UsbError> {
        if bytes.len() < FUNCTIONFS_EVENT_BYTES {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "FunctionFS events are {FUNCTIONFS_EVENT_BYTES} bytes, got {}",
                    bytes.len()
                ),
            });
        }
        let event_type = FunctionFsEventType::from_u8(bytes[8])?;
        Ok(match event_type {
            FunctionFsEventType::Bind => Self::Bind,
            FunctionFsEventType::Unbind => Self::Unbind,
            FunctionFsEventType::Enable => Self::Enable,
            FunctionFsEventType::Disable => Self::Disable,
            FunctionFsEventType::Setup => {
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&bytes[..8]);
                Self::Setup(FunctionFsControlRequest::from_bytes(setup))
            }
            FunctionFsEventType::Suspend => Self::Suspend,
            FunctionFsEventType::Resume => Self::Resume,
        })
    }
}

/// Result of one daemon event transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsEventOutcome {
    /// Event that was handled.
    pub event: FunctionFsEventType,
    /// Device-to-host payload written to `ep0`, if any.
    pub response: Option<Vec<u8>>,
    /// Function enabled after the event.
    pub enabled: bool,
    /// Configuration selected after the event.
    pub configured: bool,
    /// Descriptors written and function enabled.
    pub live: bool,
    /// Endpoint traffic has been observed after enable.
    pub running: bool,
    /// Device-level state after the event.
    pub state: FunctionFsDeviceState,
}

/// One endpoint transfer collected by a read loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsEndpointTransfer {
    /// USB endpoint address.
    pub address: u8,
    /// Transferred bytes.
    pub data: Vec<u8>,
}

/// Aggregate counters maintained by the daemon core.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsDaemonStats {
    /// Descriptor blob writes.
    pub descriptor_writes: u64,
    /// String-table writes.
    pub string_writes: u64,
    /// Bytes read from OUT endpoints.
    pub bytes_read: u64,
    /// Bytes written to IN endpoints.
    pub bytes_written: u64,
    /// Control requests handled.
    pub control_requests: u64,
}

#[derive(Clone, Debug, Default)]
struct DaemonState {
    ep0_written: bool,
    strings_written: bool,
    enabled: bool,
    configured: bool,
    alternate_setting: u8,
}

/// Transport-independent FunctionFS daemon core.
///
/// `ep0` and endpoints are injected. Endpoint files are opened only after an
/// enable event, matching FunctionFS's "ep# files appear after descriptors"
/// behavior. No method opens a mount by itself.
pub struct FunctionFsDaemonCore {
    config: FunctionFsConfig,
    mount: Option<FunctionFsMount>,
    descriptors: FunctionFsDescriptors,
    ep0: Box<dyn FunctionFsEp0Io>,
    opener: Box<dyn FunctionFsEndpointOpener>,
    endpoints: BTreeMap<u8, Box<dyn FunctionFsEndpointIo>>,
    state: DaemonState,
    device: FunctionFsStateMachine,
    stats: FunctionFsDaemonStats,
}

impl std::fmt::Debug for FunctionFsDaemonCore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FunctionFsDaemonCore")
            .field("instance", &self.config.instance)
            .field("mount", &self.mount)
            .field("endpoints", &self.config.endpoints.len())
            .field("state", &self.state)
            .field("stats", &self.stats)
            .finish()
    }
}

impl FunctionFsDaemonCore {
    /// Build a daemon core over injected `ep0` and endpoint opener.
    pub fn new(
        config: FunctionFsConfig,
        mount: Option<FunctionFsMount>,
        descriptors: FunctionFsDescriptors,
        ep0: Box<dyn FunctionFsEp0Io>,
        opener: Box<dyn FunctionFsEndpointOpener>,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        descriptors.validate()?;
        Ok(Self {
            config,
            mount,
            descriptors,
            ep0,
            opener,
            endpoints: BTreeMap::new(),
            state: DaemonState::default(),
            device: FunctionFsStateMachine::new(),
            stats: FunctionFsDaemonStats::default(),
        })
    }

    /// Function model.
    pub fn config(&self) -> &FunctionFsConfig {
        &self.config
    }

    /// Device-level lifecycle state machine snapshot.
    pub fn device_status(&self) -> FunctionFsDeviceStatus {
        self.device.status()
    }

    /// Current device-level state.
    pub fn device_state(&self) -> FunctionFsDeviceState {
        self.device.state()
    }

    /// Whether endpoint traffic has actually been observed.
    pub fn is_running(&self) -> bool {
        self.device.is_running()
    }

    /// Explicit mount, if this core writes to a filesystem path.
    pub fn mount(&self) -> Option<&FunctionFsMount> {
        self.mount.as_ref()
    }

    /// Descriptor set currently held by the core.
    pub fn descriptors(&self) -> &FunctionFsDescriptors {
        &self.descriptors
    }

    /// Counters.
    pub fn stats(&self) -> &FunctionFsDaemonStats {
        &self.stats
    }

    /// Whether `ep0` descriptors and strings were both accepted.
    pub fn descriptors_written(&self) -> bool {
        self.state.ep0_written && self.state.strings_written
    }

    /// Whether the function is enabled and endpoints may be used.
    pub fn is_enabled(&self) -> bool {
        self.state.enabled
    }

    /// Whether descriptors were accepted and the function is enabled.
    pub fn is_live(&self) -> bool {
        self.descriptors_written() && self.state.enabled
    }

    /// Snapshot used by [`FunctionFsStatus`].
    pub fn daemon_status(&self) -> FunctionFsDaemonStatus {
        let device = self.device.status();
        FunctionFsDaemonStatus {
            live: self.is_live(),
            state: device.state,
            running: device.running,
            udc_bound: device.udc_bound,
            endpoint_events: device.endpoint_events,
            ep0_written: self.state.ep0_written,
            strings_written: self.state.strings_written,
            enabled: self.state.enabled,
            configured: self.state.configured,
            open_endpoints: self.endpoints.len() as u8,
            descriptor_bytes: self
                .descriptors
                .descriptor_blob()
                .map_or(0, |blob| blob.len()),
            os_descriptors: self.descriptors.os_count() as u8,
            bytes_read: self.stats.bytes_read,
            bytes_written: self.stats.bytes_written,
            control_requests: self.stats.control_requests,
        }
    }

    /// Write the descriptor blob, then the string table, to `ep0`.
    ///
    /// The flags only become true after each individual write succeeds, so a
    /// failed string write never yields a live status.
    pub fn write_descriptors(
        &mut self,
        config: &FunctionFsConfig,
        descriptors: &FunctionFsDescriptors,
    ) -> Result<FunctionFsWriteReport, UsbError> {
        self.check_instance(config)?;
        if self.state.ep0_written || self.state.strings_written {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "FunctionFS {} descriptors were already written",
                    self.config.configfs_name()
                ),
            });
        }
        let expected = config.ep0_descriptors()?;
        if &expected != descriptors {
            return Err(UsbError::InvalidFunctionFsDescriptor {
                reason: "descriptor set does not match the FunctionFS config".to_owned(),
            });
        }
        descriptors.validate()?;
        let blob = descriptors.descriptor_blob()?;
        self.ep0.write_ep0(&blob)?;
        self.state.ep0_written = true;
        self.stats.descriptor_writes += 1;
        self.descriptors = descriptors.clone();
        self.ep0.write_ep0(descriptors.strings())?;
        self.state.strings_written = true;
        self.stats.string_writes += 1;
        self.device.bind_descriptors()?;
        // The daemon owns an `ep0` transport, so a successful descriptor and
        // string write proves the event stream is ready. This is deliberately
        // distinct from `enabled`/`running`.
        self.device.mark_ep0_ready()?;
        Ok(FunctionFsWriteReport {
            descriptor_bytes: blob.len(),
            string_bytes: descriptors.strings().len(),
        })
    }

    /// Handle one decoded event.
    pub fn handle_event(
        &mut self,
        config: &FunctionFsConfig,
        event: FunctionFsEvent,
    ) -> Result<FunctionFsEventOutcome, UsbError> {
        self.check_instance(config)?;
        match &event {
            FunctionFsEvent::Bind => {
                self.device.bind_udc()?;
            }
            FunctionFsEvent::Unbind => {
                self.close_endpoints();
                if self.device.descriptors_bound() {
                    self.device.record_unbind()?;
                }
            }
            FunctionFsEvent::Enable => {
                if !self.descriptors_written() {
                    return Err(UsbError::InvalidFunctionFsState {
                        reason: format!(
                            "FunctionFS {} cannot enable before ep0 descriptors and strings are written",
                            self.config.configfs_name()
                        ),
                    });
                }
                if !self.state.enabled {
                    self.open_endpoints()?;
                    self.state.enabled = true;
                    self.state.configured = false;
                    if let Err(error) = self.ep0.write_ep0(&[]) {
                        self.close_endpoints();
                        return Err(error);
                    }
                    // Enable is not "running": endpoint traffic must still be
                    // observed before the state machine leaves `enabled`.
                    if let Err(error) = self.device.enable() {
                        self.close_endpoints();
                        self.state.enabled = false;
                        return Err(error);
                    }
                }
            }
            FunctionFsEvent::Disable => {
                self.close_endpoints();
                if self.device.is_enabled() {
                    self.device.disable()?;
                }
            }
            FunctionFsEvent::Setup(request) => {
                if !self.state.enabled {
                    return Err(UsbError::InvalidFunctionFsState {
                        reason: format!(
                            "FunctionFS {} received a setup request while disabled",
                            self.config.configfs_name()
                        ),
                    });
                }
                self.stats.control_requests += 1;
                let response = handle_control_request(
                    &self.config,
                    *request,
                    self.state.configured,
                    self.state.alternate_setting,
                )?;
                if let Some(value) = response.configured {
                    self.state.configured = value != 0;
                }
                if request.direction() == FunctionFsDirection::In {
                    self.ep0.write_ep0(&response.data)?;
                } else {
                    self.ep0.write_ep0(&[])?;
                }
                return Ok(self.outcome(event.event_type(), Some(response.data)));
            }
            FunctionFsEvent::Suspend => {}
            FunctionFsEvent::Resume => {}
        }
        Ok(self.outcome(event.event_type(), None))
    }

    fn outcome(
        &self,
        event: FunctionFsEventType,
        response: Option<Vec<u8>>,
    ) -> FunctionFsEventOutcome {
        FunctionFsEventOutcome {
            event,
            response,
            enabled: self.state.enabled,
            configured: self.state.configured,
            live: self.is_live(),
            running: self.device.is_running(),
            state: self.device.state(),
        }
    }

    /// Read one event from `ep0` and handle it. Returns `None` when no event is
    /// currently pending.
    pub fn handle_next_event(
        &mut self,
        config: &FunctionFsConfig,
    ) -> Result<Option<FunctionFsEventOutcome>, UsbError> {
        let mut buffer = [0u8; FUNCTIONFS_EVENT_BYTES];
        let read = self.ep0.read_ep0_event(&mut buffer)?;
        if read == 0 {
            return Ok(None);
        }
        if read != FUNCTIONFS_EVENT_BYTES {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "ep0 event read returned {read} bytes, expected {FUNCTIONFS_EVENT_BYTES}"
                ),
            });
        }
        let event = FunctionFsEvent::decode(&buffer)?;
        self.handle_event(config, event).map(Some)
    }

    /// Access one open endpoint by USB address.
    pub fn endpoint_mut(&mut self, address: u8) -> Option<&mut (dyn FunctionFsEndpointIo + '_)> {
        if !self.state.enabled {
            return None;
        }
        self.endpoint_raw_mut(address)
    }

    /// Access one endpoint even when disabled (test/loopback back-end hook).
    pub fn endpoint_raw_mut<'a>(
        &'a mut self,
        address: u8,
    ) -> Option<&'a mut (dyn FunctionFsEndpointIo + 'a)> {
        match self.endpoints.get_mut(&address) {
            Some(endpoint) => Some(&mut **endpoint),
            None => None,
        }
    }

    /// Read up to `max_bytes` from one OUT endpoint.
    pub fn read_endpoint(&mut self, address: u8, max_bytes: usize) -> Result<Vec<u8>, UsbError> {
        self.require_enabled("read")?;
        check_transfer_len("endpoint read", max_bytes)?;
        let endpoint =
            self.endpoints
                .get_mut(&address)
                .ok_or_else(|| UsbError::InvalidFunctionFsState {
                    reason: format!("FunctionFS endpoint 0x{address:02x} is not open"),
                })?;
        if endpoint.endpoint().is_in() {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!("endpoint 0x{address:02x} is IN and cannot be read"),
            });
        }
        let mut buffer = vec![0u8; max_bytes];
        let read = endpoint.read(&mut buffer)?;
        buffer.truncate(read);
        self.stats.bytes_read += read as u64;
        if read > 0 {
            self.device.record_endpoint_event()?;
        }
        Ok(buffer)
    }

    /// Write one transfer to an IN endpoint.
    pub fn write_endpoint(&mut self, address: u8, data: &[u8]) -> Result<usize, UsbError> {
        self.require_enabled("write")?;
        check_transfer_len("endpoint write", data.len())?;
        let endpoint =
            self.endpoints
                .get_mut(&address)
                .ok_or_else(|| UsbError::InvalidFunctionFsState {
                    reason: format!("FunctionFS endpoint 0x{address:02x} is not open"),
                })?;
        if !endpoint.endpoint().is_in() {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!("endpoint 0x{address:02x} is OUT and cannot be written"),
            });
        }
        let written = endpoint.write(data)?;
        if written != data.len() {
            return Err(UsbError::FunctionFsEndpointIo {
                address,
                operation: "write",
                source: io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short write: {written} of {} bytes", data.len()),
                ),
            });
        }
        self.stats.bytes_written += written as u64;
        endpoint.flush()?;
        if written > 0 {
            self.device.record_endpoint_event()?;
        }
        Ok(written)
    }

    /// Read every open OUT endpoint once with a bounded buffer.
    pub fn pump_out(
        &mut self,
        max_bytes: usize,
    ) -> Result<Vec<FunctionFsEndpointTransfer>, UsbError> {
        self.require_enabled("read")?;
        check_transfer_len("endpoint read", max_bytes)?;
        let addresses: Vec<u8> = self
            .endpoints
            .iter()
            .filter(|(_, endpoint)| !endpoint.endpoint().is_in())
            .map(|(address, _)| *address)
            .collect();
        let mut transfers = Vec::new();
        for address in addresses {
            let data = self.read_endpoint(address, max_bytes)?;
            if !data.is_empty() {
                transfers.push(FunctionFsEndpointTransfer { address, data });
            }
        }
        Ok(transfers)
    }

    /// Write every IN transfer in order.
    pub fn pump_in(&mut self, transfers: &[FunctionFsEndpointTransfer]) -> Result<usize, UsbError> {
        let mut total = 0;
        for transfer in transfers {
            total += self.write_endpoint(transfer.address, &transfer.data)?;
        }
        Ok(total)
    }

    /// Close endpoints and stop reporting live readiness.
    pub fn shutdown(&mut self) {
        self.close_endpoints();
        if self.device.is_enabled() {
            let _ = self.device.disable();
        }
    }

    fn check_instance(&self, config: &FunctionFsConfig) -> Result<(), UsbError> {
        if config.instance != self.config.instance {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "daemon is bound to FunctionFS {} but received {}",
                    self.config.instance, config.instance
                ),
            });
        }
        Ok(())
    }

    fn require_enabled(&self, operation: &'static str) -> Result<(), UsbError> {
        if !self.state.enabled {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!("cannot {operation} endpoints before FunctionFS enable"),
            });
        }
        Ok(())
    }

    fn open_endpoints(&mut self) -> Result<(), UsbError> {
        let mut opened: BTreeMap<u8, Box<dyn FunctionFsEndpointIo>> = BTreeMap::new();
        for (index, endpoint) in self.config.endpoints.iter().enumerate() {
            let index = (index + 1) as u8;
            let io = self.opener.open(index, endpoint)?;
            if io.endpoint().address != endpoint.address {
                return Err(UsbError::InvalidFunctionFsState {
                    reason: format!(
                        "opened endpoint 0x{:02x} for descriptor 0x{:02x}",
                        io.endpoint().address,
                        endpoint.address
                    ),
                });
            }
            if opened.insert(endpoint.address, io).is_some() {
                return Err(UsbError::InvalidFunctionFsState {
                    reason: format!(
                        "endpoint address 0x{:02x} is declared twice",
                        endpoint.address
                    ),
                });
            }
        }
        self.endpoints = opened;
        Ok(())
    }

    fn close_endpoints(&mut self) {
        self.endpoints.clear();
        self.state.enabled = false;
        self.state.configured = false;
        self.state.alternate_setting = 0;
    }
}

/// In-memory FunctionFS daemon used by tests and dry runs.
///
/// It writes descriptors to [`MemoryEp0`], queues host events, and opens
/// [`LoopbackEndpoint`] files on enable. It never mounts a filesystem.
pub struct LoopbackFunctionFsDaemon {
    core: FunctionFsDaemonCore,
}

impl std::fmt::Debug for LoopbackFunctionFsDaemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoopbackFunctionFsDaemon")
            .field("core", &self.core)
            .finish()
    }
}

impl LoopbackFunctionFsDaemon {
    /// Build descriptors and an in-memory endpoint set for a config.
    pub fn new(config: FunctionFsConfig) -> Result<Self, UsbError> {
        let descriptors = config.ep0_descriptors()?;
        let core = FunctionFsDaemonCore::new(
            config,
            None,
            descriptors,
            Box::new(MemoryEp0::new()),
            Box::new(LoopbackEndpointOpener),
        )?;
        Ok(Self { core })
    }

    /// Underlying core.
    pub fn core(&self) -> &FunctionFsDaemonCore {
        &self.core
    }

    /// Underlying core, mutable.
    pub fn core_mut(&mut self) -> &mut FunctionFsDaemonCore {
        &mut self.core
    }

    /// Device-level state machine snapshot.
    pub fn device_status(&self) -> FunctionFsDeviceStatus {
        self.core.device_status()
    }

    /// Current device-level state.
    pub fn device_state(&self) -> FunctionFsDeviceState {
        self.core.device_state()
    }

    /// Whether endpoint traffic has been observed.
    pub fn is_running(&self) -> bool {
        self.core.is_running()
    }

    /// Queue one event for [`Self::handle_next_event`].
    pub fn push_event(&mut self, event: FunctionFsEvent) {
        self.memory_ep0_mut().push_event(event);
    }

    /// Handle the next queued event, if any.
    pub fn handle_next_event(&mut self) -> Result<Option<FunctionFsEventOutcome>, UsbError> {
        let config = self.core.config().clone();
        self.core.handle_next_event(&config)
    }

    /// Complete `ep0` writes recorded so far.
    pub fn ep0_writes(&self) -> &[Vec<u8>] {
        self.memory_ep0().writes()
    }

    /// Take all recorded `ep0` writes.
    pub fn take_ep0_writes(&mut self) -> Vec<Vec<u8>> {
        self.memory_ep0_mut().take_writes()
    }

    /// Queue bytes as if the host wrote to endpoint `address`.
    pub fn host_write(&mut self, address: u8, data: &[u8]) -> Result<(), UsbError> {
        self.loopback_endpoint_mut(address)?.push_host_data(data)
    }

    /// Remove bytes the daemon wrote toward the host on `address`.
    pub fn host_read(&mut self, address: u8) -> Result<Vec<u8>, UsbError> {
        Ok(self.loopback_endpoint_mut(address)?.take_device_data())
    }

    /// Echo all pending OUT data to the matching IN endpoint (`0x80 | number`).
    pub fn loopback_once(
        &mut self,
        max_bytes: usize,
    ) -> Result<Vec<FunctionFsEndpointTransfer>, UsbError> {
        let transfers = self.core.pump_out(max_bytes)?;
        for transfer in &transfers {
            let in_address = 0x80 | (transfer.address & 0x0f);
            if self.core.endpoint_raw_mut(in_address).is_some() {
                self.core.write_endpoint(in_address, &transfer.data)?;
            }
        }
        Ok(transfers)
    }

    fn memory_ep0(&self) -> &MemoryEp0 {
        self.core
            .ep0_ref()
            .as_any_ref()
            .downcast_ref::<MemoryEp0>()
            .expect("LoopbackFunctionFsDaemon always uses MemoryEp0")
    }

    fn memory_ep0_mut(&mut self) -> &mut MemoryEp0 {
        self.core
            .ep0_mut()
            .as_any_mut()
            .downcast_mut::<MemoryEp0>()
            .expect("LoopbackFunctionFsDaemon always uses MemoryEp0")
    }

    fn loopback_endpoint_mut(&mut self, address: u8) -> Result<&mut LoopbackEndpoint, UsbError> {
        let endpoint = self.core.endpoint_raw_mut(address).ok_or_else(|| {
            UsbError::InvalidFunctionFsState {
                reason: format!("FunctionFS endpoint 0x{address:02x} is not open"),
            }
        })?;
        endpoint
            .as_any_mut()
            .downcast_mut::<LoopbackEndpoint>()
            .ok_or_else(|| UsbError::InvalidFunctionFsState {
                reason: format!("FunctionFS endpoint 0x{address:02x} is not a loopback endpoint"),
            })
    }
}

impl FunctionFsDaemon for LoopbackFunctionFsDaemon {
    fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus {
        self.core.status(config)
    }

    fn write_descriptors(
        &mut self,
        config: &FunctionFsConfig,
        descriptors: &FunctionFsDescriptors,
    ) -> Result<FunctionFsWriteReport, UsbError> {
        self.core.write_descriptors(config, descriptors)
    }

    fn handle_event(
        &mut self,
        config: &FunctionFsConfig,
        event: FunctionFsEvent,
    ) -> Result<FunctionFsEventOutcome, UsbError> {
        self.core.handle_event(config, event)
    }

    fn endpoint_mut(&mut self, address: u8) -> Option<&mut dyn FunctionFsEndpointIo> {
        self.core.endpoint_mut(address)
    }

    fn shutdown(&mut self) {
        self.core.shutdown();
    }
}

/// Alias that makes the test/fake role explicit.
pub type FakeFunctionFsDaemon = LoopbackFunctionFsDaemon;

/// File-backed FunctionFS daemon for an explicitly supplied mount.
///
/// Construction validates the mount and opens `ep0`, but data endpoint files
/// are opened lazily on enable because FunctionFS creates them only after the
/// descriptor blob is accepted.
pub struct FileFunctionFsDaemon {
    core: FunctionFsDaemonCore,
}

impl std::fmt::Debug for FileFunctionFsDaemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileFunctionFsDaemon")
            .field("core", &self.core)
            .finish()
    }
}

impl FileFunctionFsDaemon {
    /// Open `ep0` under an explicit mount and prepare descriptors.
    ///
    /// The caller is responsible for mounting FunctionFS and creating the
    /// ConfigFS function/link; this method only uses ordinary file I/O.
    pub fn open(config: FunctionFsConfig, mount: FunctionFsMount) -> Result<Self, UsbError> {
        config.validate()?;
        mount.validate()?;
        let descriptors = config.ep0_descriptors()?;
        let ep0 = FileEp0Io::open(mount.ep0_path())?;
        let core = FunctionFsDaemonCore::new(
            config,
            Some(mount.clone()),
            descriptors,
            Box::new(ep0),
            Box::new(FileEndpointOpener::new(mount)),
        )?;
        Ok(Self { core })
    }

    /// Underlying core.
    pub fn core(&self) -> &FunctionFsDaemonCore {
        &self.core
    }

    /// Underlying core, mutable.
    pub fn core_mut(&mut self) -> &mut FunctionFsDaemonCore {
        &mut self.core
    }

    /// Device-level state machine snapshot.
    pub fn device_status(&self) -> FunctionFsDeviceStatus {
        self.core.device_status()
    }

    /// Current device-level state.
    pub fn device_state(&self) -> FunctionFsDeviceState {
        self.core.device_state()
    }

    /// Whether endpoint traffic has been observed.
    pub fn is_running(&self) -> bool {
        self.core.is_running()
    }
}

impl FunctionFsDaemon for FileFunctionFsDaemon {
    fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus {
        self.core.status(config)
    }

    fn write_descriptors(
        &mut self,
        config: &FunctionFsConfig,
        descriptors: &FunctionFsDescriptors,
    ) -> Result<FunctionFsWriteReport, UsbError> {
        self.core.write_descriptors(config, descriptors)
    }

    fn handle_event(
        &mut self,
        config: &FunctionFsConfig,
        event: FunctionFsEvent,
    ) -> Result<FunctionFsEventOutcome, UsbError> {
        self.core.handle_event(config, event)
    }

    fn endpoint_mut(&mut self, address: u8) -> Option<&mut dyn FunctionFsEndpointIo> {
        self.core.endpoint_mut(address)
    }

    fn shutdown(&mut self) {
        self.core.shutdown();
    }
}

impl FunctionFsDaemonCore {
    /// Status snapshot for a caller-supplied config.
    pub fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus {
        FunctionFsStatus::from_daemon(config, self.daemon_status())
    }

    /// Mutable `ep0` transport (test/back-end usage).
    pub fn ep0_mut(&mut self) -> &mut dyn FunctionFsEp0Io {
        self.ep0.as_mut()
    }

    /// Shared `ep0` transport.
    pub fn ep0_ref(&self) -> &dyn FunctionFsEp0Io {
        self.ep0.as_ref()
    }
}

fn encode_strings(config: &FunctionFsConfig) -> Result<Vec<u8>, UsbError> {
    let values = [
        &config.strings.manufacturer,
        &config.strings.product,
        &config.strings.serial_number,
        &config.strings.configuration,
    ];
    let mut body = Vec::new();
    for value in values {
        if value.len() > crate::functionfs::MAX_FUNCTIONFS_STRING_BYTES {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.strings",
                reason: format!(
                    "string must be at most {} bytes",
                    crate::functionfs::MAX_FUNCTIONFS_STRING_BYTES
                ),
            });
        }
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    let total = 16 + 2 + body.len();
    if total > MAX_FUNCTIONFS_EP0_WRITE_BYTES {
        return Err(UsbError::FunctionFsBuffer {
            context: "string table",
            limit: MAX_FUNCTIONFS_EP0_WRITE_BYTES,
            requested: total,
        });
    }
    let mut strings = Vec::with_capacity(total);
    strings.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    strings.extend_from_slice(&(total as u32).to_le_bytes());
    strings.extend_from_slice(&(values.len() as u32).to_le_bytes());
    strings.extend_from_slice(&1u32.to_le_bytes());
    strings.extend_from_slice(&0x0409u16.to_le_bytes());
    strings.extend_from_slice(&body);
    Ok(strings)
}

fn count_descriptors(bytes: &[u8]) -> Result<u32, UsbError> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut offset = 0usize;
    let mut count = 0u32;
    while offset < bytes.len() {
        if offset + 2 > bytes.len() {
            return Err(invalid_descriptor("descriptor header is truncated"));
        }
        let length = bytes[offset] as usize;
        if length < 2 || offset + length > bytes.len() {
            return Err(invalid_descriptor(format!(
                "descriptor at offset {offset} has invalid bLength {length}"
            )));
        }
        count += 1;
        offset += length;
    }
    Ok(count)
}

/// Clamp endpoint packet sizes to full-speed USB limits while preserving the
/// configured high-speed values. FunctionFS daemons normally advertise the
/// same endpoint addresses at both speeds with speed-appropriate packets.
fn full_speed_function_descriptors(high_speed: &[u8]) -> Result<Vec<u8>, UsbError> {
    let mut bytes = high_speed.to_vec();
    let mut offset = 0usize;
    while offset + 2 <= bytes.len() {
        let length = bytes[offset] as usize;
        if length < 2 || offset + length > bytes.len() {
            return Err(invalid_descriptor(format!(
                "descriptor at offset {offset} has invalid bLength {length}"
            )));
        }
        if bytes[offset + 1] == 0x05 && length >= 7 {
            let transfer = bytes[offset + 3] & 0x03;
            let fs_max = match transfer {
                1 => 1023, // isochronous
                2 | 3 => 64,
                _ => 64,
            };
            let current = u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]);
            let clamped = current.min(fs_max);
            bytes[offset + 4..offset + 6].copy_from_slice(&clamped.to_le_bytes());
        }
        offset += length;
    }
    Ok(bytes)
}

fn descriptor_set_len(
    bytes: &[u8],
    offset: usize,
    count: u32,
    label: &str,
) -> Result<usize, UsbError> {
    let mut cursor = offset;
    for index in 0..count {
        if cursor + 2 > bytes.len() {
            return Err(invalid_descriptor(format!(
                "{label} descriptor {index} is truncated"
            )));
        }
        let length = bytes[cursor] as usize;
        if length < 2 {
            return Err(invalid_descriptor(format!(
                "{label} descriptor {index} has bLength {length}"
            )));
        }
        if cursor + length > bytes.len() {
            return Err(invalid_descriptor(format!(
                "{label} descriptor {index} extends past the blob"
            )));
        }
        let descriptor_type = bytes[cursor + 1];
        match descriptor_type {
            0x01 | 0x02 | 0x03 => {
                return Err(invalid_descriptor(format!(
                    "{label} set contains reserved gadget descriptor type 0x{descriptor_type:02x}"
                )));
            }
            0x04 if length != 9 => {
                return Err(invalid_descriptor(format!(
                    "{label} interface descriptor has bLength {length}, expected 9"
                )));
            }
            0x05 => {
                if length != 7 && length != 9 {
                    return Err(invalid_descriptor(format!(
                        "{label} endpoint descriptor has bLength {length}, expected 7 or 9"
                    )));
                }
                let address = bytes[cursor + 2];
                if address & 0x70 != 0 || address & 0x0f == 0 {
                    return Err(invalid_descriptor(format!(
                        "{label} endpoint address 0x{address:02x} is invalid"
                    )));
                }
            }
            _ => {}
        }
        cursor += length;
    }
    Ok(cursor - offset)
}

fn validate_os_descriptors(bytes: &[u8], count: u32) -> Result<(), UsbError> {
    let mut offset = 0usize;
    for index in 0..count {
        if offset + 11 > bytes.len() {
            return Err(invalid_descriptor(format!(
                "OS descriptor {index} header is truncated"
            )));
        }
        let dw_length = read_u32(bytes, offset + 1)? as usize;
        if dw_length < 11 || offset + dw_length > bytes.len() {
            return Err(invalid_descriptor(format!(
                "OS descriptor {index} length {dw_length} is invalid"
            )));
        }
        let bcd_version = read_u16(bytes, offset + 5)?;
        if bcd_version != 0x0100 && bcd_version != 0x0001 {
            return Err(invalid_descriptor(format!(
                "OS descriptor {index} bcdVersion 0x{bcd_version:04x} is unsupported"
            )));
        }
        let w_index = read_u16(bytes, offset + 7)?;
        let descriptor = &bytes[offset..offset + dw_length];
        match w_index {
            4 => {
                let b_count = descriptor[9];
                let reserved = descriptor[10];
                if reserved != 0 {
                    return Err(invalid_descriptor(format!(
                        "OS descriptor {index} reserved byte is non-zero"
                    )));
                }
                let expected = 11 + b_count as usize * 24;
                if expected != dw_length {
                    return Err(invalid_descriptor(format!(
                        "OS descriptor {index} compatibility length mismatch"
                    )));
                }
            }
            5 => {
                let property_count = read_u16(descriptor, 9)? as usize;
                let mut cursor = 11usize;
                for property in 0..property_count {
                    if cursor + 10 > descriptor.len() {
                        return Err(invalid_descriptor(format!(
                            "OS descriptor {index} property {property} header is truncated"
                        )));
                    }
                    let size = read_u32(descriptor, cursor)? as usize;
                    if size < 14 || cursor + size > descriptor.len() {
                        return Err(invalid_descriptor(format!(
                            "OS descriptor {index} property {property} size {size} is invalid"
                        )));
                    }
                    let name_len = read_u16(descriptor, cursor + 8)? as usize;
                    let data_len_offset = cursor + 10 + name_len;
                    if name_len == 0 || name_len % 2 != 0 || data_len_offset + 4 > cursor + size {
                        return Err(invalid_descriptor(format!(
                            "OS descriptor {index} property {property} name is invalid"
                        )));
                    }
                    let data_len = read_u32(descriptor, data_len_offset)? as usize;
                    if data_len_offset + 4 + data_len != cursor + size {
                        return Err(invalid_descriptor(format!(
                            "OS descriptor {index} property {property} data length mismatch"
                        )));
                    }
                    cursor += size;
                }
                if cursor != descriptor.len() {
                    return Err(invalid_descriptor(format!(
                        "OS descriptor {index} has trailing property bytes"
                    )));
                }
            }
            other => {
                return Err(invalid_descriptor(format!(
                    "OS descriptor {index} wIndex {other} is unsupported"
                )));
            }
        }
        offset += dw_length;
    }
    if offset != bytes.len() {
        return Err(invalid_descriptor("OS descriptor bytes have trailing data"));
    }
    Ok(())
}

fn unsupported_control(request: FunctionFsControlRequest, reason: impl Into<String>) -> UsbError {
    UsbError::UnsupportedFunctionFsControl {
        bm_request_type: request.bm_request_type,
        b_request: request.b_request,
        reason: reason.into(),
    }
}

fn check_transfer_len(context: &'static str, requested: usize) -> Result<(), UsbError> {
    if requested == 0 || requested > MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES {
        return Err(UsbError::FunctionFsBuffer {
            context,
            limit: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
            requested,
        });
    }
    Ok(())
}

fn ep0_error(operation: &'static str, path: PathBuf, source: io::Error) -> UsbError {
    UsbError::FunctionFsEp0Io {
        operation,
        path,
        source,
    }
}

fn endpoint_error(address: u8, operation: &'static str, source: io::Error) -> UsbError {
    UsbError::FunctionFsEndpointIo {
        address,
        operation,
        source,
    }
}

fn invalid_descriptor(reason: impl Into<String>) -> UsbError {
    UsbError::InvalidFunctionFsDescriptor {
        reason: reason.into(),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, UsbError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid_descriptor("u16 field is truncated"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, UsbError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_descriptor("u32 field is truncated"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
