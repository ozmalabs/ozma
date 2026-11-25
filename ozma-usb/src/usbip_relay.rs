// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded USB/IP URB data relay.
//!
//! [`UsbIpDataEndpoint`] is the injectable device boundary: it receives a
//! [`UsbIpCmdSubmit`] and returns the real completion (or a typed status), and
//! it can cancel an in-flight URB. [`UsbIpKernelDataEndpoint`] is the separate
//! kernel-adapter seam that opens one such endpoint after a successful import;
//! [`NoUsbIpKernelDataEndpoint`] is its explicit no-op default. This crate does
//! not contain a kernel `vhci_hcd`/`usbip-host` stub, so the default deployment
//! still needs a real [`crate::usbip_daemon::UsbIpKernelBackend`] for
//! attach/import plus a real data seam. The relay itself is a bounded state
//! machine that can also be driven directly by tests without a kernel module or
//! a real USB device.
//!
//! [`UsbIpRelay`] is the deterministic core: it pairs `USBIP_CMD_SUBMIT` and
//! `USBIP_RET_SUBMIT` by `seqnum`, enforces an in-flight ceiling, cancels by
//! `USBIP_CMD_UNLINK`, drops completions that arrive after a successful unlink,
//! and drains every in-flight URB when the session ends.
//!
//! [`serve_data_session`] is the stream loop. It reads bounded
//! `USBIP_CMD_SUBMIT`/`USBIP_CMD_UNLINK` frames from an injectable
//! [`UsbIpTransport`], runs each submit on its own bounded worker thread, and
//! writes completions as they arrive (out of order when the endpoint completes
//! out of order). No completion is invented: an endpoint error becomes a
//! negative-status `USBIP_RET_SUBMIT` with zero bytes, never a fabricated
//! payload.
//!
//! This module never logs a URB payload. The codec's `Debug` implementations
//! report only `data_len`, and the relay counters record counts/bytes, not
//! contents.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::UsbError;
use crate::usbip::UsbIpAttachment;
use crate::usbip_server::{UsbIpClock, UsbIpSessionEnd, UsbIpTransport};
use crate::usbip_urb::{
    UsbIpCmdSubmit, UsbIpCmdUnlink, UsbIpDirection, UsbIpRetSubmit, UsbIpRetUnlink, UsbIpUrb,
    UsbIpUrbError, USBIP_URB_HEADER_BYTES, USBIP_URB_MAX_TRANSFER_BYTES, USBIP_URB_NO_ISO_PACKETS,
};

/// Maximum configurable in-flight URB ceiling.
pub const USBIP_RELAY_MAX_IN_FLIGHT_LIMIT: usize = 256;
/// Default concurrent in-flight URB ceiling.
pub const USBIP_RELAY_DEFAULT_MAX_IN_FLIGHT: usize = 32;
/// Default idle deadline between complete client frames.
pub const USBIP_RELAY_DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000;
/// Poll interval used to observe stop/idle deadlines and completions.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// `-EBUSY`: the relay's in-flight ceiling was reached.
pub const USBIP_URB_STATUS_EBUSY: i32 = -16;
/// `-EAGAIN`: a bounded worker could not be started.
pub const USBIP_URB_STATUS_EAGAIN: i32 = -11;
/// `-EINVAL`: duplicate sequence number or invalid unlink target.
pub const USBIP_URB_STATUS_EINVAL: i32 = -22;
/// `-EPROTO`: an endpoint completion was malformed.
pub const USBIP_URB_STATUS_EPROTO: i32 = -71;
/// `-EMSGSIZE`: a submit exceeded the configured transfer/ISO bounds.
pub const USBIP_URB_STATUS_EMSGSIZE: i32 = -90;

/// A typed endpoint failure carrying the URB status to return to the client.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("USB/IP endpoint failed with status {status}: {reason}")]
pub struct UsbIpDataError {
    /// Negative errno returned in `USBIP_RET_SUBMIT`/`USBIP_RET_UNLINK`.
    pub status: i32,
    /// Human-readable reason. Never contains URB payload bytes.
    pub reason: String,
}

impl UsbIpDataError {
    /// Construct an endpoint error with an explicit status.
    pub fn new(status: i32, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }

    /// Construct an error from a positive errno, storing the negative status.
    pub fn from_errno(errno: i32, reason: impl Into<String>) -> Self {
        Self::new(-errno.abs(), reason)
    }
}

/// The injectable USB/IP device endpoint.
///
/// `submit` must return real device bytes for an IN request (with
/// `actual_length` equal to `data.len()`) or a typed error. It may block; the
/// relay runs each call on a bounded worker thread so many URBs can be in
/// flight concurrently.
pub trait UsbIpDataEndpoint: Send + Sync {
    /// Execute one submitted URB and return its completion.
    fn submit(&self, request: &UsbIpCmdSubmit) -> Result<UsbIpRetSubmit, UsbIpDataError>;

    /// Cancel an in-flight URB by the submit's `seqnum`.
    fn unlink(&self, devid: u32, seqnum: u32) -> Result<UsbIpRetUnlink, UsbIpDataError>;
}

