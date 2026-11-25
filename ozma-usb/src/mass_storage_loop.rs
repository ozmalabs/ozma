// SPDX-License-Identifier: AGPL-3.0-only
//! Bulk-only transport event loop for [`MassStorageDevice`].
//!
//! The SCSI model in [`crate::mass_storage`] deliberately performs no USB
//! transfer. This module connects it to injectable bulk endpoints:
//!
//! ```text
//! bulk OUT (CBW)  -> MassStorageLoop -> MassStorageDevice::execute_cbw
//! bulk OUT (data) ->                   |
//! bulk IN (data)  <-                   |
//! bulk IN (CSW)   <-                   |
//! ```
//!
//! Every transfer is bounded. A transport reports a stalled endpoint as
//! [`UsbError::MassStorageEndpointTimeout`]; the loop records it and propagates
//! the typed failure instead of retrying forever. The default tests use
//! [`MemoryBulkTransport`] only. [`FunctionFsBulkTransport`] adapts an enabled
//! [`FunctionFsDaemonCore`] but is exercised in tests with loopback endpoints;
//! no default test mounts FunctionFS or opens a real UDC endpoint.

use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::functionfs::FunctionFsEndpoint;
use crate::functionfs_daemon::{
    FileEndpointIo, FunctionFsDaemonCore, FunctionFsEndpointIo, PollingFileEndpointIo,
    MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
};
use crate::mass_storage::{
    MassStorageDevice, ScsiCommandBlock, ScsiStatus, SCSI_CBW_BYTES, SCSI_CSW_BYTES,
    SCSI_MAX_TRANSFER_BYTES, SCSI_SECTOR_BYTES,
};

/// Default per-transfer endpoint deadline.
pub const MASS_STORAGE_DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Largest accepted endpoint deadline.
pub const MASS_STORAGE_MAX_TIMEOUT_MS: u64 = 60_000;

/// A boxed, sendable bulk transport used by the node runtime.
pub type BoxedMassStorageTransport = Box<dyn MassStorageBulkTransport + Send>;

/// Bounds and deadlines for one mass-storage event loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStorageLoopConfig {
    /// Deadline passed to each transport read/write/flush.
    pub transfer_timeout_ms: u64,
    /// Largest accepted CBW data-phase length (512..=1 MiB, sector aligned).
    pub max_transfer_bytes: usize,
    /// Optional command limit for [`MassStorageLoop::run`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_commands: Option<u64>,
}

impl Default for MassStorageLoopConfig {
    fn default() -> Self {
        Self {
            transfer_timeout_ms: MASS_STORAGE_DEFAULT_TIMEOUT_MS,
            max_transfer_bytes: SCSI_MAX_TRANSFER_BYTES,
            max_commands: None,
        }
    }
}

impl MassStorageLoopConfig {
    /// Validate the bounds before any endpoint is touched.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.transfer_timeout_ms == 0 || self.transfer_timeout_ms > MASS_STORAGE_MAX_TIMEOUT_MS {
            return Err(UsbError::InvalidField {
                field: "mass-storage.transfer_timeout_ms",
                reason: format!(
                    "timeout must be 1..={MASS_STORAGE_MAX_TIMEOUT_MS} ms, got {}",
                    self.transfer_timeout_ms
                ),
            });
        }
        if self.max_transfer_bytes < SCSI_SECTOR_BYTES
            || self.max_transfer_bytes > SCSI_MAX_TRANSFER_BYTES
            || self.max_transfer_bytes % SCSI_SECTOR_BYTES != 0
        {
            return Err(UsbError::InvalidField {
                field: "mass-storage.max_transfer_bytes",
                reason: format!(
                    "bound must be a multiple of {SCSI_SECTOR_BYTES} in \
                     {SCSI_SECTOR_BYTES}..={SCSI_MAX_TRANSFER_BYTES}, got {}",
                    self.max_transfer_bytes
                ),
            });
        }
        if matches!(self.max_commands, Some(0)) {
            return Err(UsbError::InvalidField {
                field: "mass-storage.max_commands",
                reason: "a command limit must be greater than zero".to_owned(),
            });
        }
        Ok(())
    }
}