/// Kernel-backed data-endpoint seam and bounded factory.
///
/// The control server calls [`Self::open`] at most once for an authorized,
/// successful `OP_REQ_IMPORT`, *before* it writes the success reply. The
/// returned endpoint then serves URBs on that same connection. A factory that
/// cannot obtain a real kernel `vhci_hcd`/`usbip-host` endpoint returns
/// [`UsbError::UsbIpKernelRequired`]; the default
/// [`NoUsbIpKernelDataEndpoint`] does exactly that. This is deliberately
/// distinct from a runtime [`UsbIpDataError`] from an opened endpoint, which
/// becomes a negative-status `USBIP_RET_SUBMIT` with zero bytes.
///
/// Implementations return at most one endpoint per `open` call, so the server
/// holds a single bounded endpoint for the lifetime of one data session. They
/// must not fabricate device bytes when no kernel adapter is attached.
pub trait UsbIpKernelDataEndpoint: Send + Sync {
    /// Whether a real kernel-backed URB endpoint is available.
    fn available(&self) -> bool {
        false
    }

    /// Open the URB endpoint for one imported device.
    fn open(&self, attachment: &UsbIpAttachment) -> Result<Arc<dyn UsbIpDataEndpoint>, UsbError> {
        let _ = attachment;
        Err(kernel_data_required())
    }
}

/// Explicitly absent kernel data-endpoint adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUsbIpKernelDataEndpoint;

impl UsbIpKernelDataEndpoint for NoUsbIpKernelDataEndpoint {}

fn kernel_data_required() -> UsbError {
    UsbError::UsbIpKernelRequired {
        operation: "data",
        reason: "the kernel vhci_hcd/usbip-host data adapter is not available; load the modules, \
                 run with the required privileges, and inject a UsbIpKernelDataEndpoint. \
                 No endpoint was opened and no URB data was fabricated."
            .to_owned(),
    }
}

/// Bounds for one relay session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpRelayConfig {
    /// Maximum concurrent in-flight submits.
    pub max_in_flight: usize,
    /// Largest transfer buffer accepted or produced.
    pub max_transfer_bytes: usize,
    /// Largest ISO descriptor count accepted.
    pub max_iso_packets: usize,
    /// Idle deadline between complete client frames; `None` waits forever.
    pub idle_timeout_ms: Option<u64>,
}

impl Default for UsbIpRelayConfig {
    fn default() -> Self {
        Self {
            max_in_flight: USBIP_RELAY_DEFAULT_MAX_IN_FLIGHT,
            max_transfer_bytes: USBIP_URB_MAX_TRANSFER_BYTES,
            max_iso_packets: crate::usbip_urb::USBIP_MAX_ISO_PACKETS,
            idle_timeout_ms: Some(USBIP_RELAY_DEFAULT_IDLE_TIMEOUT_MS),
        }
    }
}

impl UsbIpRelayConfig {
    /// Validate the in-flight/transfer/ISO/deadline bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !(1..=USBIP_RELAY_MAX_IN_FLIGHT_LIMIT).contains(&self.max_in_flight) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_relay.max_in_flight",
                reason: format!("max_in_flight must be 1..={USBIP_RELAY_MAX_IN_FLIGHT_LIMIT}"),
            });
        }
        if !(1..=USBIP_URB_MAX_TRANSFER_BYTES).contains(&self.max_transfer_bytes) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_relay.max_transfer_bytes",
                reason: format!("max_transfer_bytes must be 1..={USBIP_URB_MAX_TRANSFER_BYTES}"),
            });
        }
        if self.max_iso_packets > crate::usbip_urb::USBIP_MAX_ISO_PACKETS {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_relay.max_iso_packets",
                reason: format!(
                    "max_iso_packets must be 0..={}",
                    crate::usbip_urb::USBIP_MAX_ISO_PACKETS
                ),
            });
        }
        if matches!(self.idle_timeout_ms, Some(0)) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_relay.idle_timeout_ms",
                reason: "idle_timeout_ms must be greater than zero when set".to_owned(),
            });
        }
        Ok(())
    }

    /// Largest encoded URB frame implied by these bounds.
    pub fn max_frame_bytes(&self) -> usize {
        USBIP_URB_HEADER_BYTES
            + self.max_transfer_bytes
            + self.max_iso_packets * crate::usbip_urb::USBIP_ISO_PACKET_BYTES
    }
}

/// One in-flight submit tracked by the relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbIpInFlightUrb {
    /// Submit sequence number.
    pub seqnum: u32,
    /// Device id.
    pub devid: u32,
    /// Transfer direction.
    pub direction: UsbIpDirection,
    /// Endpoint number.
    pub ep: u32,
    /// Declared transfer buffer length.
    pub transfer_buffer_length: u32,
    /// Declared ISO packet count, or the non-ISO sentinel.
    pub number_of_packets: u32,
    /// Monotonic relay-clock start time.
    pub started_ms: u64,
}