/// Injectable bulk endpoint pair driven by [`MassStorageLoop`].
///
/// Implementations must:
///
/// * return at most `buffer.len()` bytes from [`Self::read_out`];
/// * return at most `data.len()` bytes from [`Self::write_in`];
/// * translate a stalled endpoint into [`UsbError::MassStorageEndpointTimeout`]
///   using the supplied `timeout_ms`, rather than blocking forever.
///
/// The loop assembles short CBWs/data phases, so a transport may return a short
/// read or write. A zero-byte transfer is treated as a transport failure.
pub trait MassStorageBulkTransport {
    /// Read host-to-device bytes from the bulk OUT endpoint.
    fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError>;

    /// Write device-to-host bytes to the bulk IN endpoint.
    fn write_in(&mut self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError>;

    /// Flush pending device-to-host bytes.
    fn flush_in(&mut self, timeout_ms: u64) -> Result<(), UsbError> {
        let _ = timeout_ms;
        Ok(())
    }
}

impl<T: MassStorageBulkTransport + ?Sized> MassStorageBulkTransport for Box<T> {
    fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError> {
        (**self).read_out(buffer, timeout_ms)
    }

    fn write_in(&mut self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError> {
        (**self).write_in(data, timeout_ms)
    }

    fn flush_in(&mut self, timeout_ms: u64) -> Result<(), UsbError> {
        (**self).flush_in(timeout_ms)
    }
}

/// A transport that has no endpoint attached (configured export, no host).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleBulkTransport;

impl MassStorageBulkTransport for IdleBulkTransport {
    fn read_out(&mut self, _buffer: &mut [u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        Err(UsbError::MassStorageNotAttached)
    }

    fn write_in(&mut self, _data: &[u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        Err(UsbError::MassStorageNotAttached)
    }

    fn flush_in(&mut self, _timeout_ms: u64) -> Result<(), UsbError> {
        Err(UsbError::MassStorageNotAttached)
    }
}

/// A fake transport failure used by [`MemoryBulkTransport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkTransportFault {
    /// The endpoint deadline expires.
    Timeout,
    /// The endpoint returns a generic I/O failure.
    Io,
}

/// In-memory bulk transport with separate host-to-device and device-to-host
/// queues. It can return short transfers and inject a timeout or I/O fault.
#[derive(Debug)]
pub struct MemoryBulkTransport {
    host_to_device: VecDeque<u8>,
    device_to_host: Vec<u8>,
    read_fault: Option<BulkTransportFault>,
    write_fault: Option<BulkTransportFault>,
    flush_fault: Option<BulkTransportFault>,
    max_read_chunk: usize,
    max_write_chunk: usize,
    max_queue_bytes: usize,
    reads: u64,
    writes: u64,
    last_timeout_ms: Option<u64>,
}

impl Default for MemoryBulkTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBulkTransport {
    /// An empty transport with full-size transfers.
    pub fn new() -> Self {
        Self {
            host_to_device: VecDeque::new(),
            device_to_host: Vec::new(),
            read_fault: None,
            write_fault: None,
            flush_fault: None,
            max_read_chunk: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
            max_write_chunk: MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
            max_queue_bytes: SCSI_MAX_TRANSFER_BYTES + SCSI_CBW_BYTES + SCSI_CSW_BYTES,
            reads: 0,
            writes: 0,
            last_timeout_ms: None,
        }
    }

    /// Limit individual read/write calls to exercise short-transfer assembly.
    pub fn with_chunks(mut self, read_chunk: usize, write_chunk: usize) -> Self {
        self.max_read_chunk = read_chunk.max(1);
        self.max_write_chunk = write_chunk.max(1);
        self
    }

    /// Override the queued-byte ceiling.
    pub fn with_max_queue(mut self, max_queue_bytes: usize) -> Self {
        self.max_queue_bytes = max_queue_bytes.max(1);
        self
    }

    /// Queue bytes as if the host had written them to the OUT endpoint.
    pub fn push_host_data(&mut self, data: &[u8]) -> Result<(), UsbError> {
        let total = self.host_to_device.len() + data.len();
        if total > self.max_queue_bytes {
            return Err(UsbError::MassStorageBuffer {
                context: "fake OUT queue",
                limit: self.max_queue_bytes,
                requested: total,
            });
        }
        self.host_to_device.extend(data);
        Ok(())
    }

    /// Remove bytes the loop wrote toward the host.
    pub fn take_device_data(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.device_to_host)
    }