impl UsbIpInFlightUrb {
    /// ISO descriptor count implied by the submit.
    pub fn iso_packet_count(&self) -> usize {
        if self.number_of_packets == 0 || self.number_of_packets == USBIP_URB_NO_ISO_PACKETS {
            0
        } else {
            self.number_of_packets as usize
        }
    }
}

/// Cumulative relay counters (no payload bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpRelayStats {
    /// Submits registered.
    pub registered: u64,
    /// Completions paired and delivered.
    pub completed: u64,
    /// Unlink requests that removed an in-flight URB.
    pub unlinked: u64,
    /// Unlink requests whose target had already completed.
    pub unlink_not_found: u64,
    /// Submits rejected at the in-flight ceiling.
    pub rejected_in_flight: u64,
    /// Submits rejected for a duplicate sequence number.
    pub rejected_duplicate: u64,
    /// Completions that arrived after their URB was unlinked/cleaned up.
    pub late_completions: u64,
    /// Endpoint completions rejected as malformed.
    pub invalid_completions: u64,
    /// URBs unlinked during disconnect/idle cleanup.
    pub cancelled_on_disconnect: u64,
}

#[derive(Debug, Default)]
struct RelayInner {
    in_flight: BTreeMap<u32, UsbIpInFlightUrb>,
    stats: UsbIpRelayStats,
}

/// A typed relay-core failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UsbIpRelayError {
    /// The configured in-flight ceiling was reached.
    #[error("USB/IP relay in-flight limit of {limit} reached")]
    InFlightLimit {
        /// Configured limit.
        limit: usize,
    },
    /// The numeric sequence number is already in flight.
    #[error("USB/IP relay already has an in-flight URB with seqnum {seqnum}")]
    DuplicateSeqnum {
        /// Duplicate sequence number.
        seqnum: u32,
    },
    /// A completion or unlink named the wrong device id.
    #[error("USB/IP relay seqnum {seqnum} belongs to devid {expected}, not devid {actual}")]
    MismatchedDevid {
        /// Sequence number involved.
        seqnum: u32,
        /// Registered device id.
        expected: u32,
        /// Presented device id.
        actual: u32,
    },
    /// A completion does not match the registered request.
    #[error("USB/IP relay completion for seqnum {seqnum} is invalid: {reason}")]
    InvalidCompletion {
        /// Sequence number involved.
        seqnum: u32,
        /// Why the completion was rejected.
        reason: String,
    },
    /// A client sent a server-only reply command.
    #[error("USB/IP relay received the server-only command {command} from a client")]
    UnexpectedClientCommand {
        /// Command name.
        command: &'static str,
    },
    /// The URB codec rejected a frame.
    #[error("invalid USB/IP URB: {0}")]
    Urb(#[from] UsbIpUrbError),
}

/// Deterministic SUBMIT/RET pairing and cancellation core.
#[derive(Debug)]
pub struct UsbIpRelay {
    config: UsbIpRelayConfig,
    inner: Mutex<RelayInner>,
}

impl UsbIpRelay {
    /// Create a relay after validating `config`.
    pub fn new(config: UsbIpRelayConfig) -> Result<Self, UsbError> {
        config.validate()?;
        Ok(Self {
            config,
            inner: Mutex::new(RelayInner::default()),
        })
    }

    /// Relay configuration.
    pub fn config(&self) -> &UsbIpRelayConfig {
        &self.config
    }