    /// Bytes waiting for the loop to read.
    pub fn pending_host_bytes(&self) -> usize {
        self.host_to_device.len()
    }

    /// Bytes waiting for the host to read.
    pub fn pending_device_bytes(&self) -> usize {
        self.device_to_host.len()
    }

    /// Number of transport read calls.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// Number of transport write calls.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Last timeout value supplied by the loop.
    pub fn last_timeout_ms(&self) -> Option<u64> {
        self.last_timeout_ms
    }

    /// Make the next (and all following until cleared) reads fail.
    pub fn set_read_fault(&mut self, fault: Option<BulkTransportFault>) {
        self.read_fault = fault;
    }

    /// Make the next (and all following until cleared) writes fail.
    pub fn set_write_fault(&mut self, fault: Option<BulkTransportFault>) {
        self.write_fault = fault;
    }

    /// Make the next (and all following until cleared) flushes fail.
    pub fn set_flush_fault(&mut self, fault: Option<BulkTransportFault>) {
        self.flush_fault = fault;
    }
}

impl MassStorageBulkTransport for MemoryBulkTransport {
    fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError> {
        self.last_timeout_ms = Some(timeout_ms);
        self.reads = self.reads.saturating_add(1);
        if let Some(fault) = self.read_fault {
            return Err(fault_error("read", timeout_ms, fault));
        }
        if self.host_to_device.is_empty() {
            return Err(UsbError::MassStorageEndpointTimeout {
                operation: "read",
                timeout_ms,
            });
        }
        let count = buffer
            .len()
            .min(self.max_read_chunk)
            .min(self.host_to_device.len());
        for slot in &mut buffer[..count] {
            *slot = self
                .host_to_device
                .pop_front()
                .expect("length was checked against the queue");
        }
        Ok(count)
    }

    fn write_in(&mut self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError> {
        self.last_timeout_ms = Some(timeout_ms);
        self.writes = self.writes.saturating_add(1);
        if let Some(fault) = self.write_fault {
            return Err(fault_error("write", timeout_ms, fault));
        }
        let count = data.len().min(self.max_write_chunk);
        let total = self.device_to_host.len() + count;
        if total > self.max_queue_bytes {
            return Err(UsbError::MassStorageBuffer {
                context: "fake IN queue",
                limit: self.max_queue_bytes,
                requested: total,
            });
        }
        self.device_to_host.extend_from_slice(&data[..count]);
        Ok(count)
    }

    fn flush_in(&mut self, timeout_ms: u64) -> Result<(), UsbError> {
        self.last_timeout_ms = Some(timeout_ms);
        if let Some(fault) = self.flush_fault {
            return Err(fault_error("flush", timeout_ms, fault));
        }
        Ok(())
    }
}

fn fault_error(operation: &'static str, timeout_ms: u64, fault: BulkTransportFault) -> UsbError {
    match fault {
        BulkTransportFault::Timeout => UsbError::MassStorageEndpointTimeout {
            operation,
            timeout_ms,
        },
        BulkTransportFault::Io => UsbError::MassStorageTransport {
            operation,
            reason: "injected endpoint I/O failure".to_owned(),
        },
    }
}

/// Counters for one mass-storage event loop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStorageLoopStats {
    /// Fully processed commands (including SCSI failures with a CSW).
    pub commands: u64,
    /// Command block wrappers read from the OUT endpoint.
    pub cbws: u64,
    /// Bytes read for OUT data phases.
    pub data_out_bytes: u64,
    /// Bytes written for IN data phases.
    pub data_in_bytes: u64,
    /// Command status wrappers written to the IN endpoint.
    pub csws: u64,
    /// `READ(10)` commands.
    pub read_commands: u64,
    /// `WRITE(10)` commands.
    pub write_commands: u64,
    /// `SYNCHRONIZE CACHE(10)` commands.
    pub flush_commands: u64,
    /// Commands that returned a SCSI check condition.
    pub check_conditions: u64,
    /// Commands that returned a SCSI phase error.
    pub phase_errors: u64,
    /// Transport failures (excluding endpoint timeouts).
    pub transport_errors: u64,
    /// Endpoint deadline failures.
    pub timeout_errors: u64,
    /// Transfers rejected by the configured loop bound.
    pub bound_errors: u64,
}

/// Result of one successful command cycle (SCSI failures are still cycles).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStorageCommandOutcome {
    /// CBW tag echoed in the CSW.
    pub tag: u32,
    /// SCSI operation code.
    pub opcode: u8,
    /// CSW status.
    pub status: ScsiStatus,
    /// CSW residue.
    pub residue: u32,
    /// Bytes accepted for an OUT data phase.
    pub data_out_bytes: usize,
    /// Bytes emitted for an IN data phase.
    pub data_in_bytes: usize,
}

/// A bounded SCSI event loop over one injectable bulk transport.
pub struct MassStorageLoop<T: MassStorageBulkTransport> {
    device: MassStorageDevice,
    transport: T,
    config: MassStorageLoopConfig,
    stats: MassStorageLoopStats,
    running: bool,
}

impl<T: MassStorageBulkTransport> std::fmt::Debug for MassStorageLoop<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MassStorageLoop")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .field("running", &self.running)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl<T: MassStorageBulkTransport> MassStorageLoop<T> {
    /// Build a loop after validating its bounds.
    pub fn new(
        device: MassStorageDevice,
        transport: T,
        config: MassStorageLoopConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        Ok(Self {
            device,
            transport,
            config,
            stats: MassStorageLoopStats::default(),
            running: false,
        })
    }