    /// Current in-flight URB count.
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight.len()
    }

    /// Whether `seqnum` is currently in flight.
    pub fn contains(&self, seqnum: u32) -> bool {
        self.lock().in_flight.contains_key(&seqnum)
    }

    /// Snapshot of cumulative counters.
    pub fn stats(&self) -> UsbIpRelayStats {
        self.lock().stats
    }

    /// Register a submit before dispatching it to the endpoint.
    ///
    /// Registration is bounded by the configured in-flight ceiling, rejects
    /// duplicate seqnums, and records only metadata (never the payload).
    pub fn register_submit(
        &self,
        submit: &UsbIpCmdSubmit,
        now_ms: u64,
    ) -> Result<UsbIpInFlightUrb, UsbIpRelayError> {
        submit.validate()?;
        let transfer_length = submit.transfer_buffer_length as usize;
        if transfer_length > self.config.max_transfer_bytes {
            return Err(UsbIpUrbError::Oversized {
                field: "transfer_buffer_length",
                limit: self.config.max_transfer_bytes,
                requested: transfer_length,
            }
            .into());
        }
        if submit.iso_packet_count() > self.config.max_iso_packets {
            return Err(UsbIpUrbError::Oversized {
                field: "number_of_packets",
                limit: self.config.max_iso_packets,
                requested: submit.iso_packet_count(),
            }
            .into());
        }
        let mut inner = self.lock();
        if inner.in_flight.contains_key(&submit.seqnum) {
            inner.stats.rejected_duplicate = inner.stats.rejected_duplicate.saturating_add(1);
            return Err(UsbIpRelayError::DuplicateSeqnum {
                seqnum: submit.seqnum,
            });
        }
        if inner.in_flight.len() >= self.config.max_in_flight {
            inner.stats.rejected_in_flight = inner.stats.rejected_in_flight.saturating_add(1);
            return Err(UsbIpRelayError::InFlightLimit {
                limit: self.config.max_in_flight,
            });
        }
        let urb = UsbIpInFlightUrb {
            seqnum: submit.seqnum,
            devid: submit.devid,
            direction: submit.direction,
            ep: submit.ep,
            transfer_buffer_length: submit.transfer_buffer_length,
            number_of_packets: submit.number_of_packets,
            started_ms: now_ms,
        };
        inner.in_flight.insert(urb.seqnum, urb);
        inner.stats.registered = inner.stats.registered.saturating_add(1);
        Ok(urb)
    }

    /// Remove a registration when dispatch failed before the endpoint ran.
    pub fn remove_in_flight(&self, seqnum: u32) -> Option<UsbIpInFlightUrb> {
        self.lock().in_flight.remove(&seqnum)
    }

    /// Pair and remove a completion by seqnum.
    ///
    /// `Ok(None)` means the URB was already unlinked (or never registered), so
    /// the completion is late and must not be sent. Errors indicate a
    /// malformed endpoint completion and also leave the URB removed.
    pub fn complete_submit(
        &self,
        completion: UsbIpRetSubmit,
    ) -> Result<Option<UsbIpRetSubmit>, UsbIpRelayError> {
        let mut inner = self.lock();
        let Some(in_flight) = inner.in_flight.remove(&completion.seqnum) else {
            inner.stats.late_completions = inner.stats.late_completions.saturating_add(1);
            return Ok(None);
        };
        if in_flight.devid != completion.devid {
            inner.stats.invalid_completions = inner.stats.invalid_completions.saturating_add(1);
            return Err(UsbIpRelayError::MismatchedDevid {
                seqnum: completion.seqnum,
                expected: in_flight.devid,
                actual: completion.devid,
            });
        }
        if in_flight.direction != completion.direction {
            inner.stats.invalid_completions = inner.stats.invalid_completions.saturating_add(1);
            return Err(UsbIpRelayError::InvalidCompletion {
                seqnum: completion.seqnum,
                reason: "completion direction differs from the submitted URB".to_owned(),
            });
        }
        if completion.actual_length > in_flight.transfer_buffer_length {
            inner.stats.invalid_completions = inner.stats.invalid_completions.saturating_add(1);
            return Err(UsbIpRelayError::InvalidCompletion {
                seqnum: completion.seqnum,
                reason: format!(
                    "actual_length {} exceeds transfer_buffer_length {}",
                    completion.actual_length, in_flight.transfer_buffer_length
                ),
            });
        }
        if let Err(error) = completion.validate() {
            inner.stats.invalid_completions = inner.stats.invalid_completions.saturating_add(1);
            return Err(UsbIpRelayError::InvalidCompletion {
                seqnum: completion.seqnum,
                reason: error.to_string(),
            });
        }
        let iso_bound = in_flight.transfer_buffer_length as usize;
        if completion.iso_packets.iter().any(|packet| {
            (packet.offset as usize)
                .checked_add(packet.length as usize)
                .map(|end| end > iso_bound)
                .unwrap_or(true)
        }) {
            inner.stats.invalid_completions = inner.stats.invalid_completions.saturating_add(1);
            return Err(UsbIpRelayError::InvalidCompletion {
                seqnum: completion.seqnum,
                reason: format!("ISO descriptor exceeds transfer_buffer_length {iso_bound}"),
            });
        }
        inner.stats.completed = inner.stats.completed.saturating_add(1);
        Ok(Some(completion))
    }

    /// Remove the target of an unlink request.
    ///
    /// `Ok(None)` means the target already completed (or never existed).
    /// A device-id mismatch leaves the target in flight and returns an error.
    pub fn unlink(
        &self,
        request: &UsbIpCmdUnlink,
    ) -> Result<Option<UsbIpInFlightUrb>, UsbIpRelayError> {
        let mut inner = self.lock();
        let Some(in_flight) = inner.in_flight.get(&request.unlink_seqnum).copied() else {
            inner.stats.unlink_not_found = inner.stats.unlink_not_found.saturating_add(1);
            return Ok(None);
        };
        if in_flight.devid != request.devid {
            return Err(UsbIpRelayError::MismatchedDevid {
                seqnum: request.unlink_seqnum,
                expected: in_flight.devid,
                actual: request.devid,
            });
        }
        inner.in_flight.remove(&request.unlink_seqnum);
        inner.stats.unlinked = inner.stats.unlinked.saturating_add(1);
        Ok(Some(in_flight))
    }

    /// Put a target back after the endpoint could not cancel it.
    pub fn restore(&self, urb: UsbIpInFlightUrb) -> bool {
        let mut inner = self.lock();
        if inner.in_flight.len() >= self.config.max_in_flight
            || inner.in_flight.contains_key(&urb.seqnum)
        {
            return false;
        }
        inner.in_flight.insert(urb.seqnum, urb);
        inner.stats.unlinked = inner.stats.unlinked.saturating_sub(1);
        true
    }

    /// Remove every in-flight URB (disconnect/idle cleanup).
    pub fn drain_in_flight(&self) -> Vec<UsbIpInFlightUrb> {
        let mut inner = self.lock();
        let drained: Vec<_> = inner.in_flight.values().copied().collect();
        inner.in_flight.clear();
        drained
    }

    fn lock(&self) -> MutexGuard<'_, RelayInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A completed data-relay session summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpRelaySessionOutcome {
    /// Complete client URB frames read.
    pub commands_received: u64,
    /// `USBIP_CMD_SUBMIT` requests handled (including rejected ones).
    pub submits: u64,
    /// `USBIP_CMD_SUBMIT` frames accepted and dispatched to the endpoint.
    pub submits_dispatched: u64,
    /// `USBIP_RET_SUBMIT` frames written.
    pub ret_submits: u64,
    /// `USBIP_CMD_UNLINK` requests handled.
    pub unlinks: u64,
    /// `USBIP_RET_UNLINK` frames written.
    pub ret_unlinks: u64,
    /// Endpoint calls that returned an error or negative status.
    pub endpoint_errors: u64,
    /// Submits rejected at the in-flight ceiling.
    pub rejected_in_flight: u64,
    /// Submits rejected for a duplicate sequence number.
    pub rejected_duplicate: u64,
    /// Submits rejected for oversized transfer/ISO bounds.
    pub rejected_oversized: u64,
    /// Unlinks whose target had already completed.
    pub unlink_not_found: u64,
    /// Completions dropped because the URB had been unlinked.
    pub late_completions: u64,
    /// Endpoint completions rejected as malformed.
    pub invalid_completions: u64,
    /// In-flight URBs unlinked during disconnect/idle cleanup.
    pub cancelled_on_disconnect: u64,
    /// Bytes read from the peer.
    pub bytes_from_peer: u64,
    /// Bytes written to the peer.
    pub bytes_to_peer: u64,
    /// Normal termination reason.
    pub end: Option<UsbIpSessionEnd>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadSignal {
    Complete,
    End(UsbIpSessionEnd),
}

struct Completion {
    ret: UsbIpRetSubmit,
    endpoint_error: bool,
}

/// Serve one USB/IP URB data session over an injected transport.
///
/// The loop pairs completions by seqnum, supports up to
/// [`UsbIpRelayConfig::max_in_flight`] concurrent submits, cancels through
/// `USBIP_CMD_UNLINK`, and unlinks every outstanding URB on peer close, stop,
/// idle timeout, or error. A submit above the ceiling is answered with a
/// negative-status `USBIP_RET_SUBMIT` (`-EBUSY`) rather than dropped silently.
pub fn serve_data_session<S: UsbIpTransport>(
    stream: &mut S,
    endpoint: Arc<dyn UsbIpDataEndpoint>,
    config: &UsbIpRelayConfig,
    clock: &dyn UsbIpClock,
    stop: &AtomicBool,
) -> Result<UsbIpRelaySessionOutcome, UsbError> {
    config.validate()?;
    let relay = UsbIpRelay::new(config.clone())?;
    let (sender, receiver) = mpsc::channel();
    let mut session = DataSession {
        stream,
        endpoint,
        config,
        clock,
        stop,
        relay,
        sender,
        receiver,
        last_activity: clock.now_millis(),
        outcome: UsbIpRelaySessionOutcome::default(),
    };
    let result = session.run();
    session.cleanup();
    match result {
        Ok(()) => Ok(session.outcome),
        Err(error) => Err(error),
    }
}

struct DataSession<'a, S: UsbIpTransport> {
    stream: &'a mut S,
    endpoint: Arc<dyn UsbIpDataEndpoint>,
    config: &'a UsbIpRelayConfig,
    clock: &'a dyn UsbIpClock,
    stop: &'a AtomicBool,
    relay: UsbIpRelay,
    sender: Sender<Completion>,
    receiver: Receiver<Completion>,
    last_activity: u64,
    outcome: UsbIpRelaySessionOutcome,
}