    /// Immutable SCSI device.
    pub fn device(&self) -> &MassStorageDevice {
        &self.device
    }

    /// Mutable SCSI device (status/commit helpers).
    pub fn device_mut(&mut self) -> &mut MassStorageDevice {
        &mut self.device
    }

    /// Inspect the transport (for fakes/counters).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Mutate the transport (for fakes/counters).
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Configured bounds.
    pub fn config(&self) -> MassStorageLoopConfig {
        self.config
    }

    /// Counters.
    pub fn stats(&self) -> &MassStorageLoopStats {
        &self.stats
    }

    /// Replace the counters, for example when a supervised restart rebuilds
    /// the device/transport but must keep the export's visible history.
    ///
    /// This does not change `running`: a rebuilt loop still reports
    /// `running: false` until a real command cycle completes.
    pub fn restore_stats(&mut self, stats: MassStorageLoopStats) {
        self.stats = stats;
    }

    /// Whether at least one command cycle completed after a transport attach.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Consume the loop and return the SCSI device.
    pub fn into_device(self) -> MassStorageDevice {
        self.device
    }

    /// Consume the loop and return both the device and its transport.
    ///
    /// A supervisor uses this to rebuild the device (for example to reload a
    /// persisted overlay) without discarding a transport it still owns.
    pub fn into_parts(self) -> (MassStorageDevice, T) {
        (self.device, self.transport)
    }

    /// Read one CBW, execute it, and write the IN data and CSW.
    ///
    /// Malformed CBWs and transport failures return a typed error. SCSI command
    /// failures are encoded in the returned [`MassStorageCommandOutcome`] after
    /// a CSW is written, so the host sees a normal check-condition completion.
    pub fn step(&mut self) -> Result<MassStorageCommandOutcome, UsbError> {
        let mut cbw = [0u8; SCSI_CBW_BYTES];
        self.read_exact_out(&mut cbw, "CBW", "CBW")?;
        self.stats.cbws = self.stats.cbws.saturating_add(1);

        let command = ScsiCommandBlock::parse(&cbw)?;
        let declared = command.transfer_length as usize;
        if declared > self.config.max_transfer_bytes {
            self.stats.bound_errors = self.stats.bound_errors.saturating_add(1);
            return Err(UsbError::MassStorageBuffer {
                context: "CBW data phase",
                limit: self.config.max_transfer_bytes,
                requested: declared,
            });
        }

        let mut data_out = Vec::new();
        if !command.direction_in && declared > 0 {
            let mut buffer = vec![0u8; declared];
            self.read_exact_out(&mut buffer, "data phase", "read")?;
            self.stats.data_out_bytes = self.stats.data_out_bytes.saturating_add(declared as u64);
            data_out = buffer;
        }

        let reply = self.device.execute_cbw(&cbw, &data_out)?;
        if reply.data_in.len() > self.config.max_transfer_bytes {
            self.stats.bound_errors = self.stats.bound_errors.saturating_add(1);
            return Err(UsbError::MassStorageBuffer {
                context: "IN data phase",
                limit: self.config.max_transfer_bytes,
                requested: reply.data_in.len(),
            });
        }
        if !reply.data_in.is_empty() {
            self.write_all_in(&reply.data_in, "data phase", "write")?;
            self.stats.data_in_bytes = self
                .stats
                .data_in_bytes
                .saturating_add(reply.data_in.len() as u64);
        }

        let csw = reply.csw(&command);
        self.write_all_in(&csw, "CSW", "write")?;
        self.transport
            .flush_in(self.config.transfer_timeout_ms)
            .map_err(|error| {
                self.record_transport_error(&error);
                error
            })?;
        self.stats.csws = self.stats.csws.saturating_add(1);
        self.stats.commands = self.stats.commands.saturating_add(1);
        self.stats.read_commands = self
            .stats
            .read_commands
            .saturating_add(u64::from(command.opcode == 0x28));
        self.stats.write_commands = self
            .stats
            .write_commands
            .saturating_add(u64::from(command.opcode == 0x2a));
        self.stats.flush_commands = self
            .stats
            .flush_commands
            .saturating_add(u64::from(command.opcode == 0x35));
        self.stats.check_conditions = self
            .stats
            .check_conditions
            .saturating_add(u64::from(reply.status == ScsiStatus::CheckCondition));
        self.stats.phase_errors = self
            .stats
            .phase_errors
            .saturating_add(u64::from(reply.status == ScsiStatus::PhaseError));
        self.running = true;

        Ok(MassStorageCommandOutcome {
            tag: command.tag,
            opcode: command.opcode,
            status: reply.status,
            residue: reply.residue,
            data_out_bytes: data_out.len(),
            data_in_bytes: reply.data_in.len(),
        })
    }

    /// Process up to `max_commands` cycles.
    ///
    /// The effective limit is the argument, then [`MassStorageLoopConfig::max_commands`].
    /// With neither set this runs until a transport error, so callers on a live
    /// endpoint normally supply a bound or run the loop on a dedicated thread.
    pub fn run(&mut self, max_commands: Option<u64>) -> Result<u64, UsbError> {
        let limit = max_commands.or(self.config.max_commands);
        let mut executed = 0u64;
        while limit.is_none_or(|limit| executed < limit) {
            self.step()?;
            executed = executed.saturating_add(1);
        }
        Ok(executed)
    }

    /// Stop the loop and commit the device according to its persistence policy.
    pub fn stop(&mut self) -> Result<(), UsbError> {
        self.running = false;
        self.device.stop()
    }