impl<S: UsbIpTransport> DataSession<'_, S> {
    fn run(&mut self) -> Result<(), UsbError> {
        loop {
            self.drain_completions()?;
            if let Some(end) = self.ending() {
                self.outcome.end = Some(end);
                return Ok(());
            }

            let mut header = [0u8; USBIP_URB_HEADER_BYTES];
            match self.read_exact(&mut header, true)? {
                ReadSignal::Complete => {}
                ReadSignal::End(end) => {
                    self.outcome.end = Some(end);
                    return Ok(());
                }
            }

            let frame_len =
                UsbIpUrb::frame_len_from_header_bounded(&header, self.config.max_transfer_bytes)?;
            if frame_len > self.config.max_frame_bytes() {
                return Err(UsbIpUrbError::Oversized {
                    field: "frame",
                    limit: self.config.max_frame_bytes(),
                    requested: frame_len,
                }
                .into());
            }
            let mut frame = vec![0u8; frame_len];
            frame[..USBIP_URB_HEADER_BYTES].copy_from_slice(&header);
            if frame_len > USBIP_URB_HEADER_BYTES {
                match self.read_exact(&mut frame[USBIP_URB_HEADER_BYTES..], false)? {
                    ReadSignal::Complete => {}
                    ReadSignal::End(end) => {
                        self.outcome.end = Some(end);
                        return Ok(());
                    }
                }
            }

            let urb = UsbIpUrb::decode_bounded(&frame, self.config.max_transfer_bytes)?;
            self.outcome.commands_received = self.outcome.commands_received.saturating_add(1);
            let command = urb.command();
            match urb {
                UsbIpUrb::CmdSubmit(submit) => self.handle_submit(submit)?,
                UsbIpUrb::CmdUnlink(unlink) => self.handle_unlink(unlink)?,
                UsbIpUrb::RetSubmit(_) | UsbIpUrb::RetUnlink(_) => {
                    return Err(UsbIpRelayError::UnexpectedClientCommand {
                        command: command.name(),
                    }
                    .into());
                }
            }
        }
    }

    fn ending(&self) -> Option<UsbIpSessionEnd> {
        if self.stop.load(Ordering::SeqCst) {
            return Some(UsbIpSessionEnd::Stopped);
        }
        if let Some(idle_ms) = self.config.idle_timeout_ms {
            let deadline = self.last_activity.saturating_add(idle_ms);
            if self.clock.now_millis() >= deadline {
                return Some(UsbIpSessionEnd::Timeout);
            }
        }
        None
    }

    fn handle_submit(&mut self, submit: UsbIpCmdSubmit) -> Result<(), UsbError> {
        self.outcome.submits = self.outcome.submits.saturating_add(1);
        match self.relay.register_submit(&submit, self.clock.now_millis()) {
            Ok(_) => {
                let fallback = error_completion(&submit, USBIP_URB_STATUS_EAGAIN);
                let seqnum = submit.seqnum;
                let endpoint = Arc::clone(&self.endpoint);
                let sender = self.sender.clone();
                let name = format!("ozma-usbip-urb-{seqnum}");
                let spawned = thread::Builder::new().name(name).spawn(move || {
                    let result = endpoint.submit(&submit);
                    let completion = normalize_completion(&submit, result);
                    let _ = sender.send(completion);
                });
                if spawned.is_err() {
                    self.relay.remove_in_flight(seqnum);
                    self.write_ret_submit(fallback)?;
                } else {
                    self.outcome.submits_dispatched =
                        self.outcome.submits_dispatched.saturating_add(1);
                }
            }
            Err(UsbIpRelayError::InFlightLimit { .. }) => {
                self.outcome.rejected_in_flight = self.outcome.rejected_in_flight.saturating_add(1);
                let completion = error_completion(&submit, USBIP_URB_STATUS_EBUSY);
                self.write_ret_submit(completion)?;
            }
            Err(UsbIpRelayError::DuplicateSeqnum { .. }) => {
                self.outcome.rejected_duplicate = self.outcome.rejected_duplicate.saturating_add(1);
                let completion = error_completion(&submit, USBIP_URB_STATUS_EINVAL);
                self.write_ret_submit(completion)?;
            }
            Err(UsbIpRelayError::Urb(UsbIpUrbError::Oversized { .. })) => {
                self.outcome.rejected_oversized = self.outcome.rejected_oversized.saturating_add(1);
                let completion = error_completion(&submit, USBIP_URB_STATUS_EMSGSIZE);
                self.write_ret_submit(completion)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn handle_unlink(&mut self, request: UsbIpCmdUnlink) -> Result<(), UsbError> {
        self.outcome.unlinks = self.outcome.unlinks.saturating_add(1);
        let removed = match self.relay.unlink(&request) {
            Ok(removed) => removed,
            Err(UsbIpRelayError::MismatchedDevid { .. }) => {
                let reply =
                    UsbIpRetUnlink::new(request.seqnum, request.devid, USBIP_URB_STATUS_EINVAL);
                self.write_ret_unlink(reply)?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        let Some(in_flight) = removed else {
            self.outcome.unlink_not_found = self.outcome.unlink_not_found.saturating_add(1);
            let reply = UsbIpRetUnlink::not_found(request.seqnum, request.devid);
            self.write_ret_unlink(reply)?;
            return Ok(());
        };

        match self.endpoint.unlink(in_flight.devid, in_flight.seqnum) {
            Ok(mut reply) => {
                reply.seqnum = request.seqnum;
                reply.devid = request.devid;
                if !unlink_cancelled(reply.status) {
                    // The endpoint could not cancel; the original URB may still
                    // complete, so keep it paired.
                    let _ = self.relay.restore(in_flight);
                }
                if reply.status < 0 && reply.status != crate::usbip_urb::USBIP_URB_UNLINK_CANCELLED
                {
                    self.outcome.endpoint_errors = self.outcome.endpoint_errors.saturating_add(1);
                }
                self.write_ret_unlink(reply)?;
            }
            Err(error) => {
                self.outcome.endpoint_errors = self.outcome.endpoint_errors.saturating_add(1);
                let _ = self.relay.restore(in_flight);
                let reply = UsbIpRetUnlink::new(request.seqnum, request.devid, error.status);
                self.write_ret_unlink(reply)?;
            }
        }
        Ok(())
    }

    fn drain_completions(&mut self) -> Result<(), UsbError> {
        loop {
            let completion = match self.receiver.try_recv() {
                Ok(completion) => completion,
                Err(_) => break,
            };
            let Completion {
                ret,
                endpoint_error,
            } = completion;
            match self.relay.complete_submit(ret) {
                Ok(Some(completion)) => {
                    if endpoint_error || completion.status < 0 {
                        self.outcome.endpoint_errors =
                            self.outcome.endpoint_errors.saturating_add(1);
                    }
                    self.write_ret_submit(completion)?;
                }
                Ok(None) => {
                    self.outcome.late_completions = self.outcome.late_completions.saturating_add(1);
                }
                Err(_) => {
                    self.outcome.invalid_completions =
                        self.outcome.invalid_completions.saturating_add(1);
                }
            }
        }
        Ok(())
    }

    fn read_exact(
        &mut self,
        buffer: &mut [u8],
        allow_clean_eof: bool,
    ) -> Result<ReadSignal, UsbError> {
        let mut filled = 0;
        while filled < buffer.len() {
            self.drain_completions()?;
            if let Some(end) = self.ending() {
                return Ok(ReadSignal::End(end));
            }
            match self.stream.read(&mut buffer[filled..]) {
                Ok(0) => {
                    if filled == 0 && allow_clean_eof {
                        return Ok(ReadSignal::End(UsbIpSessionEnd::PeerClosed));
                    }
                    return Err(UsbIpUrbError::Truncated {
                        needed: buffer.len(),
                        available: filled,
                    }
                    .into());
                }
                Ok(read) => {
                    filled += read;
                    self.outcome.bytes_from_peer =
                        self.outcome.bytes_from_peer.saturating_add(read as u64);
                    self.last_activity = self.clock.now_millis();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(POLL_INTERVAL);
                }
                Err(error) => {
                    return Err(UsbError::Io {
                        operation: "read USB/IP URB frame",
                        path: std::path::PathBuf::from("<usbip-data-stream>"),
                        source: error,
                    });
                }
            }
        }
        Ok(ReadSignal::Complete)
    }

    fn write_urb(&mut self, urb: &UsbIpUrb) -> Result<(), UsbError> {
        let bytes = urb.encode()?;
        if bytes.len() > self.config.max_frame_bytes() {
            return Err(UsbIpUrbError::Oversized {
                field: "frame",
                limit: self.config.max_frame_bytes(),
                requested: bytes.len(),
            }
            .into());
        }
        self.stream
            .write_all(&bytes)
            .map_err(|source| UsbError::Io {
                operation: "write USB/IP URB frame",
                path: std::path::PathBuf::from("<usbip-data-stream>"),
                source,
            })?;
        let _ = self.stream.flush();
        self.outcome.bytes_to_peer = self
            .outcome
            .bytes_to_peer
            .saturating_add(bytes.len() as u64);
        self.last_activity = self.clock.now_millis();
        Ok(())
    }

    fn write_ret_submit(&mut self, completion: UsbIpRetSubmit) -> Result<(), UsbError> {
        self.write_urb(&UsbIpUrb::RetSubmit(completion))?;
        self.outcome.ret_submits = self.outcome.ret_submits.saturating_add(1);
        Ok(())
    }

    fn write_ret_unlink(&mut self, reply: UsbIpRetUnlink) -> Result<(), UsbError> {
        self.write_urb(&UsbIpUrb::RetUnlink(reply))?;
        self.outcome.ret_unlinks = self.outcome.ret_unlinks.saturating_add(1);
        Ok(())
    }

    fn cleanup(&mut self) {
        let drained = self.relay.drain_in_flight();
        for urb in drained {
            let _ = self.endpoint.unlink(urb.devid, urb.seqnum);
            self.outcome.cancelled_on_disconnect =
                self.outcome.cancelled_on_disconnect.saturating_add(1);
        }
    }
}

fn error_completion(request: &UsbIpCmdSubmit, status: i32) -> UsbIpRetSubmit {
    let mut completion = UsbIpRetSubmit::error(request.seqnum, request.devid, status);
    completion.direction = request.direction;
    completion.ep = request.ep;
    completion
}

fn normalize_completion(
    request: &UsbIpCmdSubmit,
    result: Result<UsbIpRetSubmit, UsbIpDataError>,
) -> Completion {
    match result {
        Ok(mut completion) => {
            completion.seqnum = request.seqnum;
            completion.devid = request.devid;
            completion.direction = request.direction;
            completion.ep = request.ep;
            let valid = completion.actual_length <= request.transfer_buffer_length
                && completion.validate().is_ok();
            if valid {
                let endpoint_error = completion.status != 0;
                Completion {
                    ret: completion,
                    endpoint_error,
                }
            } else {
                Completion {
                    ret: error_completion(request, USBIP_URB_STATUS_EPROTO),
                    endpoint_error: true,
                }
            }
        }
        Err(error) => Completion {
            ret: error_completion(request, error.status),
            endpoint_error: true,
        },
    }
}

fn unlink_cancelled(status: i32) -> bool {
    status == 0 || status == crate::usbip_urb::USBIP_URB_UNLINK_CANCELLED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usbip_urb::{UsbIpUrbCommand, USBIP_CMD_SUBMIT};

    fn submit(seqnum: u32, devid: u32, data: &[u8]) -> UsbIpCmdSubmit {
        UsbIpCmdSubmit::out(seqnum, devid, 2, data.to_vec())
    }

    fn completion(request: &UsbIpCmdSubmit, data: &[u8]) -> UsbIpRetSubmit {
        UsbIpRetSubmit::new(
            request.seqnum,
            request.devid,
            request.direction,
            request.ep,
            data.to_vec(),
        )
    }

    #[test]
    fn core_pairs_out_of_order_and_tracks_the_ceiling() {
        let relay = UsbIpRelay::new(UsbIpRelayConfig {
            max_in_flight: 2,
            ..UsbIpRelayConfig::default()
        })
        .unwrap();
        let first = submit(1, 7, b"one");
        let second = submit(2, 7, b"two");
        relay.register_submit(&first, 0).unwrap();
        relay.register_submit(&second, 0).unwrap();
        assert_eq!(relay.in_flight(), 2);
        assert_eq!(
            relay
                .register_submit(&submit(3, 7, b"three"), 0)
                .unwrap_err(),
            UsbIpRelayError::InFlightLimit { limit: 2 }
        );

        let second_done = relay
            .complete_submit(completion(&second, b""))
            .unwrap()
            .unwrap();
        assert_eq!(second_done.seqnum, 2);
        let first_done = relay
            .complete_submit(completion(&first, b""))
            .unwrap()
            .unwrap();
        assert_eq!(first_done.seqnum, 1);
        assert_eq!(relay.in_flight(), 0);
        assert_eq!(relay.stats().completed, 2);
    }

    #[test]
    fn core_rejects_duplicate_seqnums_and_late_completions() {
        let relay = UsbIpRelay::new(UsbIpRelayConfig::default()).unwrap();
        let request = submit(9, 1, b"x");
        relay.register_submit(&request, 0).unwrap();
        assert_eq!(
            relay.register_submit(&request, 0).unwrap_err(),
            UsbIpRelayError::DuplicateSeqnum { seqnum: 9 }
        );

        let unlink = UsbIpCmdUnlink::new(10, 1, 9);
        assert_eq!(relay.unlink(&unlink).unwrap().unwrap().seqnum, 9);
        assert!(relay.unlink(&unlink).unwrap().is_none());
        assert!(relay
            .complete_submit(completion(&request, b""))
            .unwrap()
            .is_none());
        assert_eq!(relay.stats().unlink_not_found, 1);
        assert_eq!(relay.stats().late_completions, 1);
        assert_eq!(
            UsbIpUrbCommand::from_raw(USBIP_CMD_SUBMIT),
            Some(UsbIpUrbCommand::CmdSubmit)
        );
    }

    #[test]
    fn core_rejects_mismatched_devid_without_losing_the_target() {
        let relay = UsbIpRelay::new(UsbIpRelayConfig::default()).unwrap();
        let request = submit(4, 1, b"x");
        relay.register_submit(&request, 0).unwrap();
        let unlink = UsbIpCmdUnlink::new(5, 2, 4);
        assert_eq!(
            relay.unlink(&unlink).unwrap_err(),
            UsbIpRelayError::MismatchedDevid {
                seqnum: 4,
                expected: 1,
                actual: 2,
            }
        );
        assert!(relay.contains(4));
        let mut wrong = completion(&request, b"");
        wrong.devid = 2;
        assert!(matches!(
            relay.complete_submit(wrong),
            Err(UsbIpRelayError::MismatchedDevid { .. })
        ));
        assert!(!relay.contains(4));
    }

    #[test]
    fn no_kernel_data_endpoint_is_typed_and_never_opens_an_endpoint() {
        let seam = NoUsbIpKernelDataEndpoint;
        assert!(!seam.available());
        let attachment = UsbIpAttachment {
            host: "192.0.2.10".to_owned(),
            bus_id: "1-1".to_owned(),
            port: Some(7),
            vendor_id: None,
            product_id: None,
        };
        let error = seam.open(&attachment).err().unwrap();
        assert_eq!(error.kind(), "usbip-kernel-required");
        assert!(error.to_string().contains("vhci_hcd"));
    }
}