    fn read_exact_out(
        &mut self,
        buffer: &mut [u8],
        context: &'static str,
        operation: &'static str,
    ) -> Result<(), UsbError> {
        let mut filled = 0usize;
        while filled < buffer.len() {
            match self
                .transport
                .read_out(&mut buffer[filled..], self.config.transfer_timeout_ms)
            {
                Ok(0) => {
                    let error = UsbError::MassStorageTransport {
                        operation,
                        reason: format!("{context}: endpoint returned zero bytes"),
                    };
                    self.record_transport_error(&error);
                    return Err(error);
                }
                Ok(read) if read <= buffer.len() - filled => {
                    filled += read;
                }
                Ok(read) => {
                    let error = UsbError::MassStorageTransport {
                        operation,
                        reason: format!(
                            "{context}: transport returned {read} bytes for a {}-byte buffer",
                            buffer.len() - filled
                        ),
                    };
                    self.record_transport_error(&error);
                    return Err(error);
                }
                Err(error) => {
                    self.record_transport_error(&error);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn write_all_in(
        &mut self,
        data: &[u8],
        context: &'static str,
        operation: &'static str,
    ) -> Result<(), UsbError> {
        if data.len() > self.config.max_transfer_bytes + SCSI_CSW_BYTES {
            self.stats.bound_errors = self.stats.bound_errors.saturating_add(1);
            return Err(UsbError::MassStorageBuffer {
                context,
                limit: self.config.max_transfer_bytes + SCSI_CSW_BYTES,
                requested: data.len(),
            });
        }
        let mut written = 0usize;
        while written < data.len() {
            match self
                .transport
                .write_in(&data[written..], self.config.transfer_timeout_ms)
            {
                Ok(0) => {
                    let error = UsbError::MassStorageTransport {
                        operation,
                        reason: format!("{context}: endpoint accepted zero bytes"),
                    };
                    self.record_transport_error(&error);
                    return Err(error);
                }
                Ok(count) if count <= data.len() - written => {
                    written += count;
                }
                Ok(count) => {
                    let error = UsbError::MassStorageTransport {
                        operation,
                        reason: format!(
                            "{context}: transport accepted {count} bytes for a {}-byte buffer",
                            data.len() - written
                        ),
                    };
                    self.record_transport_error(&error);
                    return Err(error);
                }
                Err(error) => {
                    self.record_transport_error(&error);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn record_transport_error(&mut self, error: &UsbError) {
        match error {
            UsbError::MassStorageEndpointTimeout { .. } => {
                self.stats.timeout_errors = self.stats.timeout_errors.saturating_add(1);
            }
            _ => {
                self.stats.transport_errors = self.stats.transport_errors.saturating_add(1);
            }
        }
    }
}

impl MassStorageLoop<BoxedMassStorageTransport> {
    /// Replace the inactive/idle transport with a live one.
    ///
    /// The node starts an export with [`IdleBulkTransport`] and attaches the
    /// FunctionFS transport only when the daemon enables its endpoints. No
    /// transfer or `running` claim happens before this call.
    pub fn attach_transport(&mut self, transport: BoxedMassStorageTransport) {
        self.transport = transport;
    }
}

/// Owned file-endpoint transport for an explicitly supplied FunctionFS mount.
///
/// Unlike [`FunctionFsBulkTransport`], this type owns its endpoint handles, so
/// it can be boxed into a long-lived node export. It still opens only the
/// endpoint files the caller supplies; it never mounts FunctionFS or selects a
/// UDC. Reads/writes are clamped or chunked to the FunctionFS 64 KiB transfer
/// limit.
pub struct FileBulkTransport {
    out: FileEndpointIo,
    input: FileEndpointIo,
}

impl FileBulkTransport {
    /// Open one OUT and one IN endpoint file.
    pub fn open(
        out_endpoint: FunctionFsEndpoint,
        out_path: impl Into<PathBuf>,
        in_endpoint: FunctionFsEndpoint,
        in_path: impl Into<PathBuf>,
    ) -> Result<Self, UsbError> {
        if out_endpoint.is_in()
            || !in_endpoint.is_in()
            || out_endpoint.address & 0x0f == 0
            || in_endpoint.address & 0x0f == 0
        {
            return Err(UsbError::InvalidFunctionFsState {
                reason: "FileBulkTransport requires one non-zero OUT and one non-zero IN endpoint"
                    .to_owned(),
            });
        }
        Ok(Self {
            out: FileEndpointIo::open(out_endpoint, out_path)?,
            input: FileEndpointIo::open(in_endpoint, in_path)?,
        })
    }
}

impl MassStorageBulkTransport for FileBulkTransport {
    fn read_out(&mut self, buffer: &mut [u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        let max_bytes = buffer.len().min(MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES);
        self.out.read(&mut buffer[..max_bytes])
    }

    fn write_in(&mut self, data: &[u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES).min(data.len());
            let count = self.input.write(&data[written..end])?;
            if count == 0 {
                return Err(UsbError::MassStorageTransport {
                    operation: "write",
                    reason: "file endpoint accepted zero bytes".to_owned(),
                });
            }
            written += count;
        }
        Ok(written)
    }

    fn flush_in(&mut self, _timeout_ms: u64) -> Result<(), UsbError> {
        self.input.flush()
    }
}

/// Owned, nonblocking file-endpoint transport that enforces each
/// [`MassStorageLoop`] per-call deadline.
///
/// Unlike [`FileBulkTransport`], both endpoints are opened `O_NONBLOCK` and every
/// read/write waits with `poll(2)` for the timeout supplied by the loop. A
/// stalled peer therefore returns
/// [`UsbError::MassStorageEndpointTimeout`] instead of blocking the worker
/// forever. The endpoint files are still only the ones the caller supplies; this
/// type never mounts FunctionFS, selects a UDC, or opens a device by default.
pub struct PollingFileBulkTransport {
    out: PollingFileEndpointIo,
    input: PollingFileEndpointIo,
    default_timeout_ms: u64,
}

impl PollingFileBulkTransport {
    /// Open one OUT and one IN endpoint file with a default deadline.
    pub fn open(
        out_endpoint: FunctionFsEndpoint,
        out_path: impl Into<PathBuf>,
        in_endpoint: FunctionFsEndpoint,
        in_path: impl Into<PathBuf>,
        timeout_ms: u64,
    ) -> Result<Self, UsbError> {
        if out_endpoint.is_in()
            || !in_endpoint.is_in()
            || out_endpoint.address & 0x0f == 0
            || in_endpoint.address & 0x0f == 0
        {
            return Err(UsbError::InvalidFunctionFsState {
                reason: "PollingFileBulkTransport requires one non-zero OUT and one non-zero IN \
                         endpoint"
                    .to_owned(),
            });
        }
        Ok(Self {
            out: PollingFileEndpointIo::open(out_endpoint, out_path, timeout_ms)?,
            input: PollingFileEndpointIo::open(in_endpoint, in_path, timeout_ms)?,
            default_timeout_ms: timeout_ms,
        })
    }

    /// Configured fallback deadline.
    pub fn timeout_ms(&self) -> u64 {
        self.default_timeout_ms
    }

    fn effective_timeout(&self, timeout_ms: u64) -> u64 {
        if timeout_ms == 0 {
            self.default_timeout_ms
        } else {
            timeout_ms
        }
    }
}

impl MassStorageBulkTransport for PollingFileBulkTransport {
    fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError> {
        let max_bytes = buffer.len().min(MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES);
        let timeout_ms = self.effective_timeout(timeout_ms);
        self.out.read_timeout(&mut buffer[..max_bytes], timeout_ms)
    }

    fn write_in(&mut self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES).min(data.len());
            let count = self.input.write_timeout(&data[written..end], timeout_ms)?;
            if count == 0 {
                return Err(UsbError::MassStorageTransport {
                    operation: "write",
                    reason: "file endpoint accepted zero bytes".to_owned(),
                });
            }
            written += count;
        }
        Ok(written)
    }

    fn flush_in(&mut self, timeout_ms: u64) -> Result<(), UsbError> {
        let timeout_ms = self.effective_timeout(timeout_ms);
        self.input.flush_timeout(timeout_ms)
    }
}

/// Adapter that drives a [`FunctionFsDaemonCore`]'s enabled bulk endpoints.
///
/// Reads are clamped to the FunctionFS per-transfer limit and writes are
/// chunked to it, so a bounded 1 MiB SCSI data phase is assembled from several
/// endpoint transfers. The core itself owns no timeout; a live blocking file
/// endpoint is expected to be bounded by the host/UDC, while the default tests
/// use loopback endpoints.
pub struct FunctionFsBulkTransport<'a> {
    core: &'a mut FunctionFsDaemonCore,
    out_address: u8,
    in_address: u8,
}

impl<'a> FunctionFsBulkTransport<'a> {
    /// Bind one OUT and one IN endpoint address on a daemon core.
    pub fn new(
        core: &'a mut FunctionFsDaemonCore,
        out_address: u8,
        in_address: u8,
    ) -> Result<Self, UsbError> {
        if out_address & 0x80 != 0 || out_address & 0x0f == 0 {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "mass-storage OUT endpoint 0x{out_address:02x} must be a non-zero OUT address"
                ),
            });
        }
        if in_address & 0x80 == 0 || in_address & 0x0f == 0 {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "mass-storage IN endpoint 0x{in_address:02x} must be a non-zero IN address"
                ),
            });
        }
        Ok(Self {
            core,
            out_address,
            in_address,
        })
    }
}

impl MassStorageBulkTransport for FunctionFsBulkTransport<'_> {
    fn read_out(&mut self, buffer: &mut [u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        let max_bytes = buffer.len().min(MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES);
        let data = self.core.read_endpoint(self.out_address, max_bytes)?;
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    fn write_in(&mut self, data: &[u8], _timeout_ms: u64) -> Result<usize, UsbError> {
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES).min(data.len());
            self.core
                .write_endpoint(self.in_address, &data[written..end])?;
            written = end;
        }
        Ok(written)
    }

    fn flush_in(&mut self, _timeout_ms: u64) -> Result<(), UsbError> {
        // `FunctionFsDaemonCore::write_endpoint` flushes every chunk.
        Ok(())
    }
}
