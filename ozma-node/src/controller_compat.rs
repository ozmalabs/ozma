// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Controller compatibility surface.
//!
//! The Python controller drives node input over a small UDP protocol on the
//! HID port (7331 by default) and reads `GET /api/v1/connection` for a status
//! summary. This module owns both:
//!
//! * [`HidPacketDecoder`] turns the controller's HID v1 packets into
//!   [`ozma_protocol::InputEvent`] values.
//! * [`HidUdpListener`] receives packets and submits events to a shared
//!   [`ozma_input::InputSink`].
//! * [`connection_router`] exposes the compatibility status endpoint, which is
//!   mounted behind the node API's bearer-token guard.
//!
//! The wire format mirrors `softnode/evdev_input.py`:
//!
//! | Type | Payload |
//! |---|---|
//! | `0x01` keyboard | `[modifiers, reserved, key1..key6]` (8 bytes) |
//! | `0x02` absolute pointer | `[buttons, x_lo, x_hi, y_lo, y_hi, scroll]` |
//! | `0x05` relative pointer | `[buttons, dx_lo, dx_hi, dy_lo, dy_hi, scroll]` |
//!
//! This is deliberately a compatibility adapter, not an architecture. It can
//! be removed once clients speak the native session input channel.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::stream;
use ozma_audio::{
    next_eq_sink_name, validate_profile, AudioError, AudioPlaybackSpec, AudioProfile, AudioRouter,
    AudioSourceProfile, RoomEqSpec, RoomProfileStore, SweepOutcome, SweepProfile, SweepRequest,
    DEFAULT_EQ_CHANNELS, DEFAULT_EQ_RATE,
};
use ozma_encode::{
    backend_name, CodecOverrideState, CodecPreference, EncoderOverride, EncoderRegistry,
    OverrideReceipt,
};
use ozma_input::{linux_keycode_to_hid_usage, SharedSink, SubmitReport};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use ozma_protocol::{
    codes, ApiError, CaptureKind, Codec, EncodeCapability, EncoderBackend, MediaFormat,
};
use ozma_usb::MediaFormat as UsbMediaFormat;
use ozma_usb::{
    is_safe_identifier, polling_transport_factory, BoxedMassStorageTransport, FunctionFsConfig,
    FunctionFsEndpoint, FunctionFsHandoff, FunctionFsHandoffPhase, FunctionFsMediaDaemon,
    FunctionFsMount, FunctionFsStatus, FunctionStatus, GadgetStatus, IdleBulkTransport,
    MassStorageDevice, MassStorageImage, MassStorageLoop, MassStorageLoopConfig,
    MassStorageLoopStats, MassStorageOverlayStatus, MassStoragePersistPolicy, MediaDaemonConfig,
    MediaDaemonHandle, MediaRestartPolicy, MediaRestartTracker, MediaState, NbdExportCatalog,
    NbdExportSource, NbdOverlayPolicy, NbdServer, NbdServerConfig, NbdServerHandle, NbdServerStats,
    OverlayConfig, Uac2Status, UsbError, UsbFunctionKind, UsbGadgetCapabilities,
    UsbGadgetController, UsbMediaHotplugKind, UsbMediaHotplugLog, VirtualMedia,
    MASS_STORAGE_DEFAULT_TIMEOUT_MS, MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS,
    SCSI_MAX_TRANSFER_BYTES, SCSI_SECTOR_BYTES,
};
#[cfg(test)]
use ozma_usb::{MassStorageBulkTransport, MassStorageCommandOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::display_broker::ConsoleBroker;
use crate::display_compat::{DisplayInfo, SnapshotControl, SnapshotError, SnapshotFrame};
use crate::display_control::{DisplayControl, DisplayStatus, SelectConsoleRequest};
use crate::hls::{HlsController, HlsEncoder, HlsError, HlsStatus};
use crate::keyboard_layout::{
    key_event, resolve_compose_sequence, resolve_dead_key_sequence, CharOutcome, KeyChord,
    KeyboardLayout,
};
use crate::multi_hls::{
    CodecFanoutTarget, ConsoleHlsStreamStatus, ControllerHlsPipeline, MultiConsoleCodecOutcome,
    MultiConsoleHls, MultiConsoleHlsStatus,
};
use crate::multi_webrtc::{MultiConsoleWebrtc, MultiConsoleWebrtcStatus};
use crate::rpa::{key_events, modifier_bit, named_key_usage, OcrControl, OcrError};
use crate::usb_uac2::{Uac2BridgeError, Uac2BridgeShared};
use crate::video::{
    VideoCodec, VideoEncoding, VideoParams, VideoStatus, VideoStreams, ALIAS as VIDEO_ALIAS,
    ROUTE as VIDEO_ROUTE,
};
use crate::webrtc::{WebRtcControl, WebRtcError, WebRtcGeometryStatus};

/// HID packet type for an 8-byte boot keyboard report.
pub const PACKET_KEYBOARD: u8 = 0x01;
/// HID packet type for an absolute pointer report.
pub const PACKET_MOUSE_ABSOLUTE: u8 = 0x02;
/// HID packet type for a relative pointer report.
pub const PACKET_MOUSE_RELATIVE: u8 = 0x05;

/// Sink kind reported when the input events reach the USB HID gadget.
pub const SINK_HID_GADGET: &str = "hid-gadget";
/// Sink kind reported when input is injected through `/dev/uinput`.
pub const SINK_UINPUT: &str = "uinput";
/// Sink kind reported when input is injected through QMP `input-send-event`.
pub const SINK_QMP: &str = "qmp";
/// Sink kind reported when input is validated and recorded but not injected.
pub const SINK_FAKE_RECORDING: &str = "fake-recording";

/// Largest text accepted by `POST /input/type`, in Unicode scalar values.
pub const MAX_TYPE_TEXT_CHARS: usize = 4096;
/// Delay between typed characters so the guest sees separate keystrokes.
pub const TYPE_CHAR_DELAY: Duration = Duration::from_millis(15);
/// Minimum time a key is held down while typing.
///
/// A QEMU USB keyboard only reports its state when the guest polls the
/// interrupt endpoint (10 ms for the emulated device). Sending key-down and
/// key-up back-to-back lets one poll coalesce them into a single released
/// report, losing the character; if the release is the report that is lost, the
/// guest instead auto-repeats the key. Holding for at least two poll intervals
/// makes every press observable on its own.
pub const TYPE_KEY_HOLD: Duration = Duration::from_millis(20);
/// How long a synthesized click holds the button before releasing it.
pub const CLICK_HOLD: Duration = Duration::from_millis(20);
/// Largest absolute pointer coordinate accepted by `POST /input/mouse`.
pub const MAX_POINTER_COORDINATE: i64 = 32767;

/// HID usage id of the first keyboard modifier (left control).
const HID_MODIFIER_FIRST: u32 = 0xE0;

/// Time allowed for the FunctionFS media daemon to write `ep0` descriptors
/// before the handoff immediately binds the UDC.
const FUNCTIONFS_DAEMON_START_TIMEOUT_MS: u64 = 5_000;

/// Poll interval while waiting for the daemon's `ep0` writes.
const FUNCTIONFS_DAEMON_START_POLL_MS: u64 = 10;
/// Number of modifier bits in the boot keyboard report.
const HID_MODIFIER_COUNT: usize = 8;

/// A packet the compatibility decoder cannot process.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompatError {
    /// A zero-length datagram.
    #[error("empty HID packet")]
    Empty,
    /// A datagram shorter than the report type requires.
    #[error("short {kind} packet: expected at least {expected} payload bytes, got {actual}")]
    Short {
        /// Human-readable report kind.
        kind: &'static str,
        /// Minimum required payload length (excluding the type byte).
        expected: usize,
        /// Actual payload length.
        actual: usize,
    },
    /// A packet type this adapter does not implement.
    #[error("unknown HID packet type 0x{0:02x}")]
    UnknownType(u8),
}

/// Stateful decoder for the controller's HID v1 packets.
///
/// Keyboard and pointer state is retained between packets so only changes are
/// emitted. A duplicate report therefore produces no events. The decoder never
/// panics on malformed input; short and unknown packets return
/// [`CompatError`] and leave the retained state untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HidPacketDecoder {
    modifiers: u8,
    keys: BTreeSet<u8>,
    buttons: u8,
}

impl HidPacketDecoder {
    /// A decoder with no held keys or buttons.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one datagram into zero or more input events.
    ///
    /// The returned events are already diffed against the previous report. The
    /// decoder state only advances when the packet is valid.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<InputEvent>, CompatError> {
        let Some((&kind, payload)) = packet.split_first() else {
            return Err(CompatError::Empty);
        };

        match kind {
            PACKET_KEYBOARD => self.decode_keyboard(payload),
            PACKET_MOUSE_ABSOLUTE => self.decode_pointer(payload, false),
            PACKET_MOUSE_RELATIVE => self.decode_pointer(payload, true),
            other => Err(CompatError::UnknownType(other)),
        }
    }

    /// Forget held keys and buttons without emitting release events.
    pub fn reset(&mut self) {
        self.modifiers = 0;
        self.keys.clear();
        self.buttons = 0;
    }

    fn decode_keyboard(&mut self, payload: &[u8]) -> Result<Vec<InputEvent>, CompatError> {
        const REPORT_LEN: usize = 8;
        if payload.len() < REPORT_LEN {
            return Err(CompatError::Short {
                kind: "keyboard",
                expected: REPORT_LEN,
                actual: payload.len(),
            });
        }

        let modifiers = payload[0];
        let keys: BTreeSet<u8> = payload[2..REPORT_LEN]
            .iter()
            .copied()
            .filter(|&usage| usage != 0)
            .collect();

        let mut events = Vec::new();
        for bit in 0..HID_MODIFIER_COUNT {
            let mask = 1u8 << bit;
            let was_down = self.modifiers & mask != 0;
            let is_down = modifiers & mask != 0;
            if was_down != is_down {
                events.push(InputEvent::Key {
                    code: HID_MODIFIER_FIRST + bit as u32,
                    down: is_down,
                    // Modifier usage ids are represented by the boot report's
                    // modifier byte, not by the per-event modifier mask.
                    modifiers: KeyModifiers(0),
                });
            }
        }

        // Release before press, matching the Python translator.
        //
        // Non-modifier key events deliberately carry no per-event modifier
        // mask. `ozma-input` ORs `client_modifiers` into its effective state,
        // and only the dedicated modifier key events (usages 0xE0..=0xE7) can
        // clear that state. Copying the boot report's modifier byte here would
        // leave modifiers stuck after the controller releases them.
        for &usage in self.keys.difference(&keys) {
            events.push(InputEvent::Key {
                code: u32::from(usage),
                down: false,
                modifiers: KeyModifiers(0),
            });
        }
        for &usage in keys.difference(&self.keys) {
            events.push(InputEvent::Key {
                code: u32::from(usage),
                down: true,
                modifiers: KeyModifiers(0),
            });
        }

        self.modifiers = modifiers;
        self.keys = keys;
        Ok(events)
    }

    fn decode_pointer(
        &mut self,
        payload: &[u8],
        relative: bool,
    ) -> Result<Vec<InputEvent>, CompatError> {
        // Five bytes cover buttons plus the two deltas/coordinates. The scroll
        // byte is optional, matching the Python translator's tolerance.
        const PREFIX_LEN: usize = 5;
        if payload.len() < PREFIX_LEN {
            return Err(CompatError::Short {
                kind: if relative {
                    "relative pointer"
                } else {
                    "absolute pointer"
                },
                expected: PREFIX_LEN,
                actual: payload.len(),
            });
        }

        let buttons = payload[0];
        let scroll = payload.get(5).copied().unwrap_or(0) as i8;

        let mut events = Vec::new();
        if relative {
            // Relative deltas are signed 16-bit little-endian.
            let dx = i16::from_le_bytes([payload[1], payload[2]]);
            let dy = i16::from_le_bytes([payload[3], payload[4]]);
            events.push(InputEvent::PointerRelative {
                dx: i32::from(dx),
                dy: i32::from(dy),
            });
        } else {
            // Absolute coordinates are unsigned 16-bit little-endian on the
            // wire; the Python controller reads them as u16 as well.
            let x = u16::from_le_bytes([payload[1], payload[2]]);
            let y = u16::from_le_bytes([payload[3], payload[4]]);
            events.push(InputEvent::PointerAbsolute {
                display_id: None,
                x: i32::from(x),
                y: i32::from(y),
            });
        }
        events.extend(button_events(self.buttons, buttons));
        if scroll != 0 {
            events.push(InputEvent::Wheel {
                dx: 0,
                dy: i32::from(scroll),
            });
        }

        self.buttons = buttons;
        Ok(events)
    }
}

/// Map the controller's button bitfield to normalized pointer buttons.
///
/// Bits 1, 2, 4, 8, and 16 map to left, right, middle, back, and forward.
fn button_events(previous: u8, current: u8) -> Vec<InputEvent> {
    const BUTTONS: [(u8, PointerButton); 5] = [
        (0x01, PointerButton::Left),
        (0x02, PointerButton::Right),
        (0x04, PointerButton::Middle),
        (0x08, PointerButton::Back),
        (0x10, PointerButton::Forward),
    ];

    let mut events = Vec::new();
    for (mask, button) in BUTTONS {
        let was_down = previous & mask != 0;
        let is_down = current & mask != 0;
        if was_down != is_down {
            events.push(InputEvent::Button {
                button,
                down: is_down,
            });
        }
    }
    events
}

/// Counters and last-error state for the UDP HID compatibility listener.
///
/// Cloning is cheap: every clone shares the same atomics.
#[derive(Clone, Debug, Default)]
pub struct CompatStats {
    inner: Arc<CompatStatsInner>,
}

#[derive(Debug, Default)]
struct CompatStatsInner {
    packets_received: AtomicU64,
    packets_rejected: AtomicU64,
    events: AtomicU64,
    submit_errors: AtomicU64,
    listener_running: AtomicBool,
    bound_port: AtomicU16,
    last_error: Mutex<Option<String>>,
}

impl CompatStats {
    /// Record one received datagram, valid or not.
    pub fn record_packet(&self) {
        self.inner.packets_received.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a packet the decoder rejected and remember why.
    pub fn record_rejected(&self, error: impl fmt::Display) {
        self.inner.packets_rejected.fetch_add(1, Ordering::Relaxed);
        self.set_last_error(error);
    }

    /// Record how many events a packet produced.
    pub fn record_events(&self, count: usize) {
        self.inner.events.fetch_add(count as u64, Ordering::Relaxed);
    }

    /// Record a sink rejection. The listener keeps running.
    pub fn record_submit_error(&self, error: impl fmt::Display) {
        self.inner.submit_errors.fetch_add(1, Ordering::Relaxed);
        self.set_last_error(error);
    }

    /// Mark the listener running or stopped.
    pub fn set_listener_running(&self, running: bool) {
        self.inner
            .listener_running
            .store(running, Ordering::Relaxed);
    }

    /// Record the actual bound port (useful when configured with port 0).
    pub fn set_bound_port(&self, port: u16) {
        self.inner.bound_port.store(port, Ordering::Relaxed);
    }

    /// Record a listener-level failure and mark the listener stopped.
    pub fn record_listener_error(&self, error: impl fmt::Display) {
        self.set_listener_running(false);
        self.set_last_error(error);
    }

    /// The most recent decoded/sink/listener error, if any.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.inner.last_error).clone()
    }

    /// A serializable snapshot for `GET /api/v1/connection`.
    pub fn snapshot(&self) -> HidListenerSnapshot {
        HidListenerSnapshot {
            status: if self.inner.listener_running.load(Ordering::Relaxed) {
                "running"
            } else {
                "stopped"
            }
            .to_owned(),
            bound_port: self.inner.bound_port.load(Ordering::Relaxed),
            packets_received: self.inner.packets_received.load(Ordering::Relaxed),
            packets_rejected: self.inner.packets_rejected.load(Ordering::Relaxed),
            events: self.inner.events.load(Ordering::Relaxed),
            submit_errors: self.inner.submit_errors.load(Ordering::Relaxed),
            last_error: self.last_error(),
        }
    }

    fn set_last_error(&self, error: impl fmt::Display) {
        *lock(&self.inner.last_error) = Some(error.to_string());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// UDP HID listener counters as exposed on the connection endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HidListenerSnapshot {
    /// `running` when the socket is bound and the receive loop is active.
    pub status: String,
    /// Actual bound UDP port (resolves a configured port of 0).
    pub bound_port: u16,
    /// Datagrams received, including malformed ones.
    pub packets_received: u64,
    /// Datagrams the decoder rejected.
    pub packets_rejected: u64,
    /// Input events produced by accepted packets.
    pub events: u64,
    /// Events the shared sink rejected.
    pub submit_errors: u64,
    /// The most recent error string, or `null`.
    pub last_error: Option<String>,
}

/// A bound UDP HID listener.
///
/// [`HidUdpListener::bind`] separates binding (which callers may want to do
/// before spawning, and which reports the real port when binding port 0) from
/// [`HidUdpListener::run`].
pub struct HidUdpListener {
    socket: UdpSocket,
    decoder: HidPacketDecoder,
    sink: SharedSink,
    stats: CompatStats,
}

impl HidUdpListener {
    /// Bind `0.0.0.0:<port>` and mark the listener running.
    ///
    /// Port 0 is allowed; use [`HidUdpListener::local_addr`] to read the port
    /// the kernel selected.
    pub async fn bind(port: u16, sink: SharedSink, stats: CompatStats) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port)).await?;
        let local = socket.local_addr()?;
        stats.set_bound_port(local.port());
        stats.set_listener_running(true);
        info!(port = local.port(), "UDP HID listener bound");
        Ok(Self {
            socket,
            decoder: HidPacketDecoder::new(),
            sink,
            stats,
        })
    }

    /// The actual bound address (including the kernel-selected port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Receive and submit packets until the socket fails or the task is
    /// aborted. Malformed packets and sink rejections never stop the loop.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut buffer = [0u8; 4096];
        loop {
            let (length, peer) = match self.socket.recv_from(&mut buffer).await {
                Ok(received) => received,
                Err(error) => {
                    // A receive error is not necessarily fatal (for example a
                    // transient ICMP port-unreachable on Linux). Record it and
                    // keep serving.
                    warn!(error = %error, "UDP HID receive error");
                    self.stats.record_rejected(error);
                    continue;
                }
            };
            self.stats.record_packet();

            let events = match self.decoder.decode(&buffer[..length]) {
                Ok(events) => events,
                Err(error) => {
                    debug!(peer = %peer, bytes = length, error = %error, "Rejected HID packet");
                    self.stats.record_rejected(&error);
                    continue;
                }
            };

            self.stats.record_events(events.len());
            for event in events {
                if let Err(error) = self.sink.submit(&event) {
                    warn!(peer = %peer, error = %error, "HID input sink rejected an event");
                    self.stats.record_submit_error(&error);
                }
            }
        }
    }
}

impl Drop for HidUdpListener {
    fn drop(&mut self) {
        self.stats.set_listener_running(false);
    }
}

/// Bind and run the UDP HID listener until it fails.
pub async fn run_hid_listener(
    port: u16,
    sink: SharedSink,
    stats: CompatStats,
) -> anyhow::Result<()> {
    HidUdpListener::bind(port, sink, stats).await?.run().await
}

/// Honest codec information exposed by `GET /codec`.
///
/// Controller-facing codec state.
///
/// One HLS pipeline as reported by `GET /codec`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CodecPipelineStatus {
    /// `active` for the single `--hls` path or `console-<N>` for a per-console
    /// pipeline.
    pub id: String,
    /// `active-hls` or `per-console-hls`.
    pub kind: String,
    /// Console index for a per-console pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<u32>,
    /// True for the active console / active single stream.
    pub active: bool,
    /// Pipeline status (`running`, `failed`, `not-started`, ...).
    pub status: String,
    /// Encoder this pipeline currently runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// Bitrate this pipeline currently targets, in kbps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
}

/// Last codec fan-out state for `GET /codec`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CodecFanOutStatus {
    /// A per-console HLS manager is attached.
    pub enabled: bool,
    /// Running per-console pipelines right now.
    pub running: usize,
    /// Pipelines targeted by the last fan-out request.
    pub targeted: usize,
    /// True only when the last fan-out restarted every target with the
    /// requested encoder and no fallback.
    pub applied: bool,
    /// Rollback result of the last failed fan-out; `null` before a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<bool>,
    /// Pipeline labels that failed the last fan-out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    /// Pipeline labels whose rollback failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_failed: Vec<String>,
    /// First failure/rollback message from the last fan-out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `current_encoder` and `available` describe the encoders the node can
/// actually run. `override` is the accepted request, `receipt` is the policy
/// evaluation that was stored with it, and `pending_restart` says the running
/// HLS pipeline has **not** restarted into that selection yet. `applied` is
/// true only after every targeted HLS pipeline restarted with the requested
/// encoder. `pipelines` names the encoder each active/per-console pipeline runs,
/// and `fan_out` reports the last multi-console fan-out result. WebRTC keeps its
/// existing encoder; neither `applied` nor `current_encoder` claims a WebRTC
/// switch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CodecStatus {
    /// The encoder currently used by the HLS pipeline, or `null` with no video.
    pub current_encoder: Option<String>,
    /// The controller's accepted override, or `null` for automatic selection.
    #[serde(rename = "override")]
    pub override_setting: Option<EncoderOverride>,
    /// Video codecs the node advertises, in capability order.
    pub available: Vec<String>,
    /// Unique encoder backends advertised for those codecs (for example
    /// `software`, `vaapi`, `nvenc`).
    #[serde(default)]
    pub backends: Vec<String>,
    /// Widest source/encoder envelope advertised, not a hard-coded 1080p.
    #[serde(default)]
    pub max_width: u32,
    /// Tallest source/encoder envelope advertised.
    #[serde(default)]
    pub max_height: u32,
    /// Highest frame rate the advertised encoders can take from this source.
    #[serde(default)]
    pub max_fps: u32,
    /// True only when the configured HLS pipeline is a validated 10-bit HDR
    /// target; never inferred from a codec name or a 4K size.
    #[serde(default)]
    pub hdr: bool,
    /// Raw pixel formats this node can feed/encode (`yuv420p`, plus `p010`
    /// when 10-bit is configured).
    #[serde(default)]
    pub pixel_formats: Vec<String>,
    /// Configured NVENC CUDA ordinal, or `null` when no GPU was configured.
    ///
    /// This is the stored preference, not proof that the current encoder is
    /// NVENC; `current_encoder` says which encoder is actually running.
    #[serde(default)]
    pub nvenc_gpu: Option<u32>,
    /// The last stored policy receipt, or `null` when none was evaluated.
    pub receipt: Option<OverrideReceipt>,
    /// True while a stored override has not been applied by a pipeline restart.
    pub pending_restart: bool,
    /// True only when every targeted HLS pipeline restarted with the requested
    /// encoder (no fallback, no partial success).
    pub applied: bool,
    /// Encoder/status for each active or per-console HLS pipeline.
    #[serde(default)]
    pub pipelines: Vec<CodecPipelineStatus>,
    /// Last per-console fan-out state.
    #[serde(default)]
    pub fan_out: CodecFanOutStatus,
}

impl CodecStatus {
    /// No video capability: no encoder is claimed.
    pub fn not_configured() -> Self {
        Self::default()
    }

    /// Build status from the codecs in [`ozma_protocol::NodeCapabilities`].
    pub fn from_codecs(codecs: impl IntoIterator<Item = Codec>) -> Self {
        let available = video_codec_names(codecs);
        let current_encoder = available.first().cloned();
        Self {
            current_encoder,
            override_setting: None,
            available,
            backends: Vec::new(),
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            hdr: false,
            pixel_formats: Vec::new(),
            nvenc_gpu: None,
            receipt: None,
            pending_restart: false,
            applied: false,
            pipelines: Vec::new(),
            fan_out: CodecFanOutStatus::default(),
        }
    }
}

fn video_codec_names(codecs: impl IntoIterator<Item = Codec>) -> Vec<String> {
    let mut available: Vec<String> = Vec::new();
    for codec in codecs {
        if !codec.is_video() {
            continue;
        }
        let name = codec.as_str().to_owned();
        if !available.contains(&name) {
            available.push(name);
        }
    }
    available
}

fn video_backend_names(backends: impl IntoIterator<Item = EncoderBackend>) -> Vec<String> {
    let mut available: Vec<String> = Vec::new();
    for backend in backends {
        let name = backend_name(backend).to_owned();
        if !available.contains(&name) {
            available.push(name);
        }
    }
    available
}

/// Runtime codec override control for `GET`/`POST /codec`.
///
/// Holds the real [`EncoderRegistry`], the last accepted
/// [`CodecOverrideState`], and an optional [`HlsController`]. When HLS is
/// running, applying an override stops that pipeline and restarts it with the
/// mapped software encoder; only then is `applied: true` returned. When HLS is
/// not running the override is stored for a future start and the response keeps
/// `applied: false, pending_restart: true`. WebRTC is never restarted by this
/// path and keeps its existing encoder.
#[derive(Clone)]
pub struct CodecControl {
    registry: EncoderRegistry,
    state: Arc<Mutex<StoredCodecState>>,
    format: MediaFormat,
    hls: Option<HlsController>,
    multi_console_hls: Option<MultiConsoleHls>,
}

#[derive(Clone, Debug, Default)]
struct StoredCodecState {
    override_state: CodecOverrideState,
    /// The HLS encoder the running pipeline was actually restarted into.
    applied_encoder: Option<HlsEncoder>,
    /// True only when the last apply restarted every target with the requested
    /// encoder (no fallback).
    applied_ok: bool,
    /// Last fan-out result, retained for `GET /codec`.
    fan_out: Option<StoredFanOut>,
}

#[derive(Clone, Debug, Default)]
struct StoredFanOut {
    targeted: usize,
    applied: bool,
    rolled_back: Option<bool>,
    failed: Vec<String>,
    rollback_failed: Vec<String>,
    error: Option<String>,
}

impl CodecControl {
    /// Build a control from a probed/registered encoder set.
    pub fn new(registry: EncoderRegistry, format: MediaFormat) -> Self {
        Self {
            registry,
            state: Arc::new(Mutex::new(StoredCodecState::default())),
            format,
            hls: None,
            multi_console_hls: None,
        }
    }

    /// A control with no registry and no video capability.
    pub fn not_configured() -> Self {
        Self::new(
            EncoderRegistry::new(),
            MediaFormat::new(Codec::H264, 1920, 1080, 30, 4_000),
        )
    }

    /// Attach the HLS controller that `POST /codec` may restart.
    ///
    /// `None` keeps every mutation pending for a future pipeline start.
    pub fn with_hls(mut self, hls: Option<HlsController>) -> Self {
        self.hls = hls;
        self
    }

    /// Attach the bounded per-console HLS manager that `POST /codec` fans out
    /// to. `None` keeps the historical single active-HLS behavior.
    pub fn with_multi_console_hls(mut self, manager: Option<MultiConsoleHls>) -> Self {
        self.multi_console_hls = manager;
        self
    }

    /// The registry this control evaluates against.
    pub fn registry(&self) -> &EncoderRegistry {
        &self.registry
    }

    /// True when at least one real video encoder is advertised.
    pub fn is_available(&self) -> bool {
        self.registry
            .capabilities()
            .iter()
            .any(|capability| capability.codec.is_video())
    }

    /// Status derived from the registry, stored override state, and applied HLS
    /// encoder.
    ///
    /// This synchronous form includes the active single-HLS pipeline and the
    /// last stored fan-out result. [`Self::status_async`] additionally reads the
    /// live per-console manager, so the HTTP routes use that form.
    pub fn status(&self) -> CodecStatus {
        let available = video_codec_names(
            self.registry
                .capabilities()
                .iter()
                .map(|capability| capability.codec),
        );
        let backends = video_backend_names(
            self.registry
                .capabilities()
                .iter()
                .map(|capability| capability.backend),
        );
        let fallback_encoder = available.first().cloned();
        let state = lock(&self.state).clone();
        let applied = state
            .applied_encoder
            .map(|encoder| encoder.ffmpeg_name().to_owned());
        let pending_restart =
            state.override_state.requested.is_some() && state.applied_encoder.is_none();
        let hls_config = self.hls.as_ref().map(HlsController::config);
        let max_width = self
            .registry
            .capabilities()
            .iter()
            .map(|capability| capability.max_width)
            .max()
            .unwrap_or(self.format.width)
            .max(self.format.width);
        let max_height = self
            .registry
            .capabilities()
            .iter()
            .map(|capability| capability.max_height)
            .max()
            .unwrap_or(self.format.height)
            .max(self.format.height);
        let max_fps = self
            .registry
            .capabilities()
            .iter()
            .map(|capability| capability.max_fps)
            .max()
            .unwrap_or(self.format.fps)
            .max(self.format.fps);
        let hdr = hls_config
            .as_ref()
            .is_some_and(|config| config.hdr && config.pixel_format.is_10bit());
        let pixel_formats = if hls_config
            .as_ref()
            .is_some_and(|config| config.pixel_format.is_10bit())
        {
            vec!["yuv420p".to_owned(), "p010".to_owned()]
        } else {
            vec!["yuv420p".to_owned()]
        };
        let pipelines = self
            .hls
            .as_ref()
            .map(|hls| {
                let status = hls.status();
                let config = hls.config();
                vec![CodecPipelineStatus {
                    id: "active".to_owned(),
                    kind: "active-hls".to_owned(),
                    console: None,
                    active: true,
                    status: status.status_name().to_owned(),
                    encoder: Some(config.video_encoder.ffmpeg_name().to_owned()),
                    bitrate_kbps: Some(config.bitrate_kbps),
                }]
            })
            .unwrap_or_default();
        let fan_out = match state.fan_out.as_ref() {
            Some(fan_out) => CodecFanOutStatus {
                enabled: self.multi_console_hls.is_some(),
                running: 0,
                targeted: fan_out.targeted,
                applied: fan_out.applied,
                rolled_back: fan_out.rolled_back,
                failed: fan_out.failed.clone(),
                rollback_failed: fan_out.rollback_failed.clone(),
                error: fan_out.error.clone(),
            },
            None => CodecFanOutStatus {
                enabled: self.multi_console_hls.is_some(),
                ..CodecFanOutStatus::default()
            },
        };
        CodecStatus {
            current_encoder: applied.clone().or(fallback_encoder),
            override_setting: state.override_state.requested,
            available,
            backends,
            max_width,
            max_height,
            max_fps,
            hdr,
            pixel_formats,
            nvenc_gpu: hls_config.as_ref().and_then(|config| config.nvenc_gpu),
            receipt: state.override_state.receipt,
            pending_restart,
            applied: applied.is_some() && state.applied_ok,
            pipelines,
            fan_out,
        }
    }

    /// Live status including every per-console HLS pipeline.
    pub async fn status_async(&self) -> CodecStatus {
        let mut status = self.status();
        let Some(manager) = self.multi_console_hls.as_ref() else {
            return status;
        };
        let manager_status = manager.status().await;
        status.fan_out.enabled = true;
        status.fan_out.running = manager.running_pipeline_count().await;
        status
            .pipelines
            .retain(|pipeline| pipeline.kind != "per-console-hls");
        status
            .pipelines
            .extend(
                manager_status
                    .consoles
                    .into_iter()
                    .map(|console| CodecPipelineStatus {
                        id: format!("console-{}", console.console),
                        kind: "per-console-hls".to_owned(),
                        console: Some(console.console),
                        active: console.active,
                        status: console.status,
                        encoder: console.encoder,
                        bitrate_kbps: console.bitrate_kbps,
                    }),
            );
        status
    }

    /// Evaluate and store a controller codec request, restarting HLS when it is
    /// running.
    ///
    /// # Errors
    ///
    /// A malformed request, an unknown codec/backend/bitrate, or a selection
    /// the registry cannot honour returns a typed 400 [`ApiError`] whose
    /// `details` list the real capabilities. A selection the HLS ffmpeg path
    /// cannot run (for example a hardware backend), or a failed HLS restart,
    /// returns a typed 502 [`CodecApplyError`]. No state is stored on error.
    pub async fn apply(
        &self,
        request: &CodecRequest,
    ) -> Result<serde_json::Value, CodecApplyError> {
        let preference = self
            .preference_from_request(request)
            .map_err(CodecApplyError::invalid)?;
        let request = EncoderOverride::new(preference.clone()).with_source("controller");
        let receipt = self
            .registry
            .override_receipt(&request, &self.format)
            .map_err(CodecApplyError::invalid)?;

        // HLS is the only pipeline this control can restart. WebRTC keeps its
        // existing encoder and is never claimed as switched.
        let selected_capability = receipt.selected.as_ref();
        let mapped_encoder =
            selected_capability.and_then(|capability| hls_encoder_for_selection(capability).ok());
        let selected_backend = selected_capability
            .map(|capability| backend_name(capability.backend))
            .unwrap_or("software");
        let single_running = self.hls.as_ref().is_some_and(HlsController::is_running);
        let manager_running = match self.multi_console_hls.as_ref() {
            Some(manager) => manager.has_running_pipelines().await,
            None => false,
        };

        if !single_running && !manager_running {
            let hls_status = self
                .hls
                .as_ref()
                .map(|hls| hls.status().status_name())
                .unwrap_or("not-configured");
            *lock(&self.state) = StoredCodecState {
                override_state: CodecOverrideState::from_receipt(request, receipt.clone()),
                applied_encoder: None,
                applied_ok: false,
                fan_out: Some(StoredFanOut {
                    targeted: 0,
                    applied: false,
                    rolled_back: None,
                    failed: Vec::new(),
                    rollback_failed: Vec::new(),
                    error: None,
                }),
            };
            return Ok(serde_json::json!({
                "ok": true,
                "applied": false,
                "pending_restart": true,
                "encoder": Value::Null,
                "requested_encoder": mapped_encoder.map(HlsEncoder::ffmpeg_name),
                "backend": selected_backend,
                "vaapi_device": Value::Null,
                "nvenc_gpu": Value::Null,
                "fallback": false,
                "fallback_reason": Value::Null,
                "hls": hls_status,
                "receipt": receipt,
                "pipelines": Vec::<CodecPipelineStatus>::new(),
                "fan_out": CodecFanOutStatus {
                    enabled: self.multi_console_hls.is_some(),
                    targeted: 0,
                    ..CodecFanOutStatus::default()
                },
            }));
        }

        let selected = selected_capability.ok_or_else(|| {
            CodecApplyError::pipeline(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                "the codec policy accepted the request without selecting an encoder",
            ))
        })?;
        let encoder = hls_encoder_for_selection(selected).map_err(CodecApplyError::pipeline)?;

        // Multi-console fan-out is the wider path. It also includes the active
        // single `--hls` pipeline when one is running, so a node that has both
        // does not leave one encoder behind.
        if manager_running {
            let manager = self
                .multi_console_hls
                .as_ref()
                .expect("manager_running implies a manager");
            let mut extras = Vec::new();
            if single_running {
                if let Some(hls) = self.hls.as_ref() {
                    extras.push(CodecFanoutTarget {
                        id: "active".to_owned(),
                        kind: "active-hls",
                        console: None,
                        active: true,
                        pipeline: Arc::new(ControllerHlsPipeline::new(hls.clone())),
                    });
                }
            }
            let outcome = manager
                .apply_codec(
                    encoder,
                    preference.bitrate_kbps,
                    preference.allow_software_fallback,
                    extras,
                )
                .await;
            let stored = StoredFanOut {
                targeted: outcome.targeted,
                applied: outcome.applied,
                rolled_back: outcome.error.is_some().then_some(outcome.rolled_back),
                failed: outcome.failed.clone(),
                rollback_failed: outcome.rollback_failed.clone(),
                error: outcome.error.clone(),
            };
            if !outcome.failed.is_empty() || outcome.error.is_some() {
                // Keep the previous accepted override: this request did not
                // apply. Record the attempted fan-out so GET /codec is honest
                // about the rollback result.
                lock(&self.state).fan_out = Some(stored);
                return Err(CodecApplyError::pipeline(codec_fanout_error(
                    &outcome, encoder,
                )));
            }
            let applied_encoder = outcome.applied_encoder;
            *lock(&self.state) = StoredCodecState {
                override_state: CodecOverrideState::from_receipt(request, receipt.clone()),
                applied_encoder,
                applied_ok: outcome.applied,
                fan_out: Some(stored),
            };
            let base = manager.base_config();
            let vaapi_device = encoder
                .is_vaapi()
                .then(|| base.vaapi_device.display().to_string());
            let nvenc_gpu = applied_encoder
                .is_some_and(HlsEncoder::is_nvenc)
                .then_some(base.nvenc_gpu)
                .flatten();
            let hls_status = manager.active_status().await.status_name();
            let fan_out = CodecFanOutStatus {
                enabled: true,
                running: manager.running_pipeline_count().await,
                targeted: outcome.targeted,
                applied: outcome.applied,
                rolled_back: outcome.error.is_some().then_some(outcome.rolled_back),
                failed: outcome.failed.clone(),
                rollback_failed: outcome.rollback_failed.clone(),
                error: outcome.error.clone(),
            };
            return Ok(serde_json::json!({
                "ok": true,
                "applied": outcome.applied,
                "pending_restart": false,
                "encoder": applied_encoder.map(HlsEncoder::ffmpeg_name),
                "requested_encoder": encoder.ffmpeg_name(),
                "backend": backend_name(selected.backend),
                "vaapi_device": vaapi_device,
                "nvenc_gpu": nvenc_gpu,
                "fallback": outcome.fallback,
                "fallback_reason": outcome.fallback.then_some("software_fallback"),
                "hls": hls_status,
                "receipt": receipt,
                "pipelines": outcome.pipelines,
                "fan_out": fan_out,
            }));
        }

        let hls = self
            .hls
            .as_ref()
            .expect("single_running implies an HLS controller");
        let vaapi_device = encoder
            .is_vaapi()
            .then(|| hls.config().vaapi_device.display().to_string());
        let outcome = hls
            .restart_with_encoder_fallback(
                encoder,
                preference.bitrate_kbps,
                preference.allow_software_fallback,
            )
            .await
            .map_err(|error| CodecApplyError::pipeline(codec_pipeline_error(error, encoder)))?;

        let applied = outcome.applied_encoder;
        let nvenc_gpu = applied.is_nvenc().then(|| hls.config().nvenc_gpu).flatten();
        let fallback_reason = outcome
            .fallback
            .as_ref()
            .map(|fallback| fallback.reason.clone());
        *lock(&self.state) = StoredCodecState {
            override_state: CodecOverrideState::from_receipt(request, receipt.clone()),
            applied_encoder: Some(applied),
            applied_ok: true,
            fan_out: None,
        };
        Ok(serde_json::json!({
            "ok": true,
            "applied": true,
            "pending_restart": false,
            "encoder": applied.ffmpeg_name(),
            "requested_encoder": encoder.ffmpeg_name(),
            "backend": backend_name(selected.backend),
            "vaapi_device": vaapi_device,
            "nvenc_gpu": nvenc_gpu,
            "fallback": fallback_reason.is_some(),
            "fallback_reason": fallback_reason,
            "hls": outcome.status.status_name(),
            "receipt": receipt,
            "pipelines": [CodecPipelineStatus {
                id: "active".to_owned(),
                kind: "active-hls".to_owned(),
                console: None,
                active: true,
                status: outcome.status.status_name().to_owned(),
                encoder: Some(applied.ffmpeg_name().to_owned()),
                bitrate_kbps: Some(preference.bitrate_kbps.unwrap_or(hls.config().bitrate_kbps)),
            }],
            "fan_out": CodecFanOutStatus::default(),
        }))
    }

    fn preference_from_request(&self, request: &CodecRequest) -> Result<CodecPreference, ApiError> {
        let codec_text = request
            .codec
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| self.invalid_request("missing `codec` (for example `h264`)"))?;
        let codec =
            parse_video_codec(codec_text).map_err(|message| self.invalid_request(message))?;

        let mut preference = CodecPreference::new(codec);
        if let Some(bitrate) = &request.bitrate {
            preference = preference.with_bitrate_kbps(
                parse_bitrate_kbps(bitrate).map_err(|message| self.invalid_request(message))?,
            );
        }

        let hw_accel = request
            .hw_accel
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("auto");
        let backend =
            parse_encoder_backend(hw_accel).map_err(|message| self.invalid_request(message))?;
        let automatic = backend.is_none();
        preference = match backend {
            Some(backend) => preference.with_backend(backend),
            None => preference,
        };
        // An explicit hardware backend is a hardware claim unless the caller
        // explicitly allows a software fallback. Automatic selection may use
        // whatever the registry really has.
        preference = preference.with_software_fallback(
            request
                .allow_software_fallback
                .unwrap_or(automatic || backend == Some(EncoderBackend::Software)),
        );
        preference
            .validate()
            .map_err(|error| self.invalid_request_with_details(error.message, error.details))?;
        Ok(preference)
    }

    fn invalid_request(&self, message: impl Into<String>) -> ApiError {
        self.invalid_request_with_details(message, Value::Null)
    }

    fn invalid_request_with_details(&self, message: impl Into<String>, extra: Value) -> ApiError {
        let mut details = serde_json::json!({
            "available": self.registry.capabilities(),
        });
        if let Value::Object(extra) = extra {
            if let Value::Object(details) = &mut details {
                for (key, value) in extra {
                    details.insert(key, value);
                }
            }
        }
        ApiError::new(codes::INVALID_REQUEST, message)
            .with_details(details)
            .with_remediation(
                "Use a video codec the node advertises on GET /codec, an optional bitrate such \
                 as `8M` or `8000k`, and `auto`, `software`, `vaapi`, `nvenc`, `qsv`, or \
                 `v4l2m2m` for hw_accel.",
            )
    }
}

/// A failed codec mutation plus the HTTP status it should produce.
#[derive(Debug)]
pub struct CodecApplyError {
    /// Status for the response (400 for bad requests, 502 for HLS failures).
    pub status: StatusCode,
    /// Structured error body.
    pub error: ApiError,
}

impl CodecApplyError {
    fn invalid(error: ApiError) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error,
        }
    }

    fn pipeline(error: ApiError) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            error,
        }
    }
}

/// Map a policy-selected capability to the ffmpeg encoder the HLS path can run.
///
/// Software and hardware backends both map to a real [`HlsEncoder`]. Hardware
/// availability is still checked against the injected ffmpeg probe on restart;
/// this function only refuses combinations the pipeline has no honest argv for
/// (for example hardware AV1/MJPEG or `videotoolbox`).
fn hls_encoder_for_selection(capability: &EncodeCapability) -> Result<HlsEncoder, ApiError> {
    let encoder = match (capability.codec, capability.backend) {
        (Codec::H264, EncoderBackend::Software) => HlsEncoder::H264,
        (Codec::Hevc, EncoderBackend::Software) => HlsEncoder::Hevc,
        (Codec::Av1, EncoderBackend::Software) => HlsEncoder::Av1Svt,
        (Codec::Mjpeg, EncoderBackend::Software) => HlsEncoder::Mjpeg,
        (Codec::H264, EncoderBackend::Vaapi) => HlsEncoder::VaapiH264,
        (Codec::Hevc, EncoderBackend::Vaapi) => HlsEncoder::VaapiHevc,
        (Codec::H264, EncoderBackend::Nvenc) => HlsEncoder::NvencH264,
        (Codec::Hevc, EncoderBackend::Nvenc) => HlsEncoder::NvencHevc,
        (Codec::H264, EncoderBackend::Qsv) => HlsEncoder::QsvH264,
        (Codec::Hevc, EncoderBackend::Qsv) => HlsEncoder::QsvHevc,
        (Codec::H264, EncoderBackend::V4l2M2m) => HlsEncoder::V4l2m2mH264,
        (Codec::Hevc, EncoderBackend::V4l2M2m) => HlsEncoder::V4l2m2mHevc,
        (codec, backend) => {
            return Err(ApiError::new(
                codes::UNSUPPORTED,
                format!(
                    "The HLS ffmpeg path has no hardware encoder for {}/{:?}.",
                    codec.as_str(),
                    backend
                ),
            )
            .with_details(serde_json::json!({
                "codec": codec,
                "backend": backend,
                "hardware": capability.hardware,
            }))
            .with_remediation(
                "Use H.264 or HEVC for a hardware HLS switch, or choose a software codec.",
            ));
        }
    };
    Ok(encoder)
}

/// Turn a failed multi-pipeline codec fan-out into a typed 502 body.
///
/// `applied` is always false here; `details.rolled_back` and the per-pipeline
/// results say whether the previous encoders were restored.
fn codec_fanout_error(outcome: &MultiConsoleCodecOutcome, encoder: HlsEncoder) -> ApiError {
    let failed = if outcome.failed.is_empty() {
        outcome
            .pipelines
            .iter()
            .filter(|pipeline| pipeline.status == "failed")
            .map(|pipeline| pipeline.id.clone())
            .collect::<Vec<_>>()
    } else {
        outcome.failed.clone()
    };
    let message = match outcome.error.as_deref() {
        Some(error) if !failed.is_empty() => format!(
            "Codec fan-out failed on {} ({}); applied:false",
            failed.join(", "),
            error
        ),
        Some(error) => format!("Codec fan-out failed: {error}; applied:false"),
        None => "Codec fan-out failed; applied:false".to_owned(),
    };
    let code = if outcome.unavailable {
        codes::ENCODER_UNAVAILABLE
    } else {
        codes::INTERNAL
    };
    ApiError::new(code, message)
        .with_details(serde_json::json!({
            "applied": false,
            "rolled_back": outcome.rolled_back,
            "targeted": outcome.targeted,
            "failed_consoles": failed,
            "rollback_failed_consoles": outcome.rollback_failed,
            "encoder": encoder.ffmpeg_name(),
            "backend": encoder.backend_label(),
            "pipelines": outcome.pipelines,
        }))
        .with_remediation(
            "The previous encoder was restored on every pipeline where rollback succeeded. \
             Check the failing console(s), ffmpeg encoder availability, and whether the \
             requested hardware backend is advertised on GET /codec.",
        )
}

/// Turn an HLS restart error into a typed 502 body.
///
/// Availability and VAAPI-device failures keep `encoder.unavailable` and carry
/// the probe evidence/remediation; a generic spawn/restart failure remains
/// `internal` with the previous-pipeline outcome.
fn codec_pipeline_error(error: HlsError, encoder: HlsEncoder) -> ApiError {
    match &error {
        HlsError::EncoderUnavailable {
            encoder: name,
            backend,
            remediation,
            available,
            ..
        } => ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!("The HLS pipeline could not start the requested encoder: {error}"),
        )
        .with_details(serde_json::json!({
            "encoder": name,
            "backend": backend,
            "available": available,
        }))
        .with_remediation(remediation.clone()),
        HlsError::VaapiDevice {
            path, remediation, ..
        } => ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!("The HLS pipeline could not start the requested encoder: {error}"),
        )
        .with_details(serde_json::json!({
            "encoder": encoder.ffmpeg_name(),
            "backend": encoder.backend_label(),
            "vaapi_device": path,
        }))
        .with_remediation(remediation.clone()),
        HlsError::UnsupportedEncoder { encoder: name, .. } => ApiError::new(
            codes::UNSUPPORTED,
            format!("The HLS pipeline cannot run `{name}`: {error}"),
        )
        .with_details(serde_json::json!({ "encoder": name }))
        .with_remediation("Choose a hardware encoder this node advertises, or a software codec."),
        HlsError::UnsupportedPixelFormat {
            encoder: name,
            pixel_format,
            ..
        } => ApiError::new(
            codes::UNSUPPORTED,
            format!("The HLS pipeline cannot run `{name}` with {pixel_format}: {error}"),
        )
        .with_details(serde_json::json!({
            "encoder": name,
            "pixel_format": pixel_format,
        }))
        .with_remediation(
            "Keep the configured 10-bit target with HEVC/AV1 Main10, or restart the node \
             with an 8-bit --hls-pixel-format before selecting H.264/MJPEG/QSV/V4L2M2M.",
        ),
        _ => ApiError::new(
            codes::INTERNAL,
            format!("HLS pipeline could not apply the codec override: {error}"),
        )
        .with_details(serde_json::json!({
            "encoder": encoder.ffmpeg_name(),
            "hls_error": error.to_string(),
        }))
        .with_remediation(
            "The previous HLS pipeline was restored when possible. Check that ffmpeg has the \
             selected encoder, or choose a software codec the node advertises.",
        ),
    }
}

/// Controller `POST /codec` body, matching `CodecConfig.to_dict()`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CodecRequest {
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub bitrate: Option<Value>,
    #[serde(default)]
    pub hw_accel: Option<String>,
    /// Allow a software encoder when an explicit hardware backend is missing.
    #[serde(default)]
    pub allow_software_fallback: Option<bool>,
}

fn parse_video_codec(value: &str) -> Result<Codec, String> {
    let codec = match value.trim().to_ascii_lowercase().as_str() {
        "h264" | "avc" | "h.264" => Codec::H264,
        "h265" | "hevc" | "h.265" => Codec::Hevc,
        "av1" => Codec::Av1,
        "mjpeg" | "jpeg" | "mjpg" => Codec::Mjpeg,
        other => {
            return Err(format!(
                "codec `{other}` is not a supported video encoder on this node"
            ))
        }
    };
    Ok(codec)
}

fn parse_bitrate_kbps(value: &Value) -> Result<u32, String> {
    let text = match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| format!("bitrate `{number}` must be a positive integer"))?
            .to_string(),
        Value::String(text) => text.trim().to_owned(),
        other => {
            return Err(format!(
                "bitrate must be a string like `8M`/`8000k` or a positive number, got {other}"
            ))
        }
    };
    if text.is_empty() {
        return Err("bitrate must not be empty".to_owned());
    }
    let lower = text.to_ascii_lowercase();
    let (digits, multiplier) = if let Some(prefix) = lower.strip_suffix("kbps") {
        (prefix, 1u64)
    } else if let Some(prefix) = lower.strip_suffix("mbps") {
        (prefix, 1_000u64)
    } else if let Some(prefix) = lower.strip_suffix('m') {
        (prefix, 1_000u64)
    } else if let Some(prefix) = lower.strip_suffix('k') {
        (prefix, 1u64)
    } else {
        (lower.as_str(), 1u64)
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("bitrate `{text}` is not a number (expected e.g. `8M` or `8000k`)"))?;
    if value == 0 {
        return Err("bitrate must be greater than zero".to_owned());
    }
    let kbps = value
        .checked_mul(multiplier)
        .filter(|kbps| *kbps <= u64::from(u32::MAX))
        .ok_or_else(|| format!("bitrate `{text}` is too large"))?;
    u32::try_from(kbps).map_err(|_| format!("bitrate `{text}` is too large"))
}

fn parse_encoder_backend(value: &str) -> Result<Option<EncoderBackend>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(None),
        "vaapi" => Ok(Some(EncoderBackend::Vaapi)),
        "nvenc" => Ok(Some(EncoderBackend::Nvenc)),
        "qsv" => Ok(Some(EncoderBackend::Qsv)),
        "v4l2m2m" | "v4l2-m2m" => Ok(Some(EncoderBackend::V4l2M2m)),
        "software" | "sw" => Ok(Some(EncoderBackend::Software)),
        other => Err(format!(
            "hw_accel `{other}` is not supported; expected auto, nvenc, vaapi, qsv, \
             v4l2m2m, or software"
        )),
    }
}

/// Runtime audio routing control for the `/audio/*` compatibility routes.
///
/// `router` is `None` when the configured backend failed its startup check
/// (for example `pactl` is missing); `error` then carries the reason and the
/// routes answer a typed 503. A node with no `--audio-router` does not build a
/// control at all and keeps the typed 501 surface.
#[derive(Clone)]
pub struct AudioControl {
    router: Option<Arc<dyn AudioRouter>>,
    default_sink: Option<String>,
    backend: String,
    error: Option<String>,
    /// Opt-in storage for the last measured profile. `None` keeps the profile
    /// routes typed 501 and writes nothing.
    profile_store: Option<Arc<RoomProfileStore>>,
    /// Last successfully applied profile, used to preserve the Python
    /// controller's empty-body `/audio/remove` call.
    last_profile: Arc<Mutex<Option<String>>>,
}

impl AudioControl {
    /// A healthy control over a real router.
    pub fn configured(
        router: Arc<dyn AudioRouter>,
        default_sink: Option<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            router: Some(router),
            default_sink: default_sink.filter(|sink| !sink.trim().is_empty()),
            backend: backend.into(),
            error: None,
            profile_store: None,
            last_profile: Arc::new(Mutex::new(None)),
        }
    }

    /// A configured backend that failed to initialise.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            router: None,
            default_sink: None,
            backend: "none".to_owned(),
            error: Some(reason.into()),
            profile_store: None,
            last_profile: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach the opt-in room-profile store. Without one, profile routes stay
    /// typed 501 and no measurement is persisted.
    pub fn with_profile_store(mut self, store: Option<Arc<RoomProfileStore>>) -> Self {
        self.profile_store = store;
        self
    }

    /// The opt-in profile store, when configured.
    pub fn profile_store(&self) -> Option<&Arc<RoomProfileStore>> {
        self.profile_store.as_ref()
    }

    /// Stable backend name (`pipewire`, `pulseaudio`, or `none`).
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// The startup error, when the backend could not be used.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// True when a router is present and usable.
    pub fn is_available(&self) -> bool {
        self.router.is_some() && self.error.is_none()
    }

    /// Connection status token.
    pub fn status_name(&self) -> &'static str {
        if self.is_available() {
            "running"
        } else {
            "failed"
        }
    }

    fn router(&self) -> Result<&Arc<dyn AudioRouter>, Response> {
        match (&self.router, &self.error) {
            (Some(router), None) => Ok(router),
            (_, Some(error)) => Err(audio_unavailable(error)),
            (None, None) => Err(audio_not_implemented()),
        }
    }

    fn sink_or_default(&self, sink: Option<String>) -> Result<String, Response> {
        self.optional_sink(sink).ok_or_else(|| {
            compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                "audio-missing-sink",
                "missing `sink` and no default sink is configured",
                Some(
                    "Pass a sink name in the request, or start ozma-node with \
                     --audio-sink <name>.",
                ),
            )
        })
    }

    /// Resolve an explicit sink or the configured default, without requiring
    /// one. A source-only profile has no sink.
    fn optional_sink(&self, sink: Option<String>) -> Option<String> {
        sink.map(|sink| sink.trim().to_owned())
            .filter(|sink| !sink.is_empty())
            .or_else(|| self.default_sink.clone())
    }

    fn remember_profile(&self, profile_id: &str) {
        *lock(&self.last_profile) = Some(profile_id.to_owned());
    }

    fn last_profile(&self) -> Option<String> {
        lock(&self.last_profile).clone()
    }
}

/// Runtime USB gadget control for `/usb` and the virtual-media routes.
///
/// `controller` is `None` when `--usb-gadget` was requested but the gadget
/// could not be created or bound (missing UDC, missing ConfigFS root, invalid
/// media, ...); `error` then carries the typed reason and the routes answer a
/// 503. A node without `--usb-gadget` does not build a control at all and keeps
/// the typed 501 `/usb` surface.
#[derive(Clone)]
pub struct UsbControl {
    controller: Option<Arc<dyn UsbGadgetController>>,
    gadget: String,
    capabilities: UsbGadgetCapabilities,
    error: Option<String>,
    nbd: Arc<Mutex<Option<NbdRuntime>>>,
    media: Arc<Mutex<Option<MassStorageMediaRuntime>>>,
    media_daemon: Arc<Mutex<Option<MediaDaemonRuntime>>>,
    functionfs: Arc<Mutex<Option<FunctionFsHandoff>>>,
    media_auto: Arc<Mutex<MediaAutoStartState>>,
    hotplug: Arc<Mutex<UsbMediaHotplugState>>,
    uac2_bridge: Option<Uac2BridgeShared>,
    usbip: Arc<Mutex<Option<UsbIpRuntime>>>,
}

struct NbdRuntime {
    handle: NbdServerHandle,
    source: NbdExportSource,
    url: String,
    export: String,
    size_bytes: u64,
    read_only: bool,
}

/// State of the userspace SCSI media export behind `/usb/media*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MassStorageMediaState {
    /// The device and overlay exist, but no bulk endpoint has transferred data.
    Configured,
    /// At least one command cycle completed over an attached transport.
    Running,
    /// The export was stopped and its persistence policy applied.
    Stopped,
    /// The last command cycle or stop returned a typed error.
    Failed,
}

impl MassStorageMediaState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// Why a media export is being supervised for restart.
#[allow(dead_code)] // the live transport driver is not wired yet; tests exercise it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaRestartReason {
    /// A bulk endpoint read/write/flush failed.
    TransportError,
    /// The configured gadget was observed transitioning to unbound.
    FunctionFsRebind,
}

impl MediaRestartReason {
    fn operation(self) -> &'static str {
        match self {
            Self::TransportError => "restart",
            Self::FunctionFsRebind => "rebind",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::TransportError => "bulk transport failure triggered a bounded restart",
            Self::FunctionFsRebind => "gadget unbind observed; FunctionFS rebind requested",
        }
    }

    fn usb_error(self) -> UsbError {
        UsbError::MassStorageTransport {
            operation: self.operation(),
            reason: self.description().to_owned(),
        }
    }

    fn hotplug_kind(self) -> UsbMediaHotplugKind {
        match self {
            Self::TransportError => UsbMediaHotplugKind::Restart,
            Self::FunctionFsRebind => UsbMediaHotplugKind::Rebind,
        }
    }
}

/// Result of one bounded restart request.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MediaRestartOutcome {
    /// No export is configured.
    NotConfigured,
    /// The last export was stopped; restart is not applicable.
    Stopped,
    /// The export was rebuilt within the attempt cap.
    Restarted { attempt: u32, backoff_ms: u64 },
    /// The attempt cap was reached; the export stays failed.
    Exhausted { max_attempts: u32 },
    /// The attempt was allowed but rebuilding the device failed.
    Failed { attempt: u32, message: String },
}

/// Validated start options shared by the route and node auto-start.
#[derive(Clone, Debug)]
struct MediaExportSpec {
    path: PathBuf,
    format: UsbMediaFormat,
    read_only: bool,
    persist_path: Option<PathBuf>,
    persist_on_flush: bool,
    persist_on_stop: bool,
    reload: bool,
    max_transfer_bytes: Option<usize>,
    timeout_ms: Option<u64>,
    function: Option<String>,
    restart: MediaRestartPolicy,
}

/// A typed request-validation failure that the route renders as JSON and the
/// auto-start path stores as a section-less status error.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaSpecError {
    code: &'static str,
    message: String,
    remediation: &'static str,
}

impl MediaSpecError {
    fn response(self) -> Response {
        compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            self.code,
            self.message,
            Some(self.remediation),
        )
    }
}

impl MediaExportSpec {
    /// Validate a route request exactly as the start route always has.
    fn from_request(
        request: &UsbMediaStartRequest,
        restart: MediaRestartPolicy,
    ) -> Result<Self, MediaSpecError> {
        restart.validate().map_err(|error| MediaSpecError {
            code: "usb-invalid-restart-policy",
            message: error.to_string(),
            remediation: "Use 0..=16 attempts with a backoff ceiling at or above the first delay.",
        })?;

        let path_text = request.path.trim();
        if path_text.is_empty() || path_text.contains('\0') {
            return Err(MediaSpecError {
                code: "usb-invalid-media-path",
                message: "media `path` must be a non-empty path without NUL bytes".to_owned(),
                remediation: "Pass the absolute path of an existing raw/FAT32/ISO image.",
            });
        }
        let path = PathBuf::from(path_text);
        if !path.is_absolute() {
            return Err(MediaSpecError {
                code: "usb-invalid-media-path",
                message: "media path must be absolute".to_owned(),
                remediation:
                    "Use an absolute path; relative paths are resolved differently by the kernel.",
            });
        }

        let format = match request
            .format
            .as_deref()
            .unwrap_or("raw")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "raw" | "nbd" => UsbMediaFormat::Raw,
            "fat32" | "vfat" => UsbMediaFormat::Fat32,
            "iso" | "iso9660" => UsbMediaFormat::Iso9660,
            other => {
                return Err(MediaSpecError {
                    code: "usb-invalid-media-format",
                    message: format!("unsupported media export format {other:?}"),
                    remediation: "Use `raw`, `fat32`, or `iso9660`.",
                });
            }
        };
        let format_is_read_only = matches!(format, UsbMediaFormat::Fat32 | UsbMediaFormat::Iso9660);
        if format_is_read_only && request.read_only == Some(false) {
            return Err(MediaSpecError {
                code: "usb-read-only-media",
                message: "FAT32 and ISO 9660 media exports are always read-only".to_owned(),
                remediation: "Omit read_only or set it to true.",
            });
        }
        let read_only = request.read_only.unwrap_or(true) || format_is_read_only;

        let mut persistence = None;
        if !read_only {
            if let Some(persist_text) = request.persist_path.as_deref() {
                let persist_text = persist_text.trim();
                if persist_text.is_empty() || persist_text.contains('\0') {
                    return Err(MediaSpecError {
                        code: "usb-invalid-overlay-path",
                        message:
                            "overlay `persist_path` must be a non-empty path without NUL bytes"
                                .to_owned(),
                        remediation:
                            "Omit persist_path for an ephemeral overlay, or pass an absolute path.",
                    });
                }
                let persist = PathBuf::from(persist_text);
                if !persist.is_absolute() {
                    return Err(MediaSpecError {
                        code: "usb-invalid-overlay-path",
                        message: "overlay `persist_path` must be absolute".to_owned(),
                        remediation: "Use an absolute path outside the base image.",
                    });
                }
                if persist == path {
                    return Err(MediaSpecError {
                        code: "usb-invalid-overlay-path",
                        message: "overlay `persist_path` must not be the base image".to_owned(),
                        remediation: "Choose a separate file; the base image is never written.",
                    });
                }
                persistence = Some(persist);
            }
        }
        if request.reload && (read_only || persistence.is_none()) {
            return Err(MediaSpecError {
                code: "usb-invalid-overlay-path",
                message: "`reload` requires writable raw media with an explicit persist_path"
                    .to_owned(),
                remediation:
                    "Set read_only=false and pass persist_path of an existing overlay file.",
            });
        }

        Ok(Self {
            path,
            format,
            read_only,
            persist_path: persistence,
            persist_on_flush: request.persist_on_flush.unwrap_or(true),
            persist_on_stop: request.persist_on_stop.unwrap_or(true),
            reload: request.reload,
            max_transfer_bytes: request.max_transfer_bytes,
            timeout_ms: request.timeout_ms,
            function: request.function.clone(),
            restart,
        })
    }
}

/// How an export was started, used for the bounded hotplug log label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaStartOrigin {
    Route,
    AutoStart,
}

impl MediaStartOrigin {
    fn state(self) -> &'static str {
        match self {
            Self::Route => "configured",
            Self::AutoStart => "auto-start",
        }
    }
}

/// Opt-in node startup configuration for the userspace SCSI export.
///
/// The fields mirror [`UsbMediaStartRequest`]; `main` builds this from the
/// `--usb-media-export*` flags. It never implies a live transport: auto-start
/// builds the same `configured` export a route start would, and `running`
/// remains false until a command cycle completes.
#[derive(Clone, Debug)]
pub struct UsbMediaAutoStartConfig {
    /// Existing absolute backing image.
    pub path: PathBuf,
    /// Raw, FAT32, or ISO 9660.
    pub format: UsbMediaFormat,
    /// Serve raw media read-only. FAT32/ISO are forced read-only.
    pub read_only: bool,
    /// Optional absolute overlay persistence file for writable raw media.
    pub persist_path: Option<PathBuf>,
    /// Commit the overlay on SCSI `SYNCHRONIZE CACHE(10)`.
    pub persist_on_flush: bool,
    /// Commit the overlay when the export stops.
    pub persist_on_stop: bool,
    /// Load an existing persistence file before serving.
    pub reload: bool,
    /// Optional per-transfer endpoint deadline.
    pub timeout_ms: Option<u64>,
    /// Optional smaller data-phase bound.
    pub max_transfer_bytes: Option<usize>,
    /// Bounded restart/backoff policy.
    pub restart: MediaRestartPolicy,
    /// Explicit mounted FunctionFS directory for the background daemon.
    ///
    /// `None` keeps the node's existing configured-but-unattached export. When
    /// set, the node starts a [`ozma_usb::FunctionFsMediaDaemon`] that owns the
    /// bulk endpoints and uses `PollingFileBulkTransport`.
    pub functionfs_mount: Option<PathBuf>,
}

impl UsbMediaAutoStartConfig {
    fn spec(&self) -> MediaExportSpec {
        MediaExportSpec {
            path: self.path.clone(),
            format: self.format,
            read_only: self.read_only
                || matches!(self.format, UsbMediaFormat::Fat32 | UsbMediaFormat::Iso9660),
            persist_path: self.persist_path.clone(),
            persist_on_flush: self.persist_on_flush,
            persist_on_stop: self.persist_on_stop,
            reload: self.reload,
            max_transfer_bytes: self.max_transfer_bytes,
            timeout_ms: self.timeout_ms,
            function: None,
            restart: self.restart,
        }
    }
}

/// One userspace SCSI export and its live counters.
struct MassStorageMediaRuntime {
    runner: MassStorageLoop<BoxedMassStorageTransport>,
    state: MassStorageMediaState,
    function: String,
    last_error: Option<MassStorageMediaError>,
    source: MediaExportSpec,
    restart: MediaRestartTracker,
    pending_restart: Option<u64>,
    auto_start: bool,
}

/// Running USB/IP control/data listener and the adapters it selected.
pub struct UsbIpRuntime {
    handle: ozma_usb::UsbIpServerHandle,
    adapter: &'static str,
    host: String,
    port: u16,
}

impl UsbIpRuntime {
    /// Wrap a started server handle.
    pub fn new(
        handle: ozma_usb::UsbIpServerHandle,
        adapter: &'static str,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            handle,
            adapter,
            host: host.into(),
            port,
        }
    }

    fn status_json(&self) -> Value {
        let stats = self.handle.stats();
        let local_addr = self.handle.local_addr();
        serde_json::json!({
            "ok": true,
            "state": "listening",
            "adapter": self.adapter,
            "kernel_required": self.adapter == "kernel-required",
            "host": self.host,
            "port": local_addr.map_or(self.port, |address| address.port()),
            "active_connections": self.handle.active_connections(),
            "data_sessions": stats.imports,
            "sessions": stats.sessions,
            "connections_accepted": stats.connections_accepted,
            "import_requests": stats.import_requests,
            "import_failures": stats.import_failures,
            "denied": stats.denied,
            "protocol_errors": stats.protocol_errors,
            "bytes_read": stats.bytes_read,
            "bytes_written": stats.bytes_written,
        })
    }
}

/// Background FunctionFS media daemon owned by the node.
struct MediaDaemonRuntime {
    handle: MediaDaemonHandle,
    function: String,
    source: MediaExportSpec,
    auto_start: bool,
}

impl MediaDaemonRuntime {
    fn status_json(&self) -> Value {
        let status = self.handle.status();
        let capacity_bytes = std::fs::metadata(&self.source.path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        serde_json::json!({
            "ok": true,
            "state": status.state.as_str(),
            "running": status.running,
            "configured": status.state.is_configured(),
            "auto_start": self.auto_start,
            "function": self.function,
            "read_only": self.source.read_only,
            "capacity": capacity_bytes.map(|bytes| serde_json::json!({
                "sector_size": SCSI_SECTOR_BYTES,
                "sectors": bytes / SCSI_SECTOR_BYTES as u64,
                "bytes": bytes,
            })),
            "overlay": null,
            "stats": status.stats,
            "last_error": status.last_error,
            "restart": {
                "attempts": status.restart_attempts,
                "max_attempts": self.source.restart.max_attempts,
                "exhausted": status.restart_exhausted,
                "pending_backoff_ms": status.pending_backoff_ms,
                "reload_overlay": self.source.restart.reload_overlay,
            },
            "transport": {
                "kind": "polling-file",
                "attached": status.attached,
                "enabled": status.enabled,
                "live": status.live,
                "events": status.counters.events,
                "attaches": status.counters.attaches,
                "detaches": status.counters.detaches,
                "restarts": status.counters.restarts,
                "enables": status.counters.enables,
                "disables": status.counters.disables,
                "unbinds": status.counters.unbinds,
            },
            "daemon_stats": status.daemon_stats,
        })
    }
}

/// Counters and restart visibility carried across a supervised rebuild.
struct MediaRestore {
    stats: MassStorageLoopStats,
    restart: MediaRestartTracker,
    last_error: Option<MassStorageMediaError>,
    pending_restart: Option<u64>,
    auto_start: bool,
}

impl MediaRestore {
    fn from_runtime(runtime: &MassStorageMediaRuntime) -> Self {
        Self {
            stats: runtime.runner.stats().clone(),
            restart: runtime.restart.clone(),
            last_error: runtime.last_error.clone(),
            pending_restart: runtime.pending_restart,
            auto_start: runtime.auto_start,
        }
    }
}

/// Startup/restart policy and the last typed auto-start failure.
#[derive(Debug, Default)]
struct MediaAutoStartState {
    config: Option<UsbMediaAutoStartConfig>,
    last_error: Option<MassStorageMediaError>,
    stopped: bool,
}

/// Path-free error summary for a failed media export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MassStorageMediaError {
    kind: String,
    message: String,
}

impl MassStorageMediaError {
    fn from_usb(error: &UsbError) -> Self {
        let message = match error {
            UsbError::MassStorageEndpointTimeout {
                operation,
                timeout_ms,
            } => format!("endpoint {operation} timed out after {timeout_ms} ms"),
            UsbError::MassStorageTransport { operation, reason } => {
                format!("{operation}: {reason}")
            }
            UsbError::MassStorageBuffer {
                context,
                limit,
                requested,
            } => format!("{context} exceeds {limit} bytes (requested {requested})"),
            UsbError::MassStorageNotAttached => "no bulk endpoint transport is attached".to_owned(),
            UsbError::ReadOnlyMedia(_) => "media is read-only".to_owned(),
            UsbError::OverlayBounds { reason } => reason.clone(),
            UsbError::Protocol { protocol, reason } => format!("{protocol}: {reason}"),
            other => other.kind().to_owned(),
        };
        Self {
            kind: error.kind().to_owned(),
            message,
        }
    }
}

/// Path-free overlay/persistence summary for `/usb/media`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct UsbMediaOverlayStatus {
    block_size: usize,
    base_bytes: u64,
    allocated_bytes: u64,
    allocated_blocks: usize,
    max_overlay_bytes: u64,
    dirty: bool,
    persistence: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    persistence_bytes: Option<u64>,
    persist_on_flush: bool,
    persist_on_stop: bool,
}

impl UsbMediaOverlayStatus {
    fn from_status(status: MassStorageOverlayStatus) -> Self {
        let persistence_bytes = status
            .persistence
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        Self {
            block_size: status.block_size,
            base_bytes: status.base_bytes,
            allocated_bytes: status.allocated_bytes,
            allocated_blocks: status.allocated_blocks,
            max_overlay_bytes: status.max_overlay_bytes,
            dirty: status.dirty,
            persistence: status.persistence.is_some(),
            persistence_bytes,
            persist_on_flush: status.persist_on_flush,
            persist_on_stop: status.persist_on_stop,
        }
    }
}

/// Wait until the media daemon wrote both `ep0` blobs.
///
/// Binding before this would make the kernel reject an empty FunctionFS
/// function or bind a function with no descriptors. The wait is bounded and
/// never treats a failed daemon as ready.
fn wait_for_functionfs_descriptors(
    handle: &MediaDaemonHandle,
    timeout_ms: u64,
) -> Result<(), UsbError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let status = handle.status();
        if status.daemon_stats.descriptor_writes > 0 && status.daemon_stats.string_writes > 0 {
            return Ok(());
        }
        if let Some(error) = &status.last_error {
            return Err(UsbError::InvalidFunctionFsState {
                reason: format!(
                    "media daemon failed before writing ep0 descriptors: {}: {}",
                    error.kind, error.message
                ),
            });
        }
        if Instant::now() >= deadline {
            return Err(UsbError::FunctionFsReadinessTimeout {
                instance: "vendor0".to_owned(),
                timeout_ms,
                readiness: "ep0_not_written",
            });
        }
        std::thread::sleep(Duration::from_millis(FUNCTIONFS_DAEMON_START_POLL_MS));
    }
}

/// Build the SCSI device described by `spec`.
///
/// Shared by the route-started export and the background FunctionFS daemon so
/// both paths apply the same read-only policy, overlay persistence, and reload
/// behaviour.
fn build_media_device(spec: &MediaExportSpec, reload: bool) -> Result<MassStorageDevice, UsbError> {
    let mut media = match spec.format {
        UsbMediaFormat::Iso9660 => MassStorageImage::iso(&spec.path),
        UsbMediaFormat::Fat32 => MassStorageImage::fat32(&spec.path),
        _ => MassStorageImage::disk(&spec.path),
    };
    if spec.format == UsbMediaFormat::Raw {
        media = media.with_read_only(spec.read_only)?;
    }
    media.validate_for_attach()?;

    let policy = MassStoragePersistPolicy {
        persist_on_flush: spec.persist_on_flush,
        persist_on_stop: spec.persist_on_stop,
    };
    let persistence = spec.persist_path.as_ref().filter(|_| !spec.read_only);
    let mut overlay_config = OverlayConfig::default();
    if let Some(path) = persistence {
        overlay_config = overlay_config.with_persistence(path);
    }
    if reload && persistence.is_some() {
        let persist = persistence.expect("persistence was just checked");
        MassStorageDevice::load_writable_file(&spec.path, overlay_config, policy, persist)
    } else {
        MassStorageDevice::from_media(&media, overlay_config, policy)
    }
}

impl MassStorageMediaRuntime {
    fn status_json(&self) -> Value {
        let device = self.runner.device();
        serde_json::json!({
            "ok": true,
            "state": self.state.as_str(),
            "running": self.state == MassStorageMediaState::Running,
            "configured": matches!(
                self.state,
                MassStorageMediaState::Configured | MassStorageMediaState::Running
            ),
            "auto_start": self.auto_start,
            "function": self.function,
            "read_only": device.is_read_only(),
            "capacity": {
                "sector_size": SCSI_SECTOR_BYTES,
                "sectors": device.sector_count(),
                "bytes": device.base_len(),
            },
            "overlay": device.overlay_status().map(UsbMediaOverlayStatus::from_status),
            "stats": self.runner.stats(),
            "last_error": self.last_error,
            "restart": {
                "attempts": self.restart.attempts(),
                "max_attempts": self.source.restart.max_attempts,
                "exhausted": self.restart.is_exhausted(),
                "pending_backoff_ms": self.pending_restart,
                "reload_overlay": self.source.restart.reload_overlay,
            },
        })
    }

    #[allow(dead_code)] // used by tests until a live transport driver calls it
    fn note_transport_failure(&mut self, error: &UsbError) {
        self.state = MassStorageMediaState::Failed;
        self.last_error = Some(MassStorageMediaError::from_usb(error));
    }
}

/// Bounded media hotplug log plus the last observed gadget bind state.
#[derive(Debug, Default)]
struct UsbMediaHotplugState {
    log: UsbMediaHotplugLog,
    last_bound: Option<bool>,
}

impl UsbControl {
    /// A control over a created and bound gadget.
    pub fn configured(controller: Arc<dyn UsbGadgetController>, gadget: impl Into<String>) -> Self {
        let capabilities = controller.capabilities();
        Self {
            controller: Some(controller),
            gadget: gadget.into(),
            capabilities,
            error: None,
            nbd: Arc::new(Mutex::new(None)),
            media: Arc::new(Mutex::new(None)),
            media_daemon: Arc::new(Mutex::new(None)),
            functionfs: Arc::new(Mutex::new(None)),
            media_auto: Arc::new(Mutex::new(MediaAutoStartState::default())),
            hotplug: Arc::new(Mutex::new(UsbMediaHotplugState::default())),
            uac2_bridge: None,
            usbip: Arc::new(Mutex::new(None)),
        }
    }

    /// A configured gadget that could not be initialised.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            controller: None,
            gadget: crate::config::DEFAULT_USB_GADGET_NAME.to_owned(),
            capabilities: UsbGadgetCapabilities::unavailable(),
            error: Some(reason.into()),
            nbd: Arc::new(Mutex::new(None)),
            media: Arc::new(Mutex::new(None)),
            media_daemon: Arc::new(Mutex::new(None)),
            functionfs: Arc::new(Mutex::new(None)),
            media_auto: Arc::new(Mutex::new(MediaAutoStartState::default())),
            hotplug: Arc::new(Mutex::new(UsbMediaHotplugState::default())),
            uac2_bridge: None,
            usbip: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach a started USB/IP listener status.
    pub fn set_usbip_runtime(&self, runtime: Option<UsbIpRuntime>) {
        *lock(&self.usbip) = runtime;
    }

    /// Install a prepared FunctionFS handoff (gadget created, function mounted,
    /// UDC not yet bound). The media auto-start then starts the daemon and binds.
    pub fn set_functionfs_handoff(&self, handoff: Option<FunctionFsHandoff>) {
        *lock(&self.functionfs) = handoff;
    }

    /// Current FunctionFS handoff phase, if one is installed.
    pub fn functionfs_phase(&self) -> Option<FunctionFsHandoffPhase> {
        lock(&self.functionfs)
            .as_ref()
            .map(FunctionFsHandoff::phase)
    }

    /// Stop the USB/IP listener and return its final counters, if running.
    pub fn shutdown_usbip(&self) -> Option<ozma_usb::UsbIpServerStats> {
        let runtime = lock(&self.usbip).take()?;
        Some(runtime.handle.stop())
    }

    /// Attach the runtime UAC2 bridge status snapshot.
    ///
    /// `None` leaves descriptor-only status untouched. A configured bridge
    /// overlay is attached to every UAC2 function in `GET /usb`, including a
    /// typed `unsupported` report when the bridge could not start.
    pub fn with_uac2_bridge(mut self, bridge: Option<Uac2BridgeShared>) -> Self {
        self.uac2_bridge = bridge;
        self
    }

    /// Attach or replace the runtime UAC2 bridge status snapshot.
    pub fn set_uac2_bridge(&mut self, bridge: Option<Uac2BridgeShared>) {
        self.uac2_bridge = bridge;
    }

    /// Last typed bridge/adapter error, when one has been observed.
    pub fn uac2_bridge_error(&self) -> Option<Uac2BridgeError> {
        self.uac2_bridge
            .as_ref()
            .and_then(Uac2BridgeShared::last_error)
    }

    /// Path-free status of the userspace SCSI media export.
    fn media_status_json(&self) -> Value {
        if let Some(daemon) = lock(&self.media_daemon).as_ref() {
            return daemon.status_json();
        }
        let slot = lock(&self.media);
        let auto = lock(&self.media_auto);
        let auto_enabled = auto.config.is_some();
        match slot.as_ref() {
            Some(runtime) => runtime.status_json(),
            None => {
                let auto_failed = auto_enabled && auto.last_error.is_some();
                serde_json::json!({
                    "ok": true,
                    "state": if auto_failed {
                        "failed"
                    } else if auto_enabled && auto.stopped {
                        "stopped"
                    } else {
                        "not-configured"
                    },
                    "running": false,
                    "configured": false,
                    "auto_start": auto_enabled,
                    "function": null,
                    "read_only": null,
                    "capacity": null,
                    "overlay": null,
                    "stats": MassStorageLoopStats::default(),
                    "last_error": auto.last_error,
                    "restart": null,
                })
            }
        }
    }

    /// Build one media export from validated options without installing it.
    ///
    /// `restore` carries counters/restart visibility across a supervised
    /// restart; `reload` decides whether the persisted overlay is loaded.
    fn build_media_runtime(
        &self,
        spec: &MediaExportSpec,
        function: &str,
        reload: bool,
        restore: Option<MediaRestore>,
    ) -> Result<MassStorageMediaRuntime, UsbError> {
        let device = build_media_device(spec, reload)?;

        let loop_config = MassStorageLoopConfig {
            transfer_timeout_ms: spec.timeout_ms.unwrap_or(MASS_STORAGE_DEFAULT_TIMEOUT_MS),
            max_transfer_bytes: spec.max_transfer_bytes.unwrap_or(SCSI_MAX_TRANSFER_BYTES),
            max_commands: None,
        };
        let mut runner = MassStorageLoop::new(
            device,
            Box::new(IdleBulkTransport) as BoxedMassStorageTransport,
            loop_config,
        )?;
        let (restart, last_error, pending_restart, auto_start) = match restore {
            Some(restore) => {
                runner.restore_stats(restore.stats);
                (
                    restore.restart,
                    restore.last_error,
                    restore.pending_restart,
                    restore.auto_start,
                )
            }
            None => (MediaRestartTracker::new(), None, None, false),
        };

        Ok(MassStorageMediaRuntime {
            runner,
            state: MassStorageMediaState::Configured,
            function: function.to_owned(),
            last_error,
            source: spec.clone(),
            restart,
            pending_restart,
            auto_start,
        })
    }

    /// Install a built export, refusing to replace a live/configured one.
    fn install_media_runtime(
        &self,
        runtime: MassStorageMediaRuntime,
        origin: MediaStartOrigin,
    ) -> Result<Value, Response> {
        let mut slot = lock(&self.media);
        if slot
            .as_ref()
            .is_some_and(|runtime| runtime.state != MassStorageMediaState::Stopped)
        {
            return Err(media_export_conflict());
        }
        let body = runtime.status_json();
        let function = Some(runtime.function.clone());
        *slot = Some(runtime);
        drop(slot);
        self.record_media_hotplug(
            UsbMediaHotplugKind::ExportStart,
            function,
            Some(origin.state().to_owned()),
        );
        Ok(body)
    }

    /// Opt-in node startup: build and install the configured userspace export.
    ///
    /// The export is `configured`, never `running`; no transport is attached
    /// and no transfer is claimed. A typed failure is retained and surfaced by
    /// `/usb` and `/usb/media` instead of crashing the node.
    ///
    /// When a FunctionFS mount is configured, the gadget was already created
    /// and mounted by [`build_usb_control`](crate::build_usb_control); this
    /// starts the daemon, waits for its `ep0` descriptor writes, and only then
    /// binds the UDC.
    pub fn auto_start_media_export(&mut self, config: &UsbMediaAutoStartConfig) {
        {
            let mut auto = lock(&self.media_auto);
            auto.config = Some(config.clone());
            auto.last_error = None;
            auto.stopped = false;
        }
        if let Err(error) = config.restart.validate() {
            let error = MassStorageMediaError::from_usb(&error);
            lock(&self.media_auto).last_error = Some(error);
            self.record_media_hotplug(
                UsbMediaHotplugKind::ExportStart,
                None,
                Some("failed".to_owned()),
            );
            return;
        }
        let function = self
            .media_export_function(None)
            .unwrap_or_else(|_| crate::config::DEFAULT_USB_MASS_STORAGE_INSTANCE.to_owned());
        let spec = config.spec();
        if let Some(mount) = config.functionfs_mount.clone() {
            match self.start_functionfs_media_export(&spec, &function, mount) {
                Ok(()) => {
                    info!(
                        function = function.as_str(),
                        restart_max_attempts = config.restart.max_attempts,
                        "userspace SCSI media daemon started over FunctionFS polling endpoints \
                         and bound after ep0 descriptors"
                    );
                }
                Err(error) => {
                    warn!(
                        kind = error.kind(),
                        error = %error,
                        "userspace SCSI media daemon did not start"
                    );
                    self.note_auto_start_failure(&error);
                    self.error = Some(format!("{}: {error}", error.kind()));
                }
            }
            return;
        }
        match self.build_media_runtime(&spec, &function, spec.reload, None) {
            Ok(mut runtime) => {
                runtime.auto_start = true;
                match self.install_media_runtime(runtime, MediaStartOrigin::AutoStart) {
                    Ok(_) => {
                        info!(
                            function = function.as_str(),
                            restart_max_attempts = config.restart.max_attempts,
                            "userspace SCSI media export auto-started (configured, not running)"
                        );
                    }
                    Err(_) => {
                        let error = UsbError::InvalidConfig(
                            "a userspace SCSI media export is already configured".to_owned(),
                        );
                        self.note_auto_start_failure(&error);
                    }
                }
            }
            Err(error) => {
                warn!(
                    kind = error.kind(),
                    "userspace SCSI media export auto-start failed; no export was installed"
                );
                self.note_auto_start_failure(&error);
            }
        }
    }

    /// Start the supervised FunctionFS media daemon for the prepared handoff,
    /// wait until it wrote the descriptor/string blobs, then bind the UDC.
    fn start_functionfs_media_export(
        &mut self,
        spec: &MediaExportSpec,
        function: &str,
        mount: PathBuf,
    ) -> Result<(), UsbError> {
        let mut guard = lock(&self.functionfs);
        let handoff = guard
            .as_mut()
            .ok_or_else(|| UsbError::FunctionFsLifecycle {
                operation: "start-daemon",
                path: mount.clone(),
                reason: "no prepared FunctionFS handoff is installed".to_owned(),
            })?;
        if handoff.phase() != FunctionFsHandoffPhase::Mounted {
            return Err(UsbError::FunctionFsLifecycle {
                operation: "start-daemon",
                path: handoff.mountpoint().to_path_buf(),
                reason: format!(
                    "the FunctionFS handoff must be mounted before the daemon starts; phase is {}",
                    handoff.phase().as_str()
                ),
            });
        }
        if handoff.mountpoint() != mount.as_path() {
            return Err(UsbError::FunctionFsLifecycle {
                operation: "start-daemon",
                path: mount,
                reason: "the configured mount does not match the handoff mountpoint".to_owned(),
            });
        }
        let mount = handoff.mountpoint().to_path_buf();
        let runtime = self.spawn_media_daemon(spec, function, mount)?;
        if let Err(error) =
            wait_for_functionfs_descriptors(&runtime.handle, FUNCTIONFS_DAEMON_START_TIMEOUT_MS)
        {
            runtime.handle.shutdown();
            handoff.rollback();
            return Err(error);
        }
        if let Err(error) = handoff.daemon_started() {
            runtime.handle.shutdown();
            handoff.rollback();
            return Err(error);
        }
        if let Err(error) = handoff.bind() {
            runtime.handle.shutdown();
            return Err(error);
        }
        *lock(&self.media_daemon) = Some(runtime);
        *lock(&self.media) = None;
        self.record_media_hotplug(
            UsbMediaHotplugKind::ExportStart,
            Some(function.to_owned()),
            Some("auto-start".to_owned()),
        );
        Ok(())
    }

    /// Build and spawn the supervised FunctionFS media daemon without falling
    /// back to the in-process runner.
    fn spawn_media_daemon(
        &self,
        spec: &MediaExportSpec,
        function: &str,
        mount: PathBuf,
    ) -> Result<MediaDaemonRuntime, UsbError> {
        let function_config = FunctionFsConfig::new("vendor0", 0x1d6b, 0x0106)
            // Mass Storage / SCSI transparent / Bulk-Only Transport so the
            // host's usb-storage driver binds the FunctionFS interface.
            .with_interface(0x08, 0x06, 0x50)
            .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
            .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
        let mount = FunctionFsMount::new(mount);
        let loop_config = MassStorageLoopConfig {
            transfer_timeout_ms: spec.timeout_ms.unwrap_or(MASS_STORAGE_DEFAULT_TIMEOUT_MS),
            max_transfer_bytes: spec.max_transfer_bytes.unwrap_or(SCSI_MAX_TRANSFER_BYTES),
            max_commands: None,
        };
        let transport = polling_transport_factory(
            mount.clone(),
            &function_config,
            loop_config.transfer_timeout_ms,
        )?;
        let factory_spec = spec.clone();
        let device_factory =
            Box::new(move |reload: bool| build_media_device(&factory_spec, reload));
        let daemon = FunctionFsMediaDaemon::file(
            function_config,
            mount,
            device_factory,
            transport,
            MediaDaemonConfig {
                loop_config,
                restart: spec.restart,
                ..MediaDaemonConfig::default()
            },
        )?;
        let handle = daemon.spawn("usb-media-daemon")?;
        Ok(MediaDaemonRuntime {
            handle,
            function: function.to_owned(),
            source: spec.clone(),
            auto_start: true,
        })
    }

    fn note_auto_start_failure(&self, error: &UsbError) {
        lock(&self.media_auto).last_error = Some(MassStorageMediaError::from_usb(error));
        self.record_media_hotplug(
            UsbMediaHotplugKind::ExportStart,
            None,
            Some("failed".to_owned()),
        );
    }

    /// Apply one bounded restart to a failed export.
    ///
    /// Called after a transport failure or an observed FunctionFS unbind. The
    /// attempt cap and backoff come from the export's
    /// [`MediaRestartPolicy`]; exhausting the cap leaves the export `failed`
    /// instead of retrying forever. Counters and the prior typed error stay
    /// visible, and a writable overlay is committed before it is reloaded.
    fn restart_media_export(&self, reason: MediaRestartReason) -> MediaRestartOutcome {
        let mut slot = lock(&self.media);
        let Some(runtime) = slot.as_mut() else {
            return MediaRestartOutcome::NotConfigured;
        };
        if runtime.state == MassStorageMediaState::Stopped {
            return MediaRestartOutcome::Stopped;
        }
        let policy = runtime.source.restart;
        let Some(attempt) = runtime.restart.record_failure(&policy) else {
            runtime.pending_restart = None;
            runtime.state = MassStorageMediaState::Failed;
            let error = reason.usb_error();
            runtime.last_error = Some(MassStorageMediaError::from_usb(&error));
            let function = Some(runtime.function.clone());
            drop(slot);
            self.record_media_hotplug(reason.hotplug_kind(), function, Some("failed".to_owned()));
            return MediaRestartOutcome::Exhausted {
                max_attempts: policy.max_attempts,
            };
        };

        runtime.pending_restart = Some(attempt.backoff_ms);
        runtime.state = MassStorageMediaState::Failed;
        let error = reason.usb_error();
        runtime.last_error = Some(MassStorageMediaError::from_usb(&error));

        let source = runtime.source.clone();
        let function = runtime.function.clone();
        let reload = policy.reload_overlay && source.persist_path.is_some();
        if reload {
            if let Err(commit_error) = runtime.runner.device_mut().commit() {
                runtime.pending_restart = None;
                runtime.last_error = Some(MassStorageMediaError::from_usb(&commit_error));
                let function = Some(function);
                drop(slot);
                self.record_media_hotplug(
                    reason.hotplug_kind(),
                    function,
                    Some("failed".to_owned()),
                );
                return MediaRestartOutcome::Failed {
                    attempt: attempt.attempt,
                    message: commit_error.kind().to_owned(),
                };
            }
        }

        let restore = MediaRestore::from_runtime(runtime);
        let rebuilt = self.build_media_runtime(&source, &function, reload, Some(restore));
        match rebuilt {
            Ok(mut runtime) => {
                runtime.state = MassStorageMediaState::Configured;
                runtime.pending_restart = None;
                *slot = Some(runtime);
                let function = slot.as_ref().map(|runtime| runtime.function.clone());
                drop(slot);
                self.record_media_hotplug(
                    reason.hotplug_kind(),
                    function,
                    Some("configured".to_owned()),
                );
                MediaRestartOutcome::Restarted {
                    attempt: attempt.attempt,
                    backoff_ms: attempt.backoff_ms,
                }
            }
            Err(rebuild_error) => {
                runtime.state = MassStorageMediaState::Failed;
                runtime.pending_restart = None;
                runtime.last_error = Some(MassStorageMediaError::from_usb(&rebuild_error));
                let function = Some(runtime.function.clone());
                drop(slot);
                self.record_media_hotplug(
                    reason.hotplug_kind(),
                    function,
                    Some("failed".to_owned()),
                );
                MediaRestartOutcome::Failed {
                    attempt: attempt.attempt,
                    message: rebuild_error.kind().to_owned(),
                }
            }
        }
    }

    /// Stop the export and apply its persistence policy during node shutdown.
    ///
    /// The order is deliberate and deterministic: unbind the UDC once to
    /// quiesce host traffic, signal the daemon, wait for its bounded join (which
    /// flushes the overlay and force-closes the endpoint files), then unmount
    /// FunctionFS and remove the function/gadget. A worker that overruns its
    /// bound does not hold node exit, and no second external unbind is needed.
    pub fn shutdown_media_export(&self) -> Result<(), UsbError> {
        let mut handoff = lock(&self.functionfs).take();
        let mut first_error = None;
        if let Some(handoff) = handoff.as_mut() {
            if handoff.phase() == FunctionFsHandoffPhase::Bound {
                if let Err(error) = handoff.unbind_once() {
                    first_error = Some(error);
                }
            }
        }

        if let Some(daemon) = lock(&self.media_daemon).take() {
            let function = Some(daemon.function.clone());
            let finished = daemon.handle.shutdown_timeout(Duration::from_millis(
                MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS,
            ));
            if !finished {
                warn!(
                    timeout_ms = MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS,
                    "FunctionFS media daemon did not finish within its shutdown bound"
                );
            }
            lock(&self.media_auto).stopped = true;
            self.record_media_hotplug(
                UsbMediaHotplugKind::ExportStop,
                function,
                Some("shutdown".to_owned()),
            );
        } else {
            let mut slot = lock(&self.media);
            if let Some(runtime) = slot.as_mut() {
                if runtime.state != MassStorageMediaState::Stopped {
                    let result = runtime.runner.stop();
                    match &result {
                        Ok(()) => {
                            runtime.state = MassStorageMediaState::Stopped;
                            runtime.last_error = None;
                        }
                        Err(error) => {
                            runtime.state = MassStorageMediaState::Failed;
                            runtime.last_error = Some(MassStorageMediaError::from_usb(error));
                            first_error.get_or_insert_with(|| UsbError::MassStorageTransport {
                                operation: "stop",
                                reason: error.kind().to_owned(),
                            });
                        }
                    }
                    let function = Some(runtime.function.clone());
                    drop(slot);
                    self.record_media_hotplug(
                        UsbMediaHotplugKind::ExportStop,
                        function,
                        Some(if result.is_ok() {
                            "shutdown".to_owned()
                        } else {
                            "failed".to_owned()
                        }),
                    );
                }
            }
        }

        if let Some(handoff) = handoff.as_mut() {
            if let Err(error) = handoff.cleanup() {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Record a media export/attachment transition.
    fn record_media_hotplug(
        &self,
        kind: UsbMediaHotplugKind,
        function: Option<String>,
        state: Option<String>,
    ) {
        lock(&self.hotplug).log.record(kind, function, state);
    }

    /// Record a gadget bind→unbind transition observed from ConfigFS.
    ///
    /// A live export is reloaded within its restart policy because the
    /// FunctionFS endpoints are gone until the gadget rebinds.
    fn note_gadget_bound(&self, bound: bool) {
        let transitioned = {
            let mut hotplug = lock(&self.hotplug);
            let previous = hotplug.last_bound;
            hotplug.last_bound = Some(bound);
            if previous == Some(true) && !bound {
                hotplug.log.record(
                    UsbMediaHotplugKind::Unbind,
                    None,
                    Some("unbound".to_owned()),
                );
                true
            } else {
                false
            }
        };
        if transitioned {
            let has_export = {
                let slot = lock(&self.media);
                slot.as_ref()
                    .is_some_and(|runtime| runtime.state != MassStorageMediaState::Stopped)
            };
            if has_export {
                let _ = self.restart_media_export(MediaRestartReason::FunctionFsRebind);
            }
        }
    }

    /// Bounded hotplug view used by `GET /usb/hotplug`.
    fn media_hotplug_events(&self, since: u64, limit: usize) -> (Vec<Value>, usize, u64) {
        let hotplug = lock(&self.hotplug);
        let events = hotplug
            .log
            .events_since(since, limit)
            .into_iter()
            .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
            .collect();
        (events, hotplug.log.capacity(), hotplug.log.next_sequence())
    }

    /// Configured gadget directory name.
    pub fn gadget(&self) -> &str {
        &self.gadget
    }

    /// Capability advertisement for this controller.
    pub fn capabilities(&self) -> &UsbGadgetCapabilities {
        &self.capabilities
    }

    /// True when a controller is present and did not fail at startup.
    pub fn is_available(&self) -> bool {
        self.controller.is_some() && self.error.is_none()
    }

    /// Connection status token.
    pub fn status_name(&self) -> &'static str {
        if self.is_available() {
            "running"
        } else {
            "failed"
        }
    }

    /// The startup error, when the gadget could not be created or bound.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Project the controller capabilities onto the protocol type.
    pub fn protocol_capability(&self) -> ozma_protocol::UsbCapability {
        self.capabilities.protocol_capability()
    }

    fn controller(&self) -> Result<&Arc<dyn UsbGadgetController>, Response> {
        match (&self.controller, &self.error) {
            (Some(controller), None) => Ok(controller),
            (_, Some(error)) => Err(usb_unavailable(error)),
            (None, None) => Err(usb_not_configured()),
        }
    }

    /// Read the live gadget status without exposing paths.
    fn snapshot(&self) -> Result<UsbGadgetSnapshot, String> {
        let controller = self.controller.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| "USB is not configured".into())
        })?;
        let status = controller
            .status(&self.gadget)
            .map_err(|error| format!("{}: {error}", error.kind()))?;
        self.note_gadget_bound(status.bound);
        let mut snapshot = UsbGadgetSnapshot::from_status(status);
        if let Some(bridge) = &self.uac2_bridge {
            let report = bridge.report();
            for function in &mut snapshot.functions {
                if function.kind == UsbFunctionKind::Uac2 {
                    if let Some(uac2) = function.uac2.take() {
                        function.uac2 = Some(uac2.with_bridge_report(report.clone()));
                    }
                }
            }
        }
        Ok(snapshot)
    }

    /// The mass-storage instance to use when a request omits `function`.
    fn default_media_function(&self) -> Result<String, Response> {
        let snapshot = self
            .snapshot()
            .map_err(|error| usb_status_unavailable(&error))?;
        snapshot
            .functions
            .iter()
            .find(|function| function.kind == UsbFunctionKind::MassStorage)
            .and_then(|function| function.name.split_once('.').map(|(_, instance)| instance))
            .map(str::to_owned)
            .ok_or_else(|| {
                compat_error_with_remediation(
                    StatusCode::CONFLICT,
                    "usb-missing-function",
                    "the USB gadget has no mass-storage function",
                    Some(
                        "Start ozma-node with --usb-media <path> so the gadget includes a \
                         mass_storage function before attaching media.",
                    ),
                )
            })
    }

    /// Function label for the userspace SCSI export.
    ///
    /// The userspace FunctionFS/SCSI path is not the ConfigFS kernel LUN, so it
    /// must not require a `mass_storage.*` function. If one exists its instance
    /// is used for hotplug labels; otherwise the default instance is reported
    /// and no kernel LUN is implied.
    fn media_export_function(&self, requested: Option<&str>) -> Result<String, Response> {
        if let Some(function) = requested.map(str::trim).filter(|value| !value.is_empty()) {
            return normalized_media_function(function);
        }
        if let Ok(snapshot) = self.snapshot() {
            if let Some(instance) = snapshot
                .functions
                .iter()
                .find(|function| function.kind == UsbFunctionKind::MassStorage)
                .and_then(|function| function.name.split_once('.').map(|(_, instance)| instance))
            {
                return Ok(instance.to_owned());
            }
        }
        Ok(crate::config::DEFAULT_USB_MASS_STORAGE_INSTANCE.to_owned())
    }

    /// Compact, path-free status for `/api/v1/connection`.
    pub fn connection_status(&self) -> UsbConnectionStatus {
        let mut status = UsbConnectionStatus {
            status: self.status_name(),
            available: false,
            gadget: Some(self.gadget.clone()),
            bound: false,
            udc: None,
            configfs: self.capabilities.configfs,
            virtual_media: self.capabilities.virtual_media,
            mass_storage: self.capabilities.mass_storage,
            functions: Vec::new(),
            error: self.error.clone(),
        };
        match self.snapshot() {
            Ok(snapshot) => {
                status.available = snapshot.bound;
                status.bound = snapshot.bound;
                status.udc = snapshot.udc.clone();
                status.functions = snapshot
                    .functions
                    .iter()
                    .map(|function| function.name.clone())
                    .collect();
                if !snapshot.bound {
                    status.status = "unbound";
                }
            }
            Err(error) => {
                status.status = "failed";
                status.error = Some(error);
            }
        }
        status
    }
}

#[cfg(test)]
impl UsbControl {
    /// Attach a fake/live bulk transport to a configured media export.
    fn media_attach_test_transport(
        &self,
        transport: impl MassStorageBulkTransport + Send + 'static,
    ) -> Result<(), UsbError> {
        let mut slot = lock(&self.media);
        let runtime = slot.as_mut().ok_or(UsbError::MassStorageNotAttached)?;
        runtime.runner.attach_transport(Box::new(transport));
        Ok(())
    }

    /// Drive one command cycle for tests.
    fn media_step_test(&self) -> Result<Option<MassStorageCommandOutcome>, UsbError> {
        let mut slot = lock(&self.media);
        let Some(runtime) = slot.as_mut() else {
            return Ok(None);
        };
        match runtime.runner.step() {
            Ok(outcome) => {
                runtime.state = MassStorageMediaState::Running;
                runtime.last_error = None;
                runtime.restart.record_success();
                runtime.pending_restart = None;
                Ok(Some(outcome))
            }
            Err(error) => {
                if error.kind() != "mass-storage-not-attached" {
                    runtime.note_transport_failure(&error);
                } else {
                    runtime.last_error = Some(MassStorageMediaError::from_usb(&error));
                }
                Err(error)
            }
        }
    }
}

/// Connection-level USB status, built from the live gadget when present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsbConnectionStatus {
    /// `running` (bound), `unbound`, `failed`, or `not-configured`.
    pub status: &'static str,
    /// Whether the gadget is currently bound and readable.
    pub available: bool,
    /// Configured gadget directory name.
    pub gadget: Option<String>,
    /// Whether a UDC is bound.
    pub bound: bool,
    /// Bound UDC, when present.
    pub udc: Option<String>,
    /// ConfigFS root availability reported by the controller.
    pub configfs: bool,
    /// Virtual-media support reported by the controller.
    pub virtual_media: bool,
    /// Mass-storage support reported by the controller.
    pub mass_storage: bool,
    /// Function names visible in the gadget.
    pub functions: Vec<String>,
    /// Why the gadget is not running, when configured but failing.
    pub error: Option<String>,
}

impl Default for UsbConnectionStatus {
    fn default() -> Self {
        Self {
            status: "not-configured",
            available: false,
            gadget: None,
            bound: false,
            udc: None,
            configfs: false,
            virtual_media: false,
            mass_storage: false,
            functions: Vec::new(),
            error: None,
        }
    }
}

/// Read-only `/usb` gadget snapshot. No backing paths are included.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsbGadgetSnapshot {
    pub name: String,
    pub active: bool,
    pub bound: bool,
    pub udc: Option<String>,
    pub functions: Vec<UsbFunctionSnapshot>,
}

impl UsbGadgetSnapshot {
    fn from_status(status: GadgetStatus) -> Self {
        let functions = status
            .functions
            .iter()
            .map(UsbFunctionSnapshot::from_status)
            .collect();
        Self {
            name: status.name,
            active: status.bound,
            bound: status.bound,
            udc: status.udc,
            functions,
        }
    }

    fn media(&self) -> Vec<UsbMediaStatus> {
        self.functions
            .iter()
            .filter_map(|function| function.media.clone())
            .collect()
    }
}

/// One gadget function in the `/usb` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsbFunctionSnapshot {
    pub name: String,
    pub kind: UsbFunctionKind,
    pub subtype: String,
    pub linked: bool,
    pub requires_userspace: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<UsbMediaStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uac2: Option<Uac2Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_fs: Option<FunctionFsStatus>,
}

impl UsbFunctionSnapshot {
    fn from_status(function: &FunctionStatus) -> Self {
        let instance = function
            .name
            .split_once('.')
            .map(|(_, instance)| instance)
            .unwrap_or(function.name.as_str());
        Self {
            name: function.name.clone(),
            kind: function.kind,
            subtype: usb_function_subtype(function.kind).to_owned(),
            linked: function.linked,
            requires_userspace: function.requires_userspace,
            media: function
                .media
                .as_ref()
                .map(|media| UsbMediaStatus::from_state(instance, media)),
            uac2: function.uac2.clone(),
            function_fs: function.function_fs.clone(),
        }
    }
}

fn usb_function_subtype(kind: UsbFunctionKind) -> &'static str {
    match kind {
        UsbFunctionKind::Hid => "hid",
        UsbFunctionKind::MassStorage => "mass-storage",
        UsbFunctionKind::Uac2 => "audio-uac2",
        UsbFunctionKind::Vendor => "vendor",
    }
}

/// Attached/detached media state without the backing path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsbMediaStatus {
    pub function: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl UsbMediaStatus {
    fn from_state(function: &str, state: &MediaState) -> Self {
        match state {
            MediaState::Detached => Self {
                function: function.to_owned(),
                state: "detached".to_owned(),
                mode: None,
                read_only: None,
                size_bytes: None,
            },
            MediaState::Attached {
                mode,
                read_only,
                size_bytes,
                ..
            } => Self {
                function: function.to_owned(),
                state: "attached".to_owned(),
                mode: Some(mode_name(*mode).to_owned()),
                read_only: Some(*read_only),
                size_bytes: *size_bytes,
            },
        }
    }
}

fn mode_name(mode: ozma_usb::MediaMode) -> &'static str {
    match mode {
        ozma_usb::MediaMode::Disk => "disk",
        ozma_usb::MediaMode::CdRom => "cdrom",
    }
}

/// Input sink status exposed on `/api/v1/connection`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InputStatus {
    /// Controller-facing sink kind (`uinput`, `qmp`, `hid-gadget`, ...).
    pub sink_kind: String,
    /// Whether a shared sink is attached to the compatibility routes.
    pub available: bool,
    /// Whether events are really injected (false for a recording-only sink).
    pub injecting: bool,
}

/// Display capture status exposed on `/api/v1/connection`.
///
/// `active_console`/`provider` describe the one live capture source.
/// `limitation` is set when more consoles are advertised than have capture
/// sources (for example QMP/VNC/Looking Glass expose one default head), so the
/// controller can surface the limitation instead of showing a broken picker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DisplayCaptureStatus {
    /// Console index the live capture source targets.
    pub active_console: Option<u32>,
    /// Capture transport (`qmp`, `dbus`, `vnc`, `looking_glass`).
    pub provider: Option<String>,
    /// Why not every advertised console has a capture source.
    pub limitation: Option<String>,
}

impl DisplayCaptureStatus {
    /// Build a status from explicit parts.
    pub fn new(
        active_console: Option<u32>,
        provider: Option<String>,
        limitation: Option<String>,
    ) -> Self {
        Self {
            active_console,
            provider,
            limitation,
        }
    }
}

/// The live capture source exposed on `/api/v1/connection`.
///
/// `provider` names the configured transport (`pipewire`, `x11`, `portal`,
/// `fake`, or `none`). `kind`/`source`/`name` are filled only from a source the
/// provider actually enumerated; a failed portal handshake or absent X display
/// therefore reports `available:false` with no kind or source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureStatus {
    /// Configured capture transport.
    pub provider: String,
    /// Enumerated capture kind (`x11`, `pipewire`, ...), or `null`.
    pub kind: Option<String>,
    /// Enumerated source id, or `null`.
    pub source: Option<String>,
    /// Enumerated human-readable source name, or `null`.
    pub name: Option<String>,
    /// Whether a real source was enumerated.
    pub available: bool,
}

impl Default for CaptureStatus {
    fn default() -> Self {
        Self {
            provider: "none".to_owned(),
            kind: None,
            source: None,
            name: None,
            available: false,
        }
    }
}

impl CaptureStatus {
    /// Build the status from the configured provider and first enumerated
    /// capability.
    pub fn from_capability(
        provider: impl Into<String>,
        capability: Option<&ozma_protocol::CaptureCapability>,
    ) -> Self {
        match capability {
            Some(capability) => Self {
                provider: provider.into(),
                kind: Some(capture_kind_name(capability.kind).to_owned()),
                source: Some(capability.id.clone()),
                name: Some(capability.name.clone()),
                available: true,
            },
            None => Self {
                provider: provider.into(),
                ..Self::default()
            },
        }
    }
}

fn capture_kind_name(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::V4l2 => "v4l2",
        CaptureKind::Kms => "kms",
        CaptureKind::PipeWire => "pipewire",
        CaptureKind::Qmp => "qmp",
        CaptureKind::Dbus => "dbus",
        CaptureKind::LookingGlass => "looking_glass",
        CaptureKind::Vnc => "vnc",
        CaptureKind::X11 => "x11",
        CaptureKind::Dxgi => "dxgi",
        CaptureKind::HdmiSink => "hdmi_sink",
        CaptureKind::Fake => "fake",
    }
}

/// Static facts about the node needed by the connection endpoint.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub node_id: String,
    pub profile: String,
    pub api_port: u16,
    pub quic_port: u16,
    pub hid_udp_port: u16,
    pub sink_kind: String,
    /// Registration fields the node actually advertised, plus typed
    /// "not available" entries for controller fields it does not implement.
    pub registration: crate::registration::RegistrationExtras,
}

impl ConnectionInfo {
    /// Build connection info from the resolved node configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node_id: impl Into<String>,
        profile: impl Into<String>,
        api_port: u16,
        quic_port: u16,
        hid_udp_port: u16,
        sink_kind: impl Into<String>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            profile: profile.into(),
            api_port,
            quic_port,
            hid_udp_port,
            sink_kind: sink_kind.into(),
            registration: crate::registration::RegistrationExtras::default(),
        }
    }

    /// Attach the registration fields the node actually advertised.
    ///
    /// `/api/v1/connection.registration` then shows the same `seat_id`/`vm_ip`/
    /// VNC state as mDNS and direct registration, plus the named controller
    /// fields the node cannot provide.
    pub fn with_registration(
        mut self,
        registration: crate::registration::RegistrationExtras,
    ) -> Self {
        self.registration = registration;
        self
    }
}

/// One controller-facing surface and whether this node serves it.
///
/// The fields are serialized into `/api/v1/connection.controller_surfaces` so a
/// controller can discover both implemented RPA primitives and Python-node/
/// agent-only surfaces without probing them. Implemented entries have
/// `supported:true`; unsupported entries are backed by a typed 501.
/// `path_prefix`, `code`, and `remediation` are internal stub details and are
/// deliberately not part of the advertised document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ControllerSurface {
    /// Controller-facing surface name, for example `/api/v1/rpa/ocr`.
    pub surface: &'static str,
    /// Capability name used by the parity matrix and the typed error code.
    pub capability: &'static str,
    /// Whether this node implements the surface.
    pub supported: bool,
    /// Why the surface is implemented or absent, in operator terms.
    pub reason: &'static str,
    /// Python provider the controller used before the cutover.
    pub python_reference: &'static str,
    /// Request path prefix matched by the typed 501 handler.
    #[serde(skip)]
    pub path_prefix: &'static str,
    /// Stable typed error code returned by an unsupported stub.
    #[serde(skip)]
    pub code: &'static str,
    /// Actionable remediation returned by an unsupported stub.
    #[serde(skip)]
    pub remediation: &'static str,
}

/// Controller surfaces this node implements for real (RPA and local backup).
///
/// These are advertised alongside [`CONTROLLER_ONLY_SURFACES`] so a controller
/// can tell an implemented primitive from a typed 501 without probing.
pub const RPA_IMPLEMENTED_SURFACES: &[ControllerSurface] = &[
    ControllerSurface {
        surface: "/api/v1/rpa/type",
        capability: "rpa.type",
        supported: true,
        reason: "Types text through the configured input sink using the selected guest keyboard layout.",
        python_reference: "node/rpa.py::NodeRPA.type_text (node/node.py:402-405)",
        path_prefix: "/api/v1/rpa/type",
        code: "rpa.type",
        remediation: "",
    },
    ControllerSurface {
        surface: "/api/v1/rpa/key",
        capability: "rpa.key",
        supported: true,
        reason: "Sends named keys, printable characters, and modifier combinations through the configured input sink.",
        python_reference: "node/rpa.py::NodeRPA.key (node/node.py:394-397)",
        path_prefix: "/api/v1/rpa/key",
        code: "rpa.key",
        remediation: "",
    },
    ControllerSurface {
        surface: "/api/v1/rpa/screenshot",
        capability: "rpa.screenshot",
        supported: true,
        reason: "Returns a real JPEG from the configured display capture source; ?console= selects an advertised console.",
        python_reference: "node/rpa.py::NodeRPA.grab_frame (node/node.py:428-435)",
        path_prefix: "/api/v1/rpa/screenshot",
        code: "rpa.screenshot",
        remediation: "",
    },
    ControllerSurface {
        surface: "/api/v1/rpa/ocr",
        capability: "rpa.ocr",
        supported: true,
        reason: "Runs the optional tesseract CLI on a captured frame; without tesseract the route returns 503 rpa.ocr-unavailable.",
        python_reference: "node/rpa.py::NodeRPA.read_screen (node/node.py:382-393)",
        path_prefix: "/api/v1/rpa/ocr",
        code: "rpa.ocr",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/status",
        capability: "backup.status",
        supported: true,
        reason: "Reports the configured local restic repository state and last snapshot without exposing the password.",
        python_reference: "agent/backup.py::BackupManager.get_status",
        path_prefix: "/backup/status",
        code: "backup.status",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/init",
        capability: "backup.init",
        supported: true,
        reason: "Initializes the configured local repository idempotently with RESTIC_PASSWORD_FILE.",
        python_reference: "agent/backup.py::BackupManager._ensure_repo_init",
        path_prefix: "/backup/init",
        code: "backup.init",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/run",
        capability: "backup.run",
        supported: true,
        reason: "Runs a bounded restic backup over allow-listed --backup-source paths and returns the JSON summary.",
        python_reference: "agent/backup.py::BackupManager.run_backup",
        path_prefix: "/backup/run",
        code: "backup.run",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/snapshots",
        capability: "backup.snapshots",
        supported: true,
        reason: "Lists a bounded, secret-free restic snapshot document.",
        python_reference: "agent/backup.py::BackupManager.list_snapshots",
        path_prefix: "/backup/snapshots",
        code: "backup.snapshots",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/restore",
        capability: "backup.restore",
        supported: true,
        reason: "Restores an explicit snapshot below an allow-listed --backup-restore-dir.",
        python_reference: "agent/backup.py::BackupManager.restore",
        path_prefix: "/backup/restore",
        code: "backup.restore",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/snapshots/{id}",
        capability: "backup.delete",
        supported: true,
        reason: "Forgets one exact snapshot and prunes only unreferenced data; no retention policy is applied.",
        python_reference: "agent/backup.py::BackupManager._prune",
        path_prefix: "/backup/snapshots",
        code: "backup.delete",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/stats",
        capability: "backup.stats",
        supported: true,
        reason: "Reports repository, unique-content, and snapshot statistics from restic stats --json.",
        python_reference: "agent/backup.py::BackupManager.get_status",
        path_prefix: "/backup/stats",
        code: "backup.stats",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/verify",
        capability: "backup.verify",
        supported: true,
        reason: "Runs restic check with a bounded 1-5% read-data subset and only reports integrity on restic success.",
        python_reference: "agent/backup.py::BackupManager.verify",
        path_prefix: "/backup/verify",
        code: "backup.verify",
        remediation: "",
    },
    ControllerSurface {
        surface: "/backup/estimate",
        capability: "backup.estimate",
        supported: true,
        reason: "Scans allow-listed sources with restic backup --dry-run --json without writing a snapshot.",
        python_reference: "agent/backup.py::BackupManager.estimate_size",
        path_prefix: "/backup/estimate",
        code: "backup.estimate",
        remediation: "",
    },
];

/// Bluetooth surfaces served by the opt-in [`crate::bluetooth`] provider.
///
/// These are advertised only when the node was started with `--bluetooth`. The
/// routes are always mounted behind the bearer guard, but without the service
/// they answer a typed 503 `bluetooth.not-configured` and this list is replaced
/// by [`BLUETOOTH_UNCONFIGURED_SURFACE`].
pub const BLUETOOTH_IMPLEMENTED_SURFACES: &[ControllerSurface] = &[
    ControllerSurface {
        surface: "/bluetooth/status",
        capability: "bluetooth.status",
        supported: true,
        reason: "Reports bounded BlueZ adapter power/discoverable/discovering flags and scan limits.",
        python_reference: "controller/bluetooth.py::BluetoothManager.start",
        path_prefix: "/bluetooth/status",
        code: "bluetooth.status",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/devices",
        capability: "bluetooth.devices",
        supported: true,
        reason: "Lists bounded known devices with paired/bonded/trusted/connected flags and optional RSSI.",
        python_reference: "controller/bluetooth.py::BluetoothManager.list_devices",
        path_prefix: "/bluetooth/devices",
        code: "bluetooth.devices",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/scan",
        capability: "bluetooth.scan",
        supported: true,
        reason: "Runs a bounded scan (1-15 s) and always attempts scan off; it never pairs or trusts.",
        python_reference: "controller/bluetooth.py::BluetoothManager.discover",
        path_prefix: "/bluetooth/scan",
        code: "bluetooth.scan",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/pair",
        capability: "bluetooth.pair",
        supported: true,
        reason: "Pairs one explicit MAC address with bluetoothctl pair; it never calls trust.",
        python_reference: "controller/bluetooth.py::BluetoothManager.pair",
        path_prefix: "/bluetooth/pair",
        code: "bluetooth.pair",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/connect",
        capability: "bluetooth.connect",
        supported: true,
        reason: "Connects one explicit MAC address with a bounded bluetoothctl connect.",
        python_reference: "controller/bluetooth.py::BluetoothManager.connect",
        path_prefix: "/bluetooth/connect",
        code: "bluetooth.connect",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/disconnect",
        capability: "bluetooth.disconnect",
        supported: true,
        reason: "Disconnects one explicit MAC address with a bounded bluetoothctl disconnect.",
        python_reference: "controller/bluetooth.py::BluetoothManager.disconnect",
        path_prefix: "/bluetooth/disconnect",
        code: "bluetooth.disconnect",
        remediation: "",
    },
    ControllerSurface {
        surface: "/bluetooth/forget",
        capability: "bluetooth.forget",
        supported: true,
        reason: "Removes (forgets) exactly one explicit MAC address with bluetoothctl remove.",
        python_reference: "controller/bluetooth.py::BluetoothManager.remove",
        path_prefix: "/bluetooth/forget",
        code: "bluetooth.forget",
        remediation: "",
    },
];

/// Advertisement used when `--bluetooth` was not supplied. It is not backed by
/// a 501 stub: every route is mounted and returns a typed 503
/// `bluetooth.not-configured`, which is more accurate than "unsupported".
pub const BLUETOOTH_UNCONFIGURED_SURFACE: ControllerSurface = ControllerSurface {
    surface: "/bluetooth/*",
    capability: "bluetooth",
    supported: false,
    reason: "Bluetooth is opt-in; start the node with --bluetooth to enable bounded BlueZ status, scan, pair, connect, disconnect, and forget.",
    python_reference: "controller/bluetooth.py::BluetoothManager",
    path_prefix: "/bluetooth",
    code: "bluetooth.not-configured",
    remediation: "Restart the node with --bluetooth (and optionally --bluetooth-adapter) to enable the guarded routes.",
};

/// Serial surfaces served by the opt-in [`crate::serial`] provider.
///
/// These are advertised only when the node was started with `--serial-device`.
/// The routes are always mounted behind the bearer guard, but without the
/// service they answer a typed 503 `serial.not-configured`. The residual
/// `/serial/*` entry in [`CONTROLLER_ONLY_SURFACES`] continues to describe the
/// historical buffer/send/state/text/setup sub-surfaces that remain typed 501s.
pub const SERIAL_IMPLEMENTED_SURFACES: &[ControllerSurface] = &[
    ControllerSurface {
        surface: "/serial/status",
        capability: "serial.status",
        supported: true,
        reason: "Reports the configured device path, line settings, connection state, byte counters, bounded buffer, and typed last error.",
        python_reference: "node/serial_capture.py::NodeSerialCapture.state_dict",
        path_prefix: "/serial/status",
        code: "serial.status",
        remediation: "",
    },
    ControllerSurface {
        surface: "/serial/write",
        capability: "serial.write",
        supported: true,
        reason: "Writes exactly one bounded text or hex payload to the configured device; no newline is appended.",
        python_reference: "node/serial_capture.py::NodeSerialCapture.send",
        path_prefix: "/serial/write",
        code: "serial.write",
        remediation: "",
    },
    ControllerSurface {
        surface: "/serial/read",
        capability: "serial.read",
        supported: true,
        reason: "Returns a bounded base64 slice of the retained receive buffer with an absolute since/next cursor.",
        python_reference: "node/serial_capture.py::NodeSerialCapture.get_buffer",
        path_prefix: "/serial/read",
        code: "serial.read",
        remediation: "",
    },
    ControllerSurface {
        surface: "/serial/stream",
        capability: "serial.stream",
        supported: true,
        reason: "Streams bounded SSE data events from the receive buffer and ends on an idle timeout or a disconnected device.",
        python_reference: "node/serial_capture.py::register_serial_routes.stream_sse",
        path_prefix: "/serial/stream",
        code: "serial.stream",
        remediation: "",
    },
    ControllerSurface {
        surface: "/serial/control",
        capability: "serial.control",
        supported: true,
        reason: "Sets DTR and/or RTS only when the node was started with --serial-control; otherwise the route answers a typed 403 serial.control-disabled.",
        python_reference: "node/serial_capture.py::NodeSerialCapture.setup",
        path_prefix: "/serial/control",
        code: "serial.control",
        remediation: "",
    },
];

/// Controller-only surfaces that this node advertises as unsupported.
///
/// Each entry is backed by a typed 501 route: the controller gets a stable
/// `supported:false` document instead of a fabricated success, an empty 200, or
/// a bare 404. Keep this list in sync with the handlers in [`legacy_router`] and
/// [`public_router`], and with `docs/phase4-controller-surfaces.md`.
pub const CONTROLLER_ONLY_SURFACES: &[ControllerSurface] = &[
    ControllerSurface {
        surface: "/api/v1/rpa/*",
        capability: "rpa",
        supported: false,
        reason: "This node has no RPA script runner, wait_for_text, bitmap screen OCR, click helper, or BIOS automation.",
        python_reference: "node/rpa.py (node/node.py:437-445)",
        path_prefix: "/api/v1/rpa",
        code: "rpa.unsupported",
        remediation: "Use the implemented /api/v1/rpa/{type,key,screenshot,ocr} routes, or keep \
                      the Python hardware node for script/BIOS automation; this node does not \
                      claim an RPA action ran.",
    },
    ControllerSurface {
        surface: "/backup/*",
        capability: "backup",
        supported: false,
        reason: "Local status/init/run/snapshots/restore/delete/stats/verify/estimate are implemented; app inventory, config writes, onboarding, snapshot file browsing, and cloud destinations remain unsupported.",
        python_reference: "agent/backup.py (agent/ozma_desktop_agent.py)",
        path_prefix: "/backup",
        code: "backup.unsupported",
        remediation: "Use /backup/status|init|run|snapshots|restore|stats|verify|estimate and DELETE /backup/snapshots/{id}, or run the Python desktop agent with restic for app inventory, cloud destinations, and scheduling.",
    },
    ControllerSurface {
        surface: "/ui/windows",
        capability: "ui_windows",
        supported: false,
        reason: "This node has no in-guest accessibility or window provider.",
        python_reference: "agent/ozma_desktop_agent.py:996",
        path_prefix: "/ui/windows",
        code: "ui.windows_unsupported",
        remediation: "Install an in-guest accessibility agent on the target; this node does not \
                      invent window metadata.",
    },
    ControllerSurface {
        surface: "/serial/*",
        capability: "serial",
        supported: false,
        reason: "Serial status/write/read/stream/control are implemented opt-in with --serial-device; the historical buffer/send/state/text/setup sub-surfaces remain unsupported.",
        python_reference: "node/serial_capture.py",
        path_prefix: "/serial",
        code: "serial.unsupported",
        remediation: "Start the node with --serial-device PATH and use /serial/status, /serial/write, \
                      /serial/read, /serial/stream, or /serial/control (with --serial-control). Other \
                      /serial/* sub-surfaces are typed 501s; no buffer or send result is fabricated.",
    },
    ControllerSurface {
        surface: "/rgb/*",
        capability: "rgb",
        supported: false,
        reason: "This node has no WS2812/SPI LED output driver.",
        python_reference: "node/rgb_leds.py",
        path_prefix: "/rgb",
        code: "rgb.unsupported",
        remediation: "Configure RGB lighting on the host that owns the SPI/LED controller; this \
                      node does not claim an effect was applied.",
    },
    ControllerSurface {
        surface: "/current",
        capability: "current",
        supported: false,
        reason: "This node has no power-rail current/voltage sensor driver.",
        python_reference: "node/current_sensor.py",
        path_prefix: "/current",
        code: "power.current_unsupported",
        remediation: "Attach a sensor driver that exposes /current before enabling the \
                      controller's USB power panel.",
    },
];

/// Find the advertised controller-only surface for a request path.
fn controller_surface_for_path(path: &str) -> Option<&'static ControllerSurface> {
    CONTROLLER_ONLY_SURFACES.iter().find(|surface| {
        path == surface.path_prefix || path.starts_with(&format!("{}/", surface.path_prefix))
    })
}

/// Shared, optional power backend used by the compatibility power routes.
///
/// `None` means no power backend is configured; the routes answer 503 rather
/// than pretending the VM can be controlled.
pub type SharedPower = Arc<tokio::sync::Mutex<Box<dyn ozma_power::PowerBackend>>>;

/// Shared state for the connection, power, input, codec, and audio endpoints.
#[derive(Clone)]
pub struct ConnectionState {
    pub info: ConnectionInfo,
    pub stats: CompatStats,
    pub power: Option<SharedPower>,
    pub hls: Option<HlsStatus>,
    /// Restartable HLS pipeline, when `--hls` started a controller.
    pub hls_control: Option<HlsController>,
    /// The one sink shared with session input and the UDP HID listener.
    pub input_sink: Option<SharedSink>,
    /// Guest keyboard layout used by `POST /input/type`.
    pub keyboard_layout: KeyboardLayout,
    /// Base status derived from the advertised capabilities. When a
    /// [`CodecControl`] is attached its live status replaces this value.
    pub codecs: CodecStatus,
    /// Runtime codec override control, when the node has a video encoder.
    pub codec_control: Option<CodecControl>,
    /// Runtime audio routing control. `None` leaves `/audio/*` at typed 501.
    pub audio: Option<AudioControl>,
    /// Runtime USB gadget control. `None` leaves `/usb` at a typed 501.
    pub usb: Option<UsbControl>,
    /// External WebRTC helper. `None` means no `--webrtc-helper` was given;
    /// `Some(control)` may be a configured-but-failed helper.
    pub webrtc: Option<WebRtcControl>,
    /// One-shot display snapshot control. `None` leaves `/display/snapshot`
    /// at a typed 503 rather than fabricating an image.
    pub snapshot: Option<SnapshotControl>,
    /// Optional tesseract OCR control for `POST /api/v1/rpa/ocr`.
    ///
    /// The default resolves `tesseract` on `PATH`; without it the route keeps
    /// the typed 503 `rpa.ocr-unavailable` result.
    pub ocr: OcrControl,
    /// Bounded `/video` MJPEG stream accounting. Always present; the route
    /// itself reports a typed 503 when no snapshot control/source exists.
    pub video_streams: VideoStreams,
    /// H.264/HEVC encoder configuration for `/video?codec=h264|hevc`.
    /// `None` advertises MJPEG only and returns a typed 503 for encoded
    /// requests.
    pub video_encoding: Option<VideoEncoding>,
    /// Bounded host sampler for the public `/agent/metrics` document.
    pub metrics: crate::agent_metrics::AgentMetricsSampler,
    /// Optional deep hardware sampler used to enrich `/agent/metrics` with
    /// the documented AIDA64 sensor IDs.
    pub hardware: Option<crate::hardware::HardwareState>,
    /// Bounded read-only storage/process/self surfaces and their startup
    /// availability advertisement.
    pub host_surfaces: crate::host_surfaces::HostSurfacesState,
    /// Opt-in bounded process control installed by `--app-control`. `None`
    /// leaves every `/apps/{pid}/*` control route at a typed 503 and never
    /// signals a process.
    pub app_control: Option<crate::app_control::AppControl>,
    /// Controller-facing display outputs (VM consoles or raw host outputs).
    pub display_outputs: serde_json::Value,
    /// Live capture console/provider and any multi-console limitation.
    pub display_capture: DisplayCaptureStatus,
    /// Runtime console selector. When present it is the source of truth for
    /// `/api/v1/connection.display_outputs`/`display_capture` and backs
    /// `POST /display/select`.
    pub display_control: Option<DisplayControl>,
    /// Bounded per-console broker used by `?console=` snapshots/info. It never
    /// swaps the active HLS/WebRTC registry.
    pub console_broker: Option<ConsoleBroker>,
    /// Opt-in bounded per-console HLS manager. Its independent pipelines are
    /// never swapped into the active WebRTC/registry path.
    pub multi_console_hls: Option<MultiConsoleHls>,
    /// Opt-in bounded per-console WebRTC manager. When it owns independent
    /// D-Bus consoles it is also the source of truth for `webrtc`.
    pub multi_console_webrtc: Option<MultiConsoleWebrtc>,
    /// Live capture transport/source for `/api/v1/connection`.
    pub capture: CaptureStatus,
    /// Optional live VNC registration state. When managed, a stopped internal
    /// listener stops being advertised instead of leaving a dead port in the
    /// registration document. External QEMU endpoints remain the fallback.
    pub vnc_registration: Option<VncRegistrationState>,
    /// Desktop-profile geometry/clipboard/wallpaper controls. `None` on other
    /// profiles keeps those routes at a typed 501; on `desktop` it is always
    /// attached and reports tool absence through typed 503s.
    pub desktop_controls: Option<crate::desktop_compat::DesktopControls>,
    /// Opt-in local Restic backup provider. `None` means no `--backup-repo`
    /// was configured; core `/backup/*` routes then answer a typed 503 and
    /// unsupported sub-surfaces stay typed 501.
    pub backup: Option<crate::backup::BackupService>,
    /// Opt-in bounded BlueZ/`bluetoothctl` provider. `None` means no
    /// `--bluetooth` was configured; every `/bluetooth/*` route answers a typed
    /// 503 `bluetooth.not-configured` rather than a fake device list.
    pub bluetooth: Option<crate::bluetooth::BluetoothService>,
    /// Opt-in bounded serial/UART provider. `None` means no `--serial-device`
    /// was configured; the implemented routes answer a typed 503
    /// `serial.not-configured` and every other `/serial/*` path stays a typed
    /// 501. No device is opened unless the service is configured.
    pub serial: Option<crate::serial::SerialService>,
    /// Opt-in 1-Wire/I²C/SPI/GPIO/SocketCAN bus surface. The default enables
    /// nothing and every `/buses/*` route answers a typed 503
    /// `bus.not-configured`.
    pub buses: crate::buses::BusState,
}

/// Registration view of the optional internal RFB listener.
#[derive(Clone, Debug, Default)]
pub struct VncRegistrationState {
    live: Arc<std::sync::RwLock<Option<(String, u16)>>>,
    external: Option<(String, u16)>,
}

impl VncRegistrationState {
    /// Manage a node's VNC registration: `live` is cleared when the internal
    /// listener stops; `external` is the configured QEMU endpoint fallback.
    pub fn new(live: Option<(String, u16)>, external: Option<(String, u16)>) -> Self {
        Self {
            live: Arc::new(std::sync::RwLock::new(live)),
            external,
        }
    }

    /// Shared slot a VNC listener watcher clears on stop.
    pub fn live_handle(&self) -> Arc<std::sync::RwLock<Option<(String, u16)>>> {
        self.live.clone()
    }

    /// Currently advertised endpoint: live listener first, then external.
    pub fn resolved(&self) -> Option<(String, u16)> {
        self.live
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .or_else(|| self.external.clone())
    }
}

impl ConnectionState {
    /// Registration document for `/api/v1/connection`, applying live VNC state.
    ///
    /// Only nodes that explicitly install [`VncRegistrationState`] are managed;
    /// everything else keeps the startup [`crate::registration::RegistrationExtras`].
    pub fn registration_snapshot(&self) -> crate::registration::RegistrationExtras {
        let mut registration = self.info.registration.clone();
        if let Some(vnc) = self.vnc_registration.as_ref() {
            registration.vnc = match vnc.resolved() {
                Some((host, port)) => {
                    crate::registration::VncRegistration::Configured { host, port }
                }
                None => crate::registration::VncRegistration::NotConfigured,
            };
        }
        registration
    }

    pub fn new(info: ConnectionInfo, stats: CompatStats) -> Self {
        let host_surfaces = crate::host_surfaces::HostSurfacesState::host_with_identity(
            info.node_id.clone(),
            info.profile.clone(),
        );
        Self {
            info,
            stats,
            power: None,
            hls: None,
            hls_control: None,
            input_sink: None,
            keyboard_layout: KeyboardLayout::Us,
            codecs: CodecStatus::not_configured(),
            codec_control: None,
            audio: None,
            usb: None,
            webrtc: None,
            snapshot: None,
            ocr: OcrControl::detect(),
            video_streams: VideoStreams::default(),
            video_encoding: None,
            metrics: crate::agent_metrics::AgentMetricsSampler::default(),
            hardware: None,
            host_surfaces,
            app_control: None,
            display_outputs: serde_json::json!([]),
            display_capture: DisplayCaptureStatus::default(),
            display_control: None,
            console_broker: None,
            multi_console_hls: None,
            multi_console_webrtc: None,
            capture: CaptureStatus::default(),
            vnc_registration: None,
            desktop_controls: None,
            backup: None,
            bluetooth: None,
            serial: None,
            buses: crate::buses::BusState::default(),
        }
    }

    /// Attach the actual capture transport and enumerated source.
    ///
    /// This is derived from the registry's first advertised capability. A
    /// failed portal handshake or an absent X display keeps `available:false`
    /// with no kind/source instead of naming a source that does not exist.
    pub fn with_capture(mut self, capture: CaptureStatus) -> Self {
        self.capture = capture;
        self
    }

    /// Attach the deep hardware sampler used by `/agent/metrics`.
    ///
    /// The sampler is shared with the `/hardware` route and the opt-in AIDA64
    /// listener, so its short cache prevents three callers from running
    /// SMART/NVMe/nvidia-smi collection concurrently.
    pub fn with_hardware(mut self, hardware: crate::hardware::HardwareState) -> Self {
        self.hardware = Some(hardware);
        self
    }

    /// Attach the controller-facing display outputs and live capture status.
    ///
    /// The outputs come from `NodeConfig::resolve_display_outputs` (or the raw
    /// `--display-outputs-json` escape hatch). Only the selected console has a
    /// non-empty `capture_source_id`; the status names the limitation when the
    /// provider captures one head.
    pub fn with_display_outputs(
        mut self,
        outputs: serde_json::Value,
        capture: DisplayCaptureStatus,
    ) -> Self {
        self.display_outputs = outputs;
        self.display_capture = capture;
        self
    }

    /// Attach the runtime console selector.
    ///
    /// Once attached, `/api/v1/connection` reads the live selection from this
    /// shared control instead of the static values passed to
    /// [`ConnectionState::with_display_outputs`], so a successful
    /// `POST /display/select` is visible immediately.
    pub fn with_display_control(mut self, control: Option<DisplayControl>) -> Self {
        self.display_control = control;
        self
    }

    /// Attach the bounded multi-console snapshot broker.
    ///
    /// The broker serves `GET /display/snapshot?console=N` and
    /// `GET /display/info?console=N` for advertised consoles without replacing
    /// the active registry used by HLS/WebRTC and the default snapshot route.
    pub fn with_console_broker(mut self, broker: Option<ConsoleBroker>) -> Self {
        self.console_broker = broker;
        self
    }

    /// Attach the opt-in per-console HLS manager.
    ///
    /// When present, `/api/v1/connection` reports its bounded per-console
    /// status, `hls` describes the active console's independent pipeline, and
    /// `/display/info` includes the same per-console state.
    pub fn with_multi_console_hls(mut self, manager: Option<MultiConsoleHls>) -> Self {
        self.multi_console_hls = manager;
        self
    }

    /// Attach the opt-in per-console WebRTC manager.
    ///
    /// When the manager owns independent consoles, `/api/v1/connection`
    /// reports its active helper as `webrtc` and adds a per-console
    /// `multi_console_webrtc` object. A single-head provider keeps the legacy
    /// single helper and reports the manager's typed `unsupported` limitation.
    pub fn with_multi_console_webrtc(mut self, manager: Option<MultiConsoleWebrtc>) -> Self {
        self.multi_console_webrtc = manager;
        self
    }

    /// Current display outputs and capture status.
    pub fn display_status(&self) -> DisplayStatus {
        match self.display_control.as_ref() {
            Some(control) => control.status(),
            None => DisplayStatus {
                display_outputs: self.display_outputs.clone(),
                display_capture: self.display_capture.clone(),
            },
        }
    }

    /// Attach a connected power backend.
    pub fn with_power(mut self, power: Option<SharedPower>) -> Self {
        self.power = power;
        self
    }

    /// Attach the HLS pipeline status.
    ///
    /// `None` and a status in [`crate::hls::HlsState::NotConfigured`] both report
    /// `not-configured`; a configured pipeline reports `running` only once its
    /// playlist exists.
    pub fn with_hls(mut self, hls: Option<HlsStatus>) -> Self {
        self.hls = hls;
        self
    }

    /// Attach a restartable HLS controller.
    ///
    /// Its current status wins over the static [`HlsStatus`] passed to
    /// [`ConnectionState::with_hls`], so `/api/v1/connection` reflects a
    /// pipeline that `POST /codec` restarted.
    pub fn with_hls_controller(mut self, hls: Option<HlsController>) -> Self {
        if let Some(controller) = hls.as_ref() {
            self.hls = Some(controller.status());
        }
        self.hls_control = hls;
        self
    }

    /// Attach the shared input sink used by session input and UDP HID.
    ///
    /// This must be the same [`SharedSink`] passed to the UDP listener; the
    /// compatibility `/input/*` routes never open a second backend.
    pub fn with_input_sink(mut self, sink: Option<SharedSink>) -> Self {
        self.input_sink = sink;
        self
    }

    /// Select the guest keyboard layout used to map typed text to HID usages.
    ///
    /// The layout must match the guest's console keymap (`us`, `uk`, `de`, or
    /// `fr`). It is not detected from the guest.
    pub fn with_keyboard_layout(mut self, layout: KeyboardLayout) -> Self {
        self.keyboard_layout = layout;
        self
    }

    /// Attach codec status derived from the node's advertised capabilities.
    pub fn with_codecs(mut self, codecs: CodecStatus) -> Self {
        self.codecs = codecs;
        self
    }

    /// Attach a runtime codec override control.
    ///
    /// Its live status (including stored override/receipt) replaces the
    /// capability-derived base status on `/api/v1/connection` and `GET /codec`.
    pub fn with_codec_control(mut self, control: Option<CodecControl>) -> Self {
        if let Some(control) = control.as_ref() {
            self.codecs = control.status();
        }
        self.codec_control = control;
        self
    }

    /// Attach the runtime audio routing control.
    ///
    /// `None` keeps `audio: "not-configured"` and typed 501s. A configured but
    /// failed control reports `failed` plus `audio_error`.
    pub fn with_audio(mut self, audio: Option<AudioControl>) -> Self {
        self.audio = audio;
        self
    }

    /// Attach the runtime USB gadget control.
    ///
    /// `None` keeps `usb.status: "not-configured"` and the typed 501 `/usb`
    /// response. A configured gadget that failed to start reports `failed`
    /// with its typed reason and answers 503, never a fabricated gadget.
    pub fn with_usb(mut self, usb: Option<UsbControl>) -> Self {
        self.usb = usb;
        self
    }

    /// Attach the external WebRTC helper control surface.
    ///
    /// `None` means WebRTC was not configured and reports `not-configured`.
    /// `Some(WebRtcControl::failed(...))` is a configured helper that could
    /// not start and reports `failed` plus `webrtc_error`.
    pub fn with_webrtc(mut self, webrtc: Option<WebRtcControl>) -> Self {
        self.webrtc = webrtc;
        self
    }

    /// Install the live VNC registration view (internal listener + external
    /// fallback). Without this the startup registration document is unchanged.
    pub fn with_vnc_registration(mut self, vnc: VncRegistrationState) -> Self {
        self.vnc_registration = Some(vnc);
        self
    }

    /// Attach desktop-profile geometry, clipboard, and wallpaper controls.
    ///
    /// Only `--profile desktop` installs these. On other profiles the routes
    /// stay at a typed 501 instead of probing the host seat.
    pub fn with_desktop_controls(
        mut self,
        controls: Option<crate::desktop_compat::DesktopControls>,
    ) -> Self {
        self.desktop_controls = controls;
        self
    }

    /// Attach the opt-in local Restic backup provider.
    ///
    /// Without it, `GET /backup/status` reports `configured:false`, core
    /// `/backup/*` operations return a typed 503, and app/config/cloud
    /// sub-surfaces remain typed 501s.
    pub fn with_backup(mut self, backup: Option<crate::backup::BackupService>) -> Self {
        self.backup = backup;
        self
    }

    /// Attach the opt-in bounded Bluetooth provider.
    ///
    /// Without it, `/bluetooth/*` stays mounted behind the bearer guard and
    /// returns a typed 503 `bluetooth.not-configured`; no device list or adapter
    /// state is invented.
    pub fn with_bluetooth(mut self, bluetooth: Option<crate::bluetooth::BluetoothService>) -> Self {
        self.bluetooth = bluetooth;
        self
    }

    /// Attach the opt-in bounded serial/UART provider (`--serial-device`).
    ///
    /// Without it, `/serial/status|write|read|stream` answer a typed 503
    /// `serial.not-configured` and the other `/serial/*` sub-surfaces keep their
    /// typed 501. No serial device is opened unless this service is configured.
    pub fn with_serial(mut self, serial: Option<crate::serial::SerialService>) -> Self {
        self.serial = serial;
        self
    }

    /// Attach the opt-in bounded bus surface (`--onewire`, `--i2c-allow`,
    /// `--spi-allow`, `--gpio-allow`, `--can-allow`). Defaults to a
    /// not-configured state.
    pub fn with_buses(mut self, buses: crate::buses::BusState) -> Self {
        self.buses = buses;
        self
    }

    /// Attach the opt-in bounded application-control surface (`--app-control`).
    ///
    /// Without it, `/apps` stays read-only and every `/apps/{pid}/*` control
    /// route returns a typed 503 `apps.control-disabled`; no process is ever
    /// signalled.
    pub fn with_app_control(mut self, app_control: Option<crate::app_control::AppControl>) -> Self {
        self.app_control = app_control;
        self
    }

    /// Attach the one-shot display snapshot control.
    ///
    /// `None` keeps `GET /display/snapshot` at a typed 503; attaching an empty
    /// registry is also honest (the route reports no source) and lets
    /// `GET /display/info` describe the node.
    pub fn with_snapshot(mut self, snapshot: Option<SnapshotControl>) -> Self {
        self.snapshot = snapshot;
        self
    }

    /// Attach the optional tesseract control used by `/api/v1/rpa/ocr`.
    ///
    /// The default detects `tesseract` on `PATH`. Tests and nodes without OCR
    /// can install [`OcrControl::unavailable`] to keep the typed 503 path.
    pub fn with_ocr(mut self, ocr: OcrControl) -> Self {
        self.ocr = ocr;
        self
    }

    /// Replace the default `/video` concurrent-stream allowance.
    ///
    /// The default is [`crate::video::DEFAULT_MAX_STREAMS`]; tests and
    /// resource-constrained nodes can lower or raise it. Values below one are
    /// clamped to one.
    pub fn with_video_streams(mut self, streams: VideoStreams) -> Self {
        self.video_streams = streams;
        self
    }

    /// Attach the H.264/HEVC encoder used by `/video?codec=h264|hevc`.
    ///
    /// `None` keeps the route MJPEG-only and makes an encoded request a typed
    /// `503 video.encoder_unavailable`. This is independent of `--hls`: the
    /// live stream writes MPEG-TS directly and does not need an HLS directory.
    pub fn with_video_encoding(mut self, encoding: Option<VideoEncoding>) -> Self {
        self.video_encoding = encoding;
        self
    }

    /// True when the configured sink records without injecting.
    pub fn input_is_recording_only(&self) -> bool {
        matches!(
            self.info.sink_kind.as_str(),
            SINK_FAKE_RECORDING | "recording"
        )
    }
}

/// `GET /api/v1/connection` response.
///
/// `hls` is `"running"`, `"not-configured"`, or `"failed"`; `hls_error` carries
/// the reason when it is `failed` (including a configured pipeline whose
/// manifest does not exist yet). `webrtc` is `"running"`, `"not-configured"`,
/// or `"failed"`, with `webrtc_error` explaining a configured helper that did
/// not start. The endpoint must never imply a streaming capability the node
/// does not have. `power` is either `"not-configured"` or
/// `{"status": "...", "available": true|false}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectionResponse {
    pub ok: bool,
    pub node_id: String,
    pub profile: String,
    pub api_port: u16,
    pub quic_port: u16,
    pub hid_udp_port: u16,
    pub sink_kind: String,
    pub hid: HidListenerSnapshot,
    /// Power backend status, or the string `"not-configured"`.
    pub power: serde_json::Value,
    /// Input sink kind and whether events are actually injected.
    pub input: InputStatus,
    /// Codec state derived from the advertised video capabilities.
    pub codec: CodecStatus,
    pub hls: &'static str,
    /// Why HLS is not `running`, when it is configured and failing.
    pub hls_error: Option<String>,
    /// Whether HLS audio was configured for the active pipeline.
    ///
    /// Configuration intent only; it never claims that ffmpeg opened the
    /// monitor, decoded samples, or negotiated an audio track.
    pub hls_audio_configured: bool,
    /// Configured HLS audio monitor source, when one was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hls_audio_source: Option<String>,
    /// Exact ffmpeg HLS audio encoder, when audio was configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hls_audio_encoder: Option<String>,
    pub webrtc: &'static str,
    /// Why WebRTC is not `running`, when it is configured and failing.
    pub webrtc_error: Option<String>,
    /// Which helper implementation serves `webrtc`: `native`, `external`, or
    /// `not-configured`.
    ///
    /// `native` means the video-only Rust helper; audio is never claimed for it.
    pub webrtc_backend: &'static str,
    /// Resolved helper geometry, when a helper is configured.
    ///
    /// Additive: consumers that only read `webrtc`/`webrtc_error` are
    /// unaffected. Omitted entirely when WebRTC is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_geometry: Option<WebRtcGeometryStatus>,
    /// Audio routing status: `running`, `not-configured`, or `failed`.
    pub audio: &'static str,
    /// Why audio routing is `failed` (for example a missing `pactl`).
    pub audio_error: Option<String>,
    /// USB gadget status (`not-configured`, `running`, `unbound`, `failed`).
    pub usb: UsbConnectionStatus,
    /// Whether a WebRTC audio monitor was configured for the helper.
    ///
    /// This is configuration intent only; it does not claim that the worker
    /// opened the monitor or produced samples.
    pub webrtc_audio_configured: bool,
    /// Configured WebRTC audio monitor name, when one was supplied.
    pub webrtc_audio_source: Option<String>,
    /// Controller-facing display outputs (same JSON as registration).
    pub display_outputs: serde_json::Value,
    /// Which console the live capture source targets, plus any limitation.
    pub display_capture: DisplayCaptureStatus,
    /// Opt-in simultaneous per-console HLS state, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_console_hls: Option<MultiConsoleHlsStatus>,
    /// Opt-in simultaneous per-console WebRTC state, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_console_webrtc: Option<MultiConsoleWebrtcStatus>,
    /// Actual capture transport and enumerated source, or `available:false`.
    pub capture: CaptureStatus,
    /// `/video` MJPEG route facts and live stream count. `active_streams` is
    /// zero when idle; the document never claims a stream before one starts.
    pub video: VideoStatus,
    /// Optional registration fields (seat, guest IP, VNC) plus typed entries for
    /// controller registration fields this node does not implement.
    pub registration: crate::registration::RegistrationExtras,
    /// Controller-facing surfaces with an explicit `supported` flag.
    ///
    /// Implemented RPA primitives (`supported:true`) are listed next to the
    /// controller-only surfaces this node does not implement (`supported:false`,
    /// with a typed 501). The controller can read this document instead of
    /// probing every proxy and guessing from a 404 or timeout.
    pub controller_surfaces: Vec<ControllerSurface>,
    /// Bounded host-surface availability (`storage`, `apps`, `self`).
    ///
    /// Discovery only: availability comes from cheap startup procfs opens, and
    /// the document never contains a mount list, process list, pid, or RSS.
    pub host_surfaces: crate::host_surfaces::HostSurfaceAdvertisement,
    /// Desktop-profile geometry/clipboard/wallpaper availability. Omitted on
    /// other profiles. Availability is a `PATH` lookup, not a probe command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_controls: Option<crate::desktop_compat::DesktopControlsStatus>,
    /// Opt-in Bluetooth provider facts (configured adapter and scan bound).
    /// Omitted entirely without `--bluetooth`; no adapter is probed here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bluetooth: Option<crate::bluetooth::BluetoothAdvertisement>,
}

/// Build the control compatibility router. Mount it before the node API auth
/// layer so these endpoints are covered by the same bearer-token guard.
///
/// This router deliberately excludes the controller's read-only probes
/// (`/display/snapshot`, `/display/info`, `/video`, `/display/mjpeg`,
/// `/node/status`, `/ui/hints`, `/agent/metrics`, `/audio/nodes`) and the
/// generated-sweep `POST /audio/sweep`. Those live in [`public_router`] and must
/// be merged **after** the guard layer so the unmodified controller can poll
/// them without a bearer token. Control surfaces (`/input/*`, `/power/*`,
/// `/audio/*`, `/codec`, `/webrtc/*`, and the legacy mutating aliases) stay
/// here and stay guarded.
pub fn compat_router(state: ConnectionState) -> Router {
    connection_router(state.clone())
        .merge(power_router(state.clone()))
        .merge(input_router(state.clone()))
        .merge(rpa_router(state.clone()))
        .merge(codec_router(state.clone()))
        .merge(webrtc_router(state.clone()))
        .merge(audio_router(state.clone()))
        .merge(usb_router(state.clone()))
        .merge(display_router(state.clone()))
        .merge(crate::desktop_compat::router(state.clone()))
        .merge(crate::host_surfaces::router(state.host_surfaces.clone()))
        .merge(crate::app_control::router(state.app_control.clone()))
        .merge(crate::backup::router(state.backup.clone()))
        .merge(crate::bluetooth::router(state.bluetooth.clone()))
        .merge(crate::serial::router(state.serial.clone()))
        .merge(crate::buses::router(state.buses.clone()))
        .merge(legacy_router(state))
}

/// Build the controller's unauthenticated read-only compatibility router.
///
/// The unmodified Python controller fetches these routes without a node bearer
/// token:
///
/// * `GET /display/snapshot` (`controller/stream.py`,
///   `controller/terminal_bridge.py`, `controller/api.py`)
/// * `GET /video` and `GET /display/mjpeg` — read-only MJPEG streams backed by
///   the same capture registry and console broker as `/display/snapshot`
/// * `GET /ui/hints` (`controller/agent_engine.py`, `controller/api.py`)
/// * `GET /node/status` (`controller/mesh_intelligence.py`)
/// * `GET /agent/metrics` (`controller/device_metrics.py`)
///
/// `GET /display/info`, `GET /audio/nodes`, and `POST /audio/sweep` are mounted
/// here as well. All are read-only metadata except the sweep, which plays a
/// generated signal and captures it; none changes the graph. Merge this router
/// **after** the bearer layer; see [`crate::api::serve_with_scene_replay_and_sink`].
pub fn public_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/display/snapshot", get(display_snapshot))
        .route("/snapshot", get(display_snapshot))
        .route(VIDEO_ROUTE, get(video_stream))
        .route(VIDEO_ALIAS, get(video_stream))
        .route("/display/info", get(display_info))
        .route("/node/status", get(node_status))
        .route("/status", get(node_status))
        .route("/agent/metrics", get(agent_metrics))
        .route("/ui/hints", get(ui_hints))
        .route("/ui/windows", get(ui_windows))
        .route("/audio/nodes", get(audio_nodes))
        .route("/audio/sweep", post(audio_sweep))
        .with_state(state)
}

/// The original connection-only router, retained for callers that do not need
/// the rest of the compatibility surface.
pub fn connection_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/api/v1/connection", get(connection))
        .with_state(state)
}

/// Compatibility power routes: `GET /power/state` and `POST /power/{action}`.
pub fn power_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/power/state", get(power_state))
        .route("/power/:action", axum::routing::post(power_action))
        .with_state(state)
}

/// Controller input routes backed by the shared [`SharedSink`].
///
/// These mirror the Python soft node's `/input/key`, `/input/mouse`, and
/// `/input/type` endpoints. `key` accepts Linux evdev keycodes, while the
/// native input path carries USB HID usages; the conversion happens here.
pub fn input_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/input/key", post(input_key))
        .route("/input/mouse", post(input_mouse))
        .route("/input/type", post(input_type))
        .with_state(state)
}

/// Controller RPA routes backed by the real node primitives.
///
/// Type/key use the same [`SharedSink`], keyboard layout, and hold timing as
/// `/input/*`. Screenshot uses the configured capture source and console broker
/// (the same path as `/display/snapshot`). OCR captures one frame and runs the
/// optional tesseract CLI; it never synthesizes text. The remaining
/// `/api/v1/rpa/*` sub-surfaces stay in [`legacy_router`] as typed 501s.
pub fn rpa_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/api/v1/rpa/type", post(rpa_type))
        .route("/api/v1/rpa/key", post(rpa_key))
        .route(
            "/api/v1/rpa/screenshot",
            get(rpa_screenshot).post(rpa_screenshot),
        )
        .route("/api/v1/rpa/ocr", post(rpa_ocr))
        .with_state(state)
}

/// Codec status routes. `POST /codec` restarts the local HLS pipeline when it
/// is running; WebRTC is never switched by this route.
pub fn codec_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/codec", get(codec_get).post(codec_set))
        .with_state(state)
}

/// Controller audio-routing compatibility routes.
///
/// With `--audio-router pipewire|pulseaudio` these drive a real
/// [`AudioRouter`] (`pactl` graph control plus `pw-play`/`paplay`) and return
/// typed 200/400/502/503 results. The profile routes additionally require
/// `--audio-profile-dir`; without it they are typed 501. Filter-chain
/// application is always a typed 501. Without a router every route answers a
/// typed 501 rather than a success-shaped no-op.
pub fn audio_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/audio/apply", post(audio_apply))
        .route("/audio/remove", post(audio_remove))
        .route("/audio/play", post(audio_play))
        .route("/audio/stop", post(audio_stop))
        .route(
            "/audio/room-correction/profile",
            get(audio_profile_get)
                .post(audio_profile_put)
                .delete(audio_profile_delete),
        )
        .route(
            "/audio/room-correction/apply",
            post(audio_room_correction_apply).delete(audio_room_correction_delete),
        )
        .with_state(state)
}

/// Controller WebRTC signaling routes, proxied to the external helper.
///
/// `POST /webrtc/offer` accepts `{"sdp": "...", "type": "offer"}` and returns
/// `{"sdp": "...", "type": "answer"}` plus an optional `session` id.
/// `POST /webrtc/candidate` accepts
/// `{"session": "...", "candidate": "...", "sdpMLineIndex": 0}` and forwards a
/// late (trickled) remote ICE candidate to that helper session.
/// `POST /webrtc/bitrate` accepts `{"bitrate": <bps>}`. All are mounted through
/// [`compat_router`] before the node API's bearer guard. A node with no helper
/// answers 503; a helper that does not acknowledge a request answers a typed
/// 501/502 rather than a fake success.
pub fn webrtc_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/webrtc/offer", post(webrtc_offer))
        .route("/webrtc/candidate", post(webrtc_candidate))
        .route("/webrtc/bitrate", post(webrtc_bitrate))
        .with_state(state)
}

/// Controller USB gadget and virtual-media routes.
///
/// `GET /usb` is the controller's read-only probe (`node/node.py::_read_usb_info`
/// shape). `POST /usb/media/attach` and `/usb/media/detach` operate on the
/// configured mass-storage LUN through the [`UsbGadgetController`]; they never
/// mount the image, write a block device, or create a backing file. `GET
/// /usb/nbd` plus `POST /usb/nbd/start|stop` are an opt-in loopback NBD export
/// (one file at a time) that never mounts the image or opens a block device.
///
/// Without `--usb-gadget` every route keeps the typed 501 surface. A gadget
/// that was requested but could not be created/bound answers 503 with the
/// typed reason instead of a success-shaped response.
pub fn usb_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/usb", get(usb_status))
        .route("/usb/media", get(usb_media_status))
        .route("/usb/media/start", post(usb_media_start))
        .route("/usb/media/stop", post(usb_media_stop))
        .route("/usb/media/attach", post(usb_media_attach))
        .route("/usb/media/detach", post(usb_media_detach))
        .route("/usb/hotplug", get(usb_hotplug))
        .route("/usb/nbd", get(usb_nbd_status))
        .route("/usb/nbd/start", post(usb_nbd_start))
        .route("/usb/nbd/stop", post(usb_nbd_stop))
        .with_state(state)
}

/// Display compatibility control routes.
///
/// `GET /display/snapshot`, `GET /display/info`, `GET /video`, and
/// `GET /display/mjpeg` are read-only and live in [`public_router`] so the
/// controller's unauthenticated preview pollers work with `--api-token`. This
/// router keeps only the mutating console selector.
pub fn display_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/display/select", post(display_select))
        .with_state(state)
}

/// Legacy controller control routes.
///
/// `/node/status`, `/agent/metrics`, and `/ui/hints` are read-only and live in
/// [`public_router`]. The routes below are either mutating or unsupported
/// control surfaces: they stay guarded and answer a typed 501 with a code,
/// error, and remediation instead of a success-shaped no-op.
pub fn legacy_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/rgb/state", get(legacy_stub))
        .route("/rgb/effect", get(legacy_stub).post(legacy_stub))
        .route("/rgb/set", post(legacy_stub))
        .route("/rgb/off", post(legacy_stub))
        .route("/current", get(legacy_stub))
        .route("/current/history", get(legacy_stub))
        .route("/session/init", post(legacy_stub))
        .route("/serial/*rest", get(legacy_stub).post(legacy_stub))
        .route("/api/v1/rpa/*rest", get(legacy_stub).post(legacy_stub))
        .route("/backup/*rest", get(legacy_stub).post(legacy_stub))
        .with_state(state)
}

/// Resolve the active single-HLS status name/error from the controller or the
/// static fallback status.
fn hls_status_name(
    controller: Option<&HlsController>,
    fallback: Option<&HlsStatus>,
) -> (&'static str, Option<String>) {
    match (controller, fallback) {
        (Some(controller), _) => {
            let status = controller.status();
            (status.status_name(), status.error())
        }
        (None, None) => ("not-configured", None),
        (None, Some(status)) => (status.status_name(), status.error()),
    }
}

/// Configuration intent for the active single-HLS audio track.
fn hls_audio_intent(
    controller: Option<&HlsController>,
    fallback: Option<&HlsStatus>,
) -> (bool, Option<String>, Option<String>) {
    if let Some(controller) = controller {
        return match controller.config().audio {
            Some(audio) => {
                let encoder = audio.encoder().to_owned();
                (true, Some(audio.source), Some(encoder))
            }
            None => (false, None, None),
        };
    }
    match fallback {
        Some(status) => (
            status.audio_configured(),
            status.audio_source(),
            status.audio_encoder(),
        ),
        None => (false, None, None),
    }
}

async fn connection(State(state): State<ConnectionState>) -> Json<ConnectionResponse> {
    let info = &state.info;
    let power = match &state.power {
        None => serde_json::json!("not-configured"),
        Some(power) => {
            let mut backend = power.lock().await;
            match backend.query_status().await {
                Ok(status) => {
                    serde_json::json!({"status": status.status, "available": true})
                }
                Err(error) => {
                    warn!(error = %error, "Power state query failed for /api/v1/connection");
                    let status = if error.is_unavailable() {
                        "unavailable"
                    } else {
                        "unknown"
                    };
                    serde_json::json!({
                        "status": status,
                        "available": false,
                        "code": error.kind(),
                        "error": error.to_string(),
                    })
                }
            }
        }
    };
    let (
        hls,
        hls_error,
        multi_console_hls,
        hls_audio_configured,
        hls_audio_source,
        hls_audio_encoder,
    ) = match &state.multi_console_hls {
        Some(manager) if manager.supports_independent_consoles() => {
            let status = manager.active_status().await;
            let multi = manager.status().await;
            let active_audio = multi
                .consoles
                .iter()
                .find(|console| console.active)
                .map(|console| {
                    (
                        console.audio_configured,
                        console.audio_source.clone(),
                        console.audio_encoder.clone(),
                    )
                })
                .unwrap_or((false, None, None));
            (
                status.status_name(),
                status.error(),
                Some(multi),
                active_audio.0,
                active_audio.1,
                active_audio.2,
            )
        }
        Some(manager) => {
            // Single-head provider: the legacy active HLS pipeline (if any)
            // remains the stream source; the manager only reports why
            // per-console HLS is unsupported.
            let audio = hls_audio_intent(state.hls_control.as_ref(), state.hls.as_ref());
            let (hls, error) = hls_status_name(state.hls_control.as_ref(), state.hls.as_ref());
            (
                hls,
                error,
                Some(manager.status().await),
                audio.0,
                audio.1,
                audio.2,
            )
        }
        None => {
            let audio = hls_audio_intent(state.hls_control.as_ref(), state.hls.as_ref());
            let (hls, error) = hls_status_name(state.hls_control.as_ref(), state.hls.as_ref());
            (hls, error, None, audio.0, audio.1, audio.2)
        }
    };
    let multi_console_webrtc = match state.multi_console_webrtc.as_ref() {
        Some(manager) => Some(manager.status().await),
        None => None,
    };
    // A manager that owns independent D-Bus consoles also owns the active
    // console's helper, so `webrtc` must describe that helper. A single-head
    // manager leaves the historical `webrtc` path untouched and only reports
    // its typed `unsupported` limitation in `multi_console_webrtc`.
    let webrtc_manager_owns = state
        .multi_console_webrtc
        .as_ref()
        .is_some_and(MultiConsoleWebrtc::supports_independent_consoles);
    let (webrtc, webrtc_error, webrtc_geometry) = if webrtc_manager_owns {
        let status = state
            .multi_console_webrtc
            .as_ref()
            .expect("checked above")
            .active_status()
            .await;
        (status.status_name(), status.error(), status.geometry())
    } else {
        match &state.webrtc {
            None => ("not-configured", None, None),
            Some(control) => (control.status_name(), control.error(), control.geometry()),
        }
    };
    let (webrtc_audio_configured, webrtc_audio_source) = if webrtc_manager_owns {
        let manager = state.multi_console_webrtc.as_ref().expect("checked above");
        (manager.audio_configured(), manager.audio_source())
    } else {
        match &state.webrtc {
            None => (false, None),
            Some(control) => (control.audio_configured(), control.audio_source()),
        }
    };
    let webrtc_backend = if webrtc == "not-configured" {
        "not-configured"
    } else if webrtc_manager_owns {
        state
            .multi_console_webrtc
            .as_ref()
            .expect("checked above")
            .active_status()
            .await
            .backend()
            .as_str()
    } else {
        state
            .webrtc
            .as_ref()
            .map(|control| control.backend().as_str())
            .unwrap_or("not-configured")
    };
    let input = InputStatus {
        sink_kind: info.sink_kind.clone(),
        available: state.input_sink.is_some(),
        injecting: state.input_sink.is_some() && !state.input_is_recording_only(),
    };
    let (audio, audio_error) = match &state.audio {
        None => ("not-configured", None),
        Some(control) => (control.status_name(), control.error().map(str::to_owned)),
    };
    let usb = state
        .usb
        .as_ref()
        .map(UsbControl::connection_status)
        .unwrap_or_default();
    let codec = match state.codec_control.as_ref() {
        Some(control) => control.status_async().await,
        None => state.codecs.clone(),
    };
    let display = state.display_status();
    let video = VideoStatus::new(
        state.snapshot.is_some(),
        state.video_encoding.is_some(),
        state
            .video_encoding
            .as_ref()
            .is_some_and(VideoEncoding::av1_available),
        &state.video_streams,
    );
    let mut controller_surfaces: Vec<ControllerSurface> = RPA_IMPLEMENTED_SURFACES
        .iter()
        .chain(CONTROLLER_ONLY_SURFACES)
        .copied()
        .collect();
    if state.bluetooth.is_some() {
        controller_surfaces.extend_from_slice(BLUETOOTH_IMPLEMENTED_SURFACES);
    } else {
        controller_surfaces.push(BLUETOOTH_UNCONFIGURED_SURFACE);
    }
    if state.serial.is_some() {
        controller_surfaces.extend_from_slice(SERIAL_IMPLEMENTED_SURFACES);
    }
    Json(ConnectionResponse {
        ok: true,
        node_id: info.node_id.clone(),
        profile: info.profile.clone(),
        api_port: info.api_port,
        quic_port: info.quic_port,
        hid_udp_port: info.hid_udp_port,
        sink_kind: info.sink_kind.clone(),
        hid: state.stats.snapshot(),
        power,
        input,
        codec,
        hls,
        hls_error,
        hls_audio_configured,
        hls_audio_source,
        hls_audio_encoder,
        webrtc,
        webrtc_error,
        webrtc_backend,
        webrtc_geometry,
        audio,
        audio_error,
        usb,
        webrtc_audio_configured,
        webrtc_audio_source,
        display_outputs: display.display_outputs,
        display_capture: display.display_capture,
        registration: state.registration_snapshot(),
        multi_console_hls,
        multi_console_webrtc,
        capture: state.capture.clone(),
        video,
        controller_surfaces,
        host_surfaces: state.host_surfaces.advertisement(),
        desktop_controls: state
            .desktop_controls
            .as_ref()
            .map(|controls| controls.status()),
        bluetooth: state
            .bluetooth
            .as_ref()
            .map(|bluetooth| bluetooth.advertisement()),
    })
}

#[derive(Debug, Default, serde::Deserialize)]
struct DisplayConsoleQuery {
    #[serde(default)]
    console: Option<String>,
}

/// Capture one frame through the active snapshot control or a console broker.
///
/// Shared by `/display/snapshot`, `/api/v1/rpa/screenshot`, and
/// `/api/v1/rpa/ocr` so all three return the same real frame (and the same
/// typed errors) for a given `?console=` selection. No frame is synthesized.
async fn capture_display_frame(
    state: &ConnectionState,
    requested: Option<&str>,
) -> Result<SnapshotFrame, SnapshotError> {
    let Some(requested) = requested else {
        return state
            .snapshot
            .as_ref()
            .ok_or(SnapshotError::NotConfigured)?
            .capture()
            .await;
    };

    let control = state
        .display_control
        .as_ref()
        .ok_or(SnapshotError::ConsoleUnsupported)?;
    let broker = state
        .console_broker
        .as_ref()
        .ok_or(SnapshotError::ConsoleUnsupported)?;
    let selection = control.selection();
    let console = broker.resolve_console(Some(requested), &selection)?;

    if console == selection.active_console {
        // Backward-compatible active path: the shared registry snapshot control
        // follows exactly the same console as HLS/WebRTC.
        return state
            .snapshot
            .as_ref()
            .ok_or(SnapshotError::NotConfigured)?
            .capture()
            .await;
    }

    broker.snapshot(console, &selection).await
}

async fn display_snapshot(
    State(state): State<ConnectionState>,
    Query(query): Query<DisplayConsoleQuery>,
) -> Response {
    match capture_display_frame(&state, query.console.as_deref()).await {
        Ok(snapshot) => jpeg_response(snapshot),
        Err(error) => snapshot_error_response(error),
    }
}

fn jpeg_response(snapshot: SnapshotFrame) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        snapshot.jpeg,
    )
        .into_response()
}

fn snapshot_error_response(error: SnapshotError) -> Response {
    let status = error.status();
    let api = error.api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "code": api.code,
        "error": api.message,
        "retryable": api.retryable,
    });
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    if let Some(remediation) = api.remediation {
        body["remediation"] = serde_json::Value::String(remediation);
    }
    (status, Json(body)).into_response()
}

async fn display_info(
    State(state): State<ConnectionState>,
    Query(query): Query<DisplayConsoleQuery>,
) -> Response {
    let Some(requested) = query.console.as_deref() else {
        let mut info = match state.snapshot.as_ref() {
            Some(control) => control.info().await,
            None => unavailable_display_info(Some(
                "display snapshot support is not configured".to_owned(),
            )),
        };
        let mut hls = None;
        if let Some(control) = state.display_control.as_ref() {
            let selection = control.selection();
            info.console = Some(selection.active_console);
            info.active = true;
            if let Some(manager) = state.multi_console_hls.as_ref() {
                hls = manager.status_for(selection.active_console).await;
            }
        } else if let Some(manager) = state.multi_console_hls.as_ref() {
            let active = manager.active_console().await;
            info.console = Some(active);
            info.active = true;
            hls = manager.status_for(active).await;
        }
        return display_info_response(info, hls);
    };

    let Some(control) = state.display_control.as_ref() else {
        return snapshot_error_response(SnapshotError::ConsoleUnsupported);
    };
    let Some(broker) = state.console_broker.as_ref() else {
        return snapshot_error_response(SnapshotError::ConsoleUnsupported);
    };
    let selection = control.selection();
    let console = match broker.resolve_console(Some(requested), &selection) {
        Ok(console) => console,
        Err(error) => return snapshot_error_response(error),
    };

    let mut info = if console == selection.active_console {
        match state.snapshot.as_ref() {
            Some(snapshot) => snapshot.info().await,
            None => unavailable_display_info(Some(
                "display snapshot support is not configured".to_owned(),
            )),
        }
    } else {
        match broker.info(console, &selection).await {
            Ok(info) => info,
            Err(error) => return snapshot_error_response(error),
        }
    };
    info.console = Some(console);
    info.active = console == selection.active_console;
    let hls = match state.multi_console_hls.as_ref() {
        Some(manager) => manager.status_for(console).await,
        None => None,
    };
    display_info_response(info, hls)
}

/// Serialize `/display/info`, adding per-console HLS state when enabled.
///
/// The core [`DisplayInfo`] shape stays unchanged for nodes without
/// `--multi-console-hls`; the additive `multi_console_hls` key names this
/// console's independent pipeline status/error and playlist.
fn display_info_response(
    info: DisplayInfo,
    multi_console_hls: Option<ConsoleHlsStreamStatus>,
) -> Response {
    let Ok(mut value) = serde_json::to_value(info) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "serializing /display/info failed",
        )
            .into_response();
    };
    if let Some(hls) = multi_console_hls {
        if let Ok(hls) = serde_json::to_value(hls) {
            value["multi_console_hls"] = hls;
        }
    }
    Json(value).into_response()
}

fn unavailable_display_info(last_error: Option<String>) -> DisplayInfo {
    DisplayInfo {
        ok: true,
        source_id: None,
        source_name: None,
        width: 0,
        height: 0,
        pixel_format: None,
        capability_max_width: None,
        capability_max_height: None,
        snapshot_available: false,
        snapshot_encoding: "jpeg",
        dimensions_source: "none",
        last_error,
        console: None,
        active: false,
    }
}

/// `GET /video` and `GET /display/mjpeg` — bounded MJPEG or encoded stream.
///
/// `?codec=mjpeg` (the default) keeps the historical multipart JPEG response.
/// `?codec=h264|hevc|av1` feeds the same selected capture source to ffmpeg and
/// returns MPEG-TS (H.264/HEVC) or fragmented MP4 (AV1). In every case a real
/// frame/keyframe is produced before the response is committed, and
/// disconnecting releases the source and slot.
async fn video_stream(
    State(state): State<ConnectionState>,
    Query(params): Query<VideoParams>,
) -> Response {
    let options =
        match params.options() {
            Ok(options) => options,
            Err(error) => return compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                "video.invalid_query",
                error.message,
                Some(
                    "Use ?codec=mjpeg|h264|hevc|av1&fps=1..30&quality=1..100&bitrate=100..50000\
                     &console=<N|label|Console_N|source-id>; numeric values outside those bounds \
                     are clamped.",
                ),
            ),
        };

    if options.codec.is_encoded() {
        return encoded_video_stream(&state, &params, options).await;
    }

    let control = match video_control(&state, params.console.as_deref(), options.quality).await {
        Ok(control) => control,
        Err(error) => return snapshot_error_response(error),
    };

    let Some(guard) = state.video_streams.try_start() else {
        return video_stream_limit_response(&state);
    };

    let first = match control.capture().await {
        Ok(frame) => frame,
        Err(error) => {
            control.shutdown().await;
            return snapshot_error_response(error);
        }
    };

    let (receiver, _task) = crate::video::spawn_stream(control, first, options, guard);
    let body = Body::from_stream(stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    }));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(crate::video::CONTENT_TYPE),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-codec"),
        HeaderValue::from_static(VideoCodec::Mjpeg.as_str()),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-fps"),
        HeaderValue::from_str(&options.fps.to_string()).expect("fps is numeric"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-quality"),
        HeaderValue::from_str(&options.quality.to_string()).expect("quality is numeric"),
    );
    response
}

/// Encoded H.264/HEVC/AV1 branch of `/video`.
async fn encoded_video_stream(
    state: &ConnectionState,
    params: &VideoParams,
    options: crate::video::VideoOptions,
) -> Response {
    let Some(encoding) = state.video_encoding.as_ref() else {
        return video_encoder_unavailable_response(
            "This node has no encoded video configuration; only ?codec=mjpeg is available.",
        );
    };

    let control = match video_control(state, params.console.as_deref(), options.quality).await {
        Ok(control) => control,
        Err(error) => return snapshot_error_response(error),
    };

    let Some(guard) = state.video_streams.try_start_encoded() else {
        return video_stream_limit_response(state);
    };

    let source = match control.open_stream_source().await {
        Ok((_capability, source)) => source,
        Err(error) => {
            drop(guard);
            return snapshot_error_response(error);
        }
    };

    let started = match encoding
        .start_encoded(options.codec, &options, source)
        .await
    {
        Ok(started) => started,
        Err(error) => {
            drop(guard);
            return video_encode_error_response(error);
        }
    };

    let crate::video::StartedVideoStream {
        stream: encoded,
        encoder,
        stream_copy,
        container,
    } = started;
    let first = Some(encoded.first);
    let receiver = encoded.receiver;
    // The task is detached deliberately: it owns ffmpeg/source and exits when
    // the receiver below is dropped. `guard` travels with the body state so a
    // disconnect returns the slot only after the body really ends.
    drop(encoded.task);
    let body = Body::from_stream(stream::unfold(
        (first, receiver, guard),
        |(first, mut receiver, guard)| async move {
            if let Some(first) = first {
                return Some((Ok::<Bytes, Infallible>(first), (None, receiver, guard)));
            }
            receiver
                .recv()
                .await
                .map(|chunk| (Ok(chunk), (None, receiver, guard)))
        },
    ));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(container.content_type()),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-codec"),
        HeaderValue::from_static(options.codec.as_str()),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-container"),
        HeaderValue::from_static(container.as_str()),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-encoder"),
        HeaderValue::from_str(if stream_copy {
            "copy"
        } else {
            encoder.ffmpeg_name()
        })
        .expect("encoder names are valid header values"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-stream-copy"),
        HeaderValue::from_static(if stream_copy { "true" } else { "false" }),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-fps"),
        HeaderValue::from_str(&options.fps.to_string()).expect("fps is numeric"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-ozma-bitrate"),
        HeaderValue::from_str(&options.bitrate_kbps.to_string()).expect("bitrate is numeric"),
    );
    response
}

/// Typed 503 when an encoded stream was requested without an encoder.
fn video_encoder_unavailable_response(message: impl Into<String>) -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "video.encoder_unavailable",
        message,
        Some(
            "Enable the video capability and run the node with an ffmpeg that advertises \
             libx264/libx265/libsvtav1/libaom-av1/librav1e (or the selected VAAPI/NVENC/QSV/\
             V4L2M2M encoder), or request ?codec=mjpeg for the MJPEG stream.",
        ),
    )
}

/// Map an early MPEG-TS failure to the documented typed response.
fn video_encode_error_response(error: HlsError) -> Response {
    use crate::hls::HlsError;
    let (status, code, remediation) = match &error {
        HlsError::KeyframeTimeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            "video.keyframe_timeout",
            "The capture source produced no decodable keyframe before the stream setup bound. \
             Check the source/encoder and retry; no replacement frame is fabricated.",
        ),
        HlsError::EncoderUnavailable { .. }
        | HlsError::VaapiDevice { .. }
        | HlsError::UnsupportedEncoder { .. }
        | HlsError::UnsupportedPixelFormat { .. }
        | HlsError::UnsupportedStreamCodec { .. }
        | HlsError::Spawn { .. }
        | HlsError::StdinUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "video.encoder_unavailable",
            "Install/enable the selected ffmpeg encoder or request another codec; see \
             GET /api/v1/connection.video.codecs for what this node advertises.",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "video.stream_failed",
            "The capture source or ffmpeg encoder failed before the first keyframe. No MJPEG or \
             synthetic frame is substituted; retry with a working source.",
        ),
    };
    compat_error_with_remediation(status, code, error.to_string(), Some(remediation))
}

/// Select the snapshot control for a `/video` request.
///
/// No `?console=` uses the active one-shot control (which follows the runtime
/// registry slot). A named active console does the same; a non-active console
/// is resolved and opened through the bounded [`ConsoleBroker`], exactly like
/// `/display/snapshot?console=`.
async fn video_control(
    state: &ConnectionState,
    console: Option<&str>,
    quality: u8,
) -> Result<SnapshotControl, SnapshotError> {
    let Some(requested) = console else {
        return state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.streaming(Some(quality)))
            .ok_or(SnapshotError::NotConfigured);
    };

    let Some(display) = state.display_control.as_ref() else {
        return Err(SnapshotError::ConsoleUnsupported);
    };
    let Some(broker) = state.console_broker.as_ref() else {
        return Err(SnapshotError::ConsoleUnsupported);
    };
    let selection = display.selection();
    let index = broker.resolve_console(Some(requested), &selection)?;
    if index == selection.active_console {
        return state
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.streaming(Some(quality)))
            .ok_or(SnapshotError::NotConfigured);
    }
    broker
        .streaming_control(index, &selection, Some(quality))
        .await
}

/// Typed 503 when every allowed `/video` slot is already in use.
fn video_stream_limit_response(state: &ConnectionState) -> Response {
    let max = state.video_streams.limit();
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "video.stream_limit",
        format!("This node is already serving its limit of {max} /video stream(s)."),
        Some(
            "Close an existing /video or /display/mjpeg stream, or use GET /display/snapshot \
             for a single JPEG frame. No frame is fabricated for a rejected stream.",
        ),
    )
}

/// Runtime console selection.
///
/// Body: `{"index": <Console_N>}` or `{"id": "<label|Console_N|source-id>"}`.
/// Only D-Bus exposes selectable console indexes today. QMP, VNC, and Looking
/// Glass answer a typed 409 for a different console instead of pretending to
/// switch; no display control at all answers 501. A pipeline failure rolls back
/// where possible and returns 502, never a success-shaped body.
async fn display_select(
    State(state): State<ConnectionState>,
    payload: Result<Json<SelectConsoleRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.display_control.as_ref() else {
        return compat_error_with_remediation(
            StatusCode::NOT_IMPLEMENTED,
            "display.console_unsupported",
            "This node has no runtime console selector.",
            Some(
                "Configure --profile vm with a display capture provider; only D-Bus supports \
                 switching between advertised Console_N heads at runtime.",
            ),
        );
    };
    match control.select(&request).await {
        Ok(response) => {
            if let Some(broker) = state.console_broker.as_ref() {
                let selection = control.selection();
                let available: Vec<u32> = selection
                    .outputs
                    .iter()
                    .map(|output| output.index)
                    .collect();
                broker.on_select(response.active_console, &available).await;
            }
            Json(response).into_response()
        }
        Err(error) => api_error_response(error.status, error.error),
    }
}

async fn node_status(State(state): State<ConnectionState>) -> Json<ConnectionResponse> {
    connection(State(state)).await
}

/// `GET /agent/metrics` — flat host metrics JSON for the controller's poller.
///
/// `controller/device_metrics.py` decodes JSON and only records top-level
/// numeric values, so this returns the flat [`crate::agent_metrics`] document
/// rather than Prometheus text. Values that cannot be measured are omitted and
/// named in `unavailable`; the route is public (merged after the bearer guard)
/// because the controller's poller sends no node token.
async fn agent_metrics(State(state): State<ConnectionState>) -> Response {
    let mut document = state.metrics.collect(&state.info.node_id);

    // AIDA64's documented IDs are also a convenient stable vocabulary for
    // widget panels. Merge their numeric values into the flat host document so
    // the existing controller metric bus can resolve `@active.TCPU` etc.
    if let Some(hardware) = &state.hardware {
        if let Ok(snapshot) = hardware.snapshot(false).await {
            let aida = crate::aida64::build_snapshot(&snapshot);
            let metrics = aida.numeric_metrics();
            if let Some(object) = document.as_object_mut() {
                for (key, value) in &metrics {
                    object.insert(key.clone(), serde_json::json!(value));
                }
                object.insert("aida64".to_string(), serde_json::json!(true));
                if let Some(unavailable) = object
                    .get_mut("unavailable")
                    .and_then(|value| value.as_array_mut())
                {
                    unavailable.retain(|value| {
                        value
                            .as_str()
                            .map(|key| !metrics.contains_key(key))
                            .unwrap_or(true)
                    });
                }
            }
        }
    }

    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(document),
    )
        .into_response()
}

#[derive(Debug, Default, serde::Deserialize)]
struct UiHintsQuery {
    #[serde(default)]
    level: Option<u8>,
}

async fn ui_hints(
    State(_state): State<ConnectionState>,
    axum::extract::Query(query): axum::extract::Query<UiHintsQuery>,
) -> Json<serde_json::Value> {
    // The controller reads these four keys and tolerates an empty result. This
    // node has no in-guest accessibility agent, so the honest answer is an
    // empty hint set rather than invented windows or controls.
    Json(serde_json::json!({
        "ok": true,
        "available": false,
        "level": query.level.unwrap_or(2),
        "windows": [],
        "focused_window": serde_json::Value::Null,
        "controls": [],
        "focused_control": serde_json::Value::Null,
        "hint": "No in-guest UI hint agent is connected to this node.",
    }))
}

/// Stable typed 501 body for a controller-only surface.
///
/// The controller can distinguish an honest absence from a transient failure:
/// `supported:false`, `surface`, `capability`, `reason`, and
/// `python_reference` are always present, and no route ever fabricates a
/// success, empty buffer, snapshot, or applied effect.
fn unsupported_surface_response(surface: &'static ControllerSurface) -> Response {
    let body = serde_json::json!({
        "ok": false,
        "code": surface.code,
        "surface": surface.surface,
        "capability": surface.capability,
        "supported": surface.supported,
        "reason": surface.reason,
        "error": surface.reason,
        "python_reference": surface.python_reference,
        "remediation": surface.remediation,
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response()
}

async fn legacy_stub(State(_state): State<ConnectionState>, uri: Uri) -> Response {
    if let Some(surface) = controller_surface_for_path(uri.path()) {
        return unsupported_surface_response(surface);
    }
    if uri.path() == "/session/init" {
        return compat_error_with_remediation(
            StatusCode::NOT_IMPLEMENTED,
            "session.init_unsupported",
            "The controller's encrypted /session/init handshake is not implemented on this node.",
            Some(
                "Use the node's native QUIC/WebTransport session API; do not treat this 501 as \
                 an established encrypted session.",
            ),
        );
    }
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "legacy.unsupported",
        "This legacy controller route is not implemented on this node.",
        Some("Use the native /api/v1 surface or a provider that implements this feature."),
    )
}

/// `GET /ui/windows` — the Python agent's window-only hint surface.
///
/// This node has no in-guest accessibility provider, so it returns the same
/// typed 501 advertisement as the other controller-only surfaces instead of an
/// empty 200 that would look like a real window list.
async fn ui_windows() -> Response {
    let surface = CONTROLLER_ONLY_SURFACES
        .iter()
        .find(|surface| surface.capability == "ui_windows")
        .expect("ui_windows surface is advertised");
    unsupported_surface_response(surface)
}

/// `GET /usb` — read-only gadget status/capabilities.
async fn usb_status(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let capabilities = control.capabilities().clone();
    if let Err(response) = control.controller() {
        return response;
    }
    match control.snapshot() {
        Ok(gadget) => {
            let media = gadget.media();
            let mut body = serde_json::json!({
                "ok": true,
                "available": gadget.bound,
                "status": if gadget.bound { "running" } else { "unbound" },
                "gadget": gadget,
                "capabilities": capabilities,
                "media": media,
                // Host-port USB discovery is not implemented; this is an
                // honest empty list, never a fabricated device tree.
                "attached": [],
                "attached_scan": false,
            });
            body["media_export"] = control.media_status_json();
            body["usbip"] = match lock(&control.usbip).as_ref() {
                Some(runtime) => runtime.status_json(),
                None => serde_json::json!({
                    "ok": true,
                    "state": "not-configured",
                    "adapter": "kernel-required",
                    "kernel_required": true,
                    "data_sessions": 0,
                }),
            };
            if let Some(error) = control.uac2_bridge_error() {
                body["uac2_bridge_error"] = serde_json::json!({
                    "kind": error.kind,
                    "message": error.message,
                });
            }
            Json(body).into_response()
        }
        Err(error) => usb_status_unavailable(&error),
    }
}

#[derive(Debug, Deserialize)]
struct UsbMediaStartRequest {
    /// Existing absolute backing image.
    path: String,
    /// Mass-storage instance (`usb0` or `mass_storage.usb0`); the first
    /// mass-storage function is used when omitted.
    #[serde(default)]
    function: Option<String>,
    /// `raw` (default), `fat32`, or `iso9660`/`iso`.
    #[serde(default)]
    format: Option<String>,
    /// Serve raw media read-only. Defaults to `true`; FAT32/ISO are always
    /// read-only and reject `false`.
    #[serde(default)]
    read_only: Option<bool>,
    /// Optional absolute overlay persistence file for writable raw media.
    #[serde(default)]
    persist_path: Option<String>,
    /// Commit the overlay on SCSI `SYNCHRONIZE CACHE(10)` (defaults to true).
    #[serde(default)]
    persist_on_flush: Option<bool>,
    /// Commit the overlay on stop (defaults to true).
    #[serde(default)]
    persist_on_stop: Option<bool>,
    /// Load an existing persistence file before serving.
    #[serde(default)]
    reload: bool,
    /// Optional smaller data-phase bound (sector aligned, ≤ 1 MiB).
    #[serde(default)]
    max_transfer_bytes: Option<usize>,
    /// Optional per-transfer endpoint deadline.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct UsbHotplugQuery {
    /// Return events with a sequence strictly greater than this value.
    #[serde(default)]
    since: Option<u64>,
    /// Maximum retained events to return.
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /usb/media` — userspace SCSI export state, capacity, and counters.
async fn usb_media_status(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    if let Err(response) = control.controller() {
        return response;
    }
    Json(control.media_status_json()).into_response()
}

/// `POST /usb/media/start` — configure a userspace SCSI export.
///
/// The export is built from an existing regular file. Writable raw media uses
/// the bounded copy-on-write overlay (the base is never opened for writing);
/// FAT32/ISO stay read-only. The route never claims `running`: that state is
/// only reachable after a real command cycle over an attached bulk transport.
async fn usb_media_start(
    State(state): State<ConnectionState>,
    payload: Result<Json<UsbMediaStartRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    if let Err(response) = control.controller() {
        return response;
    }
    if lock(&control.media_daemon).is_some()
        || lock(&control.media)
            .as_ref()
            .is_some_and(|runtime| runtime.state != MassStorageMediaState::Stopped)
    {
        return media_export_conflict();
    }

    let spec = match MediaExportSpec::from_request(&request, MediaRestartPolicy::default()) {
        Ok(spec) => spec,
        Err(error) => return error.response(),
    };
    let function = match control.media_export_function(spec.function.as_deref()) {
        Ok(function) => function,
        Err(response) => return response,
    };
    let runtime = match control.build_media_runtime(&spec, &function, spec.reload, None) {
        Ok(runtime) => runtime,
        Err(error) => return usb_error_response(error),
    };
    match control.install_media_runtime(runtime, MediaStartOrigin::Route) {
        Ok(body) => Json(body).into_response(),
        Err(response) => response,
    }
}

/// `POST /usb/media/stop` — stop the export and apply its persistence policy.
async fn usb_media_stop(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let daemon_runtime = lock(&control.media_daemon).take();
    if let Some(daemon) = daemon_runtime.as_ref() {
        let function = Some(daemon.function.clone());
        let mut handoff = lock(&control.functionfs).take();
        if let Some(handoff) = handoff.as_mut() {
            if handoff.phase() == FunctionFsHandoffPhase::Bound {
                let _ = handoff.unbind_once();
            }
        }
        daemon.handle.shutdown();
        lock(&control.media_auto).stopped = true;
        if let Some(handoff) = handoff.as_mut() {
            let _ = handoff.cleanup();
        }
        let body = daemon.status_json();
        control.record_media_hotplug(
            UsbMediaHotplugKind::ExportStop,
            function,
            Some("stopped".to_owned()),
        );
        return Json(body).into_response();
    }
    let mut slot = lock(&control.media);
    let Some(runtime) = slot.as_mut() else {
        return media_export_not_running();
    };
    if runtime.state == MassStorageMediaState::Stopped {
        return media_export_not_running();
    }
    match runtime.runner.stop() {
        Ok(()) => {
            runtime.state = MassStorageMediaState::Stopped;
            runtime.last_error = None;
            let body = runtime.status_json();
            let function = Some(runtime.function.clone());
            drop(slot);
            control.record_media_hotplug(
                UsbMediaHotplugKind::ExportStop,
                function,
                Some("stopped".to_owned()),
            );
            Json(body).into_response()
        }
        Err(error) => {
            runtime.state = MassStorageMediaState::Failed;
            runtime.last_error = Some(MassStorageMediaError::from_usb(&error));
            drop(slot);
            usb_error_response(error)
        }
    }
}

/// `GET /usb/hotplug` — bounded poll view of export/attach/detach/unbind.
///
/// The log contains only transitions this node actually caused or observed in
/// the configured ConfigFS gadget. It never fabricates kernel events.
async fn usb_hotplug(
    State(state): State<ConnectionState>,
    Query(query): Query<UsbHotplugQuery>,
) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    if let Err(response) = control.controller() {
        return response;
    }
    // Observe a real bound→unbound transition; failures leave the log alone.
    let _ = control.snapshot();
    let (events, capacity, next_sequence) =
        control.media_hotplug_events(query.since.unwrap_or(0), query.limit.unwrap_or(16));
    Json(serde_json::json!({
        "ok": true,
        "events": events,
        "capacity": capacity,
        "next_sequence": next_sequence,
    }))
    .into_response()
}

fn media_export_conflict() -> Response {
    compat_error_with_remediation(
        StatusCode::CONFLICT,
        "usb-media-already-running",
        "a userspace SCSI media export is already configured on this node",
        Some("POST /usb/media/stop before starting a different export."),
    )
}

fn media_export_not_running() -> Response {
    compat_error_with_remediation(
        StatusCode::CONFLICT,
        "usb-media-not-running",
        "no userspace SCSI media export is running on this node",
        Some("POST /usb/media/start first."),
    )
}

#[derive(Debug, Deserialize)]
struct UsbMediaAttachRequest {
    /// Backing image path.
    path: String,
    /// Mass-storage instance (`usb0` or `mass_storage.usb0`); the first
    /// mass-storage function is used when omitted.
    #[serde(default)]
    function: Option<String>,
    /// Expose the LUN read-only.
    #[serde(default)]
    read_only: bool,
    /// Present the image as a read-only CD-ROM.
    #[serde(default)]
    cdrom: bool,
}

#[derive(Debug, Deserialize)]
struct UsbMediaDetachRequest {
    #[serde(default)]
    function: Option<String>,
}

/// `POST /usb/media/attach` — attach an existing image to a mass-storage LUN.
///
/// The backing file must already exist and be a regular file. The route only
/// writes ConfigFS LUN attributes (`file`, `ro`, `cdrom`); it never mounts,
/// creates, or truncates the image.
async fn usb_media_attach(
    State(state): State<ConnectionState>,
    payload: Result<Json<UsbMediaAttachRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let controller = match control.controller() {
        Ok(controller) => controller.clone(),
        Err(response) => return response,
    };
    let function = match request
        .function
        .as_deref()
        .map(str::trim)
        .filter(|function| !function.is_empty())
    {
        Some(function) => match normalized_media_function(function) {
            Ok(function) => function,
            Err(response) => return response,
        },
        None => match control.default_media_function() {
            Ok(function) => function,
            Err(response) => return response,
        },
    };

    let path_text = request.path.trim();
    if path_text.is_empty() || path_text.contains('\0') {
        return compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-invalid-media-path",
            "media `path` must be a non-empty path without NUL bytes",
            Some("Pass the absolute path of an existing disk image or ISO."),
        );
    }
    let path = Path::new(path_text);
    if !path.is_absolute() {
        return compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-invalid-media-path",
            "media path must be absolute",
            Some("Use an absolute path; relative paths are resolved differently by the kernel."),
        );
    }

    let mut media = if request.cdrom {
        // CD-ROM presentation is read-only by definition.
        VirtualMedia::cdrom(function.clone(), path)
    } else {
        VirtualMedia::disk(function.clone(), path)
    };
    if request.read_only && !request.cdrom {
        media = match media.with_read_only(true) {
            Ok(media) => media,
            Err(error) => return usb_error_response(error),
        };
    }
    if let Err(error) = media.image.validate() {
        return usb_error_response(error);
    }

    match controller.attach_media(control.gadget(), &media) {
        Ok(media_state) => {
            let media = UsbMediaStatus::from_state(&function, &media_state);
            control.record_media_hotplug(
                UsbMediaHotplugKind::Attach,
                Some(function.clone()),
                Some(media.state.clone()),
            );
            Json(serde_json::json!({
                "ok": true,
                "function": function,
                "media": media,
            }))
            .into_response()
        }
        Err(error) => usb_error_response(error),
    }
}

/// `POST /usb/media/detach` — clear the LUN `file` attribute.
///
/// The function and gadget stay in place; no file is deleted or modified.
async fn usb_media_detach(
    State(state): State<ConnectionState>,
    payload: Result<Json<UsbMediaDetachRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let controller = match control.controller() {
        Ok(controller) => controller.clone(),
        Err(response) => return response,
    };
    let function = match request
        .function
        .as_deref()
        .map(str::trim)
        .filter(|function| !function.is_empty())
    {
        Some(function) => match normalized_media_function(function) {
            Ok(function) => function,
            Err(response) => return response,
        },
        None => match control.default_media_function() {
            Ok(function) => function,
            Err(response) => return response,
        },
    };

    match controller.detach_media(control.gadget(), &function) {
        Ok(media_state) => {
            let media = UsbMediaStatus::from_state(&function, &media_state);
            control.record_media_hotplug(
                UsbMediaHotplugKind::Detach,
                Some(function.clone()),
                Some(media.state.clone()),
            );
            Json(serde_json::json!({
                "ok": true,
                "function": function,
                "media": media,
            }))
            .into_response()
        }
        Err(error) => usb_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
struct UsbNbdStartRequest {
    /// Existing absolute regular file to serve.
    path: String,
    /// Export name; defaults to `media`.
    #[serde(default)]
    export: Option<String>,
    /// `raw` (default), `fat32`, or `iso9660`/`iso`.
    #[serde(default)]
    format: Option<String>,
    /// Serve a raw image read-only. Defaults to `true`; ISO/FAT32 are always
    /// read-only and reject `false`.
    #[serde(default)]
    read_only: Option<bool>,
    /// Optional absolute path for the writable overlay's checksummed
    /// persistence file. Omit for an ephemeral overlay.
    #[serde(default)]
    persist_path: Option<String>,
    /// Commit the overlay on NBD FLUSH (defaults to true).
    #[serde(default)]
    persist_on_flush: Option<bool>,
    /// Commit the overlay when the server stops (defaults to true).
    #[serde(default)]
    persist_on_stop: Option<bool>,
}

/// `GET /usb/nbd` — running export summary and counters.
async fn usb_nbd_status(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let nbd = lock(&control.nbd);
    match nbd.as_ref() {
        None => Json(serde_json::json!({
            "ok": true,
            "running": false,
            "url": null,
            "export": null,
            "stats": NbdServerStats::default(),
        }))
        .into_response(),
        Some(runtime) => Json(serde_json::json!({
            "ok": true,
            "running": true,
            "url": runtime.url,
            "export": runtime.export,
            "size_bytes": runtime.size_bytes,
            "read_only": runtime.read_only,
            "overlay": runtime.source.backend.overlay_status(),
            "stats": runtime.handle.stats(),
        }))
        .into_response(),
    }
}

/// `POST /usb/nbd/start` — serve one existing image over loopback NBD.
///
/// This is an opt-in virtual-media path: it never mounts the image or writes a
/// block device. ISO/FAT32 files are probed read-only and force a read-only
/// export; raw images default to read-only unless `read_only: false`.
async fn usb_nbd_start(
    State(state): State<ConnectionState>,
    payload: Result<Json<UsbNbdStartRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    if lock(&control.nbd).is_some() {
        return compat_error_with_remediation(
            StatusCode::CONFLICT,
            "usb-nbd-already-running",
            "an NBD export is already running on this node",
            Some("POST /usb/nbd/stop before starting a different export."),
        );
    }

    let path_text = request.path.trim();
    if path_text.is_empty() || path_text.contains('\0') {
        return compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-invalid-media-path",
            "media `path` must be a non-empty path without NUL bytes",
            Some("Pass the absolute path of an existing FAT32/ISO/raw image."),
        );
    }
    let path = Path::new(path_text);
    if !path.is_absolute() {
        return compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-invalid-media-path",
            "media path must be absolute",
            Some("Use an absolute path; relative paths are resolved differently by the kernel."),
        );
    }

    let format = match request
        .format
        .as_deref()
        .unwrap_or("raw")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "raw" | "nbd" => UsbMediaFormat::Raw,
        "fat32" | "vfat" => UsbMediaFormat::Fat32,
        "iso" | "iso9660" => UsbMediaFormat::Iso9660,
        other => {
            return compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                "usb-invalid-media-format",
                format!("unsupported NBD media format {other:?}"),
                Some("Use `raw`, `fat32`, or `iso9660`."),
            );
        }
    };
    let requested_read_only = request.read_only;
    if format != UsbMediaFormat::Raw && requested_read_only == Some(false) {
        return compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-read-only-media",
            "ISO 9660 and FAT32 NBD exports are always read-only",
            Some("Omit read_only or set it to true."),
        );
    }
    let mut media = match format {
        UsbMediaFormat::Iso9660 => MassStorageImage::iso(path),
        UsbMediaFormat::Fat32 => MassStorageImage::fat32(path),
        _ => MassStorageImage::disk(path),
    };
    let read_only = requested_read_only.unwrap_or(true) || format.is_read_only();
    if format == UsbMediaFormat::Raw {
        media = match media.with_read_only(read_only) {
            Ok(media) => media,
            Err(error) => return usb_error_response(error),
        };
    }

    let export_name = request
        .export
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("media")
        .to_owned();

    // A writable raw export is copy-on-write: the base file is only opened
    // read-only and writes land in a bounded WriteOverlay. The optional
    // persistence path must be absolute and must not be the base itself.
    let mut overlay_config = OverlayConfig::default();
    if !read_only {
        if let Some(persist_text) = request.persist_path.as_deref() {
            let persist_text = persist_text.trim();
            if persist_text.is_empty() || persist_text.contains('\0') {
                return compat_error_with_remediation(
                    StatusCode::BAD_REQUEST,
                    "usb-invalid-overlay-path",
                    "overlay `persist_path` must be a non-empty path without NUL bytes",
                    Some("Omit persist_path for an ephemeral overlay, or pass an absolute path."),
                );
            }
            let persist = Path::new(persist_text);
            if !persist.is_absolute() {
                return compat_error_with_remediation(
                    StatusCode::BAD_REQUEST,
                    "usb-invalid-overlay-path",
                    "overlay `persist_path` must be absolute",
                    Some("Use an absolute path outside the base image."),
                );
            }
            if persist == path {
                return compat_error_with_remediation(
                    StatusCode::BAD_REQUEST,
                    "usb-invalid-overlay-path",
                    "overlay `persist_path` must not be the base image",
                    Some("Choose a separate file; the base image is never written."),
                );
            }
            overlay_config = overlay_config.with_persistence(persist);
        }
    }
    let overlay_policy = NbdOverlayPolicy {
        persist_on_flush: request.persist_on_flush.unwrap_or(true),
        persist_on_stop: request.persist_on_stop.unwrap_or(true),
    };
    let source = if read_only {
        NbdExportSource::from_media(export_name.clone(), &media)
    } else {
        NbdExportSource::overlay_media(export_name.clone(), &media, overlay_config, overlay_policy)
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => return usb_error_response(error),
    };
    let read_only = source.is_read_only();
    let size_bytes = source.export.size_bytes;
    let overlay_status = source.backend.overlay_status();
    let catalog = match NbdExportCatalog::new(vec![source.clone()]) {
        Ok(catalog) => catalog,
        Err(error) => return usb_error_response(error),
    };
    let config = NbdServerConfig {
        max_request_bytes: 1024 * 1024,
        max_clients: 4,
        handshake_timeout_ms: 10_000,
        idle_timeout_ms: Some(30_000),
        option_max_bytes: 64 * 1024,
    };
    let server = match NbdServer::bind_tcp(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        catalog,
        config,
    ) {
        Ok(server) => server,
        Err(error) => return usb_error_response(error),
    };
    let address = match server.local_addr() {
        Some(address) => address,
        None => {
            return compat_error_with_remediation(
                StatusCode::INTERNAL_SERVER_ERROR,
                "usb-nbd-error",
                "the NBD listener has no bound address",
                Some("No export was started; check the node log."),
            );
        }
    };
    let handle = match server.start() {
        Ok(handle) => handle,
        Err(error) => return usb_error_response(error),
    };
    let url = format!("nbd://{address}/{export_name}");
    let runtime = NbdRuntime {
        handle,
        source,
        url: url.clone(),
        export: export_name.clone(),
        size_bytes,
        read_only,
    };
    {
        let mut slot = lock(&control.nbd);
        if slot.is_some() {
            drop(slot);
            runtime.handle.stop();
            return compat_error_with_remediation(
                StatusCode::CONFLICT,
                "usb-nbd-already-running",
                "an NBD export is already running on this node",
                Some("POST /usb/nbd/stop before starting a different export."),
            );
        }
        *slot = Some(runtime);
    }

    Json(serde_json::json!({
        "ok": true,
        "running": true,
        "url": url,
        "export": export_name,
        "size_bytes": size_bytes,
        "read_only": read_only,
        "overlay": overlay_status,
        "max_request_bytes": 1024 * 1024,
        "stats": NbdServerStats::default(),
    }))
    .into_response()
}

/// `POST /usb/nbd/stop` — stop the opt-in NBD export and return its counters.
async fn usb_nbd_stop(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.usb.as_ref() else {
        return usb_not_configured();
    };
    let runtime = lock(&control.nbd).take();
    let Some(runtime) = runtime else {
        return compat_error_with_remediation(
            StatusCode::CONFLICT,
            "usb-nbd-not-running",
            "no NBD export is running on this node",
            Some("POST /usb/nbd/start first."),
        );
    };
    let stats = runtime.handle.stop();
    Json(serde_json::json!({
        "ok": true,
        "running": false,
        "export": runtime.export,
        "stats": stats,
    }))
    .into_response()
}

/// Accept `usb0` or `mass_storage.usb0`, returning the instance name.
fn normalized_media_function(function: &str) -> Result<String, Response> {
    let instance = function
        .strip_prefix("mass_storage.")
        .unwrap_or(function)
        .trim();
    if is_safe_identifier(instance) {
        Ok(instance.to_owned())
    } else {
        Err(compat_error_with_remediation(
            StatusCode::BAD_REQUEST,
            "usb-invalid-function",
            format!(
                "mass-storage function {function:?} must contain only letters, digits, '-' or '_'"
            ),
            Some("Pass the function instance (`usb0`) or `mass_storage.usb0`."),
        ))
    }
}

fn usb_not_configured() -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "usb.unsupported",
        "USB gadget support is not configured on this node.",
        Some(
            "Start ozma-node with --usb-gadget --usb-udc <udc> (and optionally \
             --usb-media <path>) to create and bind a composite gadget. No USB state is \
             fabricated without it.",
        ),
    )
}

fn usb_unavailable(reason: &str) -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "usb-unavailable",
        format!("USB gadget is configured but unavailable: {reason}"),
        Some(
            "Check the node log and /api/v1/connection.usb.error. The gadget was not created \
             or bound; no USB capability is advertised.",
        ),
    )
}

fn usb_status_unavailable(reason: &str) -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "usb-status-unavailable",
        format!("USB gadget status could not be read: {reason}"),
        Some(
            "The configured ConfigFS gadget is missing or unreadable. Recreate it or restart \
             the node with --usb-gadget; no gadget state is fabricated.",
        ),
    )
}

fn usb_error_response(error: UsbError) -> Response {
    let (status, code, message) = match error.kind() {
        "invalid-config" => (
            StatusCode::BAD_REQUEST,
            "usb-invalid-media",
            "the media image is not usable as a mass-storage backing file",
        ),
        "invalid-media" => (
            StatusCode::BAD_REQUEST,
            "usb-invalid-media",
            "the media image failed its declared-format probe",
        ),
        "unsupported-media-format" => (
            StatusCode::BAD_REQUEST,
            "usb-invalid-media-format",
            "the media format is not usable for this virtual-media operation",
        ),
        "media-not-found" => (
            StatusCode::BAD_REQUEST,
            "usb-media-not-found",
            "the backing image does not exist",
        ),
        "media-not-file" => (
            StatusCode::BAD_REQUEST,
            "usb-media-not-file",
            "the backing image is not a regular file",
        ),
        "read-only-media" => (
            StatusCode::BAD_REQUEST,
            "usb-read-only-media",
            "CD-ROM media cannot be made writable",
        ),
        "missing-function" => (
            StatusCode::CONFLICT,
            "usb-missing-function",
            "the gadget has no mass-storage function with that instance",
        ),
        "missing-gadget" => (
            StatusCode::NOT_FOUND,
            "usb-missing-gadget",
            "the configured USB gadget does not exist",
        ),
        "already-bound" => (
            StatusCode::CONFLICT,
            "usb-already-bound",
            "the USB gadget is already bound to another UDC",
        ),
        "missing-configfs-root" | "configfs-root-not-directory" | "configfs-io" | "io" => (
            StatusCode::SERVICE_UNAVAILABLE,
            "usb-configfs-error",
            "the ConfigFS operation failed; the gadget or LUN state was not changed",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "usb-error",
            "the USB controller rejected the request",
        ),
    };
    // Keep the path-bearing detail in the log, not the HTTP response.
    warn!(kind = error.kind(), error = %error, "USB virtual-media request rejected");
    compat_error_with_remediation(
        status,
        code,
        format!("{}: {message}", error.kind()),
        Some(
            "The virtual-media request was rejected by the USB controller; the backing image \
             was not opened for writing and no LUN state was fabricated.",
        ),
    )
}

async fn power_state(State(state): State<ConnectionState>) -> axum::response::Response {
    use axum::response::IntoResponse;

    let Some(power) = state.power else {
        // Match the Python soft node: state is always readable, with
        // `available: false`, so the controller can render a disabled control.
        return Json(serde_json::json!({
            "status": "not-configured",
            "available": false,
        }))
        .into_response();
    };

    let mut backend = power.lock().await;
    match backend.query_status().await {
        Ok(status) => Json(serde_json::json!({
            "status": status.status,
            "available": true,
        }))
        .into_response(),
        Err(error) => {
            warn!(error = %error, "Power state query failed");
            let status = if error.is_unavailable() {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            } else {
                axum::http::StatusCode::BAD_GATEWAY
            };
            // A backend that failed to connect at startup is reported as an
            // unavailable state with the retained reason, never `not-configured`.
            let state = if error.is_unavailable() {
                "unavailable"
            } else {
                "unknown"
            };
            (
                status,
                Json(serde_json::json!({
                    "status": state,
                    "available": false,
                    "code": error.kind(),
                    "error": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}

async fn power_action(
    State(state): State<ConnectionState>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use ozma_power::PowerAction;

    let action = match action.parse::<PowerAction>() {
        Ok(action) => action,
        Err(error) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
    };

    let Some(power) = state.power else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ok": false,
                "action": action.as_str(),
                "error": "No power backend configured",
            })),
        )
            .into_response();
    };

    let mut backend = power.lock().await;
    let result = match action {
        PowerAction::Start => backend
            .cont()
            .await
            .map(|_| ozma_power::PowerOutcome::accepted()),
        PowerAction::Shutdown => backend.system_powerdown_outcome().await,
        PowerAction::Reset => backend
            .system_reset()
            .await
            .map(|_| ozma_power::PowerOutcome::accepted()),
        PowerAction::Pause => backend
            .pause()
            .await
            .map(|_| ozma_power::PowerOutcome::accepted()),
        PowerAction::Resume => backend
            .resume()
            .await
            .map(|_| ozma_power::PowerOutcome::accepted()),
        PowerAction::Stop => backend
            .stop()
            .await
            .map(|_| ozma_power::PowerOutcome::accepted()),
    };

    match result {
        Ok(outcome) => {
            // An accepted-but-not-yet-applied ACPI request (the guest is still
            // running) is a 202 with `pending: true` and the real observed
            // state, never a fabricated "off".
            let status = if outcome.pending {
                axum::http::StatusCode::ACCEPTED
            } else {
                axum::http::StatusCode::OK
            };
            let mut body = serde_json::json!({
                "ok": true,
                "action": action.as_str(),
                "pending": outcome.pending,
            });
            if let Some(state) = &outcome.state {
                body["status"] = serde_json::Value::String(state.status.clone());
            }
            if let Some(reason) = &outcome.reason {
                body["reason"] = serde_json::Value::String(reason.clone());
            }
            (status, Json(body)).into_response()
        }
        Err(error) => {
            warn!(action = %action, error = %error, "Power action failed");
            let status = if error.is_unavailable() {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            } else {
                axum::http::StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(serde_json::json!({
                    "ok": false,
                    "action": action.as_str(),
                    "code": error.kind(),
                    "error": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Input compatibility routes
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct KeyRequest {
    /// Linux evdev keycode. `Option` lets us return a typed 400 for a missing
    /// field instead of axum's generic extractor rejection.
    #[serde(default)]
    keycode: Option<i64>,
    #[serde(default = "default_down")]
    down: bool,
}

fn default_down() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MouseAction {
    #[default]
    Move,
    Press,
    Release,
    Click,
}

#[derive(Debug, Default, Deserialize)]
struct MouseRequest {
    #[serde(default)]
    x: Option<i64>,
    #[serde(default)]
    y: Option<i64>,
    #[serde(default)]
    action: MouseAction,
    #[serde(default)]
    button: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct TypeRequest {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct RpaTypeRequest {
    #[serde(default)]
    text: String,
    /// Optional per-character delay in seconds, as accepted by the Python node.
    /// Bounded and clamped by the handler.
    #[serde(default)]
    delay: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct RpaKeyRequest {
    /// Named key or one printable character.
    #[serde(default)]
    key: String,
    /// USB HID boot-keyboard modifier bitmask, as used by the Python node.
    #[serde(default)]
    modifier: Option<i64>,
    /// Modifier names (`ctrl`, `shift`, `alt`, `meta`, `altgr`, ...).
    #[serde(default)]
    modifiers: Vec<String>,
    /// Controller hotkey form: modifier names followed by one final key.
    #[serde(default)]
    keys: Vec<String>,
}

/// Largest per-character delay accepted by `POST /api/v1/rpa/type`.
const MAX_RPA_TYPE_DELAY: Duration = Duration::from_millis(100);

/// A text-to-keystroke mapping failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextMappingError {
    /// A character has no direct key or documented sequence on the layout.
    #[error(
        "unsupported character {character:?} at index {index} for keyboard layout `{layout}`; \
         the layout has no direct key or documented dead-key/compose sequence, and IME input is not supported"
    )]
    Unsupported {
        /// The offending character.
        character: char,
        /// Zero-based character (not byte) index.
        index: usize,
        /// The selected layout.
        layout: KeyboardLayout,
    },
    /// A dead key has no following composable character, or cannot be typed.
    #[error(
        "dead key {character:?} at index {index} on keyboard layout `{layout}` has no following \
         composable character; emitting it alone would consume the next key, so nothing was typed"
    )]
    DeadKey {
        /// The offending character.
        character: char,
        /// Zero-based character (not byte) index.
        index: usize,
        /// The selected layout.
        layout: KeyboardLayout,
    },
    /// The request exceeded [`MAX_TYPE_TEXT_CHARS`].
    #[error("text is {actual} characters; the limit is {limit}")]
    TooLong {
        /// Submitted character count.
        actual: usize,
        /// Accepted maximum.
        limit: usize,
    },
}

/// Resolve text into one chord per physical keystroke, validating the whole
/// request.
///
/// Known dead-key and compose pairs become two chords (dead key, then base
/// key); every other character becomes one chord. A prefix is consumed only
/// when the pair is documented, so unmatched prefixes fall through to their
/// normal direct/dead/unsupported mapping. Validation finishes before any
/// event or sink is touched, so an unsupported character or dangling dead key
/// anywhere in the request is rejected atomically.
fn text_to_chords(text: &str, layout: KeyboardLayout) -> Result<Vec<KeyChord>, TextMappingError> {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() > MAX_TYPE_TEXT_CHARS {
        return Err(TextMappingError::TooLong {
            actual: characters.len(),
            limit: MAX_TYPE_TEXT_CHARS,
        });
    }

    let mut chords = Vec::with_capacity(characters.len());
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if let Some(&next) = characters.get(index + 1) {
            if let Some(sequence) = resolve_dead_key_sequence(layout, character, next) {
                chords.extend(sequence.chords);
                index += 2;
                continue;
            }
            if let Some(sequence) = resolve_compose_sequence(layout, character, next) {
                chords.extend(sequence.chords);
                index += 2;
                continue;
            }
        }
        match layout.chord(character) {
            CharOutcome::Direct(chord) => chords.push(chord),
            CharOutcome::DeadKey => {
                return Err(TextMappingError::DeadKey {
                    character,
                    index,
                    layout,
                });
            }
            CharOutcome::Unsupported => {
                return Err(TextMappingError::Unsupported {
                    character,
                    index,
                    layout,
                });
            }
        }
        index += 1;
    }
    Ok(chords)
}

/// Convert text into per-keystroke key event chunks.
///
/// Each chunk contains one chord's key-down/up pair plus any Shift and AltGr
/// down/up events. A dead-key/compose sequence therefore has one chunk for the
/// dead key and one for the base key. The handler sleeps between chunks so the
/// guest sees separate keystrokes; [`text_to_input_events`] flattens the same
/// mapping for callers and tests that only need the event sequence.
fn text_to_key_chunks(
    text: &str,
    layout: KeyboardLayout,
) -> Result<Vec<Vec<InputEvent>>, TextMappingError> {
    Ok(text_to_chords(text, layout)?
        .into_iter()
        .map(KeyChord::events)
        .collect())
}

/// Map text to normalized key events without touching a sink.
///
/// This is the pure, testable core of `POST /input/type`. Unsupported
/// characters are rejected rather than silently dropped or mapped to an
/// invented keycode. `layout` selects the guest's console keymap; the event
/// sequence includes explicit left-Shift and right-Alt (AltGr) press/release
/// pairs where the layout requires them.
pub fn text_to_input_events(
    text: &str,
    layout: KeyboardLayout,
) -> Result<Vec<InputEvent>, TextMappingError> {
    Ok(text_to_key_chunks(text, layout)?
        .into_iter()
        .flatten()
        .collect())
}

/// Map the controller's Linux mouse button code to a pointer button.
///
/// The Python soft node used QEMU's `{0: left, 1: left, 2: right, 4: middle}`;
/// back/forward are the natural extensions for Ozma's normalized buttons.
fn pointer_button(button: i64) -> Option<PointerButton> {
    match button {
        0 | 1 => Some(PointerButton::Left),
        2 => Some(PointerButton::Right),
        4 => Some(PointerButton::Middle),
        8 => Some(PointerButton::Back),
        16 => Some(PointerButton::Forward),
        _ => None,
    }
}

fn compat_error_with_remediation(
    status: StatusCode,
    code: &'static str,
    error: impl Into<String>,
    remediation: Option<&str>,
) -> Response {
    let mut body = serde_json::json!({
        "ok": false,
        "code": code,
        "error": error.into(),
    });
    if let Some(remediation) = remediation {
        body["remediation"] = serde_json::Value::String(remediation.to_owned());
    }
    (status, Json(body)).into_response()
}

fn compat_error(status: StatusCode, code: &'static str, error: impl Into<String>) -> Response {
    compat_error_with_remediation(status, code, error, None)
}

fn json_rejection(error: JsonRejection) -> Response {
    compat_error(StatusCode::BAD_REQUEST, "invalid-json", error.body_text())
}

fn require_input_sink(state: &ConnectionState) -> Result<SharedSink, Response> {
    state.input_sink.clone().ok_or_else(|| {
        compat_error_with_remediation(
            StatusCode::SERVICE_UNAVAILABLE,
            "input-unavailable",
            "input sink is unavailable",
            Some(
                "Configure a uinput, QMP, HID gadget, or recording input provider and retry; \
                 see /api/v1/connection.input.",
            ),
        )
    })
}

fn submit_input(sink: &SharedSink, event: &InputEvent) -> Result<SubmitReport, Response> {
    sink.submit_report(event).map_err(input_error_response)
}

fn input_error_response(error: ApiError) -> Response {
    let mut body = serde_json::json!({
        "ok": false,
        "code": "input-rejected",
        "sink_code": error.code,
        "error": error.message,
    });
    if let Some(remediation) = error.remediation {
        body["remediation"] = serde_json::Value::String(remediation);
    }
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}

/// Best-effort recovery after a failed typing chunk. Nothing is emitted when the
/// sink already considers every key released, so the recording sink does not
/// see a spurious `ReleaseAll`.
fn release_held_input(sink: &SharedSink) {
    if !sink.held().is_empty() {
        let _ = sink.release_all();
    }
}

async fn input_key(
    State(state): State<ConnectionState>,
    payload: Result<Json<KeyRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    let Some(keycode) = request.keycode else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-keycode",
            "missing `keycode` (Linux evdev keycode)",
        );
    };
    if !(0..=i64::from(u16::MAX)).contains(&keycode) {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-keycode",
            format!("keycode {keycode} is outside the Linux evdev u16 range"),
        );
    }
    let keycode = keycode as u16;
    let Some(usage) = linux_keycode_to_hid_usage(keycode) else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "unknown-keycode",
            format!("Linux evdev keycode {keycode} has no USB HID keyboard usage"),
        );
    };

    let sink = match require_input_sink(&state) {
        Ok(sink) => sink,
        Err(response) => return response,
    };
    if let Err(response) = submit_input(&sink, &key_event(u32::from(usage), request.down)) {
        return response;
    }

    let mut body = serde_json::json!({
        "ok": true,
        "sink": state.info.sink_kind,
    });
    if state.input_is_recording_only() {
        body["injected"] = serde_json::Value::Bool(false);
    }
    Json(body).into_response()
}

async fn input_mouse(
    State(state): State<ConnectionState>,
    payload: Result<Json<MouseRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    let x = request.x.unwrap_or(0);
    let y = request.y.unwrap_or(0);
    for (axis, value) in [("x", x), ("y", y)] {
        if !(0..=MAX_POINTER_COORDINATE).contains(&value) {
            return compat_error(
                StatusCode::BAD_REQUEST,
                "invalid-coordinate",
                format!(
                    "{axis}={value} is outside the absolute pointer range 0..={MAX_POINTER_COORDINATE}"
                ),
            );
        }
    }

    let button_code = request.button.unwrap_or(0);
    let Some(button) = pointer_button(button_code) else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "unknown-button",
            format!(
                "mouse button {button_code} is not supported; expected 0/1 (left), 2 (right), \
                 4 (middle), 8 (back), or 16 (forward)"
            ),
        );
    };

    let sink = match require_input_sink(&state) {
        Ok(sink) => sink,
        Err(response) => return response,
    };

    let absolute = InputEvent::PointerAbsolute {
        display_id: None,
        x: x as i32,
        y: y as i32,
    };
    if let Err(response) = submit_input(&sink, &absolute) {
        return response;
    }

    match request.action {
        MouseAction::Move => {}
        MouseAction::Press => {
            if let Err(response) = submit_input(&sink, &InputEvent::Button { button, down: true }) {
                return response;
            }
        }
        MouseAction::Release => {
            if let Err(response) = submit_input(
                &sink,
                &InputEvent::Button {
                    button,
                    down: false,
                },
            ) {
                return response;
            }
        }
        MouseAction::Click => {
            if let Err(response) = submit_input(&sink, &InputEvent::Button { button, down: true }) {
                return response;
            }
            tokio::time::sleep(CLICK_HOLD).await;
            if let Err(response) = submit_input(
                &sink,
                &InputEvent::Button {
                    button,
                    down: false,
                },
            ) {
                return response;
            }
        }
    }

    Json(serde_json::json!({"ok": true})).into_response()
}

/// Submit one character's events, holding the non-modifier key down long
/// enough for the guest to observe it.
///
/// A chunk starts with zero to two modifier-down events, followed by the
/// character key-down/up pair and the matching modifier-up events. Modifier
/// down can share a USB report with the key-down, and modifier up can share one
/// with the key-up, but the key-down/key-up pair must straddle a guest poll. If
/// a submit fails midway, the caller's `release_all` recovers the modifiers and
/// keys already sent.
async fn submit_key_chunk(sink: &SharedSink, chunk: &[InputEvent]) -> Result<usize, Response> {
    let press_index = chunk
        .iter()
        .position(|event| match event {
            InputEvent::Key {
                code, down: true, ..
            } => !(0xE0..=0xE7).contains(code),
            _ => false,
        })
        .unwrap_or(0);
    let Some(press) = chunk.get(press_index) else {
        return Ok(0);
    };

    for event in &chunk[..press_index] {
        submit_input(sink, event)?;
    }
    submit_input(sink, press)?;
    tokio::time::sleep(TYPE_KEY_HOLD).await;
    for event in &chunk[press_index + 1..] {
        submit_input(sink, event)?;
    }

    Ok(chunk.len())
}

/// Validate, map, and type text through the shared sink.
///
/// Used by both `/input/type` and `/api/v1/rpa/type` so the RPA route is
/// layout-aware in exactly the same way. The complete request is validated
/// before the sink is consulted, so a bad character never produces a partial
/// keystroke sequence.
async fn type_text_through_sink(
    state: &ConnectionState,
    text: &str,
    char_delay: Duration,
) -> Result<usize, Response> {
    let chunks = match text_to_key_chunks(text, state.keyboard_layout) {
        Ok(chunks) => chunks,
        Err(error) => {
            let (code, remediation) = match &error {
                TextMappingError::TooLong { .. } => ("text-too-long", None),
                TextMappingError::Unsupported { .. } => (
                    "unsupported-character",
                    Some(
                        "Select the guest's layout with --keyboard-layout (us, uk, de, fr) and \
                         send characters that layout types directly or through a documented \
                         dead-key/compose sequence. IME, CJK, emoji, and combining marks are not \
                         supported; send raw /input/key events for anything else. Nothing was typed.",
                    ),
                ),
                TextMappingError::DeadKey { .. } => (
                    "dead-key-required",
                    Some(
                        "A dead key must be followed by a composable base character (for example \
                         `´e`, `` `e ``, `^e`, `\"u`, `~n`, or `,c`), or use a layout that types \
                         the character directly, or send raw /input/key events. Nothing was typed.",
                    ),
                ),
            };
            return Err(compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                code,
                error.to_string(),
                remediation,
            ));
        }
    };

    let sink = require_input_sink(state)?;

    let mut submitted = 0usize;
    // `submit_report` is synchronous and releases the sink lock before it
    // returns; the only awaits here are the fixed hold/character delays, so the
    // shared QMP client cannot be held across an await by this handler.
    for (index, chunk) in chunks.iter().enumerate() {
        match submit_key_chunk(&sink, chunk).await {
            Ok(count) => submitted += count,
            Err(response) => {
                // A failed chunk may have left a modifier or key logically down
                // in the sink. Release everything before reporting the error.
                release_held_input(&sink);
                return Err(response);
            }
        }
        if index + 1 < chunks.len() && !char_delay.is_zero() {
            tokio::time::sleep(char_delay).await;
        }
    }

    Ok(submitted)
}

fn input_result_body(state: &ConnectionState) -> serde_json::Value {
    let mut body = serde_json::json!({
        "ok": true,
        "sink": state.info.sink_kind,
    });
    if state.input_is_recording_only() {
        body["injected"] = serde_json::Value::Bool(false);
    }
    body
}

async fn input_type(
    State(state): State<ConnectionState>,
    payload: Result<Json<TypeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    match type_text_through_sink(&state, &request.text, TYPE_CHAR_DELAY).await {
        Ok(submitted) => Json(serde_json::json!({"ok": true, "events": submitted})).into_response(),
        Err(response) => response,
    }
}

async fn rpa_type(
    State(state): State<ConnectionState>,
    payload: Result<Json<RpaTypeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    let delay = match request.delay {
        None => TYPE_CHAR_DELAY,
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            Duration::from_secs_f64(seconds.min(MAX_RPA_TYPE_DELAY.as_secs_f64()))
        }
        Some(seconds) => {
            return compat_error(
                StatusCode::BAD_REQUEST,
                "invalid-delay",
                format!("delay {seconds} must be a finite, non-negative number of seconds"),
            );
        }
    };

    match type_text_through_sink(&state, &request.text, delay).await {
        Ok(submitted) => {
            let mut body = input_result_body(&state);
            body["events"] = serde_json::json!(submitted);
            Json(body).into_response()
        }
        Err(response) => response,
    }
}

/// Resolve an RPA key request to one HID usage plus modifier bits.
fn resolve_rpa_key(state: &ConnectionState, request: &RpaKeyRequest) -> Result<(u8, u8), Response> {
    let mut bits = 0u8;
    if let Some(modifier) = request.modifier {
        if !(0..=i64::from(u8::MAX)).contains(&modifier) {
            return Err(compat_error(
                StatusCode::BAD_REQUEST,
                "invalid-modifier",
                format!("modifier {modifier} is outside the USB HID boot-keyboard byte range"),
            ));
        }
        bits |= modifier as u8;
    }
    for name in &request.modifiers {
        let Some(bit) = modifier_bit(name) else {
            return Err(compat_error(
                StatusCode::BAD_REQUEST,
                "unknown-modifier",
                format!("unknown modifier {name:?}; use ctrl, shift, alt, meta/gui, or altgr"),
            ));
        };
        bits |= bit;
    }

    let key_name = if request.keys.is_empty() {
        if request.key.is_empty() {
            return Err(compat_error(
                StatusCode::BAD_REQUEST,
                "invalid-key",
                "missing `key` (name such as enter/f1 or one printable character)",
            ));
        }
        request.key.as_str()
    } else {
        let mut final_key: Option<&str> = None;
        for name in &request.keys {
            if let Some(bit) = modifier_bit(name) {
                bits |= bit;
            } else if final_key.is_some() {
                return Err(compat_error(
                    StatusCode::BAD_REQUEST,
                    "invalid-combo",
                    "a key combo must contain modifiers and exactly one non-modifier key",
                ));
            } else {
                final_key = Some(name.as_str());
            }
        }
        match final_key {
            Some(key) => key,
            None => {
                return Err(compat_error(
                    StatusCode::BAD_REQUEST,
                    "invalid-combo",
                    "a key combo must contain one non-modifier key",
                ));
            }
        }
    };

    if let Some(usage) = named_key_usage(key_name) {
        return Ok((usage, bits));
    }

    let mut characters = key_name.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) => match state.keyboard_layout.chord(character) {
            CharOutcome::Direct(chord) => {
                // A layout shift/AltGr requirement is additive to any requested
                // modifiers, but never duplicates an already-selected side.
                if chord.shift && bits & (0x02 | 0x20) == 0 {
                    bits |= 0x02;
                }
                if chord.altgr && bits & 0x40 == 0 {
                    bits |= 0x40;
                }
                Ok((chord.usage, bits))
            }
            CharOutcome::DeadKey => Err(compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                "dead-key-required",
                format!(
                    "dead key {character:?} cannot be sent alone on keyboard layout `{}`",
                    state.keyboard_layout
                ),
                Some(
                    "Send the dead key followed by a composable base character through \
                     /api/v1/rpa/type, or use /api/v1/rpa/key with a named key.",
                ),
            )),
            CharOutcome::Unsupported => Err(compat_error(
                StatusCode::BAD_REQUEST,
                "unsupported-character",
                format!(
                    "keyboard layout `{}` has no direct key for {character:?}",
                    state.keyboard_layout
                ),
            )),
        },
        _ => Err(compat_error(
            StatusCode::BAD_REQUEST,
            "unknown-key",
            format!(
                "unknown key {key_name:?}; use a named key (enter, esc, f1, up, ...) or one printable character"
            ),
        )),
    }
}

async fn rpa_key(
    State(state): State<ConnectionState>,
    payload: Result<Json<RpaKeyRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    let (usage, modifiers) = match resolve_rpa_key(&state, &request) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let events = key_events(usage, modifiers);
    let sink = match require_input_sink(&state) {
        Ok(sink) => sink,
        Err(response) => return response,
    };

    match submit_key_chunk(&sink, &events).await {
        Ok(submitted) => {
            let mut body = input_result_body(&state);
            body["events"] = serde_json::json!(submitted);
            Json(body).into_response()
        }
        Err(response) => {
            release_held_input(&sink);
            response
        }
    }
}

async fn rpa_screenshot(
    State(state): State<ConnectionState>,
    Query(query): Query<DisplayConsoleQuery>,
) -> Response {
    match capture_display_frame(&state, query.console.as_deref()).await {
        Ok(frame) => jpeg_response(frame),
        Err(error) => snapshot_error_response(error),
    }
}

async fn rpa_ocr(
    State(state): State<ConnectionState>,
    Query(query): Query<DisplayConsoleQuery>,
) -> Response {
    if !state.ocr.is_available() {
        return ocr_error_response(OcrError::Unavailable);
    }

    let frame = match capture_display_frame(&state, query.console.as_deref()).await {
        Ok(frame) => frame,
        Err(error) => return snapshot_error_response(error),
    };
    let text = match state
        .ocr
        .recognise(&frame.jpeg, frame.format.width, frame.format.height)
        .await
    {
        Ok(text) => text,
        Err(error) => return ocr_error_response(error),
    };

    Json(serde_json::json!({
        "ok": true,
        "available": true,
        "engine": "tesseract",
        "text": text.clone(),
        "full_text": text,
        "width": frame.format.width,
        "height": frame.format.height,
        "source_id": frame.source_id,
        "source_name": frame.source_name,
        "text_regions": [],
    }))
    .into_response()
}

fn ocr_error_response(error: OcrError) -> Response {
    match &error {
        OcrError::Unavailable => compat_error_with_remediation(
            StatusCode::SERVICE_UNAVAILABLE,
            "rpa.ocr-unavailable",
            "tesseract is not installed or not on PATH",
            Some(
                "Install the tesseract-ocr package (or expose a tesseract binary on PATH) and \
                 retry. No OCR text is invented when the engine is absent.",
            ),
        ),
        OcrError::FrameTooLarge { .. } | OcrError::FrameTooManyPixels { .. } => compat_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "rpa.ocr-frame-too-large",
            error.to_string(),
        ),
        OcrError::Timeout { .. } => compat_error(
            StatusCode::GATEWAY_TIMEOUT,
            "rpa.ocr-timeout",
            error.to_string(),
        ),
        OcrError::OutputTooLarge { .. } => compat_error(
            StatusCode::BAD_GATEWAY,
            "rpa.ocr-output-too-large",
            error.to_string(),
        ),
        OcrError::Spawn(_) | OcrError::Io(_) | OcrError::Failed { .. } => {
            compat_error(StatusCode::BAD_GATEWAY, "rpa.ocr-failed", error.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Codec and audio compatibility routes
// ---------------------------------------------------------------------------

async fn codec_get(State(state): State<ConnectionState>) -> Json<CodecStatus> {
    let status = match state.codec_control.as_ref() {
        Some(control) => control.status_async().await,
        None => state.codecs.clone(),
    };
    Json(status)
}

async fn codec_set(
    State(state): State<ConnectionState>,
    payload: Result<Json<CodecRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.codec_control.as_ref() else {
        return codec_unavailable();
    };
    if !control.is_available() {
        return codec_unavailable();
    }
    match control.apply(&request).await {
        Ok(body) => Json(body).into_response(),
        Err(error) => api_error_response(error.status, error.error),
    }
}

fn codec_unavailable() -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "codec-video-unavailable",
        "no video encoder is available on this node",
        Some(
            "Enable the video capability and a capture source so the node advertises a video \
             encoder on GET /codec; a node without video cannot accept a codec override.",
        ),
    )
}

#[derive(Debug, Default, Deserialize)]
struct AudioApplyRequest {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    sink: Option<String>,
    #[serde(default)]
    source: Option<String>,
    /// Kept as `i64` so an out-of-range value is a typed 400 rather than a
    /// generic extractor rejection.
    #[serde(default)]
    volume: Option<i64>,
    #[serde(default)]
    muted: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct AudioRemoveRequest {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    sink: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AudioPlayRequest {
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    sink: Option<String>,
}

async fn audio_apply(
    State(state): State<ConnectionState>,
    payload: Result<Json<AudioApplyRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.audio.as_ref() else {
        return audio_not_implemented();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };
    let source = request
        .source
        .as_deref()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_owned);
    let requested_sink = request
        .sink
        .as_deref()
        .map(str::trim)
        .filter(|sink| !sink.is_empty())
        .map(str::to_owned);
    let sink = match requested_sink {
        Some(sink) => Some(sink),
        // A source-only profile must not mutate the configured default sink.
        None if source.is_some() => None,
        None => control.optional_sink(None),
    };
    if sink.is_none() && source.is_none() {
        // Preserve the typed 400 for a request that names neither side.
        return control.sink_or_default(None).unwrap_err();
    }
    let profile_id = match required_profile_id(request.profile_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Some(volume) = request.volume {
        if !(0..=100).contains(&volume) {
            return compat_error(
                StatusCode::BAD_REQUEST,
                "audio-invalid-volume",
                format!("volume {volume} is outside 0..=100"),
            );
        }
    }
    let volume = request.volume.map(|volume| volume as u8);
    let muted = request.muted;

    let result = match sink.clone() {
        Some(sink) => {
            let mut profile = match AudioProfile::new(profile_id.clone(), sink.clone()) {
                Ok(profile) => profile,
                Err(error) => return audio_error_response(error),
            };
            if let Some(source) = &source {
                profile = match profile.with_source(source.clone()) {
                    Ok(profile) => profile,
                    Err(error) => return audio_error_response(error),
                };
            }
            if let Some(volume) = volume {
                profile = match profile.with_volume(volume) {
                    Ok(profile) => profile,
                    Err(error) => return audio_error_response(error),
                };
            }
            if let Some(muted) = muted {
                profile = profile.with_muted(muted);
            }
            router.apply_profile(profile).await
        }
        None => {
            let mut profile = match AudioSourceProfile::new(
                profile_id.clone(),
                source.clone().unwrap_or_default(),
            ) {
                Ok(profile) => profile,
                Err(error) => return audio_error_response(error),
            };
            if let Some(volume) = volume {
                profile = match profile.with_volume(volume) {
                    Ok(profile) => profile,
                    Err(error) => return audio_error_response(error),
                };
            }
            if let Some(muted) = muted {
                profile = profile.with_muted(muted);
            }
            router.apply_source_profile(profile).await
        }
    };

    match result {
        Ok(()) => {
            control.remember_profile(&profile_id);
            Json(serde_json::json!({
                "ok": true,
                "profile_id": profile_id,
                "sink": sink,
                "source": source,
                "backend": control.backend(),
            }))
            .into_response()
        }
        Err(error) => audio_error_response(error),
    }
}

async fn audio_remove(
    State(state): State<ConnectionState>,
    payload: Result<Json<AudioRemoveRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.audio.as_ref() else {
        return audio_not_implemented();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };
    let profile_id = match request
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    {
        Some(value) => value,
        None => match control.last_profile() {
            // Preserve the Python controller's empty-body remove: it targets the
            // profile most recently applied through this control.
            Some(value) => value,
            // `None` is always an error here, and carries the typed 400.
            None => return required_profile_id(None).unwrap_err(),
        },
    };
    let _ = request.sink;

    match router.remove_profile(&profile_id).await {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "profile_id": profile_id,
            "backend": control.backend(),
        }))
        .into_response(),
        Err(error) => audio_error_response(error),
    }
}

async fn audio_play(
    State(state): State<ConnectionState>,
    payload: Result<Json<AudioPlayRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(control) = state.audio.as_ref() else {
        return audio_not_implemented();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };
    let track = request
        .track
        .as_deref()
        .map(str::trim)
        .filter(|track| !track.is_empty());
    let Some(track) = track else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "audio-missing-track",
            "missing `track` (a single file name such as `sweep.flac`)",
        );
    };
    let sink = match control.sink_or_default(request.sink) {
        Ok(sink) => sink,
        Err(response) => return response,
    };
    let spec = match AudioPlaybackSpec::new(track, sink.clone()) {
        Ok(spec) => spec,
        Err(error) => return audio_error_response(error),
    };

    match router.play_reference(spec).await {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "track": track,
            "sink": sink,
            "backend": control.backend(),
        }))
        .into_response(),
        Err(error) => audio_error_response(error),
    }
}

async fn audio_stop(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_not_implemented();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };

    match router.stop_playback().await {
        Ok(()) => {
            Json(serde_json::json!({"ok": true, "backend": control.backend()})).into_response()
        }
        Err(error) => audio_error_response(error),
    }
}

/// `GET /audio/room-correction/profile` — read the stored profile.
async fn audio_profile_get(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_profile_store_unsupported();
    };
    let Some(store) = control.profile_store() else {
        return audio_profile_store_unsupported();
    };
    match store.load() {
        Ok(Some(profile)) => Json(serde_json::json!({
            "ok": true,
            "available": true,
            "stored": true,
            "backend": control.backend(),
            "profile": profile,
        }))
        .into_response(),
        Ok(None) => Json(serde_json::json!({
            "ok": true,
            "available": true,
            "stored": false,
            "backend": control.backend(),
            "profile": Value::Null,
        }))
        .into_response(),
        Err(error) => audio_profile_error_response(error),
    }
}

/// `POST /audio/room-correction/profile` — validate and store a profile.
///
/// The body may be the profile itself or `{"profile": {...}}`. Only the
/// measurement fields are stored; no EQ is applied.
async fn audio_profile_put(
    State(state): State<ConnectionState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_profile_store_unsupported();
    };
    let Some(store) = control.profile_store() else {
        return audio_profile_store_unsupported();
    };
    let Json(body) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let value = match body.get("profile") {
        Some(profile) if !profile.is_null() => profile.clone(),
        Some(_) => {
            return audio_profile_error_response(AudioError::InvalidSpec {
                field: "room profile".to_owned(),
                reason: "`profile` must be an object, not null".to_owned(),
            })
        }
        None => body,
    };
    let profile: SweepProfile = match serde_json::from_value(value) {
        Ok(profile) => profile,
        Err(error) => {
            return audio_profile_error_response(AudioError::InvalidSpec {
                field: "room profile".to_owned(),
                reason: format!("invalid profile JSON: {error}"),
            })
        }
    };
    match store.store(&profile) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "available": true,
            "stored": true,
            "backend": control.backend(),
            "profile": profile,
        }))
        .into_response(),
        Err(error) => audio_profile_error_response(error),
    }
}

/// `DELETE /audio/room-correction/profile` — clear the stored profile.
async fn audio_profile_delete(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_profile_store_unsupported();
    };
    let Some(store) = control.profile_store() else {
        return audio_profile_store_unsupported();
    };
    match store.clear() {
        Ok(cleared) => Json(serde_json::json!({
            "ok": true,
            "available": true,
            "cleared": cleared,
            "backend": control.backend(),
        }))
        .into_response(),
        Err(error) => audio_profile_error_response(error),
    }
}

/// `POST /audio/room-correction/apply` — insert a reversible filter chain.
///
/// The body may carry a full `profile` object (or be the profile itself), or
/// omit it to apply the stored profile. `profile_id` selects/checks the stored
/// profile. `sink` (or `target_sink`) is the explicit target; when omitted the
/// node's configured `--audio-sink` is used. Nothing is moved onto the new EQ
/// sink and no default device is changed.
async fn audio_room_correction_apply(
    State(state): State<ConnectionState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_eq_application_unsupported();
    };
    let Some(store) = control.profile_store() else {
        return audio_eq_application_unsupported();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };
    let Json(body) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let Some(body) = body.as_object() else {
        return audio_eq_error_response(AudioError::InvalidSpec {
            field: "room eq request".to_owned(),
            reason: "must be a JSON object".to_owned(),
        });
    };
    let requested_id = match body.get("profile_id") {
        Some(Value::String(id)) => {
            let id = id.trim();
            if id.is_empty() {
                return audio_eq_error_response(AudioError::InvalidSpec {
                    field: "room eq profile_id".to_owned(),
                    reason: "must not be empty".to_owned(),
                });
            }
            Some(id.to_owned())
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return audio_eq_error_response(AudioError::InvalidSpec {
                field: "room eq profile_id".to_owned(),
                reason: "must be a string".to_owned(),
            })
        }
    };

    let inline = body.get("profile").filter(|profile| !profile.is_null());
    let profile = if let Some(value) = inline {
        match serde_json::from_value::<SweepProfile>(value.clone()) {
            Ok(profile) => profile,
            Err(error) => return audio_eq_error_response(invalid_profile_json(error)),
        }
    } else if body.contains_key("bands") {
        match serde_json::from_value::<SweepProfile>(Value::Object(body.clone())) {
            Ok(profile) => profile,
            Err(error) => return audio_eq_error_response(invalid_profile_json(error)),
        }
    } else {
        match store.load() {
            Ok(Some(profile)) => profile,
            Ok(None) => return audio_eq_missing_profile(),
            Err(error) => return audio_eq_error_response(error),
        }
    };
    if let Some(requested_id) = requested_id {
        if requested_id != profile.id {
            return audio_eq_error_response(AudioError::InvalidSpec {
                field: "room eq profile_id".to_owned(),
                reason: format!("`{requested_id}` does not match profile `{}`", profile.id),
            });
        }
    }
    if let Err(error) = validate_profile(&profile) {
        return audio_eq_error_response(error);
    }

    let requested_sink = match body.get("sink").or_else(|| body.get("target_sink")) {
        Some(Value::String(sink)) => Some(sink.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return audio_eq_error_response(AudioError::InvalidSpec {
                field: "room eq sink".to_owned(),
                reason: "must be a string".to_owned(),
            })
        }
    };
    let target_sink = match control.sink_or_default(requested_sink) {
        Ok(sink) => sink,
        Err(response) => return response,
    };
    let rate = match integer_field(body, "rate", u64::from(DEFAULT_EQ_RATE)) {
        Ok(value) => value as u32,
        Err(response) => return response,
    };
    let channels = match integer_field(body, "channels", u64::from(DEFAULT_EQ_CHANNELS)) {
        Ok(value) => value as u16,
        Err(response) => return response,
    };

    let spec = RoomEqSpec {
        eq_sink: next_eq_sink_name(&profile.id),
        profile_id: profile.id.clone(),
        target_sink,
        bands: profile.bands.clone(),
        rate,
        channels,
    };
    // A previous record describes a chain that the router is about to unload.
    // Clear it first so a failed replacement cannot leave a stale "active"
    // claim behind; the new record is written only after the sink is observed.
    if let Err(error) = store.clear_active() {
        return audio_eq_error_response(error);
    }
    let application = match router.apply_room_eq(spec).await {
        Ok(application) => application,
        Err(error) => return audio_eq_error_response(error),
    };

    // The state file is written only after the sink was observed loaded. If it
    // cannot be persisted, tear the module down again rather than leaving an
    // untracked filter chain in the graph.
    if let Err(error) = store.store_active(&application) {
        let _ = router.remove_room_eq().await;
        return audio_eq_error_response(error);
    }

    Json(serde_json::json!({
        "ok": true,
        "available": true,
        "active": true,
        "backend": control.backend(),
        "profile_id": application.profile_id,
        "target_sink": application.target_sink,
        "eq_sink": application.eq_sink,
        "module_id": application.module_id,
        "module_name": application.module_name,
        "active_bands": application.active_bands,
        "band_count": application.band_count,
        "insertion": application.insertion,
        "application": application,
    }))
    .into_response()
}

/// `DELETE /audio/room-correction/apply` — unload the active filter chain.
async fn audio_room_correction_delete(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_eq_application_unsupported();
    };
    let Some(store) = control.profile_store() else {
        return audio_eq_application_unsupported();
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };
    let removed = match router.remove_room_eq().await {
        Ok(removed) => removed,
        Err(error) => return audio_eq_error_response(error),
    };
    if let Err(error) = store.clear_active() {
        return audio_eq_error_response(error);
    }
    let body = match removed {
        Some(application) => serde_json::json!({
            "ok": true,
            "available": true,
            "active": false,
            "removed": true,
            "backend": control.backend(),
            "profile_id": application.profile_id,
            "target_sink": application.target_sink,
            "eq_sink": application.eq_sink,
            "module_id": application.module_id,
        }),
        None => serde_json::json!({
            "ok": true,
            "available": true,
            "active": false,
            "removed": false,
            "backend": control.backend(),
        }),
    };
    Json(body).into_response()
}

fn integer_field(
    body: &serde_json::Map<String, Value>,
    field: &str,
    default: u64,
) -> Result<u64, Response> {
    match body.get(field) {
        Some(Value::Null) | None => Ok(default),
        Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
            audio_eq_error_response(AudioError::InvalidSpec {
                field: format!("room eq {field}"),
                reason: "must be a non-negative integer".to_owned(),
            })
        }),
        Some(_) => Err(audio_eq_error_response(AudioError::InvalidSpec {
            field: format!("room eq {field}"),
            reason: "must be a non-negative integer".to_owned(),
        })),
    }
}

fn invalid_profile_json(error: serde_json::Error) -> AudioError {
    AudioError::InvalidSpec {
        field: "room profile".to_owned(),
        reason: format!("invalid profile JSON: {error}"),
    }
}

fn audio_eq_missing_profile() -> Response {
    let body = serde_json::json!({
        "ok": false,
        "available": true,
        "active": false,
        "code": "audio-profile-missing",
        "error": "No stored room-correction profile is available to apply.",
        "remediation": "Measure with POST /audio/sweep first, store a profile with \
                        /audio/room-correction/profile, or send a full `profile` object. \
                        No filter chain was loaded.",
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

fn audio_eq_application_unsupported() -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "audio-eq-application-unsupported",
        "room-correction EQ application is not enabled on this node",
        Some(
            "Start ozma-node with --audio-router pipewire and --audio-profile-dir \
             <path> (or OZMA_AUDIO_PROFILE_DIR). The active-state record needs the same \
             opt-in directory as the measured profile. Nothing was loaded.",
        ),
    )
}

fn audio_eq_error_response(error: AudioError) -> Response {
    let status = match &error {
        AudioError::InvalidSpec { .. } | AudioError::InvalidTarget(_) => StatusCode::BAD_REQUEST,
        AudioError::Refused { .. } => StatusCode::BAD_REQUEST,
        AudioError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        AudioError::MissingCommand { .. }
        | AudioError::Unavailable { .. }
        | AudioError::Process { .. } => StatusCode::SERVICE_UNAVAILABLE,
        AudioError::OutputParse { .. }
        | AudioError::CommandFailed { .. }
        | AudioError::Capture { .. }
        | AudioError::Closed { .. }
        | AudioError::EndOfStream
        | AudioError::InvalidFrame(_)
        | AudioError::FormatMismatch { .. }
        | AudioError::Encode { .. }
        | AudioError::Decode { .. }
        | AudioError::ChannelClosed => StatusCode::BAD_GATEWAY,
        AudioError::TrackNotFound { .. } => StatusCode::BAD_REQUEST,
    };
    let api = error.to_api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "available": false,
        "active": false,
        "code": api.code,
        "error": api.message,
    });
    if let Some(remediation) = api.remediation {
        body["remediation"] = Value::String(remediation);
    }
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    (status, Json(body)).into_response()
}

fn audio_profile_store_unsupported() -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "audio-profile-store-unsupported",
        "room-correction profile storage is not enabled on this node",
        Some(
            "Start ozma-node with --audio-router pipewire|pulseaudio and \
             --audio-profile-dir <path> (or OZMA_AUDIO_PROFILE_DIR) to opt in. \
             Without a directory nothing is written; no profile is invented.",
        ),
    )
}

fn audio_profile_error_response(error: AudioError) -> Response {
    let status = match &error {
        AudioError::InvalidSpec { .. } | AudioError::InvalidTarget(_) => StatusCode::BAD_REQUEST,
        AudioError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        AudioError::OutputParse { .. } => StatusCode::BAD_GATEWAY,
        AudioError::MissingCommand { .. } | AudioError::Unavailable { .. } => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        AudioError::Process { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    let api = error.to_api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "available": false,
        "stored": false,
        "profile": Value::Null,
        "code": api.code,
        "error": api.message,
    });
    if let Some(remediation) = api.remediation {
        body["remediation"] = Value::String(remediation);
    }
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    (status, Json(body)).into_response()
}

fn required_profile_id(value: Option<&str>) -> Result<String, Response> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            compat_error(
                StatusCode::BAD_REQUEST,
                "audio-missing-profile",
                "missing `profile_id`",
            )
        })
}

fn audio_not_implemented() -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "audio-routing-unsupported",
        "audio routing is not implemented on this node",
        Some(
            "Start ozma-node with --audio-router pipewire or --audio-router pulseaudio to \
             control an existing PipeWire/PulseAudio graph. The UAC2 gadget bridge is not a \
             substitute for /audio/apply, /audio/remove, /audio/play, or /audio/stop; see \
             docs/parity-matrix.md.",
        ),
    )
}

fn audio_unavailable(reason: &str) -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "audio-routing-unavailable",
        format!("audio routing is unavailable: {reason}"),
        Some(
            "Check the node log and /api/v1/connection.audio_error. The configured audio \
             backend did not initialise; no command was run and no success is claimed.",
        ),
    )
}

fn audio_error_response(error: AudioError) -> Response {
    let status = match &error {
        AudioError::InvalidSpec { .. } | AudioError::InvalidTarget(_) => StatusCode::BAD_REQUEST,
        AudioError::Refused { .. } => StatusCode::BAD_REQUEST,
        AudioError::TrackNotFound { .. } => StatusCode::BAD_REQUEST,
        AudioError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        AudioError::MissingCommand { .. }
        | AudioError::Unavailable { .. }
        | AudioError::Process { .. } => StatusCode::SERVICE_UNAVAILABLE,
        AudioError::Capture { .. }
        | AudioError::Closed { .. }
        | AudioError::EndOfStream
        | AudioError::InvalidFrame(_)
        | AudioError::FormatMismatch { .. }
        | AudioError::Encode { .. }
        | AudioError::Decode { .. }
        | AudioError::ChannelClosed
        | AudioError::CommandFailed { .. }
        | AudioError::OutputParse { .. } => StatusCode::BAD_GATEWAY,
    };
    api_error_response(status, error.to_api_error())
}

/// `GET /audio/nodes` — a bounded, read-only `pactl -f json` graph snapshot.
///
/// The body matches the soft-node shape the controller expects: `nodes`,
/// `links`, `default_sink`, and `default_source`. `streams` carries active
/// sink-inputs/source-outputs, and `links_available` is false because `pactl`
/// has no graph-link listing. A router that cannot enumerate returns a typed
/// 503; an empty list is never fabricated for a missing server or tool. The
/// route is public because the controller's proxy sends no node token, and it
/// runs only `list`/`info`, so it cannot change graph state.
async fn audio_nodes(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.audio.as_ref() else {
        return audio_graph_unavailable(
            "audio routing is not configured on this node",
            "Start ozma-node with --audio-router pipewire or --audio-router pulseaudio so \
             `pactl -f json` can enumerate the existing graph. No graph was fabricated.",
        );
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };

    match router.graph().await {
        Ok(graph) => {
            let mut body = match serde_json::to_value(&graph) {
                Ok(value) => value,
                Err(error) => {
                    return audio_graph_unavailable(
                        &format!("could not serialise graph snapshot: {error}"),
                        "This is an internal error; no nodes were fabricated.",
                    )
                }
            };
            if let Some(object) = body.as_object_mut() {
                object.insert("ok".to_owned(), Value::Bool(true));
                object.insert("available".to_owned(), Value::Bool(true));
                object.insert(
                    "backend".to_owned(),
                    Value::String(control.backend().to_owned()),
                );
            }
            Json(body).into_response()
        }
        Err(error) => audio_graph_error_response(error),
    }
}

/// `POST /audio/sweep` — a real generated-sweep measurement.
///
/// Accepts a bounded frequency/duration/profile request. The router plays a
/// generated log sweep to `sink`, captures `source` (normally
/// `<sink>.monitor`), and returns a profile only after audio was captured. A
/// missing tool, sink, source, or silent capture is a typed error with
/// `ok:false`, `available:false`, and `profile:null`; no measurement is
/// invented. The route is public because the controller's existing proxy sends
/// no node token.
async fn audio_sweep(
    State(state): State<ConnectionState>,
    payload: Result<Json<AudioSweepRequest>, JsonRejection>,
) -> Response {
    // Availability is checked before the body so an unconfigured node answers
    // a typed 503 instead of a body-extractor status, regardless of body shape.
    let Some(control) = state.audio.as_ref() else {
        return audio_sweep_error_response(sweep_unavailable());
    };
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let router = match control.router() {
        Ok(router) => router.clone(),
        Err(response) => return response,
    };

    let Some(source) = non_empty(payload.source.clone()) else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "audio-missing-source",
            "missing `source` (an existing source or `<sink>.monitor`)",
        );
    };
    let Some(sink) = non_empty(payload.sink.clone()) else {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "audio-missing-sink",
            "missing `sink` (an existing sink to play the generated sweep through)",
        );
    };

    let mut request = SweepRequest::new(source, sink);
    if let Some(value) = payload.start_hz {
        request.start_hz = value;
    }
    if let Some(value) = payload.end_hz {
        request.end_hz = value;
    }
    if let Some(value) = payload.duration_secs {
        request.duration_secs = value;
    }
    if let Some(value) = payload.sample_rate {
        request.sample_rate = value;
    }
    if let Some(value) = payload.channels {
        request.channels = value;
    }
    if let Some(value) = payload.amplitude {
        request.amplitude = value;
    }
    if let Some(value) = payload.start_delay_ms {
        request.start_delay_ms = value;
    }
    if let Some(value) = payload.tail_secs {
        request.tail_secs = value;
    }
    for (field, value) in [
        ("phone_model", &mut request.phone_model),
        ("target_curve", &mut request.target_curve),
        ("room_name", &mut request.room_name),
        ("node_id", &mut request.node_id),
        ("mic_type", &mut request.mic_type),
        ("mic_model", &mut request.mic_model),
    ] {
        if let Some(provided) = payload.profile_value(field) {
            *value = provided;
        }
    }

    // Validate before the router call so every backend rejects out-of-range
    // or unsafe requests, including test/alternate routers.
    if let Err(error) = request.validate() {
        return audio_sweep_error_response(error);
    }

    match router.sweep(request).await {
        Ok(outcome) => {
            let storage = control
                .profile_store()
                .map(|store| store.store(&outcome.profile));
            sweep_success_response(control.backend(), outcome, storage)
        }
        Err(error) => audio_sweep_error_response(error),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Default, Deserialize)]
struct AudioSweepRequest {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    sink: Option<String>,
    #[serde(default)]
    start_hz: Option<f64>,
    #[serde(default)]
    end_hz: Option<f64>,
    #[serde(default, alias = "duration")]
    duration_secs: Option<f64>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    channels: Option<u16>,
    #[serde(default)]
    amplitude: Option<f64>,
    #[serde(default)]
    start_delay_ms: Option<u64>,
    #[serde(default)]
    tail_secs: Option<f64>,
    #[serde(default)]
    phone_model: Option<String>,
    #[serde(default)]
    target_curve: Option<String>,
    #[serde(default)]
    room_name: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    mic_type: Option<String>,
    #[serde(default)]
    mic_model: Option<String>,
}

impl AudioSweepRequest {
    fn profile_value(&self, field: &str) -> Option<String> {
        match field {
            "phone_model" => self.phone_model.clone(),
            "target_curve" => self.target_curve.clone(),
            "room_name" => self.room_name.clone(),
            "node_id" => self.node_id.clone(),
            "mic_type" => self.mic_type.clone(),
            "mic_model" => self.mic_model.clone(),
            _ => None,
        }
    }
}

fn sweep_success_response(
    backend: &str,
    outcome: SweepOutcome,
    storage: Option<Result<(), AudioError>>,
) -> Response {
    let mut profile = match serde_json::to_value(&outcome.profile) {
        Ok(profile) => profile,
        Err(error) => {
            return audio_sweep_error_response(AudioError::Process {
                command: "sweep profile".to_owned(),
                message: error.to_string(),
            })
        }
    };
    let measurement = profile.get("measurement").cloned().unwrap_or(Value::Null);
    if let Some(object) = profile.as_object_mut() {
        object.insert("ok".to_owned(), Value::Bool(true));
        object.insert("available".to_owned(), Value::Bool(true));
        object.insert("backend".to_owned(), Value::String(backend.to_owned()));
    }
    let (storage_enabled, stored, storage_error) = match storage {
        None => (false, false, Value::Null),
        Some(Ok(())) => (true, true, Value::Null),
        Some(Err(error)) => {
            let api = error.to_api_error();
            (
                true,
                false,
                serde_json::json!({"code": api.code, "error": api.message}),
            )
        }
    };
    Json(serde_json::json!({
        "ok": true,
        "available": true,
        "backend": backend,
        "profile": profile,
        "measurement": measurement,
        "storage_enabled": storage_enabled,
        "stored": stored,
        "storage_error": storage_error,
    }))
    .into_response()
}

fn audio_graph_unavailable(error: &str, remediation: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "ok": false,
            "available": false,
            "code": "audio-graph-unavailable",
            "error": format!("audio graph enumeration is unavailable: {error}"),
            "remediation": remediation,
            "nodes": [],
            "links": [],
            "default_sink": Value::Null,
            "default_source": Value::Null,
        })),
    )
        .into_response()
}

fn audio_graph_error_response(error: AudioError) -> Response {
    let status = match &error {
        AudioError::InvalidSpec { .. } | AudioError::InvalidTarget(_) => StatusCode::BAD_REQUEST,
        AudioError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        AudioError::MissingCommand { .. }
        | AudioError::Unavailable { .. }
        | AudioError::Process { .. }
        | AudioError::CommandFailed { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    let api = error.to_api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "available": false,
        "code": api.code,
        "error": api.message,
        "nodes": [],
        "links": [],
        "default_sink": Value::Null,
        "default_source": Value::Null,
    });
    if let Some(remediation) = api.remediation {
        body["remediation"] = Value::String(remediation);
    }
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    (status, Json(body)).into_response()
}

fn sweep_unavailable() -> AudioError {
    AudioError::Unavailable {
        device: "audio sweep".to_owned(),
        message: "audio routing is not configured on this node".to_owned(),
        remediation: "Start ozma-node with --audio-router pipewire or --audio-router \
                      pulseaudio and ensure pw-play/pw-record or paplay/parec are installed. \
                      No sweep was generated or played."
            .to_owned(),
    }
}

fn audio_sweep_error_response(error: AudioError) -> Response {
    let status = match &error {
        AudioError::InvalidSpec { .. } | AudioError::InvalidTarget(_) => StatusCode::BAD_REQUEST,
        AudioError::Refused { .. } | AudioError::TrackNotFound { .. } => StatusCode::BAD_REQUEST,
        AudioError::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        AudioError::MissingCommand { .. }
        | AudioError::Unavailable { .. }
        | AudioError::Process { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    let api = error.to_api_error();
    let mut body = serde_json::json!({
        "ok": false,
        "available": false,
        "profile": Value::Null,
        "code": api.code,
        "error": api.message,
    });
    if let Some(remediation) = api.remediation {
        body["remediation"] = Value::String(remediation);
    }
    if !api.details.is_null() {
        body["details"] = api.details;
    }
    if api.retryable {
        body["retryable"] = Value::Bool(true);
    }
    (status, Json(body)).into_response()
}

fn api_error_response(status: StatusCode, error: ApiError) -> Response {
    let mut body = serde_json::json!({
        "ok": false,
        "code": error.code,
        "error": error.message,
    });
    if !error.details.is_null() {
        body["details"] = error.details;
    }
    if let Some(remediation) = error.remediation {
        body["remediation"] = Value::String(remediation);
    }
    if error.retryable {
        body["retryable"] = Value::Bool(true);
    }
    (status, Json(body)).into_response()
}

/// Optional per-console WebRTC selector in the query string.
#[derive(Debug, Default, Deserialize)]
struct WebrtcConsoleQuery {
    #[serde(default)]
    console: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

/// A `console` value that may be a JSON number or string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WebrtcConsoleSelector {
    Index(u32),
    Text(String),
}

impl WebrtcConsoleSelector {
    fn as_text(&self) -> String {
        match self {
            Self::Index(index) => index.to_string(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// `POST /webrtc/offer` body, matching the controller/soft-node shape.
#[derive(Debug, Deserialize)]
struct WebrtcOfferRequest {
    sdp: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Zero-based console index, `Console_N`, label, or source id.
    #[serde(default)]
    console: Option<WebrtcConsoleSelector>,
    /// Console identifier (same accepted forms as `console`).
    #[serde(default)]
    id: Option<String>,
}

/// `POST /webrtc/offer` response, matching the soft node's answer shape.
#[derive(Debug, Serialize)]
struct WebrtcOfferResponse {
    sdp: String,
    #[serde(rename = "type")]
    kind: &'static str,
    /// Helper session id for later `/webrtc/candidate` messages. Omitted when
    /// the configured worker does not emit one (old helpers stay compatible).
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// Console whose helper answered, when per-console WebRTC is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    console: Option<u32>,
}

/// `POST /webrtc/candidate` body; a late remote ICE candidate.
#[derive(Debug, Deserialize)]
struct WebrtcCandidateRequest {
    #[serde(default)]
    session: Option<String>,
    candidate: String,
    #[serde(default, rename = "sdpMLineIndex")]
    sdp_mline_index: u32,
    #[serde(default)]
    console: Option<WebrtcConsoleSelector>,
    #[serde(default)]
    id: Option<String>,
}

/// `POST /webrtc/bitrate` body; `bitrate` is bits per second.
#[derive(Debug, Deserialize)]
struct WebrtcBitrateRequest {
    #[serde(default = "default_webrtc_bitrate_bps")]
    bitrate: u64,
    #[serde(default)]
    console: Option<WebrtcConsoleSelector>,
    #[serde(default)]
    id: Option<String>,
}

/// Resolve the optional `console`/`id` selector from query and body.
async fn webrtc_requested_console(
    state: &ConnectionState,
    query: &WebrtcConsoleQuery,
    body_console: Option<&WebrtcConsoleSelector>,
    body_id: Option<&str>,
) -> Result<Option<u32>, Response> {
    let raw = query
        .console
        .clone()
        .or_else(|| body_console.map(WebrtcConsoleSelector::as_text))
        .or_else(|| query.id.clone())
        .or_else(|| body_id.map(str::to_owned));
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(manager) = state.multi_console_webrtc.as_ref() else {
        return Err(webrtc_console_unsupported(
            "this node has no per-console WebRTC manager",
        ));
    };
    if !manager.supports_independent_consoles() {
        return Err(webrtc_console_unsupported(
            "the configured capture provider exposes one default console",
        ));
    }
    manager
        .resolve_console(&raw)
        .await
        .map(Some)
        .map_err(|message| compat_error(StatusCode::BAD_REQUEST, "unknown-console", message))
}

/// True when the per-console manager owns the active helper too.
fn webrtc_manager_owns(state: &ConnectionState) -> bool {
    state
        .multi_console_webrtc
        .as_ref()
        .is_some_and(MultiConsoleWebrtc::supports_independent_consoles)
}

fn default_webrtc_bitrate_bps() -> u64 {
    4_000_000
}

const WEBRTC_BITRATE_MIN_BPS: u64 = 500_000;
const WEBRTC_BITRATE_MAX_BPS: u64 = 50_000_000;

async fn webrtc_offer(
    State(state): State<ConnectionState>,
    Query(query): Query<WebrtcConsoleQuery>,
    payload: Result<Json<WebrtcOfferRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    if let Some(kind) = request.kind.as_deref() {
        if !kind.eq_ignore_ascii_case("offer") {
            return compat_error(
                StatusCode::BAD_REQUEST,
                "invalid-offer-type",
                format!("expected `type: \"offer\"`, got `{kind}`"),
            );
        }
    }
    if request.sdp.trim().is_empty() {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-sdp",
            "the `sdp` field must not be empty",
        );
    }

    let requested = match webrtc_requested_console(
        &state,
        &query,
        request.console.as_ref(),
        request.id.as_deref(),
    )
    .await
    {
        Ok(requested) => requested,
        Err(response) => return response,
    };

    if requested.is_some() || webrtc_manager_owns(&state) {
        let Some(manager) = state.multi_console_webrtc.as_ref() else {
            return webrtc_not_configured();
        };
        if !manager.supports_independent_consoles() {
            return webrtc_console_unsupported(
                "the configured capture provider exposes one default console",
            );
        }
        let console = match requested {
            Some(console) => Some(console),
            None => Some(manager.active_console().await),
        };
        return match manager.offer(requested, &request.sdp).await {
            Ok(answer) => Json(WebrtcOfferResponse {
                sdp: answer.sdp,
                kind: "answer",
                session: answer.session,
                console,
            })
            .into_response(),
            Err(error) => webrtc_offer_failure(error),
        };
    }

    let Some(control) = state.webrtc.as_ref() else {
        return webrtc_not_configured();
    };
    if let Some(reason) = control.unavailable_reason() {
        return webrtc_unavailable(&reason);
    }

    match control.offer_answer(&request.sdp).await {
        Ok(answer) => Json(WebrtcOfferResponse {
            sdp: answer.sdp,
            kind: "answer",
            session: answer.session,
            console: None,
        })
        .into_response(),
        Err(error) => webrtc_offer_failure(error),
    }
}

async fn webrtc_candidate(
    State(state): State<ConnectionState>,
    Query(query): Query<WebrtcConsoleQuery>,
    payload: Result<Json<WebrtcCandidateRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    let candidate = request.candidate.trim();
    if candidate.is_empty() {
        return compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-candidate",
            "the `candidate` field must not be empty",
        );
    }

    let requested = match webrtc_requested_console(
        &state,
        &query,
        request.console.as_ref(),
        request.id.as_deref(),
    )
    .await
    {
        Ok(requested) => requested,
        Err(response) => return response,
    };

    if requested.is_some() || webrtc_manager_owns(&state) {
        let Some(manager) = state.multi_console_webrtc.as_ref() else {
            return webrtc_not_configured();
        };
        return match manager
            .candidate(
                requested,
                request.session.as_deref(),
                candidate,
                request.sdp_mline_index,
            )
            .await
        {
            Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
            Err(error) => webrtc_candidate_failure(error),
        };
    }

    let Some(control) = state.webrtc.as_ref() else {
        return webrtc_not_configured();
    };
    if let Some(reason) = control.unavailable_reason() {
        return webrtc_unavailable(&reason);
    }

    match control
        .add_ice_candidate(
            request.session.as_deref(),
            candidate,
            request.sdp_mline_index,
        )
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(error) => webrtc_candidate_failure(error),
    }
}

async fn webrtc_bitrate(
    State(state): State<ConnectionState>,
    Query(query): Query<WebrtcConsoleQuery>,
    payload: Result<Json<WebrtcBitrateRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };
    // The Python soft node clamps to this range for 4K60; mirror that so the
    // controller sees the same accepted value. The helper may still refuse.
    let bitrate = request
        .bitrate
        .clamp(WEBRTC_BITRATE_MIN_BPS, WEBRTC_BITRATE_MAX_BPS);

    let requested = match webrtc_requested_console(
        &state,
        &query,
        request.console.as_ref(),
        request.id.as_deref(),
    )
    .await
    {
        Ok(requested) => requested,
        Err(response) => return response,
    };

    if requested.is_some() || webrtc_manager_owns(&state) {
        let Some(manager) = state.multi_console_webrtc.as_ref() else {
            return webrtc_not_configured();
        };
        return match manager
            .set_bitrate(requested, (bitrate / 1_000).max(1) as u32)
            .await
        {
            Ok(()) => Json(serde_json::json!({"ok": true, "bitrate": bitrate})).into_response(),
            Err(error) => webrtc_bitrate_failure(error),
        };
    }

    let Some(control) = state.webrtc.as_ref() else {
        return webrtc_not_configured();
    };
    if let Some(reason) = control.unavailable_reason() {
        return webrtc_unavailable(&reason);
    }

    match control.set_bitrate_bps(bitrate as u32).await {
        Ok(()) => Json(serde_json::json!({"ok": true, "bitrate": bitrate})).into_response(),
        Err(error) => webrtc_bitrate_failure(error),
    }
}

fn webrtc_not_configured() -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "webrtc-not-configured",
        "WebRTC is not configured on this node",
        Some(
            "Start ozma-node with --webrtc-helper (and optionally \
             --webrtc-helper-script) pointing at the GStreamer WebRTC worker; see \
             docs/parity-matrix.md.",
        ),
    )
}

fn webrtc_unavailable(reason: &str) -> Response {
    compat_error_with_remediation(
        StatusCode::SERVICE_UNAVAILABLE,
        "webrtc-unavailable",
        reason.to_owned(),
        Some(
            "Check the node log and /api/v1/connection.webrtc_error; the external WebRTC \
             helper did not start or has stopped.",
        ),
    )
}

fn webrtc_console_unsupported(reason: &str) -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "webrtc-console-unsupported",
        format!("per-console WebRTC is unavailable: {reason}"),
        Some(
            "Per-console WebRTC requires --multi-console-webrtc plus a D-Bus capture \
             provider that exposes independent Console_N heads. Single-head providers keep \
             the active WebRTC stream; use POST /display/select to change it.",
        ),
    )
}

fn webrtc_offer_failure(error: WebRtcError) -> Response {
    match &error {
        WebRtcError::InvalidConfig(_) => {
            compat_error(StatusCode::BAD_REQUEST, "invalid-sdp", error.to_string())
        }
        WebRtcError::Unsupported(message) => webrtc_console_unsupported(message),
        _ => compat_error_with_remediation(
            StatusCode::SERVICE_UNAVAILABLE,
            "webrtc-unavailable",
            error.to_string(),
            Some(
                "The external helper did not answer the offer. Check that it is running and \
                 still owns its signaling socket; see the node log.",
            ),
        ),
    }
}

fn webrtc_candidate_failure(error: WebRtcError) -> Response {
    match &error {
        WebRtcError::InvalidConfig(_) => compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-candidate",
            error.to_string(),
        ),
        WebRtcError::Unsupported(_) => compat_error_with_remediation(
            StatusCode::NOT_IMPLEMENTED,
            "webrtc-candidate-unsupported",
            error.to_string(),
            Some(
                "The configured helper does not acknowledge trickled ICE candidates. \
                 Keep candidates embedded in the offer or update the helper.",
            ),
        ),
        WebRtcError::Timeout { .. } => compat_error_with_remediation(
            StatusCode::BAD_GATEWAY,
            "webrtc-helper-timeout",
            error.to_string(),
            Some("The helper did not acknowledge the candidate in time."),
        ),
        WebRtcError::NotConfigured => webrtc_not_configured(),
        WebRtcError::NotRunning => webrtc_unavailable(&error.to_string()),
        _ => compat_error_with_remediation(
            StatusCode::SERVICE_UNAVAILABLE,
            "webrtc-helper-error",
            error.to_string(),
            Some("Check the node log for the WebRTC helper's failure."),
        ),
    }
}

fn webrtc_bitrate_failure(error: WebRtcError) -> Response {
    match &error {
        WebRtcError::InvalidConfig(_) => compat_error(
            StatusCode::BAD_REQUEST,
            "invalid-bitrate",
            error.to_string(),
        ),
        WebRtcError::Unsupported(_) => compat_error_with_remediation(
            StatusCode::NOT_IMPLEMENTED,
            "webrtc-bitrate-unsupported",
            error.to_string(),
            Some(
                "The current GStreamer worker does not implement signaling bitrate changes. \
                 Configure the encoder bitrate when starting the helper instead of claiming the \
                 change was applied.",
            ),
        ),
        WebRtcError::Timeout { .. } => compat_error_with_remediation(
            StatusCode::BAD_GATEWAY,
            "webrtc-helper-timeout",
            error.to_string(),
            Some(
                "The helper did not acknowledge the bitrate request. \
                 scripts/webrtc/gst_webrtc_worker.py ignores it today, so this is an unsupported \
                 operation rather than a successful change.",
            ),
        ),
        WebRtcError::NotConfigured => webrtc_not_configured(),
        WebRtcError::NotRunning => webrtc_unavailable(&error.to_string()),
        _ => compat_error_with_remediation(
            StatusCode::SERVICE_UNAVAILABLE,
            "webrtc-helper-error",
            error.to_string(),
            Some("Check the node log for the WebRTC helper's failure."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_control::ConsoleSourceFactory;
    use crate::hls::mpegts_test_support::{mpegts_repeated, RecordingMpegTsSpawner};
    use crate::hls::{
        ffmpeg_argv, EncoderAvailability, EncoderProbe, FfmpegSpawner, FrameSink, HlsConfig,
        HlsController, HlsEncoder, HlsError, HlsSourceFactory, SpawnedFfmpeg, StaticEncoderProbe,
    };
    use crate::webrtc::WebRtcStatus;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use ozma_capture::{CaptureRegistry, FakeCaptureProvider};
    use ozma_input::{FakeInputSink, InputSink, RecordingSink};
    use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
    use ozma_power::{PowerBackend, PowerError, PowerOutcome, PowerState, UnavailablePower};
    use ozma_usb::{
        BulkTransportFault, ConfigFsUsbController, FakeFunctionFsMounter, FakeUsbGadgetController,
        FunctionFsHandoff, MassStorageDevice, MassStorageImage, MemoryBulkTransport, OverlayConfig,
        ScsiStatus, UsbFunction, UsbGadgetConfig, SCSI_SECTOR_BYTES,
    };
    use serde_json::Value;
    use std::time::Duration;
    use tower::ServiceExt;

    struct FakePower {
        status: String,
        fail: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakePower {
        fn running() -> Self {
            Self {
                status: "running".into(),
                fail: false,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing() -> Self {
            Self {
                status: "unknown".into(),
                fail: true,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn shared(self) -> SharedPower {
            Arc::new(tokio::sync::Mutex::new(
                Box::new(self) as Box<dyn PowerBackend>
            ))
        }

        /// Share the backend and return its call log for assertions.
        fn shared_recording(self) -> (SharedPower, Arc<Mutex<Vec<&'static str>>>) {
            let calls = Arc::clone(&self.calls);
            let power = Arc::new(tokio::sync::Mutex::new(
                Box::new(self) as Box<dyn PowerBackend>
            ));
            (power, calls)
        }

        fn record(&mut self, action: &'static str) -> Result<(), PowerError> {
            if self.fail {
                return Err(PowerError::NotConnected);
            }
            lock(&self.calls).push(action);
            Ok(())
        }
    }

    #[async_trait]
    impl PowerBackend for FakePower {
        async fn query_status(&mut self) -> Result<PowerState, PowerError> {
            if self.fail {
                Err(PowerError::NotConnected)
            } else {
                Ok(PowerState::new(self.status.clone()))
            }
        }

        async fn cont(&mut self) -> Result<(), PowerError> {
            self.record("on")
        }

        async fn pause(&mut self) -> Result<(), PowerError> {
            self.record("pause")
        }

        async fn resume(&mut self) -> Result<(), PowerError> {
            self.record("resume")
        }

        async fn system_powerdown(&mut self) -> Result<(), PowerError> {
            self.record("off")
        }

        async fn system_reset(&mut self) -> Result<(), PowerError> {
            self.record("reset")
        }

        async fn stop(&mut self) -> Result<(), PowerError> {
            self.record("force-off")
        }
    }

    /// A backend whose QMP connection has closed underneath it, as it does
    /// after a hard `quit`. The node must report a typed 502, not a fake state.
    struct ClosedQmpPower;

    #[async_trait]
    impl PowerBackend for ClosedQmpPower {
        async fn query_status(&mut self) -> Result<PowerState, PowerError> {
            Err(PowerError::Qmp(ozma_qmp::QmpError::ConnectionClosed))
        }

        async fn cont(&mut self) -> Result<(), PowerError> {
            Err(PowerError::Qmp(ozma_qmp::QmpError::ConnectionClosed))
        }

        async fn system_powerdown(&mut self) -> Result<(), PowerError> {
            Err(PowerError::Qmp(ozma_qmp::QmpError::ConnectionClosed))
        }

        async fn system_reset(&mut self) -> Result<(), PowerError> {
            Err(PowerError::Qmp(ozma_qmp::QmpError::ConnectionClosed))
        }

        async fn stop(&mut self) -> Result<(), PowerError> {
            Err(PowerError::Qmp(ozma_qmp::QmpError::ConnectionClosed))
        }
    }

    /// A backend that accepts ACPI shutdown but reports the guest still running,
    /// as libvirt does when the guest ignores the request.
    struct PendingOffPower;

    #[async_trait]
    impl PowerBackend for PendingOffPower {
        async fn query_status(&mut self) -> Result<PowerState, PowerError> {
            Ok(PowerState::running())
        }

        async fn cont(&mut self) -> Result<(), PowerError> {
            Ok(())
        }

        async fn system_powerdown(&mut self) -> Result<(), PowerError> {
            Ok(())
        }

        async fn system_powerdown_outcome(&mut self) -> Result<PowerOutcome, PowerError> {
            Ok(PowerOutcome::pending(
                PowerState::running(),
                "acpi-requested",
            ))
        }

        async fn system_reset(&mut self) -> Result<(), PowerError> {
            Ok(())
        }

        async fn stop(&mut self) -> Result<(), PowerError> {
            Ok(())
        }
    }

    use ozma_audio::{
        audio_codes, AudioGraphNode, AudioGraphSnapshot, AudioSinkSpec, AudioSinkState,
        AudioSourceProfile, AudioSourceRef, AudioSourceSpec, AudioSourceState, AudioStreamRef,
        RoomEqApplication, RoomEqSpec, SweepBand, SweepDescriptor, SweepMeasurement, SweepProfile,
    };

    struct FakeAudioRouter {
        calls: Arc<Mutex<Vec<String>>>,
        failure: Option<AudioError>,
        active_eq: Arc<Mutex<Option<RoomEqApplication>>>,
    }

    impl FakeAudioRouter {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                failure: None,
                active_eq: Arc::new(Mutex::new(None)),
            }
        }

        fn failing(error: AudioError) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                failure: Some(error),
                active_eq: Arc::new(Mutex::new(None)),
            }
        }

        fn calls(&self) -> Vec<String> {
            lock(&self.calls).clone()
        }

        fn active_eq(&self) -> Option<RoomEqApplication> {
            lock(&self.active_eq).clone()
        }

        fn record(&self, entry: String) -> Result<(), AudioError> {
            if let Some(error) = &self.failure {
                return Err(error.clone());
            }
            lock(&self.calls).push(entry);
            Ok(())
        }
    }

    #[async_trait]
    impl AudioRouter for FakeAudioRouter {
        async fn ensure_sink(&self, spec: AudioSinkSpec) -> Result<AudioSinkState, AudioError> {
            self.record(format!("ensure:{}", spec.name))?;
            Ok(AudioSinkState {
                monitor_target: format!("{}.monitor", spec.name),
                name: spec.name,
                created: false,
            })
        }

        async fn remove_sink(&self, name: &str) -> Result<(), AudioError> {
            self.record(format!("remove-sink:{name}"))
        }

        async fn move_stream(&self, stream: AudioStreamRef, sink: &str) -> Result<(), AudioError> {
            self.record(format!("move:{}:{sink}", stream.index))
        }

        async fn ensure_source(
            &self,
            spec: AudioSourceSpec,
        ) -> Result<AudioSourceState, AudioError> {
            self.record(format!("ensure-source:{}", spec.name))?;
            Ok(AudioSourceState {
                name: spec.name,
                master: spec.master,
                created: false,
            })
        }

        async fn remove_source(&self, name: &str) -> Result<(), AudioError> {
            self.record(format!("remove-source:{name}"))
        }

        async fn move_source_output(
            &self,
            stream: AudioSourceRef,
            source: &str,
        ) -> Result<(), AudioError> {
            self.record(format!("move-source:{}:{source}", stream.index))
        }

        async fn apply_source_profile(
            &self,
            profile: AudioSourceProfile,
        ) -> Result<(), AudioError> {
            self.record(format!(
                "apply-source:{}:{}:{:?}:{:?}",
                profile.id, profile.source, profile.volume, profile.muted
            ))
        }

        async fn apply_profile(&self, profile: AudioProfile) -> Result<(), AudioError> {
            self.record(format!(
                "apply:{}:{}:{:?}:{:?}:{:?}",
                profile.id, profile.sink, profile.source, profile.volume, profile.muted
            ))
        }

        async fn remove_profile(&self, profile_id: &str) -> Result<(), AudioError> {
            self.record(format!("remove:{profile_id}"))
        }

        async fn play_reference(&self, spec: AudioPlaybackSpec) -> Result<(), AudioError> {
            self.record(format!("play:{}:{}", spec.track, spec.sink))
        }

        async fn stop_playback(&self) -> Result<(), AudioError> {
            self.record("stop".to_owned())
        }

        async fn graph(&self) -> Result<AudioGraphSnapshot, AudioError> {
            self.record("graph".to_owned())?;
            Ok(AudioGraphSnapshot {
                nodes: vec![AudioGraphNode {
                    id: 1,
                    name: "ozma-vm1".to_owned(),
                    description: "Ozma VM 1".to_owned(),
                    media_class: "Audio/Sink".to_owned(),
                    default: true,
                    state: "RUNNING".to_owned(),
                    mute: false,
                    volume_percent: Some(75.0),
                    monitor_source: Some("ozma-vm1.monitor".to_owned()),
                    monitor_of_sink: None,
                }],
                streams: Vec::new(),
                links: Vec::new(),
                default_sink: Some("ozma-vm1".to_owned()),
                default_source: Some("ozma-vm1.monitor".to_owned()),
                truncated: false,
                links_available: false,
            })
        }

        async fn sweep(&self, request: SweepRequest) -> Result<SweepOutcome, AudioError> {
            self.record(format!(
                "sweep:{}:{}:{:.1}-{:.1}:{:.1}",
                request.source,
                request.sink,
                request.start_hz,
                request.end_hz,
                request.duration_secs
            ))?;
            Ok(SweepOutcome {
                profile: SweepProfile {
                    id: "sweep-test".to_owned(),
                    name: if request.room_name.trim().is_empty() {
                        "Test room".to_owned()
                    } else {
                        request.room_name.trim().to_owned()
                    },
                    created_at: 1.0,
                    bands: Vec::new(),
                    band_count: 0,
                    mic_type: request.mic_type,
                    phone_model: request.phone_model,
                    mic_model: request.mic_model,
                    target_curve: request.target_curve,
                    room_name: request.room_name,
                    node_id: request.node_id,
                    active: false,
                    analysis: None,
                    measurement: SweepMeasurement {
                        sample_rate: request.sample_rate,
                        channels: request.channels,
                        captured_frames: 48_000,
                        captured_secs: 1.0,
                        peak: 0.5,
                        peak_dbfs: -6.0,
                        rms_dbfs: -12.0,
                        noise_floor_dbfs: -80.0,
                        impulse_peak: 0.5,
                        impulse_index: 100,
                        impulse_index_secs: 0.002,
                        rt60_secs: Some(0.4),
                        decay_db_per_sec: Some(-150.0),
                        sweep: SweepDescriptor {
                            start_hz: request.start_hz,
                            end_hz: request.end_hz,
                            duration_secs: request.duration_secs,
                        },
                    },
                },
            })
        }

        async fn apply_room_eq(&self, spec: RoomEqSpec) -> Result<RoomEqApplication, AudioError> {
            self.record(format!(
                "eq-apply:{}:{}:{}:{}:{}",
                spec.profile_id,
                spec.target_sink,
                spec.eq_sink,
                spec.bands.len(),
                spec.channels
            ))?;
            let application = RoomEqApplication::from_spec(&spec, 4242);
            *lock(&self.active_eq) = Some(application.clone());
            Ok(application)
        }

        async fn remove_room_eq(&self) -> Result<Option<RoomEqApplication>, AudioError> {
            self.record("eq-remove".to_owned())?;
            Ok(lock(&self.active_eq).take())
        }

        async fn room_eq_application(&self) -> Result<Option<RoomEqApplication>, AudioError> {
            Ok(lock(&self.active_eq).clone())
        }
    }

    fn vm_state(power: Option<SharedPower>) -> ConnectionState {
        ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        )
        .with_power(power)
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: KeyModifiers(0),
        }
    }

    #[test]
    fn keyboard_press_and_release() {
        let mut decoder = HidPacketDecoder::new();
        // "a" (usage 0x04) down.
        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(events, vec![key(0x04, true)]);

        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(events, vec![key(0x04, false)]);
    }

    #[test]
    fn keyboard_multiple_keys_and_duplicate_report() {
        let mut decoder = HidPacketDecoder::new();
        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(events, vec![key(0x04, true), key(0x05, true)]);

        // Identical report: no flood.
        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0x05, 0, 0, 0, 0])
            .unwrap();
        assert!(events.is_empty());

        // One key released, one still held.
        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x05, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(events, vec![key(0x04, false)]);
    }

    #[test]
    fn keyboard_modifier_press_and_release() {
        let mut decoder = HidPacketDecoder::new();
        let events = decoder
            .decode(&[0x01, 0x02, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::Key {
                    code: 0xE1, // left shift
                    down: true,
                    modifiers: KeyModifiers(0),
                },
                InputEvent::Key {
                    code: 0x04,
                    down: true,
                    modifiers: KeyModifiers(0),
                },
            ]
        );

        // Shift released while the key stays down: only the modifier changes.
        let events = decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(
            events,
            vec![InputEvent::Key {
                code: 0xE1,
                down: false,
                modifiers: KeyModifiers(0),
            }]
        );
    }

    #[test]
    fn modifier_release_clears_held_modifier_state() {
        let mut decoder = HidPacketDecoder::new();
        let sink = FakeInputSink::new();

        // Ctrl down.
        for event in decoder
            .decode(&[0x01, 0x01, 0x00, 0, 0, 0, 0, 0, 0])
            .unwrap()
        {
            sink.submit(&event).unwrap();
        }
        // 'a' down while Ctrl is held.
        for event in decoder
            .decode(&[0x01, 0x01, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap()
        {
            sink.submit(&event).unwrap();
        }
        assert_eq!(sink.held().modifiers, 0x01);
        assert!(sink.held().has_key(0x04));

        // Ctrl released, 'a' still held. The key event must not have seeded
        // `client_modifiers`, so the modifier state clears immediately.
        for event in decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap()
        {
            sink.submit(&event).unwrap();
        }
        assert_eq!(sink.held().modifiers, 0);
        assert!(sink.held().has_key(0x04));

        // 'a' released.
        for event in decoder
            .decode(&[0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0])
            .unwrap()
        {
            sink.submit(&event).unwrap();
        }
        assert!(sink.held().is_empty());
    }

    #[test]
    fn absolute_coordinates_are_read_as_unsigned() {
        let mut decoder = HidPacketDecoder::new();
        // x = 0xFFFF (65535), y = 0x7FFF (32767). Both are valid unsigned
        // wire values even though the previous signed parser would have
        // produced -1 for x.
        let events = decoder
            .decode(&[0x02, 0x00, 0xFF, 0xFF, 0xFF, 0x7F, 0x00])
            .unwrap();
        assert_eq!(
            events,
            vec![InputEvent::PointerAbsolute {
                display_id: None,
                x: 65535,
                y: 32767,
            }]
        );
    }

    #[test]
    fn absolute_pointer_move_button_and_wheel() {
        let mut decoder = HidPacketDecoder::new();
        // x = 320, y = 240, no buttons, wheel +1.
        let events = decoder
            .decode(&[0x02, 0x00, 0x40, 0x01, 0xF0, 0x00, 0x01])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 320,
                    y: 240,
                },
                InputEvent::Wheel { dx: 0, dy: 1 },
            ]
        );

        // Left button pressed, scroll -1 (0xFF).
        let events = decoder
            .decode(&[0x02, 0x01, 0x40, 0x01, 0xF0, 0x00, 0xFF])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 320,
                    y: 240,
                },
                InputEvent::Button {
                    button: PointerButton::Left,
                    down: true,
                },
                InputEvent::Wheel { dx: 0, dy: -1 },
            ]
        );

        // Button release.
        let events = decoder
            .decode(&[0x02, 0x00, 0x40, 0x01, 0xF0, 0x00, 0x00])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 320,
                    y: 240,
                },
                InputEvent::Button {
                    button: PointerButton::Left,
                    down: false,
                },
            ]
        );
    }

    #[test]
    fn relative_pointer_move_button_and_wheel() {
        let mut decoder = HidPacketDecoder::new();
        // dx = -2 (0xFFFE), dy = 3, right button, wheel +1.
        let events = decoder
            .decode(&[0x05, 0x02, 0xFE, 0xFF, 0x03, 0x00, 0x01])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerRelative { dx: -2, dy: 3 },
                InputEvent::Button {
                    button: PointerButton::Right,
                    down: true,
                },
                InputEvent::Wheel { dx: 0, dy: 1 },
            ]
        );

        let events = decoder
            .decode(&[0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
            .unwrap();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerRelative { dx: 0, dy: 0 },
                InputEvent::Button {
                    button: PointerButton::Right,
                    down: false,
                },
            ]
        );
    }

    #[test]
    fn rejects_short_and_unknown_packets_without_panicking() {
        let mut decoder = HidPacketDecoder::new();
        assert_eq!(decoder.decode(&[]), Err(CompatError::Empty));
        assert_eq!(
            decoder.decode(&[0x01, 0x00]),
            Err(CompatError::Short {
                kind: "keyboard",
                expected: 8,
                actual: 1,
            })
        );
        assert_eq!(
            decoder.decode(&[0x02, 0x00]),
            Err(CompatError::Short {
                kind: "absolute pointer",
                expected: 5,
                actual: 1,
            })
        );
        assert_eq!(
            decoder.decode(&[0x05, 0x00]),
            Err(CompatError::Short {
                kind: "relative pointer",
                expected: 5,
                actual: 1,
            })
        );
        assert_eq!(
            decoder.decode(&[0x09, 0x00]),
            Err(CompatError::UnknownType(0x09))
        );

        // A rejected packet must not disturb retained state.
        decoder
            .decode(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0])
            .unwrap();
        assert!(decoder.decode(&[0x01, 0x00]).is_err());
        assert_eq!(
            decoder
                .decode(&[0x01, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0])
                .unwrap(),
            vec![key(0x04, false)]
        );
    }

    #[tokio::test]
    async fn udp_listener_decodes_and_submits_events() {
        let recording = Arc::new(RecordingSink::new(FakeInputSink::new()));
        let sink: SharedSink = recording.clone();
        let stats = CompatStats::default();
        let listener = HidUdpListener::bind(0, sink, stats.clone()).await.unwrap();
        let address = listener.local_addr().unwrap();
        assert_ne!(address.port(), 0);
        assert_eq!(stats.snapshot().bound_port, address.port());

        let task = tokio::spawn(async move { listener.run().await });
        let sender = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();

        sender
            .send_to(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0], address)
            .await
            .unwrap();
        wait_for(|| recording.held().has_key(0x04)).await;

        sender
            .send_to(&[0x01, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0], address)
            .await
            .unwrap();
        wait_for(|| !recording.held().has_key(0x04)).await;

        sender
            .send_to(&[0x02, 0x01, 0x40, 0x01, 0xF0, 0x00, 0x00], address)
            .await
            .unwrap();
        wait_for(|| recording.held().has_button(PointerButton::Left)).await;

        // Malformed packet: counted, listener stays up.
        sender.send_to(&[0x09], address).await.unwrap();
        wait_for(|| stats.snapshot().packets_rejected == 1).await;

        let events = recording.take_events();
        assert_eq!(
            events,
            vec![
                key(0x04, true),
                key(0x04, false),
                InputEvent::PointerAbsolute {
                    display_id: None,
                    x: 320,
                    y: 240,
                },
                InputEvent::Button {
                    button: PointerButton::Left,
                    down: true,
                },
            ]
        );

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.status, "running");
        assert_eq!(snapshot.packets_received, 4);
        assert_eq!(snapshot.packets_rejected, 1);
        assert_eq!(snapshot.events, 4);
        assert!(snapshot
            .last_error
            .unwrap()
            .contains("unknown HID packet type"));

        task.abort();
        let _ = task.await;
        assert_eq!(stats.snapshot().status, "stopped");
    }

    #[tokio::test]
    async fn sink_rejections_are_counted_and_do_not_stop_the_listener() {
        // The default fake sink is absolute-pointer only, so a relative event
        // is rejected by the sink while the listener must keep serving.
        let recording = Arc::new(RecordingSink::new(FakeInputSink::new()));
        let sink: SharedSink = recording.clone();
        let stats = CompatStats::default();
        let listener = HidUdpListener::bind(0, sink, stats.clone()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { listener.run().await });
        let sender = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();

        sender
            .send_to(&[0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00], address)
            .await
            .unwrap();
        wait_for(|| stats.snapshot().submit_errors == 1).await;

        sender
            .send_to(&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0], address)
            .await
            .unwrap();
        wait_for(|| recording.held().has_key(0x04)).await;

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.status, "running");
        assert_eq!(snapshot.submit_errors, 1);
        assert!(snapshot.last_error.is_some());

        task.abort();
        let _ = task.await;
    }

    #[test]
    fn live_vnc_registration_retracts_and_falls_back_to_external() {
        let extras = crate::registration::RegistrationExtras::resolve(
            None,
            Some("10.0.0.5"),
            Some(5901),
            None,
        );
        let info = ConnectionInfo::new(
            "node-a._ozma._udp.local.",
            "hardware",
            7382,
            7384,
            7331,
            SINK_FAKE_RECORDING,
        )
        .with_registration(extras);
        let vnc = VncRegistrationState::new(
            Some(("127.0.0.1".to_owned(), 40_000)),
            Some(("10.0.0.5".to_owned(), 5901)),
        );
        let live = vnc.live_handle();
        let state = ConnectionState::new(info, CompatStats::default()).with_vnc_registration(vnc);

        match state.registration_snapshot().vnc {
            crate::registration::VncRegistration::Configured { host, port } => {
                assert_eq!((host.as_str(), port), ("127.0.0.1", 40_000));
            }
            other => panic!("expected the live listener, got {other:?}"),
        }
        *live
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        match state.registration_snapshot().vnc {
            crate::registration::VncRegistration::Configured { host, port } => {
                assert_eq!((host.as_str(), port), ("10.0.0.5", 5901));
            }
            other => panic!("expected the external fallback, got {other:?}"),
        }

        let no_external = VncRegistrationState::new(Some(("127.0.0.1".to_owned(), 1)), None);
        let live = no_external.live_handle();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-b._ozma._udp.local.",
                "desktop",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_vnc_registration(no_external);
        *live
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        assert!(matches!(
            state.registration_snapshot().vnc,
            crate::registration::VncRegistration::NotConfigured
        ));
    }

    #[tokio::test]
    async fn connection_endpoint_reports_honest_status() {
        let stats = CompatStats::default();
        stats.set_bound_port(7331);
        stats.set_listener_running(true);
        stats.record_packet();
        stats.record_events(2);

        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            stats,
        );
        let app = connection_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connection")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["ok"], true);
        assert_eq!(json["node_id"], "node-a._ozma._udp.local.");
        assert_eq!(json["profile"], "hardware");
        assert_eq!(json["api_port"], 7382);
        assert_eq!(json["quic_port"], 7384);
        assert_eq!(json["hid_udp_port"], 7331);
        assert_eq!(json["sink_kind"], "fake-recording");
        assert_eq!(json["hid"]["status"], "running");
        assert_eq!(json["hid"]["bound_port"], 7331);
        assert_eq!(json["hid"]["packets_received"], 1);
        assert_eq!(json["hid"]["packets_rejected"], 0);
        assert_eq!(json["hid"]["events"], 2);
        assert_eq!(json["hid"]["submit_errors"], 0);
        assert!(json["hid"]["last_error"].is_null());
        assert_eq!(json["power"], "not-configured");
        assert_eq!(json["input"]["sink_kind"], "fake-recording");
        assert_eq!(json["input"]["available"], false);
        assert_eq!(json["input"]["injecting"], false);
        assert!(json["codec"]["current_encoder"].is_null());
        assert!(json["codec"]["receipt"].is_null());
        assert_eq!(json["codec"]["pending_restart"], false);
        assert_eq!(json["hls"], "not-configured");
        assert!(json["hls_error"].is_null());
        assert_eq!(json["webrtc"], "not-configured");
        assert!(json["webrtc_error"].is_null());
        assert!(json["webrtc_geometry"].is_null());
        assert_eq!(json["audio"], "not-configured");
        assert!(json["audio_error"].is_null());
        assert_eq!(json["display_outputs"], serde_json::json!([]));
        assert!(json["display_capture"]["active_console"].is_null());
        assert!(json["display_capture"]["provider"].is_null());
        assert!(json["display_capture"]["limitation"].is_null());
    }

    #[tokio::test]
    async fn connection_reports_display_outputs_and_the_capture_limitation() {
        let state = ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        )
        .with_display_outputs(
            serde_json::json!([
                {
                    "index": 0,
                    "name": "Console_0",
                    "source_type": "qmp",
                    "capture_source_id": "qmp0",
                    "width": 1920,
                    "height": 1080,
                    "primary": true
                },
                {
                    "index": 1,
                    "name": "Console_1",
                    "source_type": "qmp",
                    "capture_source_id": "",
                    "width": 1280,
                    "height": 1024,
                    "primary": false
                }
            ]),
            DisplayCaptureStatus::new(
                Some(0),
                Some("qmp".into()),
                Some("only Console_0 has a live capture source".into()),
            ),
        );

        let (status, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["display_outputs"].as_array().unwrap().len(), 2);
        assert_eq!(json["display_outputs"][0]["capture_source_id"], "qmp0");
        assert_eq!(json["display_outputs"][1]["capture_source_id"], "");
        assert_eq!(json["display_capture"]["active_console"], 0);
        assert_eq!(json["display_capture"]["provider"], "qmp");
        assert_eq!(
            json["display_capture"]["limitation"],
            "only Console_0 has a live capture source"
        );
    }

    struct PerConsoleHlsFactory;

    #[async_trait]
    impl crate::multi_hls::ConsoleHlsFactory for PerConsoleHlsFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Dbus)
        }

        async fn start(
            &self,
            _console: u32,
            config: HlsConfig,
        ) -> Result<Arc<dyn crate::multi_hls::ConsoleHlsPipeline>, String> {
            std::fs::create_dir_all(&config.output_dir).map_err(|error| error.to_string())?;
            let manifest = config.manifest_path();
            std::fs::write(&manifest, "#EXTM3U\n").map_err(|error| error.to_string())?;
            Ok(Arc::new(PerConsoleHlsPipeline { manifest }))
        }
    }

    struct PerConsoleHlsPipeline {
        manifest: std::path::PathBuf,
    }

    #[async_trait]
    impl crate::multi_hls::ConsoleHlsPipeline for PerConsoleHlsPipeline {
        fn status(&self) -> HlsStatus {
            HlsStatus::running(self.manifest.clone())
        }

        fn is_running(&self) -> bool {
            true
        }

        fn encoder(&self) -> HlsEncoder {
            HlsEncoder::H264
        }

        fn bitrate_kbps(&self) -> u32 {
            4_000
        }

        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }

        async fn restart_with_encoder(
            &self,
            encoder: HlsEncoder,
            _bitrate_kbps: Option<u32>,
            _allow_software_fallback: bool,
        ) -> Result<crate::hls::HlsRestartOutcome, crate::multi_hls::ConsoleHlsRestartError>
        {
            Ok(crate::hls::HlsRestartOutcome {
                status: HlsStatus::running(self.manifest.clone()),
                requested_encoder: encoder,
                applied_encoder: encoder,
                fallback: None,
            })
        }

        fn manifest_path(&self) -> std::path::PathBuf {
            self.manifest.clone()
        }
    }

    #[tokio::test]
    async fn connection_and_display_info_report_per_console_hls() {
        let root =
            std::env::temp_dir().join(format!("ozma-compat-multi-hls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let manager = crate::multi_hls::MultiConsoleHls::new(
            Arc::new(PerConsoleHlsFactory),
            crate::multi_hls::MultiConsoleHlsConfig {
                max_pipelines: 2,
                idle_timeout: Duration::ZERO,
                root: root.clone(),
                base: HlsConfig {
                    output_dir: root,
                    ..HlsConfig::default()
                },
                audio_source_template: None,
                audio_bitrate_kbps: crate::hls::DEFAULT_AUDIO_BITRATE_KBPS,
            },
        );
        let outputs = vec![
            crate::display_outputs::DisplayOutput {
                index: 0,
                name: "Console_0".into(),
                source_type: "dbus".into(),
                capture_source_id: "dbus0".into(),
                width: 1280,
                height: 800,
                primary: true,
            },
            crate::display_outputs::DisplayOutput {
                index: 1,
                name: "Console_1".into(),
                source_type: "dbus".into(),
                capture_source_id: String::new(),
                width: 720,
                height: 400,
                primary: false,
            },
        ];
        manager
            .sync(&crate::display_outputs::DisplayOutputSelection {
                outputs: outputs.clone(),
                active_console: 0,
                limitation: None,
            })
            .await;

        let state = ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        )
        .with_display_outputs(
            serde_json::to_value(&outputs).unwrap(),
            DisplayCaptureStatus::new(Some(0), Some("dbus".into()), None),
        )
        .with_multi_console_hls(Some(manager));

        let (status, json) =
            get_json(&connection_router(state.clone()), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["hls"], "running");
        assert_eq!(json["multi_console_hls"]["enabled"], true);
        assert_eq!(json["multi_console_hls"]["max_pipelines"], 2);
        assert_eq!(json["multi_console_hls"]["consoles"][1]["console"], 1);
        assert_eq!(
            json["multi_console_hls"]["consoles"][1]["status"],
            "running"
        );
        assert_eq!(
            json["multi_console_hls"]["consoles"][1]["playlist"],
            "/stream/console-1/stream.m3u8"
        );

        let (status, info) = get_json(&public_router(state), "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["console"], 0);
        assert_eq!(info["multi_console_hls"]["console"], 0);
        assert_eq!(info["multi_console_hls"]["status"], "running");
    }

    struct SingleHeadHlsFactory;

    #[async_trait]
    impl crate::multi_hls::ConsoleHlsFactory for SingleHeadHlsFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Qmp)
        }

        async fn start(
            &self,
            _console: u32,
            _config: HlsConfig,
        ) -> Result<Arc<dyn crate::multi_hls::ConsoleHlsPipeline>, String> {
            Err("single-head provider must not start a per-console pipeline".to_owned())
        }
    }

    #[tokio::test]
    async fn single_head_per_console_hls_keeps_the_active_single_hls_status() {
        let root = std::env::temp_dir().join(format!(
            "ozma-compat-multi-hls-single-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let manifest = root.join("stream.m3u8");
        std::fs::write(&manifest, "#EXTM3U\n").unwrap();
        let manager = crate::multi_hls::MultiConsoleHls::new(
            Arc::new(SingleHeadHlsFactory),
            crate::multi_hls::MultiConsoleHlsConfig {
                max_pipelines: 2,
                idle_timeout: Duration::ZERO,
                root: root.clone(),
                base: HlsConfig {
                    output_dir: root,
                    ..HlsConfig::default()
                },
                audio_source_template: None,
                audio_bitrate_kbps: crate::hls::DEFAULT_AUDIO_BITRATE_KBPS,
            },
        );
        manager
            .sync(&crate::display_outputs::DisplayOutputSelection {
                outputs: vec![
                    crate::display_outputs::DisplayOutput {
                        index: 0,
                        name: "Console_0".into(),
                        source_type: "qmp".into(),
                        capture_source_id: "qmp0".into(),
                        width: 1280,
                        height: 800,
                        primary: true,
                    },
                    crate::display_outputs::DisplayOutput {
                        index: 1,
                        name: "Console_1".into(),
                        source_type: "qmp".into(),
                        capture_source_id: String::new(),
                        width: 720,
                        height: 400,
                        primary: false,
                    },
                ],
                active_console: 0,
                limitation: None,
            })
            .await;

        let state = ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        )
        .with_hls(Some(HlsStatus::running(manifest)))
        .with_multi_console_hls(Some(manager));

        let (status, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["hls"], "running",
            "single-head providers keep the active single HLS status"
        );
        assert_eq!(json["multi_console_hls"]["enabled"], true);
        assert!(json["multi_console_hls"]["limitation"]
            .as_str()
            .is_some_and(|note| note.contains("one default console")));
        assert!(json["multi_console_hls"]["consoles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|console| console["status"] == "unsupported"));
    }

    struct SwitchConsoleFactory;

    impl crate::display_control::ConsoleSourceFactory for SwitchConsoleFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Dbus)
        }

        fn default_console(&self) -> u32 {
            0
        }

        fn supports_console_switch(&self) -> bool {
            true
        }

        fn registry(&self, console: u32) -> Result<CaptureRegistry, String> {
            Ok(CaptureRegistry::new().with(
                FakeCaptureProvider::new()
                    .with_identity(format!("console-{console}"), format!("Console_{console}")),
            ))
        }

        fn selection(
            &self,
            console: u32,
        ) -> Result<crate::display_outputs::DisplayOutputSelection, String> {
            crate::display_outputs::outputs_from_specs(
                &[
                    "0:Console_0:1920x1080:primary:dbus".to_owned(),
                    "1:Console_1:1280x1024:dbus".to_owned(),
                ],
                ozma_display::vm::VmConsoleBackend::Dbus,
                Some("dbus0"),
                Some(console),
            )
        }
    }

    struct SingleHeadConsoleFactory;

    impl crate::display_control::ConsoleSourceFactory for SingleHeadConsoleFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Qmp)
        }

        fn default_console(&self) -> u32 {
            0
        }

        fn supports_console_switch(&self) -> bool {
            false
        }

        fn registry(&self, _console: u32) -> Result<CaptureRegistry, String> {
            Ok(CaptureRegistry::new()
                .with(FakeCaptureProvider::new().with_identity("qmp0", "QMP display")))
        }

        fn selection(
            &self,
            console: u32,
        ) -> Result<crate::display_outputs::DisplayOutputSelection, String> {
            crate::display_outputs::outputs_from_specs(
                &[
                    "0:Console_0:1024x768:primary:qmp".to_owned(),
                    "1:Console_1:800x600:qmp".to_owned(),
                ],
                ozma_display::vm::VmConsoleBackend::Qmp,
                Some("qmp0"),
                Some(console),
            )
        }
    }

    fn console_state(
        factory: Arc<dyn crate::display_control::ConsoleSourceFactory>,
    ) -> ConnectionState {
        let slot = Arc::new(std::sync::RwLock::new(
            factory.registry(0).expect("active registry"),
        ));
        let control =
            DisplayControl::new(factory.clone(), slot, Vec::new()).expect("display control");
        let broker = crate::display_broker::ConsoleBroker::new(
            factory,
            crate::display_broker::ConsoleBrokerConfig::default(),
        );
        ConnectionState::new(
            ConnectionInfo::new(
                "vm-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(FakeCaptureProvider::new()),
        )))
        .with_display_control(Some(control))
        .with_console_broker(Some(broker))
    }

    #[tokio::test]
    async fn display_snapshot_and_info_accept_a_per_console_query() {
        let app = public_router(console_state(Arc::new(SwitchConsoleFactory)));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot?console=Console_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.starts_with(&[0xFF, 0xD8]));
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&body));
        decoder.decode().expect("valid JPEG");

        let (status, info) = get_json(&app, "/display/info?console=1").await;
        assert_eq!(status, StatusCode::OK, "{info}");
        assert_eq!(info["console"], 1);
        assert_eq!(info["active"], false);
        assert_eq!(info["source_id"], "console-1");
        assert_eq!(info["snapshot_available"], true);

        // The default route still describes the active console (backward
        // compatible shape with the new console metadata).
        let (status, info) = get_json(&app, "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["console"], 0);
        assert_eq!(info["active"], true);
        assert_eq!(info["source_id"], "fake0");

        let (status, body) = get_json(&app, "/display/snapshot?console=Console_9").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "not_found");
        assert_eq!(body["details"]["available"], serde_json::json!([0, 1]));

        let (status, body) = get_json(&app, "/display/info?console=9").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
    }

    #[tokio::test]
    async fn display_snapshot_rejects_a_single_head_console_query() {
        let app = public_router(console_state(Arc::new(SingleHeadConsoleFactory)));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot?console=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "conflict");
        assert_eq!(body["details"]["single_head"], true);
        assert_eq!(body["details"]["provider"], "qmp");
        assert_eq!(body["details"]["requested_console"], 1);

        // Info explains unavailability instead of inventing a source.
        let (status, info) = get_json(&app, "/display/info?console=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["console"], 1);
        assert_eq!(info["active"], false);
        assert_eq!(info["snapshot_available"], false);
        assert!(info["last_error"]
            .as_str()
            .unwrap()
            .contains("one default console"));

        // The active console still snapshots normally.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot?console=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
    }

    #[tokio::test]
    async fn display_select_updates_the_connection_shape_and_capture_source() {
        let slot = Arc::new(std::sync::RwLock::new(
            <SwitchConsoleFactory as crate::display_control::ConsoleSourceFactory>::registry(
                &SwitchConsoleFactory,
                0,
            )
            .expect("registry"),
        ));
        let control = DisplayControl::new(Arc::new(SwitchConsoleFactory), slot, Vec::new())
            .expect("display control");
        let state = ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        )
        .with_display_control(Some(control.clone()));
        let app = connection_router(state.clone());

        let (status, first) = get_json(&app, "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["display_capture"]["active_console"], 0);
        assert_eq!(first["display_capture"]["provider"], "dbus");
        assert_eq!(first["display_outputs"][0]["capture_source_id"], "dbus0");
        assert_eq!(first["display_outputs"][1]["capture_source_id"], "");

        let (status, selected) = post_json(
            &display_router(state.clone()),
            "/display/select",
            serde_json::json!({"index": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected["active_console"], 1);
        assert_eq!(selected["previous_console"], 0);
        assert_eq!(selected["changed"], true);
        assert_eq!(selected["display_capture"]["active_console"], 1);
        assert_eq!(selected["display_outputs"][0]["capture_source_id"], "");
        assert_eq!(selected["display_outputs"][1]["capture_source_id"], "dbus0");

        let (status, after) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(after["display_capture"]["active_console"], 1);
        assert_eq!(after["display_outputs"][0]["capture_source_id"], "");
        assert_eq!(after["display_outputs"][1]["capture_source_id"], "dbus0");
        assert!(after["display_capture"]["limitation"]
            .as_str()
            .is_some_and(|note| note.contains("Console_1")));
    }

    #[tokio::test]
    async fn display_select_without_a_control_is_a_typed_501() {
        let state = ConnectionState::new(
            ConnectionInfo::new("vm-a._ozma._udp.local.", "vm", 7382, 7384, 7331, SINK_QMP),
            CompatStats::default(),
        );
        let (status, body) = post_json(
            &display_router(state),
            "/display/select",
            serde_json::json!({"index": 1}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "display.console_unsupported");
    }

    #[tokio::test]
    async fn connection_reports_hls_audio_intent_without_sample_claims() {
        let directory = codec_test_dir("hls-audio-status");
        let controller = HlsController::with_spawner(
            pending_factory(),
            HlsConfig {
                output_dir: directory.clone(),
                audio: Some(crate::hls::HlsAudioConfig::new("ozma-vm1.monitor", 128)),
                ..HlsConfig::default()
            },
            Arc::new(CodecSpawner::new(Vec::new())),
        );
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "vm-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_UINPUT,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller));
        let (status, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["hls_audio_configured"], true);
        assert_eq!(json["hls_audio_source"], "ozma-vm1.monitor");
        assert_eq!(json["hls_audio_encoder"], "aac");

        // Video-only is explicit, not an omitted/false audio claim.
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "vm-b._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_UINPUT,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller_with(
            &codec_test_dir("hls-audio-video-only"),
            Arc::new(CodecSpawner::new(Vec::new())),
        )));
        let (_, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(json["hls_audio_configured"], false);
        assert!(json["hls_audio_source"].is_null());
        assert!(json["hls_audio_encoder"].is_null());
    }

    #[tokio::test]
    async fn connection_reports_actual_capture_kind_and_source() {
        let capability = ozma_protocol::CaptureCapability {
            id: "x11".into(),
            name: "X11 display".into(),
            kind: CaptureKind::X11,
            formats: vec!["rgb24".into()],
            max_width: 1920,
            max_height: 1080,
            max_fps: 30,
            compressed: false,
            hdr: false,
            has_audio: false,
        };
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "desk-a._ozma._udp.local.",
                "desktop",
                7382,
                7384,
                7331,
                SINK_UINPUT,
            ),
            CompatStats::default(),
        )
        .with_capture(CaptureStatus::from_capability("x11", Some(&capability)));
        let (status, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["capture"]["provider"], "x11");
        assert_eq!(json["capture"]["kind"], "x11");
        assert_eq!(json["capture"]["source"], "x11");
        assert_eq!(json["capture"]["name"], "X11 display");
        assert_eq!(json["capture"]["available"], true);

        // A portal stream reports the real PipeWire kind and selected source id,
        // while the provider remains `portal`.
        let portal = ozma_protocol::CaptureCapability {
            kind: CaptureKind::PipeWire,
            id: "portal0".into(),
            name: "Desktop portal screen cast".into(),
            ..capability.clone()
        };
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "desk-a._ozma._udp.local.",
                "desktop",
                7382,
                7384,
                7331,
                SINK_UINPUT,
            ),
            CompatStats::default(),
        )
        .with_capture(CaptureStatus::from_capability("portal", Some(&portal)));
        let (_, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(json["capture"]["provider"], "portal");
        assert_eq!(json["capture"]["kind"], "pipewire");
        assert_eq!(json["capture"]["source"], "portal0");

        // With no enumerated source the document names no kind/source.
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "desk-a._ozma._udp.local.",
                "desktop",
                7382,
                7384,
                7331,
                SINK_UINPUT,
            ),
            CompatStats::default(),
        )
        .with_capture(CaptureStatus::from_capability("portal", None));
        let (_, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(json["capture"]["provider"], "portal");
        assert!(json["capture"]["kind"].is_null());
        assert!(json["capture"]["source"].is_null());
        assert_eq!(json["capture"]["available"], false);
    }

    #[tokio::test]
    async fn connection_reports_failed_hls_with_an_error_string() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls(Some(HlsStatus::failed("no capture source is available")));
        let app = connection_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connection")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["hls"], "failed");
        assert_eq!(json["hls_error"], "no capture source is available");
        assert_eq!(json["webrtc"], "not-configured");
    }

    #[tokio::test]
    async fn connection_reports_a_failed_webrtc_helper_with_an_error() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_webrtc(Some(WebRtcControl::failed(
            "helper program is missing".to_owned(),
        )));

        let (status, json) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["webrtc"], "failed");
        assert_eq!(json["webrtc_error"], "helper program is missing");
        // A failed helper is not a running stream and must not claim to be.
        assert_eq!(json["hls"], "not-configured");
    }

    #[tokio::test]
    async fn display_info_before_the_first_frame_reports_unknown_frame_geometry() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(FakeCaptureProvider::new()),
        )));
        let app = public_router(state);

        let (status, info) = get_json(&app, "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["snapshot_available"], true);
        assert_eq!(info["width"], 0);
        assert_eq!(info["height"], 0);
        assert_eq!(info["dimensions_source"], "unknown");
        assert_eq!(info["capability_max_width"], 320);
        assert_eq!(info["capability_max_height"], 240);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let (status, info) = get_json(&app, "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["width"], 320);
        assert_eq!(info["height"], 240);
        assert_eq!(info["dimensions_source"], "last-frame");
        assert_eq!(info["capability_max_width"], 320);
        assert_eq!(info["capability_max_height"], 240);
    }

    #[tokio::test]
    async fn display_snapshot_returns_a_real_jpeg_with_no_store() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(FakeCaptureProvider::new()),
        )));
        let app = public_router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/display/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.starts_with(&[0xFF, 0xD8]));
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&body));
        decoder.decode().expect("valid JPEG");
        let info = decoder.info().expect("JPEG info");
        assert_eq!((info.width, info.height), (320, 240));

        let (status, info) = get_json(&app, "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["ok"], true);
        assert_eq!(info["source_id"], "fake0");
        assert_eq!(info["width"], 320);
        assert_eq!(info["height"], 240);
        assert_eq!(info["pixel_format"], "rgb24");
        assert_eq!(info["snapshot_available"], true);
        assert_eq!(info["snapshot_encoding"], "jpeg");
        assert_eq!(info["dimensions_source"], "last-frame");
        assert!(info["last_error"].is_null());

        // The desktop profile's legacy /snapshot alias serves the same JPEG.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.starts_with(&[0xFF, 0xD8]));
    }

    #[tokio::test]
    async fn display_snapshot_without_a_source_is_a_typed_503() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(CaptureRegistry::new())));
        let app = public_router(state);

        let (status, body) = get_json(&app, "/display/snapshot").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "capture.unavailable");
        assert!(body["remediation"]
            .as_str()
            .unwrap()
            .contains("--qmp-socket"));

        let (status, info) = get_json(&app, "/display/info").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["snapshot_available"], false);
        assert_eq!(info["dimensions_source"], "none");
    }

    #[tokio::test]
    async fn display_snapshot_rejects_an_unsupported_frame_format() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(CaptureRegistry::new().with(
            FakeCaptureProvider::new().with_format(FrameFormat::new(PixelFormat::H264, 4, 4)),
        ))));
        let (status, body) = get_json(&public_router(state), "/display/snapshot").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "display.unsupported_format");
        assert!(body["error"].as_str().unwrap().contains("h264"));
        assert!(body["remediation"].as_str().unwrap().contains("yuyv"));
    }

    #[tokio::test]
    async fn display_snapshot_encodes_yuyv_and_nv12_sources() {
        for format in [
            FrameFormat::new(PixelFormat::Yuyv, 16, 8),
            FrameFormat::new(PixelFormat::Nv12, 16, 8),
        ] {
            let state = ConnectionState::new(
                ConnectionInfo::new(
                    "node-a._ozma._udp.local.",
                    "hardware",
                    7382,
                    7384,
                    7331,
                    SINK_FAKE_RECORDING,
                ),
                CompatStats::default(),
            )
            .with_snapshot(Some(SnapshotControl::new(
                CaptureRegistry::new().with(FakeCaptureProvider::new().with_format(format)),
            )));
            let app = public_router(state);
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/display/snapshot")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{format:?}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(body.starts_with(&[0xFF, 0xD8]), "{format:?}");
            let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&body));
            decoder.decode().expect("valid JPEG");
            let info = decoder.info().expect("JPEG info");
            assert_eq!((info.width, info.height), (16, 8), "{format:?}");

            let (status, info) = get_json(&app, "/display/info").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(info["pixel_format"], format.pixel_format.as_str());
            assert_eq!(info["dimensions_source"], "last-frame");
        }
    }

    fn video_state(streams: Option<VideoStreams>) -> ConnectionState {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(FakeCaptureProvider::new()),
        )));
        match streams {
            Some(streams) => state.with_video_streams(streams),
            None => state,
        }
    }

    async fn wait_for_video_idle(streams: &VideoStreams) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while streams.active() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "stream slot was not released after disconnect"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn encoded_video_state(
        format: Option<FrameFormat>,
        streams: Option<VideoStreams>,
        spawner: Arc<RecordingMpegTsSpawner>,
        probe: Arc<dyn EncoderProbe>,
        encoder: HlsEncoder,
    ) -> ConnectionState {
        let provider = match format {
            Some(format) => FakeCaptureProvider::new().with_format(format),
            None => FakeCaptureProvider::new(),
        };
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            CaptureRegistry::new().with(provider),
        )))
        .with_video_encoding(Some(VideoEncoding::with_components(
            HlsConfig {
                fps: 10,
                bitrate_kbps: 2_000,
                video_encoder: encoder,
                ..HlsConfig::default()
            },
            probe,
            spawner,
        )));
        match streams {
            Some(streams) => state.with_video_streams(streams),
            None => state,
        }
    }

    #[tokio::test]
    async fn video_stream_has_multipart_headers_and_real_changing_frames() {
        let app = public_router(video_state(None));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/video?fps=30&quality=60")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "multipart/x-mixed-replace; boundary=ozma-frame"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-ozma-fps"], "30");
        assert_eq!(response.headers()["x-ozma-quality"], "60");

        let (frames, _) = read_mjpeg_frames(response, 3).await;
        assert!(frames.len() >= 3, "expected at least three frames");
        for frame in &frames {
            assert!(frame.starts_with(&[0xFF, 0xD8]), "not a JPEG frame");
            assert_eq!(jpeg_size(frame), (320, 240));
        }
        assert_ne!(frames[0], frames[1], "frames must change over time");
    }

    #[tokio::test]
    async fn video_stream_honours_fps_bound_and_stops_on_disconnect() {
        let streams = VideoStreams::new(1);
        let state = video_state(Some(streams.clone()));
        let app = public_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/video?fps=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(streams.active(), 1);

        let (frames, elapsed) = read_mjpeg_frames(response, 3).await;
        assert!(frames.len() >= 3);
        // First frame is immediate; the next two are at 500 ms intervals.
        assert!(
            elapsed >= Duration::from_millis(700),
            "fps=2 stream produced frames too quickly: {elapsed:?}"
        );
        wait_for_video_idle(&streams).await;
    }

    #[tokio::test]
    async fn video_stream_caps_concurrent_clients_and_recovers_after_disconnect() {
        let streams = VideoStreams::new(1);
        let state = video_state(Some(streams.clone()));
        let app = public_router(state);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/video?fps=30")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(streams.active(), 1);

        let (status, body) = get_json(&app, "/video?fps=30").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "video.stream_limit");
        assert!(body["remediation"]
            .as_str()
            .unwrap()
            .contains("/display/snapshot"));

        drop(first);
        wait_for_video_idle(&streams).await;

        let after = app
            .oneshot(
                Request::builder()
                    .uri("/video?fps=30")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(after.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn video_stream_reports_unavailable_sources_and_bad_queries_typed() {
        let no_source = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(CaptureRegistry::new())));
        let (status, body) = get_json(&public_router(no_source), "/video").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "capture.unavailable");

        let not_configured = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = get_json(&public_router(not_configured), "/video").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "capture.unavailable");

        let (status, body) = get_json(&public_router(video_state(None)), "/video?fps=fast").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "video.invalid_query");
        assert!(body["error"].as_str().unwrap().contains("fps"));
    }

    #[tokio::test]
    async fn video_h264_and_hevc_are_mpegts_with_a_real_first_keyframe() {
        for (codec, encoder, fixture, expected_encoder) in [
            ("h264", HlsEncoder::H264, HlsEncoder::H264, "libx264"),
            ("hevc", HlsEncoder::Hevc, HlsEncoder::Hevc, "libx265"),
        ] {
            let spawner = Arc::new(RecordingMpegTsSpawner::new(fixture, true));
            let state = encoded_video_state(
                None,
                None,
                spawner.clone(),
                Arc::new(StaticEncoderProbe::software()),
                encoder,
            );
            let response = public_router(state)
                .oneshot(
                    Request::builder()
                        .uri(format!("/video?codec={codec}&fps=10&bitrate=2000"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{codec}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "video/mp2t",
                "{codec}"
            );
            assert_eq!(response.headers()["x-ozma-codec"], codec);
            assert_eq!(response.headers()["x-ozma-encoder"], expected_encoder);
            assert_eq!(response.headers()["x-ozma-stream-copy"], "false");
            assert_eq!(response.headers()["x-ozma-fps"], "10");
            assert_eq!(response.headers()["x-ozma-bitrate"], "2000");

            let body = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect())
                .await
                .expect("body ends when the fake ffmpeg stream ends")
                .expect("body")
                .to_bytes();
            assert_eq!(body[0], 0x47, "{codec} MPEG-TS sync byte");
            assert_eq!(body.len() % 188, 0, "{codec} whole TS packets");
            assert!(
                spawner
                    .argv()
                    .windows(2)
                    .any(|pair| pair == ["-f", "mpegts"]),
                "{codec} argv must use the mpegts muxer"
            );
        }
    }

    #[tokio::test]
    async fn video_av1_is_fragmented_mp4_with_init_and_a_real_sync_fragment() {
        let spawner = Arc::new(RecordingMpegTsSpawner::fmp4(true));
        let state = encoded_video_state(
            None,
            None,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::software()),
            HlsEncoder::Av1Svt,
        );
        let response = public_router(state)
            .oneshot(
                Request::builder()
                    .uri("/video?codec=av1&fps=10&bitrate=2000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "video/mp4",
            "AV1 must use fragmented MP4, not MPEG-TS"
        );
        assert_eq!(response.headers()["x-ozma-codec"], "av1");
        assert_eq!(response.headers()["x-ozma-container"], "fmp4");
        assert_eq!(response.headers()["x-ozma-encoder"], "libsvtav1");
        assert_eq!(response.headers()["x-ozma-stream-copy"], "false");
        let body = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect())
            .await
            .expect("body ends when the fake fMP4 stream ends")
            .expect("body")
            .to_bytes();
        assert_eq!(&body[4..8], b"ftyp", "init segment starts with ftyp");
        for kind in [b"moov", b"moof", b"mdat"] {
            assert!(
                body.windows(4).any(|window| window == kind),
                "fMP4 body is missing {}",
                String::from_utf8_lossy(kind)
            );
        }
        let argv = spawner.argv();
        assert!(argv.windows(2).any(|pair| pair == ["-f", "mp4"]));
        assert!(argv.windows(2).any(|pair| pair == ["-c:v", "libsvtav1"]));
        assert!(!argv.windows(2).any(|pair| pair == ["-f", "mpegts"]));
    }

    #[tokio::test]
    async fn video_av1_copies_an_av1_source_without_an_encoder() {
        let spawner = Arc::new(RecordingMpegTsSpawner::fmp4(true));
        let state = encoded_video_state(
            Some(FrameFormat::new(PixelFormat::Av1, 320, 240)),
            None,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::new(EncoderAvailability::default())),
            HlsEncoder::Av1Svt,
        );
        let response = public_router(state)
            .oneshot(
                Request::builder()
                    .uri("/video?codec=av1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-ozma-stream-copy"], "true");
        assert_eq!(response.headers()["x-ozma-encoder"], "copy");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        let argv = spawner.argv();
        assert!(argv.windows(2).any(|pair| pair == ["-f", "obu"]));
        assert!(argv.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        let _ = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect()).await;
    }

    #[tokio::test]
    async fn video_av1_without_an_encoder_is_a_typed_refusal() {
        let spawner = Arc::new(RecordingMpegTsSpawner::fmp4(true));
        let state = encoded_video_state(
            None,
            None,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::new(EncoderAvailability::default())),
            HlsEncoder::Av1Svt,
        );
        let (status, body) = get_json(&public_router(state), "/video?codec=av1").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "video.encoder_unavailable");
        assert_eq!(
            spawner.spawns.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no ffmpeg should start when no AV1 encoder is advertised"
        );
    }

    #[tokio::test]
    async fn connection_advertises_av1_only_when_the_encoder_was_probed() {
        let spawner = Arc::new(RecordingMpegTsSpawner::fmp4(true));
        let with_av1 = encoded_video_state(
            None,
            None,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::with_encoders([HlsEncoder::Av1Aom])),
            HlsEncoder::Av1Svt,
        );
        let (status, body) = get_json(&connection_router(with_av1), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["video"]["codecs"],
            serde_json::json!(["mjpeg", "h264", "hevc", "av1"])
        );

        let without_av1 = encoded_video_state(
            None,
            None,
            spawner,
            Arc::new(StaticEncoderProbe::with_encoders([HlsEncoder::H264])),
            HlsEncoder::H264,
        );
        let (status, body) = get_json(&connection_router(without_av1), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["video"]["codecs"],
            serde_json::json!(["mjpeg", "h264", "hevc"])
        );
    }

    #[tokio::test]
    async fn video_encoded_selection_uses_compressed_passthrough_or_reencode() {
        let h264_format = FrameFormat::new(PixelFormat::H264, 320, 240);

        let copy_spawner = Arc::new(RecordingMpegTsSpawner::new(HlsEncoder::H264, true));
        let copy_state = encoded_video_state(
            Some(h264_format),
            None,
            copy_spawner.clone(),
            Arc::new(StaticEncoderProbe::software()),
            HlsEncoder::H264,
        );
        let response = public_router(copy_state)
            .oneshot(
                Request::builder()
                    .uri("/video?codec=h264")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-ozma-stream-copy"], "true");
        assert_eq!(response.headers()["x-ozma-encoder"], "copy");
        assert_eq!(response.headers()["x-ozma-codec"], "h264");
        let argv = copy_spawner.argv();
        assert!(argv.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(argv.windows(2).any(|pair| pair == ["-f", "h264"]));
        let _ = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect()).await;

        let reencode_spawner = Arc::new(RecordingMpegTsSpawner::new(HlsEncoder::Hevc, true));
        let reencode_state = encoded_video_state(
            Some(h264_format),
            None,
            reencode_spawner.clone(),
            Arc::new(StaticEncoderProbe::software()),
            HlsEncoder::Hevc,
        );
        let response = public_router(reencode_state)
            .oneshot(
                Request::builder()
                    .uri("/video?codec=hevc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-ozma-stream-copy"], "false");
        assert_eq!(response.headers()["x-ozma-encoder"], "libx265");
        assert_eq!(response.headers()["x-ozma-codec"], "hevc");
        let argv = reencode_spawner.argv();
        assert!(argv.windows(2).any(|pair| pair == ["-f", "h264"]));
        assert!(argv.windows(2).any(|pair| pair == ["-c:v", "libx265"]));
        assert!(!argv.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        let _ = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect()).await;
    }

    #[tokio::test]
    async fn video_encoded_unavailable_and_bad_codecs_are_typed() {
        let (status, body) = get_json(&public_router(video_state(None)), "/video?codec=h264").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "video.encoder_unavailable");

        let spawner = Arc::new(RecordingMpegTsSpawner::new(HlsEncoder::H264, true));
        let state = encoded_video_state(
            None,
            None,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::new(EncoderAvailability::default())),
            HlsEncoder::H264,
        );
        let (status, body) = get_json(&public_router(state), "/video?codec=h264").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "video.encoder_unavailable");
        assert_eq!(
            spawner.spawns.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no ffmpeg should start when the encoder is unavailable"
        );

        let (status, body) = get_json(&public_router(video_state(None)), "/video?codec=vp9").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "video.invalid_query");
        assert!(body["error"].as_str().unwrap().contains("vp9"));
    }

    #[tokio::test]
    async fn video_encoded_disconnect_releases_slot_and_stops_ffmpeg() {
        let streams = VideoStreams::new(1);
        let spawner = Arc::new(
            RecordingMpegTsSpawner::with_data(mpegts_repeated(HlsEncoder::H264, true, 200))
                .with_chunk(188),
        );
        let state = encoded_video_state(
            None,
            Some(streams.clone()),
            spawner.clone(),
            Arc::new(StaticEncoderProbe::software()),
            HlsEncoder::H264,
        );
        let response = public_router(state)
            .oneshot(
                Request::builder()
                    .uri("/video?codec=h264")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(streams.active(), 1);
        assert_eq!(streams.active_encoded(), 1);

        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .expect("first chunk before timeout")
            .expect("chunk")
            .expect("infallible");
        assert_eq!(first[0], 0x47);
        drop(body);
        wait_for_video_idle(&streams).await;
        assert_eq!(streams.active_encoded(), 0);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !spawner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "ffmpeg stdin was not closed after disconnect"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn video_mjpeg_default_reports_its_codec_and_leaves_the_stream_unchanged() {
        let response = public_router(video_state(None))
            .oneshot(
                Request::builder()
                    .uri("/video?fps=10&quality=60")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-ozma-codec"], "mjpeg");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "multipart/x-mixed-replace; boundary=ozma-frame"
        );
    }

    struct VideoConsoleFactory;

    impl crate::display_control::ConsoleSourceFactory for VideoConsoleFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Dbus)
        }

        fn default_console(&self) -> u32 {
            0
        }

        fn supports_console_switch(&self) -> bool {
            true
        }

        fn registry(&self, console: u32) -> Result<CaptureRegistry, String> {
            let (width, height) = if console == 0 { (32, 24) } else { (64, 48) };
            Ok(CaptureRegistry::new().with(
                FakeCaptureProvider::new()
                    .with_identity(format!("console-{console}"), format!("Console_{console}"))
                    .with_format(FrameFormat::rgb24(width, height)),
            ))
        }

        fn selection(
            &self,
            console: u32,
        ) -> Result<crate::display_outputs::DisplayOutputSelection, String> {
            crate::display_outputs::outputs_from_specs(
                &[
                    "0:Console_0:1920x1080:primary:dbus".to_owned(),
                    "1:Console_1:1280x1024:dbus".to_owned(),
                ],
                ozma_display::vm::VmConsoleBackend::Dbus,
                Some("dbus0"),
                Some(console),
            )
        }
    }

    fn video_console_state() -> ConnectionState {
        let factory = Arc::new(VideoConsoleFactory);
        let slot = Arc::new(std::sync::RwLock::new(
            factory.registry(0).expect("active registry"),
        ));
        let control =
            DisplayControl::new(factory.clone(), slot, Vec::new()).expect("display control");
        let broker = crate::display_broker::ConsoleBroker::new(
            factory.clone(),
            crate::display_broker::ConsoleBrokerConfig::default(),
        );
        ConnectionState::new(
            ConnectionInfo::new(
                "vm-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_snapshot(Some(SnapshotControl::new(
            factory.registry(0).expect("active registry"),
        )))
        .with_display_control(Some(control))
        .with_console_broker(Some(broker))
    }

    #[tokio::test]
    async fn video_stream_honours_console_selection() {
        let state = video_console_state();
        let app = public_router(state);

        let non_active = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/video?console=Console_1&fps=30")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(non_active.status(), StatusCode::OK);
        let (frames, _) = read_mjpeg_frames(non_active, 2).await;
        assert!(frames.len() >= 2);
        assert_eq!(jpeg_size(&frames[0]), (64, 48));

        let active = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/video?console=0&fps=30")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(active.status(), StatusCode::OK);
        let (frames, _) = read_mjpeg_frames(active, 2).await;
        assert!(frames.len() >= 2);
        assert_eq!(jpeg_size(&frames[0]), (32, 24));

        let (status, body) = get_json(&app, "/video?console=Console_9").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "not_found");
    }

    #[tokio::test]
    async fn connection_reports_video_availability_without_claiming_a_stream() {
        let streams = VideoStreams::new(2);
        let state = video_state(Some(streams.clone()));
        let connection_app = connection_router(state.clone());
        let public_app = public_router(state);

        let (status, idle) = get_json(&connection_app, "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(idle["video"]["supported"], true);
        assert_eq!(idle["video"]["available"], true);
        assert_eq!(idle["video"]["route"], "/video");
        assert_eq!(idle["video"]["alias"], "/display/mjpeg");
        assert_eq!(idle["video"]["active_streams"], 0);
        assert_eq!(idle["video"]["max_streams"], 2);
        assert_eq!(idle["video"]["encoding"], "jpeg");

        let response = public_app
            .oneshot(
                Request::builder()
                    .uri("/display/mjpeg?fps=30")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (_, live) = get_json(&connection_app, "/api/v1/connection").await;
        assert_eq!(live["video"]["active_streams"], 1);
        drop(response);
        wait_for_video_idle(&streams).await;
    }

    #[tokio::test]
    async fn node_status_aliases_the_connection_summary() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let app = public_router(state);
        let (status, body) = get_json(&app, "/node/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["node_id"], "node-a._ozma._udp.local.");
        assert_eq!(body["api_port"], 7382);
        assert_eq!(body["hid_udp_port"], 7331);
        assert_eq!(body["hls"], "not-configured");
        assert_eq!(body["host_surfaces"]["storage"]["supported"], true);
        assert_eq!(body["host_surfaces"]["storage"]["path"], "/storage");
        assert_eq!(body["host_surfaces"]["storage"]["guarded"], true);
        assert_eq!(body["host_surfaces"]["apps"]["limit"], 100);
        assert_eq!(body["host_surfaces"]["self"]["path"], "/self");

        // The desktop profile's legacy /status alias is the same document.
        let (status, alias) = get_json(&app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(alias["ok"], true);
        assert_eq!(alias["node_id"], body["node_id"]);
        assert_eq!(alias["profile"], body["profile"]);
    }

    #[tokio::test]
    async fn ui_hints_returns_the_minimal_empty_shape() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = get_json(&public_router(state), "/ui/hints?level=3").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["available"], false);
        assert_eq!(body["level"], 3);
        assert_eq!(body["windows"], serde_json::json!([]));
        assert!(body["focused_window"].is_null());
        assert_eq!(body["controls"], serde_json::json!([]));
        assert!(body["focused_control"].is_null());
    }

    #[tokio::test]
    async fn connection_advertises_configured_bluetooth_without_probing() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_bluetooth(Some(crate::bluetooth::BluetoothService::new(
            crate::bluetooth::BluetoothConfig {
                adapter: Some("80:84:89:F6:E2:E8".to_owned()),
                ..crate::bluetooth::BluetoothConfig::default()
            },
        )));
        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);

        assert_eq!(connection["bluetooth"]["configured"], true);
        assert_eq!(connection["bluetooth"]["adapter"], "80:84:89:F6:E2:E8");
        assert_eq!(
            connection["bluetooth"]["max_scan_seconds"],
            crate::bluetooth::MAX_SCAN_SECONDS
        );

        let advertised = connection["controller_surfaces"]
            .as_array()
            .expect("controller_surfaces must be an array");
        for capability in [
            "bluetooth.status",
            "bluetooth.devices",
            "bluetooth.scan",
            "bluetooth.pair",
            "bluetooth.connect",
            "bluetooth.disconnect",
            "bluetooth.forget",
        ] {
            let entry = advertised
                .iter()
                .find(|entry| entry["capability"] == capability)
                .unwrap_or_else(|| panic!("missing advertised capability {capability}"));
            assert_eq!(entry["supported"], true);
        }
        assert!(
            !advertised
                .iter()
                .any(|entry| entry["capability"] == "bluetooth"
                    && entry["surface"] == "/bluetooth/*"),
            "configured Bluetooth must not also advertise the unconfigured surface"
        );
    }

    #[tokio::test]
    async fn connection_advertises_configured_serial_without_opening_it() {
        let config = crate::serial::SerialConfig::new(
            "/dev/fake-serial",
            115_200,
            8,
            crate::serial::SerialParity::None,
            1,
            Duration::from_millis(100),
        )
        .expect("valid serial config");
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_serial(Some(crate::serial::SerialService::new(config)));
        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);

        let advertised = connection["controller_surfaces"]
            .as_array()
            .expect("controller_surfaces must be an array");
        for capability in [
            "serial.status",
            "serial.write",
            "serial.read",
            "serial.stream",
        ] {
            let entry = advertised
                .iter()
                .find(|entry| entry["capability"] == capability)
                .unwrap_or_else(|| panic!("missing advertised capability {capability}"));
            assert_eq!(entry["supported"], true);
        }
        let residual = advertised
            .iter()
            .find(|entry| entry["capability"] == "serial" && entry["surface"] == "/serial/*")
            .expect("residual serial sub-surfaces stay advertised as unsupported");
        assert_eq!(residual["supported"], false);
    }

    #[tokio::test]
    async fn legacy_routes_are_typed_501s_with_remediation() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let app = compat_router(state);

        for (uri, method, expected) in [
            ("/usb", "GET", "usb.unsupported"),
            ("/rgb/state", "GET", "rgb.unsupported"),
            ("/rgb/effect", "POST", "rgb.unsupported"),
            ("/rgb/off", "POST", "rgb.unsupported"),
            ("/current", "GET", "power.current_unsupported"),
            ("/current/history", "GET", "power.current_unsupported"),
            ("/session/init", "POST", "session.init_unsupported"),
            ("/serial/buffer?lines=50", "GET", "serial.unsupported"),
            ("/api/v1/rpa/screen", "GET", "rpa.unsupported"),
            ("/api/v1/rpa/script", "POST", "rpa.unsupported"),
            ("/api/v1/rpa/enter_bios", "POST", "rpa.unsupported"),
            ("/backup/apps", "GET", "backup.unsupported"),
            ("/backup/config", "GET", "backup.unsupported"),
            ("/backup/apps/restore", "POST", "backup.unsupported"),
        ] {
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
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{uri}");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["ok"], false, "{uri}");
            assert_eq!(body["code"], expected, "{uri}");
            assert!(body["error"].is_string(), "{uri}");
            assert!(body["remediation"].is_string(), "{uri}");
        }
    }

    #[tokio::test]
    async fn backup_core_is_typed_not_configured_without_a_repo() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let app = compat_router(state);

        let (status, body) = get_json(&app, "/backup/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["supported"], true);
        assert_eq!(body["configured"], false);
        assert_eq!(body["repo"]["state"], "unconfigured");
        assert!(body["last_snapshot"].is_null());

        for (uri, method) in [
            ("/backup/init", "POST"),
            ("/backup/run", "POST"),
            ("/backup/snapshots", "GET"),
            ("/backup/restore", "POST"),
            ("/backup/snapshots/0123456789abcdef", "DELETE"),
            ("/backup/stats", "GET"),
            ("/backup/verify", "POST"),
            ("/backup/estimate", "POST"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], "backup.not-configured", "{uri}");
            assert_eq!(body["supported"], true, "{uri}");
            assert!(
                !body
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("restic_password"),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn controller_only_surfaces_are_advertised_and_return_typed_501s() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, connection) =
            get_json(&connection_router(state.clone()), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);

        let advertised = connection["controller_surfaces"]
            .as_array()
            .expect("controller_surfaces must be an array");
        let implemented = [
            ("/api/v1/rpa/type", "rpa.type"),
            ("/api/v1/rpa/key", "rpa.key"),
            ("/api/v1/rpa/screenshot", "rpa.screenshot"),
            ("/api/v1/rpa/ocr", "rpa.ocr"),
            ("/backup/status", "backup.status"),
            ("/backup/init", "backup.init"),
            ("/backup/run", "backup.run"),
            ("/backup/snapshots", "backup.snapshots"),
            ("/backup/restore", "backup.restore"),
            ("/backup/snapshots/{id}", "backup.delete"),
            ("/backup/stats", "backup.stats"),
            ("/backup/verify", "backup.verify"),
            ("/backup/estimate", "backup.estimate"),
        ];
        let unsupported = [
            ("/api/v1/rpa/*", "rpa"),
            ("/backup/*", "backup"),
            ("/ui/windows", "ui_windows"),
            ("/serial/*", "serial"),
            ("/rgb/*", "rgb"),
            ("/current", "current"),
            ("/bluetooth/*", "bluetooth"),
        ];
        assert_eq!(advertised.len(), implemented.len() + unsupported.len());
        for (surface, capability) in implemented {
            let entry = advertised
                .iter()
                .find(|entry| entry["capability"] == capability)
                .unwrap_or_else(|| panic!("missing advertised capability {capability}"));
            assert_eq!(entry["surface"], surface);
            assert_eq!(entry["supported"], true);
            assert!(entry["reason"].is_string());
            let reference = entry["python_reference"]
                .as_str()
                .expect("python_reference is a string");
            assert!(
                reference.contains("node/")
                    || reference.contains("agent/")
                    || reference.contains("controller/")
            );
        }
        for (surface, capability) in unsupported {
            let entry = advertised
                .iter()
                .find(|entry| entry["capability"] == capability)
                .unwrap_or_else(|| panic!("missing advertised capability {capability}"));
            assert_eq!(entry["surface"], surface);
            assert_eq!(entry["supported"], false);
            assert!(entry["reason"].is_string());
            let reference = entry["python_reference"]
                .as_str()
                .expect("python_reference is a string");
            assert!(
                reference.contains("node/")
                    || reference.contains("agent/")
                    || reference.contains("controller/")
            );
        }

        let app = compat_router(state.clone()).merge(public_router(state));
        for (uri, method, capability) in [
            ("/api/v1/rpa/screen", "GET", "rpa"),
            ("/api/v1/rpa/script", "POST", "rpa"),
            ("/api/v1/rpa/enter_bios", "POST", "rpa"),
            ("/api/v1/rpa/set_boot_usb", "POST", "rpa"),
            ("/api/v1/rpa/wait_for_text", "POST", "rpa"),
            ("/api/v1/rpa/click", "POST", "rpa"),
            ("/backup/apps", "GET", "backup"),
            ("/backup/config", "GET", "backup"),
            ("/backup/apps/restore", "POST", "backup"),
            ("/ui/windows", "GET", "ui_windows"),
            ("/serial/state", "GET", "serial"),
            ("/rgb/state", "GET", "rgb"),
            ("/current", "GET", "current"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{uri}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/json",
                "{uri}"
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["ok"], false, "{uri}");
            assert_eq!(body["supported"], false, "{uri}");
            assert_eq!(body["capability"], capability, "{uri}");
            assert!(body["surface"].is_string(), "{uri}");
            assert!(body["reason"].is_string(), "{uri}");
            assert!(body["python_reference"].is_string(), "{uri}");
        }
    }

    #[tokio::test]
    async fn rpa_type_route_dispatches_layout_aware_events_through_the_sink() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = rpa_router(state);

        let (status, body) =
            post_json(&app, "/api/v1/rpa/type", serde_json::json!({"text": "Hi!"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 10);
        assert_eq!(body["sink"], SINK_FAKE_RECORDING);
        assert_eq!(body["injected"], false);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE1, true),
                key(0x0B, true),
                key(0x0B, false),
                key(0xE1, false),
                key(0x0C, true),
                key(0x0C, false),
                key(0xE1, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE1, false),
            ]
        );
        assert!(recording.held().is_empty(), "every key must be released");

        let no_sink = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = post_json(
            &rpa_router(no_sink),
            "/api/v1/rpa/type",
            serde_json::json!({"text": "a"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "input-unavailable");
    }

    #[tokio::test]
    async fn rpa_key_route_supports_named_keys_characters_and_combos() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = rpa_router(state);

        let (status, body) =
            post_json(&app, "/api/v1/rpa/key", serde_json::json!({"key": "enter"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 2);
        assert_eq!(body["injected"], false);
        assert_eq!(
            recording.take_events(),
            vec![key(0x28, true), key(0x28, false)]
        );

        // Controller hotkey form: modifiers followed by one final key.
        let (status, body) = post_json(
            &app,
            "/api/v1/rpa/key",
            serde_json::json!({"keys": ["ctrl", "c"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 4);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE0, true),
                key(0x06, true),
                key(0x06, false),
                key(0xE0, false),
            ]
        );

        // A printable character is resolved through the selected layout.
        let (status, body) =
            post_json(&app, "/api/v1/rpa/key", serde_json::json!({"key": "?"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 4);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE1, true),
                key(0x38, true),
                key(0x38, false),
                key(0xE1, false),
            ]
        );

        // The Python node's integer modifier bitmask is also accepted.
        let (status, body) = post_json(
            &app,
            "/api/v1/rpa/key",
            serde_json::json!({"key": "c", "modifier": 2}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 4);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE1, true),
                key(0x06, true),
                key(0x06, false),
                key(0xE1, false),
            ]
        );

        let (status, body) = post_json(
            &app,
            "/api/v1/rpa/key",
            serde_json::json!({"key": "not-a-key"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unknown-key");
        assert!(recording.events().is_empty());

        let no_sink = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = post_json(
            &rpa_router(no_sink),
            "/api/v1/rpa/key",
            serde_json::json!({"key": "enter"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "input-unavailable");
    }

    #[tokio::test]
    async fn rpa_screenshot_returns_real_jpeg_and_honours_console_selection() {
        let app = rpa_router(video_console_state());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/rpa/screenshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "must be a real JPEG");
        assert_eq!(jpeg_size(&bytes), (32, 24));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/rpa/screenshot?console=Console_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(jpeg_size(&bytes), (64, 48));

        // No configured source stays a typed error, never a fabricated image.
        let no_source = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = get_json(&rpa_router(no_source), "/api/v1/rpa/screenshot").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "capture.unavailable");
    }

    #[tokio::test]
    async fn rpa_ocr_without_tesseract_is_a_typed_503() {
        let state = video_state(None).with_ocr(OcrControl::unavailable());
        let (status, body) =
            post_json(&rpa_router(state), "/api/v1/rpa/ocr", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "rpa.ocr-unavailable");
        assert!(body["error"].as_str().unwrap().contains("tesseract"));
        assert!(body["remediation"].is_string());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpa_ocr_returns_exactly_the_engine_text() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("fake-tesseract");
        std::fs::write(
            &program,
            "#!/bin/sh\ncat >/dev/null\nprintf 'HELLO 123\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let state = video_state(None).with_ocr(OcrControl::with_program(program));
        let (status, body) =
            post_json(&rpa_router(state), "/api/v1/rpa/ocr", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["available"], true);
        assert_eq!(body["engine"], "tesseract");
        assert_eq!(body["text"], "HELLO 123");
        assert_eq!(body["full_text"], "HELLO 123");
        assert_eq!(body["width"], 320);
        assert_eq!(body["height"], 240);
        assert_eq!(body["source_id"], "fake0");
        assert_eq!(body["text_regions"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn rpa_ocr_runs_installed_tesseract_over_a_real_frame() {
        let state = video_state(None);
        if !state.ocr.is_available() {
            eprintln!("skipping: tesseract is not installed");
            return;
        }

        let (status, body) =
            post_json(&rpa_router(state), "/api/v1/rpa/ocr", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["engine"], "tesseract");
        assert!(body["text"].is_string());
        assert_eq!(body["width"], 320);
        assert_eq!(body["height"], 240);
    }

    #[tokio::test]
    async fn connection_reports_webrtc_audio_configuration_without_claiming_samples() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_webrtc(Some(WebRtcControl::failed_with_audio(
            "helper did not start".to_owned(),
            Some("ozma-vm1.monitor".to_owned()),
        )));
        let (status, body) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["webrtc"], "failed");
        assert_eq!(body["webrtc_audio_configured"], true);
        assert_eq!(body["webrtc_audio_source"], "ozma-vm1.monitor");
        // Audio configuration is not a claim that the helper produced samples.
        assert_eq!(body["webrtc"], "failed");
        assert_eq!(body["webrtc_backend"], "external");
    }

    #[tokio::test]
    async fn connection_reports_native_backend_audio_intent() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_webrtc(Some(WebRtcControl::failed_with_audio_and_backend(
            "native helper did not start".to_owned(),
            Some("ozma-vm1.monitor".to_owned()),
            crate::webrtc::WebRtcBackend::Native,
        )));
        let (status, body) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["webrtc"], "failed");
        assert_eq!(body["webrtc_backend"], "native");
        assert_eq!(body["webrtc_audio_configured"], true);
        assert_eq!(body["webrtc_audio_source"], "ozma-vm1.monitor");
    }

    #[tokio::test]
    async fn webrtc_routes_answer_typed_503_without_a_helper() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let app = webrtc_router(state);

        let (status, body) = post_json(
            &app,
            "/webrtc/offer",
            serde_json::json!({"sdp": "v=0", "type": "offer"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "webrtc-not-configured");

        let (status, body) = post_json(
            &app,
            "/webrtc/bitrate",
            serde_json::json!({"bitrate": 4_000_000}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "webrtc-not-configured");

        let (status, body) = post_json(
            &app,
            "/webrtc/candidate",
            serde_json::json!({
                "session": "sess-1",
                "candidate": "candidate:1 1 UDP 1 192.0.2.1 5000 typ host",
                "sdpMLineIndex": 0
            }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "webrtc-not-configured");

        let (status, body) = post_json(
            &app,
            "/webrtc/candidate",
            serde_json::json!({"session": "sess-1", "candidate": "  "}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-candidate");
    }

    #[tokio::test]
    async fn webrtc_offer_rejects_malformed_requests() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let app = webrtc_router(state);

        let (status, body) = post_json(
            &app,
            "/webrtc/offer",
            serde_json::json!({"sdp": "", "type": "offer"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-sdp");

        let (status, body) = post_json(
            &app,
            "/webrtc/offer",
            serde_json::json!({"sdp": "v=0", "type": "answer"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-offer-type");

        let (status, body) = post_json(&app, "/webrtc/offer", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-json");
    }

    #[tokio::test]
    async fn connection_endpoint_reports_power_when_available() {
        let state = vm_state(Some(FakePower::running().shared()));
        let response = connection_router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/connection")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["sink_kind"], "qmp");
        assert_eq!(json["power"]["status"], "running");
        assert_eq!(json["power"]["available"], true);
    }

    #[tokio::test]
    async fn power_state_reports_backend_status() {
        let app = power_router(vm_state(Some(FakePower::running().shared())));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["status"], "running");
        assert_eq!(json["available"], true);
    }

    #[tokio::test]
    async fn power_state_is_unavailable_without_a_backend() {
        let app = power_router(vm_state(None));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["status"], "not-configured");
        assert_eq!(json["available"], false);
    }

    #[tokio::test]
    async fn power_actions_accept_canonical_names_and_aliases() {
        for (path, canonical) in [
            ("/power/on", "on"),
            ("/power/start", "on"),
            ("/power/off", "off"),
            ("/power/shutdown", "off"),
            ("/power/reset", "reset"),
            ("/power/pause", "pause"),
            ("/power/suspend", "pause"),
            ("/power/resume", "resume"),
            ("/power/force-off", "force-off"),
            ("/power/stop", "force-off"),
        ] {
            let app = power_router(vm_state(Some(FakePower::running().shared())));
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["ok"], true, "{path}");
            assert_eq!(json["action"], canonical, "{path}");
        }
    }

    #[tokio::test]
    async fn power_actions_map_to_backend_methods() {
        for (path, expected) in [
            ("/power/on", "on"),
            ("/power/start", "on"),
            ("/power/cont", "on"),
            ("/power/off", "off"),
            ("/power/shutdown", "off"),
            ("/power/powerdown", "off"),
            ("/power/reset", "reset"),
            ("/power/pause", "pause"),
            ("/power/suspend", "pause"),
            ("/power/resume", "resume"),
            ("/power/force-off", "force-off"),
            ("/power/force_off", "force-off"),
            ("/power/stop", "force-off"),
            ("/power/destroy", "force-off"),
        ] {
            let (power, calls) = FakePower::running().shared_recording();
            let app = power_router(vm_state(Some(power)));
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["ok"], true, "{path}");
            assert_eq!(
                *lock(&calls),
                vec![expected],
                "{path} must call the {expected} backend method"
            );
        }
    }

    #[tokio::test]
    async fn power_action_reports_closed_qmp_connection_as_typed_502() {
        let power: SharedPower = Arc::new(tokio::sync::Mutex::new(
            Box::new(ClosedQmpPower) as Box<dyn PowerBackend>
        ));
        let app = power_router(vm_state(Some(Arc::clone(&power))));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let json = json_body(response).await;
        assert_eq!(json["ok"], false);
        assert_eq!(json["action"], "force-off");
        assert_eq!(json["code"], "qmp-closed");
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("connection closed"));

        // A later state read is unavailable and typed, not a fabricated state.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let json = json_body(response).await;
        assert_eq!(json["status"], "unknown");
        assert_eq!(json["available"], false);
        assert_eq!(json["code"], "qmp-closed");
    }

    #[tokio::test]
    async fn power_state_reports_a_retained_startup_failure_as_unavailable() {
        let error = PowerError::DomainNotFound {
            domain: "typo-vm".to_string(),
            stderr: "error: failed to get domain 'typo-vm'".to_string(),
        };
        let power: SharedPower = Arc::new(tokio::sync::Mutex::new(Box::new(UnavailablePower::new(
            "libvirt", &error,
        ))
            as Box<dyn PowerBackend>));
        let app = power_router(vm_state(Some(power)));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = json_body(response).await;
        assert_eq!(json["status"], "unavailable");
        assert_eq!(json["available"], false);
        assert_eq!(json["code"], "domain-not-found");
        assert!(json["error"].as_str().unwrap().contains("typo-vm"));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/off")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = json_body(response).await;
        assert_eq!(json["ok"], false);
        assert_eq!(json["code"], "domain-not-found");
        assert!(json["error"].as_str().unwrap().contains("typo-vm"));
    }

    #[tokio::test]
    async fn power_off_reports_a_pending_acpi_request_as_202() {
        let power: SharedPower = Arc::new(tokio::sync::Mutex::new(
            Box::new(PendingOffPower) as Box<dyn PowerBackend>
        ));
        let app = power_router(vm_state(Some(power)));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/off")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let json = json_body(response).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["action"], "off");
        assert_eq!(json["pending"], true);
        assert_eq!(json["reason"], "acpi-requested");
        assert_eq!(json["status"], "running");
    }

    #[tokio::test]
    async fn power_action_rejects_unknown_actions() {
        let app = power_router(vm_state(Some(FakePower::running().shared())));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/eject")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["ok"], false);
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("unknown power action"));
    }

    #[tokio::test]
    async fn power_action_maps_backend_errors_to_a_typed_response() {
        let app = power_router(vm_state(Some(FakePower::failing().shared())));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/on")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let json = json_body(response).await;
        assert_eq!(json["ok"], false);
        assert_eq!(json["action"], "on");
        assert!(json["error"].as_str().unwrap().contains("not connected"));
    }

    #[tokio::test]
    async fn power_action_is_unavailable_without_a_backend() {
        let app = power_router(vm_state(None));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/power/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = json_body(response).await;
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "No power backend configured");
    }

    #[tokio::test]
    async fn power_routes_are_merged_behind_the_bearer_guard() {
        let backend: Arc<dyn ozma_api::NodeBackend> =
            Arc::new(crate::backend::SessionBackend::new(
                ozma_protocol::NodeCapabilities::fake(ozma_protocol::NodeId::new("test-node")),
                ozma_auth::ConsentPolicy::for_machine(ozma_auth::MachineClass::Server),
            ));
        let app = ozma_api::router::router_with_sinks_and_extra(
            backend,
            None,
            None,
            Some("secret-token".to_owned()),
            Some(compat_router(vm_state(Some(FakePower::running().shared())))),
        );

        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/power/state")
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    fn input_state(kind: &str) -> (ConnectionState, Arc<RecordingSink<FakeInputSink>>) {
        input_state_with_layout(kind, KeyboardLayout::Us)
    }

    fn input_state_with_layout(
        kind: &str,
        layout: KeyboardLayout,
    ) -> (ConnectionState, Arc<RecordingSink<FakeInputSink>>) {
        let recording = Arc::new(RecordingSink::new(FakeInputSink::new()));
        let sink: SharedSink = recording.clone();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                kind,
            ),
            CompatStats::default(),
        )
        .with_input_sink(Some(sink))
        .with_keyboard_layout(layout);
        (state, recording)
    }

    fn absolute(x: i32, y: i32) -> InputEvent {
        InputEvent::PointerAbsolute {
            display_id: None,
            x,
            y,
        }
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post_json(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
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

    async fn delete_json(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
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

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn mjpeg_parts(body: &[u8]) -> Vec<Vec<u8>> {
        let marker = b"--ozma-frame";
        let mut frames = Vec::new();
        let mut rest = body;
        while let Some(position) = find_bytes(rest, marker) {
            rest = &rest[position + marker.len()..];
            if rest.starts_with(b"\r\n") {
                rest = &rest[2..];
            }
            let Some(headers_end) = find_bytes(rest, b"\r\n\r\n") else {
                break;
            };
            let headers = rest[..headers_end].to_ascii_lowercase();
            let Some(length_at) = find_bytes(&headers, b"content-length:") else {
                break;
            };
            let length_text = &headers[length_at + "content-length:".len()..];
            let length_end = find_bytes(length_text, b"\r\n").unwrap_or(length_text.len());
            let Ok(length) = std::str::from_utf8(&length_text[..length_end])
                .unwrap_or("")
                .trim()
                .parse::<usize>()
            else {
                break;
            };
            let body_start = headers_end + 4;
            let Some(body_end) = body_start.checked_add(length) else {
                break;
            };
            if rest.len() < body_end {
                break;
            }
            frames.push(rest[body_start..body_end].to_vec());
            rest = &rest[body_end..];
        }
        frames
    }

    /// Read multipart parts until `minimum` real JPEGs have arrived.
    async fn read_mjpeg_frames(
        response: axum::response::Response,
        minimum: usize,
    ) -> (Vec<Vec<u8>>, Duration) {
        let started = std::time::Instant::now();
        let limit = Duration::from_secs(5);
        let mut chunks = response.into_body().into_data_stream();
        let mut buffer = Vec::new();
        while started.elapsed() < limit {
            let remaining = limit.saturating_sub(started.elapsed());
            let next = tokio::time::timeout(remaining, chunks.next()).await;
            let Ok(Some(Ok(chunk))) = next else {
                break;
            };
            buffer.extend_from_slice(&chunk);
            let frames = mjpeg_parts(&buffer);
            if frames.len() >= minimum {
                return (frames, started.elapsed());
            }
        }
        (mjpeg_parts(&buffer), started.elapsed())
    }

    fn jpeg_size(jpeg: &[u8]) -> (u16, u16) {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
        decoder.decode().expect("valid JPEG frame");
        let info = decoder.info().expect("JPEG info");
        (info.width, info.height)
    }

    #[test]
    fn text_mapping_covers_ascii_letters_digits_punctuation_and_whitespace() {
        let events = |text: &str| text_to_input_events(text, KeyboardLayout::Us).unwrap();

        assert_eq!(events("a"), vec![key(0x04, true), key(0x04, false)]);
        assert_eq!(events("z"), vec![key(0x1D, true), key(0x1D, false)]);
        assert_eq!(
            events("A"),
            vec![
                key(0xE1, true),
                key(0x04, true),
                key(0x04, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(events("0"), vec![key(0x27, true), key(0x27, false)]);
        assert_eq!(events("9"), vec![key(0x26, true), key(0x26, false)]);
        assert_eq!(
            events("!"),
            vec![
                key(0xE1, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(
            events("?"),
            vec![
                key(0xE1, true),
                key(0x38, true),
                key(0x38, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(events(" "), vec![key(0x2C, true), key(0x2C, false)]);
        assert_eq!(events("\n"), vec![key(0x28, true), key(0x28, false)]);
        assert_eq!(events("\r"), vec![key(0x28, true), key(0x28, false)]);
        assert_eq!(events("\t"), vec![key(0x2B, true), key(0x2B, false)]);
        assert!(events("").is_empty());
    }

    /// Canonical US-layout HID usages for unshifted printable ASCII plus the
    /// supported whitespace controls.
    const UNSHIFTED_HID: &[(char, u8)] = &[
        ('a', 0x04),
        ('b', 0x05),
        ('c', 0x06),
        ('d', 0x07),
        ('e', 0x08),
        ('f', 0x09),
        ('g', 0x0A),
        ('h', 0x0B),
        ('i', 0x0C),
        ('j', 0x0D),
        ('k', 0x0E),
        ('l', 0x0F),
        ('m', 0x10),
        ('n', 0x11),
        ('o', 0x12),
        ('p', 0x13),
        ('q', 0x14),
        ('r', 0x15),
        ('s', 0x16),
        ('t', 0x17),
        ('u', 0x18),
        ('v', 0x19),
        ('w', 0x1A),
        ('x', 0x1B),
        ('y', 0x1C),
        ('z', 0x1D),
        ('1', 0x1E),
        ('2', 0x1F),
        ('3', 0x20),
        ('4', 0x21),
        ('5', 0x22),
        ('6', 0x23),
        ('7', 0x24),
        ('8', 0x25),
        ('9', 0x26),
        ('0', 0x27),
        ('\n', 0x28),
        ('\r', 0x28),
        ('\t', 0x2B),
        (' ', 0x2C),
        ('-', 0x2D),
        ('=', 0x2E),
        ('[', 0x2F),
        (']', 0x30),
        ('\\', 0x31),
        (';', 0x33),
        ('\'', 0x34),
        ('`', 0x35),
        (',', 0x36),
        ('.', 0x37),
        ('/', 0x38),
    ];

    /// Canonical US-layout HID usages for shifted printable ASCII.
    const SHIFTED_HID: &[(char, u8)] = &[
        ('A', 0x04),
        ('B', 0x05),
        ('C', 0x06),
        ('D', 0x07),
        ('E', 0x08),
        ('F', 0x09),
        ('G', 0x0A),
        ('H', 0x0B),
        ('I', 0x0C),
        ('J', 0x0D),
        ('K', 0x0E),
        ('L', 0x0F),
        ('M', 0x10),
        ('N', 0x11),
        ('O', 0x12),
        ('P', 0x13),
        ('Q', 0x14),
        ('R', 0x15),
        ('S', 0x16),
        ('T', 0x17),
        ('U', 0x18),
        ('V', 0x19),
        ('W', 0x1A),
        ('X', 0x1B),
        ('Y', 0x1C),
        ('Z', 0x1D),
        ('!', 0x1E),
        ('@', 0x1F),
        ('#', 0x20),
        ('$', 0x21),
        ('%', 0x22),
        ('^', 0x23),
        ('&', 0x24),
        ('*', 0x25),
        ('(', 0x26),
        (')', 0x27),
        ('_', 0x2D),
        ('+', 0x2E),
        ('{', 0x2F),
        ('}', 0x30),
        ('|', 0x31),
        (':', 0x33),
        ('"', 0x34),
        ('~', 0x35),
        ('<', 0x36),
        ('>', 0x37),
        ('?', 0x38),
    ];

    /// Unshifted/shifted pairs must use the same physical key.
    const SHIFT_PAIRS: &[(char, char)] = &[
        ('a', 'A'),
        ('b', 'B'),
        ('c', 'C'),
        ('d', 'D'),
        ('e', 'E'),
        ('f', 'F'),
        ('g', 'G'),
        ('h', 'H'),
        ('i', 'I'),
        ('j', 'J'),
        ('k', 'K'),
        ('l', 'L'),
        ('m', 'M'),
        ('n', 'N'),
        ('o', 'O'),
        ('p', 'P'),
        ('q', 'Q'),
        ('r', 'R'),
        ('s', 'S'),
        ('t', 'T'),
        ('u', 'U'),
        ('v', 'V'),
        ('w', 'W'),
        ('x', 'X'),
        ('y', 'Y'),
        ('z', 'Z'),
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
        ('-', '_'),
        ('=', '+'),
        ('[', '{'),
        (']', '}'),
        ('\\', '|'),
        (';', ':'),
        ('\'', '"'),
        ('`', '~'),
        (',', '<'),
        ('.', '>'),
        ('/', '?'),
    ];

    fn expected_char_events(usage: u8, shift: bool) -> Vec<InputEvent> {
        let mut events = Vec::with_capacity(if shift { 4 } else { 2 });
        if shift {
            events.push(key(0xE1, true));
        }
        events.push(key(u32::from(usage), true));
        events.push(key(u32::from(usage), false));
        if shift {
            events.push(key(0xE1, false));
        }
        events
    }

    fn us_direct(character: char) -> (u8, bool) {
        match KeyboardLayout::Us.chord(character) {
            CharOutcome::Direct(chord) => (chord.usage, chord.shift),
            other => panic!("US {character:?} is {other:?}"),
        }
    }

    #[test]
    fn text_mapping_covers_every_printable_ascii_character_exactly_once() {
        for &(character, usage) in UNSHIFTED_HID {
            assert_eq!(
                us_direct(character),
                (usage, false),
                "unshifted {character:?}"
            );
            assert_eq!(
                text_to_input_events(&character.to_string(), KeyboardLayout::Us).unwrap(),
                expected_char_events(usage, false),
                "events for {character:?}"
            );
        }
        for &(character, usage) in SHIFTED_HID {
            assert_eq!(us_direct(character), (usage, true), "shifted {character:?}");
            assert_eq!(
                text_to_input_events(&character.to_string(), KeyboardLayout::Us).unwrap(),
                expected_char_events(usage, true),
                "events for {character:?}"
            );
        }

        let mapped: std::collections::BTreeSet<char> = UNSHIFTED_HID
            .iter()
            .chain(SHIFTED_HID)
            .map(|&(character, _)| character)
            .filter(|character| character.is_ascii() && !character.is_ascii_control())
            .collect();
        let printable_ascii: std::collections::BTreeSet<char> =
            (0x20u8..=0x7E).map(char::from).collect();
        assert_eq!(mapped, printable_ascii);
    }

    #[test]
    fn every_shift_pair_shares_a_usage_and_differs_only_in_shift() {
        for &(plain, shifted) in SHIFT_PAIRS {
            let (plain_usage, plain_shift) = us_direct(plain);
            let (shifted_usage, shifted_shift) = us_direct(shifted);
            assert!(!plain_shift, "{plain:?} must not require shift");
            assert!(shifted_shift, "{shifted:?} must require shift");
            assert_eq!(
                plain_usage, shifted_usage,
                "{plain:?} and {shifted:?} must share one HID usage"
            );
        }
    }

    #[test]
    fn text_mapping_rejects_unsupported_characters_and_oversized_text() {
        assert_eq!(
            text_to_input_events("aé", KeyboardLayout::Us).unwrap_err(),
            TextMappingError::Unsupported {
                character: 'é',
                index: 1,
                layout: KeyboardLayout::Us,
            }
        );
        assert_eq!(
            text_to_input_events("日本", KeyboardLayout::Us).unwrap_err(),
            TextMappingError::Unsupported {
                character: '日',
                index: 0,
                layout: KeyboardLayout::Us,
            }
        );

        let oversized = "a".repeat(MAX_TYPE_TEXT_CHARS + 1);
        assert_eq!(
            text_to_input_events(&oversized, KeyboardLayout::Us).unwrap_err(),
            TextMappingError::TooLong {
                actual: MAX_TYPE_TEXT_CHARS + 1,
                limit: MAX_TYPE_TEXT_CHARS,
            }
        );
        // The exact bound is accepted.
        let at_limit = "a".repeat(MAX_TYPE_TEXT_CHARS);
        assert_eq!(
            text_to_input_events(&at_limit, KeyboardLayout::Us)
                .unwrap()
                .len(),
            MAX_TYPE_TEXT_CHARS * 2
        );
    }

    #[test]
    fn uk_layout_uses_iso_positions_for_shifted_punctuation() {
        let events = |text: &str| text_to_input_events(text, KeyboardLayout::Uk).unwrap();

        assert_eq!(
            events("\""),
            vec![
                key(0xE1, true),
                key(0x1F, true),
                key(0x1F, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(
            events("£"),
            vec![
                key(0xE1, true),
                key(0x20, true),
                key(0x20, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(events("#"), vec![key(0x32, true), key(0x32, false)]);
        assert_eq!(
            events("~"),
            vec![
                key(0xE1, true),
                key(0x32, true),
                key(0x32, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(
            events("@"),
            vec![
                key(0xE1, true),
                key(0x34, true),
                key(0x34, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(events("\\"), vec![key(0x64, true), key(0x64, false)]);
        assert_eq!(
            events("|"),
            vec![
                key(0xE1, true),
                key(0x64, true),
                key(0x64, false),
                key(0xE1, false),
            ]
        );
    }

    #[test]
    fn de_layout_uses_qwertz_altgr_and_umlauts() {
        let events = |text: &str| text_to_input_events(text, KeyboardLayout::De).unwrap();

        assert_eq!(events("z"), vec![key(0x1C, true), key(0x1C, false)]);
        assert_eq!(events("y"), vec![key(0x1D, true), key(0x1D, false)]);
        assert_eq!(events("ä"), vec![key(0x34, true), key(0x34, false)]);
        assert_eq!(events("ß"), vec![key(0x2D, true), key(0x2D, false)]);
        assert_eq!(events("#"), vec![key(0x32, true), key(0x32, false)]);
        assert_eq!(
            events("'"),
            vec![
                key(0xE1, true),
                key(0x32, true),
                key(0x32, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(
            events("@"),
            vec![
                key(0xE6, true),
                key(0x14, true),
                key(0x14, false),
                key(0xE6, false),
            ]
        );
        assert_eq!(
            events("€"),
            vec![
                key(0xE6, true),
                key(0x08, true),
                key(0x08, false),
                key(0xE6, false),
            ]
        );
        assert_eq!(
            events("|"),
            vec![
                key(0xE6, true),
                key(0x64, true),
                key(0x64, false),
                key(0xE6, false),
            ]
        );
    }

    #[test]
    fn fr_layout_uses_azerty_shifted_digits_and_altgr_punctuation() {
        let events = |text: &str| text_to_input_events(text, KeyboardLayout::Fr).unwrap();

        assert_eq!(events("a"), vec![key(0x14, true), key(0x14, false)]);
        assert_eq!(events("q"), vec![key(0x04, true), key(0x04, false)]);
        assert_eq!(events("m"), vec![key(0x33, true), key(0x33, false)]);
        assert_eq!(
            events("1"),
            vec![
                key(0xE1, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE1, false),
            ]
        );
        assert_eq!(events("&"), vec![key(0x1E, true), key(0x1E, false)]);
        assert_eq!(events("é"), vec![key(0x1F, true), key(0x1F, false)]);
        assert_eq!(
            events("@"),
            vec![
                key(0xE6, true),
                key(0x27, true),
                key(0x27, false),
                key(0xE6, false),
            ]
        );
        assert_eq!(
            events("^"),
            vec![
                key(0xE6, true),
                key(0x26, true),
                key(0x26, false),
                key(0xE6, false),
            ]
        );
        assert_eq!(
            events("~"),
            vec![
                key(0xE6, true),
                key(0x1F, true),
                key(0x1F, false),
                key(0xE6, false),
            ]
        );
        // Alpine's `fr-latin9` and `fr-latin9_nodeadkeys` put the direct grave
        // character at Shift+AltGr+`p`; both modifiers must be released after
        // the character key.
        assert_eq!(
            events("`"),
            vec![
                key(0xE1, true),
                key(0xE6, true),
                key(0x13, true),
                key(0x13, false),
                key(0xE6, false),
                key(0xE1, false),
            ]
        );
    }

    #[test]
    fn text_mapping_rejects_dead_keys_and_layout_specific_characters_atomically() {
        // German `^`, `~`, and `` ` `` are dead keys in `de-latin1`; emitting
        // the dead key alone would swallow the next character.
        for character in ['^', '~', '`'] {
            assert_eq!(
                text_to_input_events(&format!("ab{character}"), KeyboardLayout::De).unwrap_err(),
                TextMappingError::DeadKey {
                    character,
                    index: 2,
                    layout: KeyboardLayout::De,
                }
            );
        }

        // Non-ASCII is accepted only where the layout has a direct key.
        assert_eq!(
            text_to_input_events("ä", KeyboardLayout::Us).unwrap_err(),
            TextMappingError::Unsupported {
                character: 'ä',
                index: 0,
                layout: KeyboardLayout::Us,
            }
        );
        assert_eq!(
            text_to_input_events("é", KeyboardLayout::Uk).unwrap_err(),
            TextMappingError::Unsupported {
                character: 'é',
                index: 0,
                layout: KeyboardLayout::Uk,
            }
        );
        assert!(!text_to_input_events("é", KeyboardLayout::Fr)
            .unwrap()
            .is_empty());
        assert!(!text_to_input_events("ä", KeyboardLayout::De)
            .unwrap()
            .is_empty());
        assert!(!text_to_input_events("£", KeyboardLayout::Uk)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn text_mapping_resolves_dead_key_sequences() {
        let de = |text: &str| text_to_input_events(text, KeyboardLayout::De).unwrap();
        assert_eq!(
            de("´e"),
            vec![
                key(0x2E, true),
                key(0x2E, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            de("`e"),
            vec![
                key(0xE1, true),
                key(0x2E, true),
                key(0x2E, false),
                key(0xE1, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            de("^e"),
            vec![
                key(0x35, true),
                key(0x35, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            de("~n"),
            vec![
                key(0xE6, true),
                key(0x30, true),
                key(0x30, false),
                key(0xE6, false),
                key(0x11, true),
                key(0x11, false),
            ]
        );
        assert_eq!(
            de("¨u"),
            vec![
                key(0xE6, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE6, false),
                key(0x18, true),
                key(0x18, false),
            ]
        );
        assert_eq!(
            de("¸c"),
            vec![
                key(0xE6, true),
                key(0x2E, true),
                key(0x2E, false),
                key(0xE6, false),
                key(0x06, true),
                key(0x06, false),
            ]
        );
        // The base character may itself need Shift.
        assert_eq!(
            de("´E"),
            vec![
                key(0x2E, true),
                key(0x2E, false),
                key(0xE1, true),
                key(0x08, true),
                key(0x08, false),
                key(0xE1, false),
            ]
        );

        let fr = |text: &str| text_to_input_events(text, KeyboardLayout::Fr).unwrap();
        assert_eq!(
            fr("´e"),
            vec![
                key(0xE6, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE6, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            fr("`e"),
            vec![
                key(0xE6, true),
                key(0x24, true),
                key(0x24, false),
                key(0xE6, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            fr("^e"),
            vec![
                key(0x2F, true),
                key(0x2F, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            fr("~n"),
            vec![
                key(0xE6, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE6, false),
                key(0x11, true),
                key(0x11, false),
            ]
        );
        assert_eq!(
            fr("¨u"),
            vec![
                key(0xE1, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE1, false),
                key(0x18, true),
                key(0x18, false),
            ]
        );
    }

    #[test]
    fn text_mapping_resolves_the_documented_compose_set() {
        let de = |text: &str| text_to_input_events(text, KeyboardLayout::De).unwrap();
        assert_eq!(
            de("'e"),
            vec![
                key(0x2E, true),
                key(0x2E, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            de("\"u"),
            vec![
                key(0xE6, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE6, false),
                key(0x18, true),
                key(0x18, false),
            ]
        );
        assert_eq!(
            de(",c"),
            vec![
                key(0xE6, true),
                key(0x2E, true),
                key(0x2E, false),
                key(0xE6, false),
                key(0x06, true),
                key(0x06, false),
            ]
        );

        let fr = |text: &str| text_to_input_events(text, KeyboardLayout::Fr).unwrap();
        assert_eq!(
            fr("'e"),
            vec![
                key(0xE6, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE6, false),
                key(0x08, true),
                key(0x08, false),
            ]
        );
        assert_eq!(
            fr("\"u"),
            vec![
                key(0xE1, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE1, false),
                key(0x18, true),
                key(0x18, false),
            ]
        );
        // fr-latin9 has no dead cedilla; ç is a direct key.
        assert_eq!(fr(",c"), vec![key(0x26, true), key(0x26, false)]);

        // Unmatched prefixes are ordinary characters, not dropped.
        assert_eq!(
            text_to_input_events("'b", KeyboardLayout::De).unwrap(),
            vec![
                key(0xE1, true),
                key(0x32, true),
                key(0x32, false),
                key(0xE1, false),
                key(0x05, true),
                key(0x05, false),
            ]
        );
        // On fr, ^ has a direct key, so ^x is typed literally rather than
        // being consumed as a failed composition.
        assert_eq!(
            text_to_input_events("^x", KeyboardLayout::Fr).unwrap(),
            vec![
                key(0xE6, true),
                key(0x26, true),
                key(0x26, false),
                key(0xE6, false),
                key(0x1B, true),
                key(0x1B, false),
            ]
        );
        // US and UK have no dead keys, so the compose prefixes are literal.
        assert_eq!(
            text_to_input_events("'e", KeyboardLayout::Us).unwrap(),
            vec![
                key(0x34, true),
                key(0x34, false),
                key(0x08, true),
                key(0x08, false)
            ]
        );
    }

    #[test]
    fn text_mapping_rejects_dangling_dead_keys_and_ime_characters_atomically() {
        for (text, character, index) in [("ab^", '^', 2), ("a´", '´', 1), ("^x", '^', 0)] {
            assert_eq!(
                text_to_input_events(text, KeyboardLayout::De).unwrap_err(),
                TextMappingError::DeadKey {
                    character,
                    index,
                    layout: KeyboardLayout::De,
                },
                "DE {text:?}"
            );
        }
        assert_eq!(
            text_to_input_events("a¸", KeyboardLayout::De).unwrap_err(),
            TextMappingError::DeadKey {
                character: '¸',
                index: 1,
                layout: KeyboardLayout::De,
            }
        );
        assert_eq!(
            text_to_input_events("´x", KeyboardLayout::Fr).unwrap_err(),
            TextMappingError::DeadKey {
                character: '´',
                index: 0,
                layout: KeyboardLayout::Fr,
            }
        );
        for character in ['日', '🙂'] {
            assert_eq!(
                text_to_input_events(&format!("a{character}"), KeyboardLayout::Fr).unwrap_err(),
                TextMappingError::Unsupported {
                    character,
                    index: 1,
                    layout: KeyboardLayout::Fr,
                },
                "FR {character:?}"
            );
        }
        // Plain accented characters still require a direct key or a sequence;
        // the parser must not guess one on a layout without it.
        assert_eq!(
            text_to_input_events("é", KeyboardLayout::De).unwrap_err(),
            TextMappingError::Unsupported {
                character: 'é',
                index: 0,
                layout: KeyboardLayout::De,
            }
        );
    }

    #[tokio::test]
    async fn input_key_route_maps_evdev_letters_modifiers_and_arrows() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        for keycode in [30, 42, 103] {
            let (status, body) = post_json(
                &app,
                "/input/key",
                serde_json::json!({"keycode": keycode, "down": true}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "keycode {keycode}");
            assert_eq!(body["ok"], true);
            assert_eq!(body["sink"], SINK_FAKE_RECORDING);
            assert_eq!(body["injected"], false);
        }

        assert_eq!(
            recording.take_events(),
            vec![key(0x04, true), key(0xE1, true), key(0x52, true)]
        );
    }

    #[tokio::test]
    async fn input_key_route_handles_release_and_reports_duplicate_as_a_noop() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        let (status, body) = post_json(
            &app,
            "/input/key",
            serde_json::json!({"keycode": 30, "down": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert!(recording.held().has_key(0x04));

        // A second press of the same key is accepted but does not change state.
        let (status, body) = post_json(
            &app,
            "/input/key",
            serde_json::json!({"keycode": 30, "down": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert!(recording.held().has_key(0x04));

        let (status, body) = post_json(
            &app,
            "/input/key",
            serde_json::json!({"keycode": 30, "down": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert!(!recording.held().has_key(0x04));

        // The recording sink still records both presses, even though the second
        // was a state no-op.
        assert_eq!(
            recording.take_events(),
            vec![key(0x04, true), key(0x04, true), key(0x04, false),]
        );
    }

    #[tokio::test]
    async fn input_key_route_rejects_bad_keycodes_and_missing_sink() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        let (status, body) = post_json(
            &app,
            "/input/key",
            serde_json::json!({"keycode": 0, "down": true}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "unknown-keycode");

        let (status, body) = post_json(
            &app,
            "/input/key",
            serde_json::json!({"keycode": 70000, "down": true}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-keycode");

        let (status, body) = post_json(&app, "/input/key", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-keycode");

        let no_sink = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = post_json(
            &input_router(no_sink),
            "/input/key",
            serde_json::json!({"keycode": 30, "down": true}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "input-unavailable");
    }

    #[tokio::test]
    async fn input_mouse_route_emits_absolute_motion_and_buttons() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        let (status, body) = post_json(
            &app,
            "/input/mouse",
            serde_json::json!({"x": 100, "y": 200}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);

        for (action, button) in [("press", 2), ("release", 2)] {
            let (status, body) = post_json(
                &app,
                "/input/mouse",
                serde_json::json!({"x": 100, "y": 200, "action": action, "button": button}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{action}");
            assert_eq!(body["ok"], true);
        }

        let (status, body) = post_json(
            &app,
            "/input/mouse",
            serde_json::json!({"x": 100, "y": 200, "action": "click", "button": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);

        assert_eq!(
            recording.take_events(),
            vec![
                absolute(100, 200),
                absolute(100, 200),
                InputEvent::Button {
                    button: PointerButton::Right,
                    down: true,
                },
                absolute(100, 200),
                InputEvent::Button {
                    button: PointerButton::Right,
                    down: false,
                },
                absolute(100, 200),
                InputEvent::Button {
                    button: PointerButton::Middle,
                    down: true,
                },
                InputEvent::Button {
                    button: PointerButton::Middle,
                    down: false,
                },
            ]
        );
        assert!(recording.held().is_empty(), "click must release the button");

        assert_eq!(pointer_button(0), Some(PointerButton::Left));
        assert_eq!(pointer_button(1), Some(PointerButton::Left));
        assert_eq!(pointer_button(2), Some(PointerButton::Right));
        assert_eq!(pointer_button(4), Some(PointerButton::Middle));
        assert_eq!(pointer_button(8), Some(PointerButton::Back));
        assert_eq!(pointer_button(16), Some(PointerButton::Forward));
        assert_eq!(pointer_button(3), None);
    }

    #[tokio::test]
    async fn input_mouse_route_rejects_bad_coordinates_actions_and_buttons() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        for body in [
            serde_json::json!({"x": 32768, "y": 0}),
            serde_json::json!({"x": -1, "y": 0}),
            serde_json::json!({"x": 0, "y": 32768}),
        ] {
            let (status, response) = post_json(&app, "/input/mouse", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["code"], "invalid-coordinate");
        }

        let (status, body) = post_json(
            &app,
            "/input/mouse",
            serde_json::json!({"x": 1, "y": 2, "button": 3}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unknown-button");

        let (status, body) = post_json(
            &app,
            "/input/mouse",
            serde_json::json!({"x": 1, "y": 2, "action": "drag"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid-json");
    }

    #[tokio::test]
    async fn input_type_route_submits_shifted_text_through_the_sink() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "Hi!"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 10);

        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE1, true),
                key(0x0B, true),
                key(0x0B, false),
                key(0xE1, false),
                key(0x0C, true),
                key(0x0C, false),
                key(0xE1, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE1, false),
            ]
        );
        assert!(recording.held().is_empty(), "every key must be released");
    }

    #[tokio::test]
    async fn input_type_route_types_shifted_punctuation_without_a_stuck_modifier() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        // The live-guest stall was reported around `_` and `>`; assert the exact
        // shift/key order for both and that neither shift nor the key remains.
        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "_>"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 8);

        assert_eq!(
            recording.take_events(),
            vec![
                key(0xE1, true),
                key(0x2D, true),
                key(0x2D, false),
                key(0xE1, false),
                key(0xE1, true),
                key(0x37, true),
                key(0x37, false),
                key(0xE1, false),
            ]
        );
        assert!(
            recording.held().is_empty(),
            "shift and punctuation must be released"
        );
    }

    #[tokio::test]
    async fn input_type_route_rejects_unsupported_and_oversized_text_atomically() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        // An unsupported character anywhere in the request rejects the whole
        // request before the first event reaches the sink.
        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "oké"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unsupported-character");
        assert!(recording.events().is_empty());

        let (status, body) = post_json(
            &app,
            "/input/type",
            serde_json::json!({"text": "a".repeat(MAX_TYPE_TEXT_CHARS + 1)}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "text-too-long");
        assert!(recording.events().is_empty());

        // Missing text is the same as empty text, not an error.
        let (status, body) = post_json(&app, "/input/type", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 0);
    }

    #[tokio::test]
    async fn input_type_route_uses_the_configured_layout_and_releases_altgr() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::De);
        let app = input_router(state);

        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "z@€"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 10);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0x1C, true),
                key(0x1C, false),
                key(0xE6, true),
                key(0x14, true),
                key(0x14, false),
                key(0xE6, false),
                key(0xE6, true),
                key(0x08, true),
                key(0x08, false),
                key(0xE6, false),
            ]
        );
        assert!(
            recording.held().is_empty(),
            "AltGr and the character keys must be released"
        );
    }

    #[tokio::test]
    async fn input_type_route_rejects_dead_keys_and_unsupported_characters_before_typing() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::De);
        let app = input_router(state);

        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "ab^"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "dead-key-required");
        assert!(body["remediation"].is_string());
        assert!(recording.events().is_empty());

        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);
        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "okä"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unsupported-character");
        assert!(body["remediation"].is_string());
        assert!(recording.events().is_empty());
    }

    #[tokio::test]
    async fn input_type_route_fr_layout_accepts_french_accents_and_altgr() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::Fr);
        let app = input_router(state);

        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "é@"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"], 6);
        assert_eq!(
            recording.take_events(),
            vec![
                key(0x1F, true),
                key(0x1F, false),
                key(0xE6, true),
                key(0x27, true),
                key(0x27, false),
                key(0xE6, false),
            ]
        );
        assert!(recording.held().is_empty());
    }

    #[tokio::test]
    async fn input_type_route_types_dead_key_sequences_on_the_german_layout() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::De);
        let app = input_router(state);

        let (status, body) = post_json(
            &app,
            "/input/type",
            serde_json::json!({"text": "´e`e^e~n¨u¸c"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 32);
        assert_eq!(
            recording.take_events(),
            vec![
                // ´e
                key(0x2E, true),
                key(0x2E, false),
                key(0x08, true),
                key(0x08, false),
                // `e
                key(0xE1, true),
                key(0x2E, true),
                key(0x2E, false),
                key(0xE1, false),
                key(0x08, true),
                key(0x08, false),
                // ^e
                key(0x35, true),
                key(0x35, false),
                key(0x08, true),
                key(0x08, false),
                // ~n
                key(0xE6, true),
                key(0x30, true),
                key(0x30, false),
                key(0xE6, false),
                key(0x11, true),
                key(0x11, false),
                // ¨u
                key(0xE6, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE6, false),
                key(0x18, true),
                key(0x18, false),
                // ¸c
                key(0xE6, true),
                key(0x2E, true),
                key(0x2E, false),
                key(0xE6, false),
                key(0x06, true),
                key(0x06, false),
            ]
        );
        assert!(
            recording.held().is_empty(),
            "dead keys, base keys, Shift, and AltGr must all be released"
        );
    }

    #[tokio::test]
    async fn input_type_route_types_dead_keys_and_compose_on_the_french_layout() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::Fr);
        let app = input_router(state);

        let (status, body) = post_json(
            &app,
            "/input/type",
            serde_json::json!({"text": "´e`e^e~n¨u,c"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["events"], 30);
        assert_eq!(
            recording.take_events(),
            vec![
                // ´e (dead acute is AltGr+1)
                key(0xE6, true),
                key(0x1E, true),
                key(0x1E, false),
                key(0xE6, false),
                key(0x08, true),
                key(0x08, false),
                // `e (dead grave is AltGr+è)
                key(0xE6, true),
                key(0x24, true),
                key(0x24, false),
                key(0xE6, false),
                key(0x08, true),
                key(0x08, false),
                // ^e
                key(0x2F, true),
                key(0x2F, false),
                key(0x08, true),
                key(0x08, false),
                // ~n
                key(0xE6, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE6, false),
                key(0x11, true),
                key(0x11, false),
                // ¨u (dead diaeresis is Shift+^ key)
                key(0xE1, true),
                key(0x2F, true),
                key(0x2F, false),
                key(0xE1, false),
                key(0x18, true),
                key(0x18, false),
                // ,c composes to the direct ç key
                key(0x26, true),
                key(0x26, false),
            ]
        );
        assert!(recording.held().is_empty());
    }

    #[tokio::test]
    async fn input_type_route_rejects_dangling_dead_keys_and_ime_before_typing() {
        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::Fr);
        let app = input_router(state);

        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "ab´"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "dead-key-required");
        assert!(body["remediation"].is_string());
        assert!(recording.events().is_empty());

        // A known dead prefix followed by a non-composable character is not a
        // composition; on fr `^` falls back to its direct key, while a CJK
        // character remains a typed 400 with no partial typing.
        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "a日"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unsupported-character");
        assert!(recording.events().is_empty());

        let (state, recording) = input_state_with_layout(SINK_FAKE_RECORDING, KeyboardLayout::De);
        let app = input_router(state);
        let (status, body) =
            post_json(&app, "/input/type", serde_json::json!({"text": "a🙂"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "unsupported-character");
        assert!(recording.events().is_empty());
    }

    struct PendingCapture {
        format: FrameFormat,
        stopped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl CaptureSource for PendingCapture {
        fn id(&self) -> &str {
            "compat-pending"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            std::future::pending().await
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    struct NullSink;

    #[async_trait]
    impl FrameSink for NullSink {
        async fn write(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct CodecSpawner {
        calls: std::sync::atomic::AtomicUsize,
        fail_on: Mutex<Vec<usize>>,
        spawns: Mutex<Vec<Vec<String>>>,
    }

    impl CodecSpawner {
        fn new(fail_on: Vec<usize>) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail_on: Mutex::new(fail_on),
                spawns: Mutex::new(Vec::new()),
            }
        }

        fn spawns(&self) -> Vec<Vec<String>> {
            lock(&self.spawns).clone()
        }
    }

    impl FfmpegSpawner for CodecSpawner {
        fn spawn(
            &self,
            config: &HlsConfig,
            width: u32,
            height: u32,
        ) -> Result<SpawnedFfmpeg, HlsError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if lock(&self.fail_on).contains(&call) {
                return Err(HlsError::Spawn {
                    bin: config.ffmpeg_bin.clone(),
                    message: format!("fake ffmpeg spawn failure #{call}"),
                });
            }
            lock(&self.spawns).push(ffmpeg_argv(config, width, height));
            Ok(SpawnedFfmpeg {
                sink: Box::new(NullSink),
                child: None,
            })
        }
    }

    /// Advertises a hardware encoder by name, then fails the real runtime probe.
    struct RejectingRuntimeProbe {
        availability: EncoderAvailability,
    }

    impl RejectingRuntimeProbe {
        fn new(encoders: impl IntoIterator<Item = HlsEncoder>) -> Self {
            Self {
                availability: EncoderAvailability::from_names(
                    encoders.into_iter().map(HlsEncoder::ffmpeg_name),
                ),
            }
        }
    }

    impl EncoderProbe for RejectingRuntimeProbe {
        fn availability(
            &self,
            _ffmpeg_bin: &std::path::Path,
        ) -> Result<EncoderAvailability, String> {
            Ok(self.availability.clone())
        }

        fn runtime_check(&self, _config: &HlsConfig) -> Result<(), String> {
            Err("synthetic runtime probe rejected the encoder".to_owned())
        }
    }

    fn pending_factory() -> HlsSourceFactory {
        Arc::new(|| {
            Ok(Box::new(PendingCapture {
                format: FrameFormat::rgb24(320, 240),
                stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }) as Box<dyn CaptureSource>)
        })
    }

    fn controller_with(
        directory: &std::path::Path,
        spawner: Arc<dyn FfmpegSpawner>,
    ) -> HlsController {
        HlsController::with_spawner(
            pending_factory(),
            HlsConfig {
                output_dir: directory.to_path_buf(),
                ..HlsConfig::default()
            },
            spawner,
        )
    }

    fn controller_with_probe(
        directory: &std::path::Path,
        vaapi_device: &std::path::Path,
        spawner: Arc<dyn FfmpegSpawner>,
        probe: Arc<dyn EncoderProbe>,
    ) -> HlsController {
        HlsController::with_spawner_and_probe(
            pending_factory(),
            HlsConfig {
                output_dir: directory.to_path_buf(),
                vaapi_device: vaapi_device.to_path_buf(),
                ..HlsConfig::default()
            },
            spawner,
            probe,
        )
    }

    fn codec_test_dir(label: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("ozma-codec-restart-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create codec test directory");
        directory
    }

    fn software_capability(codec: Codec) -> EncodeCapability {
        EncodeCapability {
            codec,
            backend: EncoderBackend::Software,
            hardware: false,
            max_width: 1920,
            max_height: 1080,
            max_fps: 30,
            hdr: false,
            profiles: Vec::new(),
        }
    }

    fn hardware_capability(codec: Codec, backend: EncoderBackend) -> EncodeCapability {
        EncodeCapability {
            codec,
            backend,
            hardware: true,
            max_width: 1920,
            max_height: 1080,
            max_fps: 30,
            hdr: false,
            profiles: Vec::new(),
        }
    }

    fn codec_control(registry: EncoderRegistry) -> CodecControl {
        CodecControl::new(
            registry,
            MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
        )
    }

    fn h264_registry() -> EncoderRegistry {
        EncoderRegistry::from_capabilities([ozma_encode::H264SoftwareEncoder::capability()])
    }

    fn hardware_registry(entries: &[(Codec, EncoderBackend)]) -> EncoderRegistry {
        let mut capabilities = vec![ozma_encode::H264SoftwareEncoder::capability()];
        capabilities.extend(
            entries
                .iter()
                .map(|(codec, backend)| hardware_capability(*codec, *backend)),
        );
        EncoderRegistry::from_capabilities(capabilities)
    }

    fn mixed_hardware_registry() -> EncoderRegistry {
        hardware_registry(&[
            (Codec::H264, EncoderBackend::Vaapi),
            (Codec::Hevc, EncoderBackend::Vaapi),
            (Codec::H264, EncoderBackend::Nvenc),
            (Codec::Hevc, EncoderBackend::Nvenc),
        ])
    }

    fn codec_state(control: Option<CodecControl>) -> ConnectionState {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        match control {
            Some(control) => state
                .with_codecs(control.status())
                .with_codec_control(Some(control)),
            None => state.with_codecs(CodecStatus::from_codecs([Codec::H264, Codec::Opus])),
        }
    }

    #[tokio::test]
    async fn codec_route_reports_real_encoders_and_stores_a_pending_override() {
        let app = codec_router(codec_state(Some(codec_control(h264_registry()))));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current_encoder"], "h264");
        assert!(body["override"].is_null());
        assert_eq!(body["available"], serde_json::json!(["h264"]));
        assert!(body["receipt"].is_null());
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["applied"], false);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "bitrate": "8M", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        // HLS is not running: never claim the override is live.
        assert_eq!(body["applied"], false);
        assert_eq!(body["pending_restart"], true);
        assert!(body["encoder"].is_null());
        assert_eq!(body["hls"], "not-configured");
        assert_eq!(body["receipt"]["requested"]["codec"], "h264");
        assert_eq!(body["receipt"]["requested"]["bitrate_kbps"], 8_000);
        assert_eq!(body["receipt"]["requested"]["backend"], "software");
        assert_eq!(body["receipt"]["applied"], true);
        assert_eq!(body["receipt"]["selected"]["backend"], "software");

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current_encoder"], "h264");
        assert_eq!(body["override"]["preference"]["codec"], "h264");
        assert_eq!(body["override"]["source"], "controller");
        assert_eq!(body["receipt"]["requested"]["bitrate_kbps"], 8_000);
        assert_eq!(body["pending_restart"], true);
        assert_eq!(body["applied"], false);
        assert_eq!(body["available"], serde_json::json!(["h264"]));
    }

    #[tokio::test]
    async fn codec_route_lists_real_codecs_and_backends_from_the_registry() {
        let app = codec_router(codec_state(Some(codec_control(mixed_hardware_registry()))));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current_encoder"], "h264");
        assert_eq!(body["available"], serde_json::json!(["h264", "hevc"]));
        assert_eq!(
            body["backends"],
            serde_json::json!(["software", "vaapi", "nvenc"])
        );
        assert!(body["receipt"].is_null());
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["applied"], false);
    }

    #[tokio::test]
    async fn codec_route_selects_every_advertised_hardware_entry_without_hls() {
        let registry = hardware_registry(&[
            (Codec::H264, EncoderBackend::Vaapi),
            (Codec::Hevc, EncoderBackend::Vaapi),
            (Codec::H264, EncoderBackend::Nvenc),
            (Codec::Hevc, EncoderBackend::Nvenc),
            (Codec::H264, EncoderBackend::Qsv),
            (Codec::Hevc, EncoderBackend::Qsv),
            (Codec::H264, EncoderBackend::V4l2M2m),
            (Codec::Hevc, EncoderBackend::V4l2M2m),
        ]);
        let app = codec_router(codec_state(Some(codec_control(registry))));

        for (codec, hw_accel, backend, receipt_backend, encoder) in [
            ("h264", "vaapi", "vaapi", "vaapi", "h264_vaapi"),
            ("hevc", "vaapi", "vaapi", "vaapi", "hevc_vaapi"),
            ("h264", "nvenc", "nvenc", "nvenc", "h264_nvenc"),
            ("hevc", "nvenc", "nvenc", "nvenc", "hevc_nvenc"),
            ("h264", "qsv", "qsv", "qsv", "h264_qsv"),
            ("hevc", "qsv", "qsv", "qsv", "hevc_qsv"),
            ("h264", "v4l2m2m", "v4l2m2m", "v4l2_m2m", "h264_v4l2m2m"),
            ("hevc", "v4l2m2m", "v4l2m2m", "v4l2_m2m", "hevc_v4l2m2m"),
        ] {
            let (status, body) = post_json(
                &app,
                "/codec",
                serde_json::json!({"codec": codec, "hw_accel": hw_accel}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{codec}/{hw_accel}: {body}");
            // No HLS pipeline: selection is stored, never claimed live.
            assert_eq!(body["applied"], false, "{codec}/{hw_accel}: {body}");
            assert_eq!(body["pending_restart"], true, "{codec}/{hw_accel}: {body}");
            assert!(body["encoder"].is_null(), "{codec}/{hw_accel}: {body}");
            assert_eq!(
                body["requested_encoder"], encoder,
                "{codec}/{hw_accel}: {body}"
            );
            assert_eq!(body["backend"], backend, "{codec}/{hw_accel}: {body}");
            assert_eq!(
                body["receipt"]["selected"]["codec"], codec,
                "{codec}/{hw_accel}: {body}"
            );
            assert_eq!(
                body["receipt"]["selected"]["backend"], receipt_backend,
                "{codec}/{hw_accel}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn codec_route_never_applies_hardware_that_fails_the_runtime_probe() {
        let directory = codec_test_dir("hardware-runtime-rejected");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake VAAPI device");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = controller_with_probe(
            &directory,
            &device,
            spawner.clone(),
            Arc::new(RejectingRuntimeProbe::new([
                HlsEncoder::H264,
                HlsEncoder::Hevc,
                HlsEncoder::VaapiH264,
                HlsEncoder::VaapiHevc,
            ])),
        );
        controller.start().await.expect("start HLS");

        let registry = hardware_registry(&[(Codec::Hevc, EncoderBackend::Vaapi)]);
        let control =
            CodecControl::new(registry, MediaFormat::new(Codec::H264, 320, 240, 30, 4_000))
                .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "hw_accel": "vaapi"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], codes::ENCODER_UNAVAILABLE);
        assert_eq!(body["details"]["encoder"], "hevc_vaapi");
        assert_eq!(body["details"]["backend"], "vaapi");
        assert!(
            body.get("applied").is_none(),
            "a failed runtime probe must not return applied"
        );
        assert_eq!(
            spawner.spawns().len(),
            2,
            "the initial generation plus the restored H.264 generation"
        );
        assert!(controller.is_running(), "the old pipeline was restored");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], false);
        assert!(body["override"].is_null());

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_maps_controller_bitrate_and_backend_names() {
        let app = codec_router(codec_state(Some(codec_control(h264_registry()))));

        for (body, expected_kbps) in [
            (
                serde_json::json!({"codec": "h264", "bitrate": "8000k"}),
                8_000,
            ),
            (
                serde_json::json!({"codec": "h264", "bitrate": 8_000}),
                8_000,
            ),
            (serde_json::json!({"codec": "h264", "bitrate": "8M"}), 8_000),
            (
                serde_json::json!({"codec": "H264", "bitrate": "8Mbps"}),
                8_000,
            ),
        ] {
            let (status, response) = post_json(&app, "/codec", body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(
                response["receipt"]["requested"]["bitrate_kbps"],
                expected_kbps
            );
        }

        for (hw, backend) in [
            ("vaapi", "vaapi"),
            ("nvenc", "nvenc"),
            ("qsv", "qsv"),
            ("v4l2m2m", "v4l2m2m"),
        ] {
            let (status, response) = post_json(
                &app,
                "/codec",
                serde_json::json!({"codec": "h264", "hw_accel": hw}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
            assert_eq!(response["code"], codes::ENCODER_UNAVAILABLE);
            assert_eq!(
                response["details"]["requested_backend"].as_str(),
                Some(backend)
            );
        }
    }

    #[tokio::test]
    async fn codec_route_rejects_unknown_codec_bitrate_and_backend() {
        let app = codec_router(codec_state(Some(codec_control(h264_registry()))));

        for (body, needle) in [
            (serde_json::json!({}), "missing `codec`"),
            (serde_json::json!({"codec": "vp9"}), "vp9"),
            (
                serde_json::json!({"codec": "h264", "bitrate": "fast"}),
                "bitrate",
            ),
            (
                serde_json::json!({"codec": "h264", "hw_accel": "warp"}),
                "hw_accel",
            ),
        ] {
            let (status, response) = post_json(&app, "/codec", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
            assert_eq!(response["ok"], false);
            assert!(
                response["error"].as_str().unwrap().contains(needle),
                "{response}"
            );
            assert!(response["details"]["available"].is_array(), "{response}");
            assert_eq!(response["details"]["available"][0]["backend"], "software");
        }

        // No state was stored after any rejected request.
        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["override"].is_null());
        assert_eq!(body["pending_restart"], false);
    }

    #[tokio::test]
    async fn codec_route_honours_or_denies_software_fallback() {
        let app = codec_router(codec_state(Some(codec_control(h264_registry()))));

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "nvenc"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], codes::ENCODER_UNAVAILABLE);
        assert_eq!(body["details"]["available"][0]["backend"], "software");

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({
                "codec": "h264",
                "hw_accel": "nvenc",
                "allow_software_fallback": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], false);
        assert_eq!(body["pending_restart"], true);
        assert_eq!(body["receipt"]["applied"], false);
        assert_eq!(
            body["receipt"]["fallback_reason"],
            "requested_backend_unavailable"
        );
        assert_eq!(body["receipt"]["selected"]["backend"], "software");
    }

    #[tokio::test]
    async fn codec_route_applies_an_encoder_change_by_restarting_hls() {
        let directory = codec_test_dir("applied");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = controller_with(&directory, spawner.clone());
        controller.start().await.expect("start HLS");

        let control = CodecControl::new(
            EncoderRegistry::from_capabilities([software_capability(Codec::Hevc)]),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state.clone());

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "bitrate": "2M", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["encoder"], "libx265");
        assert_eq!(body["hls"], "running");
        assert_eq!(body["receipt"]["selected"]["codec"], "hevc");
        assert_eq!(body["receipt"]["selected"]["backend"], "software");
        assert_eq!(controller.config().video_encoder, HlsEncoder::Hevc);
        assert_eq!(controller.config().bitrate_kbps, 2_000);

        let spawns = spawner.spawns();
        assert_eq!(spawns.len(), 2, "initial generation plus restarted one");
        assert!(spawns[1]
            .windows(2)
            .any(|window| window == ["-c:v", "libx265"]));
        assert!(spawns[1].contains(&"2000k".to_owned()));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], true);
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["current_encoder"], "libx265");
        assert_eq!(body["override"]["preference"]["codec"], "hevc");
        assert_eq!(body["receipt"]["selected"]["codec"], "hevc");
        assert_eq!(body["available"], serde_json::json!(["hevc"]));

        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(connection["codec"]["applied"], true);
        assert_eq!(connection["codec"]["pending_restart"], false);
        assert_eq!(connection["codec"]["current_encoder"], "libx265");
        assert_eq!(connection["hls"], "running");

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_returns_502_and_keeps_the_previous_pipeline_on_restart_failure() {
        let directory = codec_test_dir("restart-failure");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        // Generation 2 is the requested HEVC pipeline; generation 3 is the
        // restoration of the original H.264 pipeline.
        let spawner = Arc::new(CodecSpawner::new(vec![2]));
        let controller = controller_with(&directory, spawner.clone());
        controller.start().await.expect("start HLS");

        let control = CodecControl::new(
            EncoderRegistry::from_capabilities([software_capability(Codec::Hevc)]),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], codes::INTERNAL);
        assert_eq!(body["details"]["encoder"], "libx265");
        assert!(
            body.get("applied").is_none(),
            "a failed restart must not report applied"
        );
        assert!(controller.is_running(), "the old pipeline must be restored");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);

        let spawns = spawner.spawns();
        assert_eq!(spawns.len(), 2, "initial plus restored generation");
        assert!(spawns[1]
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"]));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], false);
        assert_eq!(body["pending_restart"], false);
        assert!(body["override"].is_null());
        assert!(body["receipt"].is_null());

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_reports_unavailable_hardware_as_typed_502_without_fallback() {
        let directory = codec_test_dir("hardware-unavailable");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = controller_with(&directory, spawner.clone());
        controller.start().await.expect("start HLS");

        let control = CodecControl::new(
            EncoderRegistry::from_capabilities([hardware_capability(
                Codec::H264,
                EncoderBackend::Nvenc,
            )]),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "nvenc"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], codes::ENCODER_UNAVAILABLE);
        assert_eq!(body["details"]["encoder"], "h264_nvenc");
        assert_eq!(body["details"]["backend"], "nvenc");
        assert!(body.get("applied").is_none());
        assert_eq!(
            spawner.spawns().len(),
            2,
            "the initial generation plus the restored software generation"
        );
        assert!(spawner.spawns()[1]
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"]));
        assert!(controller.is_running(), "the old pipeline was restored");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], false);
        assert!(body["override"].is_null());

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_honours_an_explicit_hardware_software_fallback() {
        let directory = codec_test_dir("hardware-fallback");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = controller_with(&directory, spawner.clone());
        controller.start().await.expect("start HLS");

        let control = CodecControl::new(
            EncoderRegistry::from_capabilities([hardware_capability(
                Codec::H264,
                EncoderBackend::Nvenc,
            )]),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({
                "codec": "h264",
                "hw_accel": "nvenc",
                "allow_software_fallback": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["encoder"], "libx264");
        assert_eq!(body["requested_encoder"], "h264_nvenc");
        assert_eq!(body["backend"], "nvenc");
        assert_eq!(body["fallback"], true);
        assert!(body["fallback_reason"].is_string(), "{body}");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);
        assert_eq!(spawner.spawns().len(), 2);
        assert!(spawner.spawns()[1]
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"]));

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_maps_and_applies_verified_hardware() {
        let directory = codec_test_dir("hardware-vaapi");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake VAAPI device");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = controller_with_probe(
            &directory,
            &device,
            spawner.clone(),
            Arc::new(StaticEncoderProbe::new(EncoderAvailability::from_names([
                HlsEncoder::H264.ffmpeg_name(),
                HlsEncoder::VaapiH264.ffmpeg_name(),
            ]))),
        );
        controller.start().await.expect("start HLS");

        let control = CodecControl::new(
            EncoderRegistry::from_capabilities([hardware_capability(
                Codec::H264,
                EncoderBackend::Vaapi,
            )]),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "vaapi"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["encoder"], "h264_vaapi");
        assert_eq!(body["requested_encoder"], "h264_vaapi");
        assert_eq!(body["backend"], "vaapi");
        assert_eq!(body["vaapi_device"], device.display().to_string());
        assert_eq!(body["fallback"], false);
        assert_eq!(body["receipt"]["selected"]["backend"], "vaapi");
        assert_eq!(controller.config().video_encoder, HlsEncoder::VaapiH264);

        let spawns = spawner.spawns();
        assert_eq!(spawns.len(), 2);
        let argv = &spawns[1];
        assert!(argv
            .windows(2)
            .any(|window| window == ["-c:v", "h264_vaapi"]));
        assert!(argv.windows(2).any(
            |window| window[0] == "-vaapi_device" && window[1] == device.display().to_string()
        ));
        assert!(argv
            .windows(2)
            .any(|window| window == ["-vf", "format=nv12,hwupload"]));
        assert!(argv
            .windows(2)
            .any(|window| window == ["-hls_flags", "delete_segments"]));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], true);
        assert_eq!(body["current_encoder"], "h264_vaapi");

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_preserves_and_reports_the_configured_nvenc_gpu() {
        let directory = codec_test_dir("nvenc-gpu");
        std::fs::write(directory.join("stream.m3u8"), "#EXTM3U\n").expect("manifest");
        let spawner = Arc::new(CodecSpawner::new(Vec::new()));
        let controller = HlsController::with_spawner_and_probe(
            pending_factory(),
            HlsConfig {
                output_dir: directory.clone(),
                nvenc_gpu: Some(1),
                ..HlsConfig::default()
            },
            spawner.clone(),
            Arc::new(StaticEncoderProbe::new(EncoderAvailability::from_names([
                HlsEncoder::H264.ffmpeg_name(),
                HlsEncoder::NvencH264.ffmpeg_name(),
                HlsEncoder::NvencHevc.ffmpeg_name(),
            ]))),
        );
        controller.start().await.expect("start HLS");
        let control = CodecControl::new(
            mixed_hardware_registry(),
            MediaFormat::new(Codec::H264, 320, 240, 30, 4_000),
        )
        .with_hls(Some(controller.clone()));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_hls_controller(Some(controller.clone()))
        .with_codec_control(Some(control));
        let app = codec_router(state);

        // The preference is reported before any NVENC switch, and no GPU is
        // invented when it was not configured.
        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["nvenc_gpu"], 1);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "nvenc"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["encoder"], "h264_nvenc");
        assert_eq!(body["nvenc_gpu"], 1);
        assert_eq!(controller.config().nvenc_gpu, Some(1));
        assert!(spawner.spawns()[1]
            .windows(2)
            .any(|window| window == ["-gpu", "1"]));

        // A software switch keeps the stored preference but omits `-gpu`.
        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["encoder"], "libx264");
        assert!(
            body["nvenc_gpu"].is_null(),
            "a software encoder must not claim a GPU: {body}"
        );
        assert_eq!(
            controller.config().nvenc_gpu,
            Some(1),
            "the preference must survive the software switch"
        );
        assert!(!spawner.spawns()[2]
            .iter()
            .any(|argument| argument == "-gpu"));

        // Switching back to NVENC targets the same GPU.
        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "hw_accel": "nvenc"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["encoder"], "hevc_nvenc");
        assert_eq!(body["nvenc_gpu"], 1);
        assert!(spawner.spawns()[3]
            .windows(2)
            .any(|window| window == ["-gpu", "1"]));

        controller.stop().await.expect("stop HLS");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn codec_route_is_empty_and_503_without_a_video_capability() {
        let app = codec_router(codec_state(None));

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current_encoder"], "h264");
        assert!(body["override"].is_null());
        assert_eq!(body["available"], serde_json::json!(["h264"]));

        let (status, body) = post_json(&app, "/codec", serde_json::json!({"codec": "h264"})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "codec-video-unavailable");

        // A node with no video capability at all stays empty.
        let empty = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = get_json(&codec_router(empty), "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["current_encoder"].is_null());
        assert_eq!(body["available"], serde_json::json!([]));
    }

    struct FanoutHlsFactory {
        fail_restart: Arc<Mutex<Vec<(u32, HlsEncoder)>>>,
        restarts: Arc<Mutex<Vec<(u32, HlsEncoder, u32)>>>,
    }

    #[async_trait]
    impl crate::multi_hls::ConsoleHlsFactory for FanoutHlsFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            Some(ozma_display::vm::VmConsoleBackend::Dbus)
        }

        async fn start(
            &self,
            console: u32,
            config: HlsConfig,
        ) -> Result<Arc<dyn crate::multi_hls::ConsoleHlsPipeline>, String> {
            std::fs::create_dir_all(&config.output_dir).map_err(|error| error.to_string())?;
            let manifest = config.manifest_path();
            std::fs::write(&manifest, "#EXTM3U\n").map_err(|error| error.to_string())?;
            Ok(Arc::new(FanoutHlsPipeline {
                console,
                manifest,
                encoder: Mutex::new(config.video_encoder),
                bitrate_kbps: Mutex::new(config.bitrate_kbps),
                fail_restart: self.fail_restart.clone(),
                restarts: self.restarts.clone(),
            }))
        }
    }

    struct FanoutHlsPipeline {
        console: u32,
        manifest: std::path::PathBuf,
        encoder: Mutex<HlsEncoder>,
        bitrate_kbps: Mutex<u32>,
        fail_restart: Arc<Mutex<Vec<(u32, HlsEncoder)>>>,
        restarts: Arc<Mutex<Vec<(u32, HlsEncoder, u32)>>>,
    }

    #[async_trait]
    impl crate::multi_hls::ConsoleHlsPipeline for FanoutHlsPipeline {
        fn status(&self) -> HlsStatus {
            HlsStatus::running(self.manifest.clone())
        }

        fn is_running(&self) -> bool {
            true
        }

        fn encoder(&self) -> HlsEncoder {
            *self.encoder.lock().unwrap()
        }

        fn bitrate_kbps(&self) -> u32 {
            *self.bitrate_kbps.lock().unwrap()
        }

        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }

        async fn restart_with_encoder(
            &self,
            encoder: HlsEncoder,
            bitrate_kbps: Option<u32>,
            _allow_software_fallback: bool,
        ) -> Result<crate::hls::HlsRestartOutcome, crate::multi_hls::ConsoleHlsRestartError>
        {
            if self
                .fail_restart
                .lock()
                .unwrap()
                .contains(&(self.console, encoder))
            {
                return Err(crate::multi_hls::ConsoleHlsRestartError::new(
                    format!(
                        "fake `{}` for Console_{} failed",
                        encoder.ffmpeg_name(),
                        self.console
                    ),
                    encoder.is_hardware(),
                ));
            }
            *self.encoder.lock().unwrap() = encoder;
            let bitrate = bitrate_kbps.unwrap_or_else(|| self.bitrate_kbps());
            *self.bitrate_kbps.lock().unwrap() = bitrate;
            self.restarts
                .lock()
                .unwrap()
                .push((self.console, encoder, bitrate));
            Ok(crate::hls::HlsRestartOutcome {
                status: HlsStatus::running(self.manifest.clone()),
                requested_encoder: encoder,
                applied_encoder: encoder,
                fallback: None,
            })
        }

        fn manifest_path(&self) -> std::path::PathBuf {
            self.manifest.clone()
        }
    }

    struct FanoutHarness {
        manager: crate::multi_hls::MultiConsoleHls,
        fail_restart: Arc<Mutex<Vec<(u32, HlsEncoder)>>>,
        restarts: Arc<Mutex<Vec<(u32, HlsEncoder, u32)>>>,
    }

    async fn fanout_harness(label: &str, consoles: &[u32]) -> FanoutHarness {
        let root =
            std::env::temp_dir().join(format!("ozma-codec-fanout-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fail_restart = Arc::new(Mutex::new(Vec::new()));
        let restarts = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(FanoutHlsFactory {
            fail_restart: fail_restart.clone(),
            restarts: restarts.clone(),
        });
        let manager = crate::multi_hls::MultiConsoleHls::new(
            factory,
            crate::multi_hls::MultiConsoleHlsConfig {
                max_pipelines: 2,
                idle_timeout: Duration::ZERO,
                root: root.clone(),
                base: HlsConfig {
                    output_dir: root,
                    ..HlsConfig::default()
                },
                audio_source_template: None,
                audio_bitrate_kbps: crate::hls::DEFAULT_AUDIO_BITRATE_KBPS,
            },
        );
        let outputs: Vec<crate::display_outputs::DisplayOutput> = consoles
            .iter()
            .map(|index| crate::display_outputs::DisplayOutput {
                index: *index,
                name: format!("Console_{index}"),
                source_type: "dbus".into(),
                capture_source_id: format!("dbus{index}"),
                width: 1280,
                height: 800,
                primary: *index == 0,
            })
            .collect();
        manager
            .sync(&crate::display_outputs::DisplayOutputSelection {
                outputs,
                active_console: consoles.first().copied().unwrap_or(0),
                limitation: None,
            })
            .await;
        FanoutHarness {
            manager,
            fail_restart,
            restarts,
        }
    }

    fn fanout_control(manager: crate::multi_hls::MultiConsoleHls) -> CodecControl {
        let registry = EncoderRegistry::from_capabilities([
            software_capability(Codec::H264),
            software_capability(Codec::Hevc),
            hardware_capability(Codec::H264, EncoderBackend::Vaapi),
        ]);
        CodecControl::new(
            registry,
            MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
        )
        .with_multi_console_hls(Some(manager))
    }

    fn fanout_state(
        control: CodecControl,
        manager: crate::multi_hls::MultiConsoleHls,
    ) -> ConnectionState {
        ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_codec_control(Some(control))
        .with_multi_console_hls(Some(manager))
    }

    #[tokio::test]
    async fn codec_route_fans_out_to_every_per_console_pipeline() {
        let harness = fanout_harness("all", &[0, 1]).await;
        let manager = harness.manager.clone();
        let state = fanout_state(fanout_control(manager.clone()), manager.clone());
        let app = codec_router(state.clone());

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "bitrate": "2M", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true, "{body}");
        assert_eq!(body["pending_restart"], false);
        assert_eq!(body["encoder"], "libx265");
        assert_eq!(body["fan_out"]["enabled"], true);
        assert_eq!(body["fan_out"]["targeted"], 2);
        assert_eq!(body["fan_out"]["applied"], true);
        assert_eq!(body["pipelines"].as_array().unwrap().len(), 2);
        assert_eq!(body["pipelines"][0]["id"], "console-0");
        assert_eq!(body["pipelines"][1]["id"], "console-1");
        assert_eq!(body["pipelines"][1]["status"], "applied");
        assert_eq!(body["pipelines"][1]["encoder"], "libx265");
        assert_eq!(
            harness.restarts.lock().unwrap().clone(),
            vec![(0, HlsEncoder::Hevc, 2_000), (1, HlsEncoder::Hevc, 2_000)]
        );

        let (status, body) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["applied"], true);
        assert_eq!(body["fan_out"]["running"], 2);
        assert_eq!(body["pipelines"][0]["encoder"], "libx265");
        assert_eq!(body["pipelines"][1]["encoder"], "libx265");
        assert_eq!(body["pipelines"][1]["console"], 1);

        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(connection["codec"]["applied"], true);
        assert_eq!(connection["codec"]["fan_out"]["targeted"], 2);
        assert_eq!(
            connection["multi_console_hls"]["consoles"][0]["encoder"],
            "libx265"
        );
        assert_eq!(
            connection["multi_console_hls"]["consoles"][1]["encoder"],
            "libx265"
        );
    }

    #[tokio::test]
    async fn codec_route_fanout_failure_rolls_back_and_never_reports_applied() {
        let harness = fanout_harness("rollback", &[0, 1]).await;
        harness
            .fail_restart
            .lock()
            .unwrap()
            .push((1, HlsEncoder::Hevc));
        let manager = harness.manager.clone();
        let state = fanout_state(fanout_control(manager.clone()), manager.clone());
        let app = codec_router(state);

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "bitrate": "2M", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["details"]["applied"], false);
        assert_eq!(body["details"]["rolled_back"], true);
        assert_eq!(
            body["details"]["failed_consoles"],
            serde_json::json!(["console-1"])
        );
        assert_eq!(body["details"]["pipelines"][0]["status"], "rolled-back");
        assert_eq!(body["details"]["pipelines"][1]["status"], "failed");
        assert!(
            body.get("applied").is_none(),
            "a failed fan-out must not return a top-level applied field"
        );

        let status = manager.status().await;
        assert_eq!(status.consoles[0].encoder.as_deref(), Some("libx264"));
        assert_eq!(status.consoles[1].encoder.as_deref(), Some("libx264"));
        let (status, codec) = get_json(&app, "/codec").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(codec["applied"], false);
        assert_eq!(codec["fan_out"]["rolled_back"], true);
        assert_eq!(codec["fan_out"]["failed"], serde_json::json!(["console-1"]));
    }

    #[tokio::test]
    async fn codec_route_fanout_reports_a_failed_rollback() {
        let harness = fanout_harness("rollback-failed", &[0, 1]).await;
        harness
            .fail_restart
            .lock()
            .unwrap()
            .extend([(1, HlsEncoder::Hevc), (0, HlsEncoder::H264)]);
        let manager = harness.manager.clone();
        let app = codec_router(fanout_state(
            fanout_control(manager),
            harness.manager.clone(),
        ));

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["details"]["applied"], false);
        assert_eq!(body["details"]["rolled_back"], false);
        assert_eq!(
            body["details"]["rollback_failed_consoles"],
            serde_json::json!(["console-0"])
        );
        assert_eq!(body["details"]["pipelines"][0]["status"], "rollback-failed");
    }

    #[tokio::test]
    async fn codec_route_fanout_with_no_pipelines_keeps_pending_restart() {
        let harness = fanout_harness("none", &[]).await;
        let manager = harness.manager.clone();
        let app = codec_router(fanout_state(
            fanout_control(manager),
            harness.manager.clone(),
        ));

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "hevc", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], false);
        assert_eq!(body["pending_restart"], true);
        assert_eq!(body["encoder"], Value::Null);
        assert_eq!(body["fan_out"]["enabled"], true);
        assert_eq!(body["fan_out"]["targeted"], 0);
    }

    #[tokio::test]
    async fn codec_route_fanout_rejects_unavailable_hardware_as_typed_502() {
        let harness = fanout_harness("hardware", &[0]).await;
        harness
            .fail_restart
            .lock()
            .unwrap()
            .push((0, HlsEncoder::VaapiH264));
        let manager = harness.manager.clone();
        let app = codec_router(fanout_state(
            fanout_control(manager),
            harness.manager.clone(),
        ));

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "hw_accel": "vaapi"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["code"], codes::ENCODER_UNAVAILABLE);
        assert_eq!(body["details"]["applied"], false);
        assert_eq!(body["details"]["rolled_back"], true);
        assert_eq!(
            body["details"]["failed_consoles"],
            serde_json::json!(["console-0"])
        );
    }

    #[tokio::test]
    async fn audio_routes_are_explicitly_not_implemented_without_a_router() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let app = audio_router(state);

        for path in [
            "/audio/apply",
            "/audio/remove",
            "/audio/play",
            "/audio/stop",
        ] {
            let (status, body) = post_json(&app, path, serde_json::json!({})).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{path}");
            assert_eq!(body["ok"], false, "{path}");
            assert_eq!(body["code"], "audio-routing-unsupported", "{path}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .contains("audio routing is not implemented"),
                "{path}: {body}"
            );
            assert!(body["remediation"].is_string(), "{path}");
        }
    }

    fn audio_state(
        router: Option<Arc<FakeAudioRouter>>,
        default_sink: Option<&str>,
    ) -> ConnectionState {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let audio = router.map(|router| {
            AudioControl::configured(router, default_sink.map(str::to_owned), "pipewire")
        });
        state.with_audio(audio)
    }

    fn audio_state_with_store(
        router: Option<Arc<FakeAudioRouter>>,
        default_sink: Option<&str>,
        directory: std::path::PathBuf,
    ) -> ConnectionState {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let audio = router.map(|router| {
            AudioControl::configured(router, default_sink.map(str::to_owned), "pipewire")
                .with_profile_store(Some(Arc::new(RoomProfileStore::new(directory))))
        });
        state.with_audio(audio)
    }

    fn profile_directory(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ozma-audio-profile-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn audio_profile_routes_are_typed_501_without_an_opt_in_directory() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state(Some(router), Some("ozma-vm1")));

        let (status, body) = get_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "audio-profile-store-unsupported");
        assert_eq!(body["profile"], Value::Null);
        assert!(body["remediation"].is_string());

        let (status, body) = delete_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["code"], "audio-profile-store-unsupported");

        let (status, body) = post_json(
            &app,
            "/audio/room-correction/apply",
            serde_json::json!({"profile_id": "sweep-test"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "audio-eq-application-unsupported");
        assert!(body["remediation"].is_string());
    }

    #[tokio::test]
    async fn audio_profile_round_trips_through_the_opt_in_store() {
        let directory = profile_directory("round-trip");
        let router = Arc::new(FakeAudioRouter::new());
        let state =
            audio_state_with_store(Some(router.clone()), Some("ozma-vm1"), directory.clone());
        let app = public_router(state.clone()).merge(audio_router(state));

        let (status, body) = get_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["stored"], false);
        assert_eq!(body["profile"], Value::Null);

        let (status, body) = post_json(
            &app,
            "/audio/sweep",
            serde_json::json!({
                "source": "ozma-vm1.monitor",
                "sink": "ozma-vm1",
                "start_hz": 30,
                "end_hz": 16000,
                "duration": 3,
                "room_name": "Office",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["stored"], true);
        assert_eq!(body["storage_enabled"], true);
        assert!(body["storage_error"].is_null());

        let (status, body) = get_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["stored"], true);
        assert_eq!(body["profile"]["id"], "sweep-test");
        assert_eq!(body["backend"], "pipewire");

        // The POST route accepts either the profile directly or a wrapper.
        let (status, body) = post_json(
            &app,
            "/audio/room-correction/profile",
            serde_json::json!({"profile": body["profile"].clone()}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["stored"], true);
        assert_eq!(body["profile"]["room_name"], "Office");

        let (status, body) = delete_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cleared"], true);
        let (status, body) = get_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["stored"], false);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn audio_profile_store_rejects_invalid_and_corrupt_profiles() {
        let directory = profile_directory("invalid");
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state_with_store(
            Some(router),
            Some("ozma-vm1"),
            directory.clone(),
        ));

        let (status, body) = post_json(
            &app,
            "/audio/room-correction/profile",
            serde_json::json!({"profile": {"id": "broken"}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], audio_codes::AUDIO_INVALID_SPEC);
        assert_eq!(body["profile"], Value::Null);

        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(ozma_audio::ROOM_PROFILE_FILE), b"not json").unwrap();
        let (status, body) = get_json(&app, "/audio/room-correction/profile").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["code"], audio_codes::AUDIO_OUTPUT_PARSE);
        assert_eq!(body["profile"], Value::Null);
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn eq_profile() -> SweepProfile {
        SweepProfile {
            id: "eq-test".to_owned(),
            name: "Office".to_owned(),
            created_at: 1_790_000_000.0,
            bands: vec![SweepBand {
                freq: 1_000.0,
                gain: -3.0,
                q: 1.4,
                band_type: "peaking".to_owned(),
            }],
            band_count: 1,
            mic_type: "phone".to_owned(),
            phone_model: "generic".to_owned(),
            mic_model: String::new(),
            target_curve: "harman".to_owned(),
            room_name: "Office".to_owned(),
            node_id: "node-a".to_owned(),
            active: false,
            measurement: SweepMeasurement {
                sample_rate: 48_000,
                channels: 1,
                captured_frames: 74_400,
                captured_secs: 1.55,
                peak: 0.5,
                peak_dbfs: -6.0,
                rms_dbfs: -12.0,
                noise_floor_dbfs: -80.0,
                impulse_peak: 0.5,
                impulse_index: 18_000,
                impulse_index_secs: 0.375,
                rt60_secs: Some(0.4),
                decay_db_per_sec: Some(-150.0),
                sweep: SweepDescriptor {
                    start_hz: 100.0,
                    end_hz: 8_000.0,
                    duration_secs: 1.0,
                },
            },
            analysis: None,
        }
    }

    #[tokio::test]
    async fn audio_room_eq_applies_stored_profile_persists_and_deletes() {
        let directory = profile_directory("eq-apply");
        let store = RoomProfileStore::new(&directory);
        store.store(&eq_profile()).unwrap();
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state_with_store(
            Some(router.clone()),
            Some("ozma-vm1"),
            directory.clone(),
        ));
        let before = store.load_active().unwrap();
        assert!(before.is_none());

        let (status, body) = post_json(
            &app,
            "/audio/room-correction/apply",
            serde_json::json!({"profile_id": "eq-test"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["active"], true);
        assert_eq!(body["profile_id"], "eq-test");
        assert_eq!(body["target_sink"], "ozma-vm1");
        assert_eq!(body["module_id"], 4242);
        assert_eq!(body["active_bands"], 1);
        assert_eq!(body["band_count"], 1);
        assert_eq!(body["insertion"], "pipewire-filter-chain-smart-sink");
        let eq_sink = body["eq_sink"].as_str().unwrap();
        assert!(eq_sink.starts_with("ozma-eq-eq-test-"), "{eq_sink}");

        // State is written only after the router reported a loaded module.
        let active = store
            .load_active()
            .expect("read active state")
            .expect("active state after apply");
        assert_eq!(active.profile_id, "eq-test");
        assert_eq!(active.eq_sink, eq_sink);
        assert_eq!(
            router.active_eq().as_ref().map(|a| a.eq_sink.clone()),
            Some(eq_sink.to_owned())
        );
        let calls = router.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].starts_with("eq-apply:eq-test:ozma-vm1:ozma-eq-eq-test-"),
            "{calls:?}"
        );

        let (status, body) = delete_json(&app, "/audio/room-correction/apply").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["active"], false);
        assert_eq!(body["removed"], true);
        assert_eq!(body["eq_sink"], eq_sink);
        assert!(store.load_active().unwrap().is_none());
        assert!(router.active_eq().is_none());
        assert_eq!(router.calls().last().map(String::as_str), Some("eq-remove"));

        let (status, body) = delete_json(&app, "/audio/room-correction/apply").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["removed"], false);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn audio_room_eq_rejects_missing_invalid_and_mismatched_profiles() {
        let directory = profile_directory("eq-invalid");
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state_with_store(
            Some(router.clone()),
            Some("ozma-vm1"),
            directory.clone(),
        ));

        // No stored profile and no inline profile: a typed 404, not a fake apply.
        let (status, body) =
            post_json(&app, "/audio/room-correction/apply", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "audio-profile-missing");
        assert_eq!(body["active"], false);
        assert!(router.calls().is_empty());

        // An out-of-range gain is rejected before the router is asked.
        let mut invalid = eq_profile();
        invalid.bands[0].gain = 13.0;
        let (status, body) = post_json(
            &app,
            "/audio/room-correction/apply",
            serde_json::json!({"profile": invalid}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], audio_codes::AUDIO_INVALID_SPEC);
        assert!(router.calls().is_empty());

        // A stored profile exists, but the requested id does not match it.
        RoomProfileStore::new(&directory)
            .store(&eq_profile())
            .unwrap();
        let (status, body) = post_json(
            &app,
            "/audio/room-correction/apply",
            serde_json::json!({"profile_id": "other-profile"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], audio_codes::AUDIO_INVALID_SPEC);
        assert!(router.calls().is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn audio_room_eq_failure_does_not_persist_active_state() {
        let directory = profile_directory("eq-failure");
        let store = RoomProfileStore::new(&directory);
        store.store(&eq_profile()).unwrap();
        // Simulate a previous active chain: a failed replacement must not leave
        // its stale record claiming that EQ is still loaded.
        let previous = RoomEqApplication::from_spec(
            &RoomEqSpec {
                profile_id: "old-profile".to_owned(),
                target_sink: "ozma-vm1".to_owned(),
                eq_sink: "ozma-eq-old-profile-9".to_owned(),
                bands: eq_profile().bands,
                rate: 48_000,
                channels: 2,
            },
            99,
        );
        store.store_active(&previous).unwrap();
        let router = Arc::new(FakeAudioRouter::failing(AudioError::Process {
            command: "pipewire -c".to_owned(),
            message: "filter chain did not publish the sink".to_owned(),
        }));
        let app = audio_router(audio_state_with_store(
            Some(router),
            Some("ozma-vm1"),
            directory.clone(),
        ));

        let (status, body) =
            post_json(&app, "/audio/room-correction/apply", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["active"], false);
        assert_eq!(body["code"], audio_codes::AUDIO_PROCESS_FAILED);
        assert!(
            store.load_active().unwrap().is_none(),
            "a failed apply must not leave an active record"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn audio_routes_apply_remove_play_and_stop_through_the_router() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state(Some(router.clone()), Some("ozma-default")));

        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({
                "profile_id": "profile-a",
                "sink": "ozma-vm1",
                "volume": 42,
                "muted": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["profile_id"], "profile-a");
        assert_eq!(body["sink"], "ozma-vm1");
        assert_eq!(body["backend"], "pipewire");

        // A missing sink derives the configured default sink.
        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({"profile_id": "profile-b", "volume": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sink"], "ozma-default");

        let (status, body) = post_json(
            &app,
            "/audio/remove",
            serde_json::json!({"profile_id": "profile-a", "sink": "ozma-vm1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profile_id"], "profile-a");

        // The Python controller proxies an empty body; remove the profile most
        // recently applied instead of failing.
        let (status, body) = post_json(&app, "/audio/remove", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["profile_id"], "profile-b");

        let (status, body) = post_json(
            &app,
            "/audio/play",
            serde_json::json!({"track": "sweep.flac", "sink": "ozma-vm1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["track"], "sweep.flac");

        let (status, body) = post_json(&app, "/audio/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);

        assert_eq!(
            router.calls(),
            vec![
                "apply:profile-a:ozma-vm1:None:Some(42):Some(true)",
                "apply:profile-b:ozma-default:None:Some(0):None",
                "remove:profile-a",
                "remove:profile-b",
                "play:sweep.flac:ozma-vm1",
                "stop",
            ]
        );
    }

    #[tokio::test]
    async fn audio_apply_routes_source_profiles_through_the_router() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state(Some(router.clone()), Some("ozma-default")));

        // A source-only request needs no sink and must not fall back to the
        // configured default sink.
        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({
                "profile_id": "mic-profile",
                "source": "ozma-vm1-mic",
                "volume": 30,
                "muted": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["profile_id"], "mic-profile");
        assert!(body["sink"].is_null());
        assert_eq!(body["source"], "ozma-vm1-mic");

        // With a sink the same route applies both sides through one profile.
        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({
                "profile_id": "duplex",
                "sink": "ozma-vm1",
                "source": "ozma-vm1-mic",
                "volume": 40,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sink"], "ozma-vm1");
        assert_eq!(body["source"], "ozma-vm1-mic");

        let (status, body) = post_json(
            &app,
            "/audio/remove",
            serde_json::json!({"profile_id": "mic-profile"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        assert_eq!(
            router.calls(),
            vec![
                "apply-source:mic-profile:ozma-vm1-mic:Some(30):Some(true)",
                "apply:duplex:ozma-vm1:Some(\"ozma-vm1-mic\"):Some(40):None",
                "remove:mic-profile",
            ]
        );
    }

    #[tokio::test]
    async fn audio_apply_source_request_without_a_router_is_a_typed_501() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let app = audio_router(state);

        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({"profile_id": "mic", "source": "ozma-vm1-mic"}),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "audio-routing-unsupported");
    }

    #[tokio::test]
    async fn public_audio_nodes_returns_the_router_snapshot() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = public_router(audio_state(Some(router.clone()), Some("ozma-vm1")));

        let (status, body) = get_json(&app, "/audio/nodes").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["available"], true);
        assert_eq!(body["backend"], "pipewire");
        assert_eq!(body["default_sink"], "ozma-vm1");
        assert_eq!(body["default_source"], "ozma-vm1.monitor");
        assert_eq!(body["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(body["nodes"][0]["name"], "ozma-vm1");
        assert_eq!(body["nodes"][0]["media_class"], "Audio/Sink");
        assert_eq!(body["nodes"][0]["volume_percent"], 75.0);
        assert_eq!(body["links"], serde_json::json!([]));
        assert_eq!(body["links_available"], false);
        assert_eq!(router.calls(), vec!["graph"]);
    }

    #[tokio::test]
    async fn public_audio_nodes_without_a_router_is_a_typed_503() {
        let app = public_router(audio_state(None, None));

        let (status, body) = get_json(&app, "/audio/nodes").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["available"], false);
        assert_eq!(body["code"], "audio-graph-unavailable");
        assert_eq!(body["nodes"], serde_json::json!([]));
        assert_eq!(body["links"], serde_json::json!([]));
        assert!(body["remediation"].is_string());
    }

    #[tokio::test]
    async fn public_audio_sweep_returns_a_measured_profile() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = public_router(audio_state(Some(router.clone()), Some("ozma-vm1")));

        let (status, body) = post_json(
            &app,
            "/audio/sweep",
            serde_json::json!({
                "source": "ozma-vm1.monitor",
                "sink": "ozma-vm1",
                "start_hz": 30,
                "end_hz": 16000,
                "duration": 3,
                "room_name": "Office",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["available"], true);
        assert_eq!(body["profile"]["id"], "sweep-test");
        assert_eq!(body["profile"]["name"], "Office");
        assert_eq!(body["profile"]["band_count"], 0);
        assert_eq!(body["profile"]["active"], false);
        assert_eq!(body["measurement"]["captured_frames"], 48_000);
        assert_eq!(body["measurement"]["sweep"]["start_hz"], 30.0);
        assert_eq!(
            router.calls(),
            vec!["sweep:ozma-vm1.monitor:ozma-vm1:30.0-16000.0:3.0"]
        );
    }

    #[tokio::test]
    async fn public_audio_sweep_rejects_missing_or_invalid_requests() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = public_router(audio_state(Some(router.clone()), Some("ozma-vm1")));

        for (body, code) in [
            (
                serde_json::json!({"sink": "ozma-vm1"}),
                "audio-missing-source",
            ),
            (
                serde_json::json!({"source": "ozma-vm1.monitor"}),
                "audio-missing-sink",
            ),
        ] {
            let (status, response) = post_json(&app, "/audio/sweep", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
            assert_eq!(response["ok"], false);
            assert_eq!(response["profile"], serde_json::Value::Null);
            assert_eq!(response["code"], code);
        }

        let (status, response) = post_json(
            &app,
            "/audio/sweep",
            serde_json::json!({"source": "bad source", "sink": "ozma-vm1"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_eq!(response["code"], audio_codes::AUDIO_INVALID_SPEC);
        assert!(router.calls().is_empty());
    }

    #[tokio::test]
    async fn public_audio_sweep_without_a_router_is_a_typed_503() {
        let app = public_router(audio_state(None, None));
        let (status, body) = post_json(
            &app,
            "/audio/sweep",
            serde_json::json!({"source": "monitor", "sink": "sink"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["ok"], false);
        assert_eq!(body["available"], false);
        assert_eq!(body["profile"], serde_json::Value::Null);
        assert_eq!(body["code"], audio_codes::AUDIO_UNAVAILABLE);
    }

    #[tokio::test]
    async fn audio_routes_reject_invalid_bodies_without_calling_the_router() {
        let router = Arc::new(FakeAudioRouter::new());
        let app = audio_router(audio_state(Some(router.clone()), None));

        for (path, body, code) in [
            (
                "/audio/apply",
                serde_json::json!({"profile_id": "p", "sink": "s", "volume": 101}),
                "audio-invalid-volume",
            ),
            (
                "/audio/apply",
                serde_json::json!({"profile_id": "p"}),
                "audio-missing-sink",
            ),
            (
                "/audio/apply",
                serde_json::json!({"sink": "s"}),
                "audio-missing-profile",
            ),
            (
                "/audio/apply",
                serde_json::json!({"profile_id": "p", "source": "bad source"}),
                audio_codes::AUDIO_INVALID_SPEC,
            ),
            (
                "/audio/play",
                serde_json::json!({"sink": "s"}),
                "audio-missing-track",
            ),
            (
                "/audio/play",
                serde_json::json!({"track": "../evil.flac", "sink": "s"}),
                audio_codes::AUDIO_INVALID_SPEC,
            ),
        ] {
            let (status, response) = post_json(&app, path, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {response}");
            assert_eq!(response["code"], code, "{path}: {response}");
        }
        assert!(router.calls().is_empty());
    }

    #[tokio::test]
    async fn audio_routes_report_backend_failures_typed() {
        let router = Arc::new(FakeAudioRouter::failing(AudioError::CommandFailed {
            command: "pactl set-sink-volume".to_owned(),
            status: 1,
            stderr: "no such sink".to_owned(),
        }));
        let app = audio_router(audio_state(Some(router), None));

        let (status, body) = post_json(
            &app,
            "/audio/apply",
            serde_json::json!({"profile_id": "p", "sink": "missing"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], audio_codes::AUDIO_COMMAND_FAILED);
        assert!(body["details"].as_str().unwrap().contains("no such sink"));
    }

    #[tokio::test]
    async fn connection_reports_input_sink_and_codec_status() {
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let state = state.with_codecs(CodecStatus::from_codecs([Codec::H264]));
        let (status, body) = get_json(&connection_router(state), "/api/v1/connection").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["input"]["sink_kind"], SINK_FAKE_RECORDING);
        assert_eq!(body["input"]["available"], true);
        assert_eq!(body["input"]["injecting"], false);
        assert_eq!(body["codec"]["current_encoder"], "h264");
        assert_eq!(body["codec"]["available"], serde_json::json!(["h264"]));
        assert_eq!(body["codec"]["pending_restart"], false);
        assert_eq!(body["audio"], "not-configured");
        assert!(body["audio_error"].is_null());
    }

    #[tokio::test]
    async fn connection_reports_audio_running_and_failed_states() {
        let router = Arc::new(FakeAudioRouter::new());
        let running = audio_state(Some(router), Some("ozma"));
        let (status, body) = get_json(&connection_router(running), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["audio"], "running");
        assert!(body["audio_error"].is_null());

        let failed = audio_state(None, None)
            .with_audio(Some(AudioControl::failed("pactl is not installed")));
        let (status, body) =
            get_json(&connection_router(failed.clone()), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["audio"], "failed");
        assert_eq!(body["audio_error"], "pactl is not installed");

        let (status, body) = post_json(
            &audio_router(failed),
            "/audio/apply",
            serde_json::json!({"profile_id": "p", "sink": "s"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "audio-routing-unavailable");
    }

    #[tokio::test]
    async fn input_codec_and_audio_routes_are_merged_behind_the_bearer_guard() {
        let backend: Arc<dyn ozma_api::NodeBackend> =
            Arc::new(crate::backend::SessionBackend::new(
                ozma_protocol::NodeCapabilities::fake(ozma_protocol::NodeId::new("test-node")),
                ozma_auth::ConsentPolicy::for_machine(ozma_auth::MachineClass::Server),
            ));
        let (state, _) = input_state(SINK_FAKE_RECORDING);
        let state = state.with_codecs(CodecStatus::from_codecs([Codec::H264]));
        let app = ozma_api::router::router_with_sinks_and_extra(
            backend,
            None,
            None,
            Some("secret-token".to_owned()),
            Some(compat_router(state)),
        );

        for (method, uri, body) in [
            ("GET", "/codec", None),
            ("POST", "/codec", Some(serde_json::json!({"codec": "h264"}))),
            (
                "POST",
                "/input/key",
                Some(serde_json::json!({"keycode": 30, "down": true})),
            ),
            ("POST", "/audio/apply", Some(serde_json::json!({}))),
            (
                "POST",
                "/webrtc/offer",
                Some(serde_json::json!({"sdp": "v=0", "type": "offer"})),
            ),
            (
                "POST",
                "/webrtc/bitrate",
                Some(serde_json::json!({"bitrate": 4_000_000})),
            ),
            ("GET", "/usb", None),
            ("POST", "/usb/media/attach", Some(serde_json::json!({}))),
            ("POST", "/usb/media/detach", Some(serde_json::json!({}))),
            ("GET", "/usb/nbd", None),
            ("POST", "/usb/nbd/start", Some(serde_json::json!({}))),
            ("POST", "/usb/nbd/stop", Some(serde_json::json!({}))),
            ("GET", "/storage", None),
            ("GET", "/api/v1/storage", None),
            ("GET", "/apps?limit=5", None),
            ("GET", "/api/v1/apps", None),
            ("GET", "/self", None),
            ("GET", "/api/v1/self", None),
            ("GET", "/bluetooth/status", None),
            ("GET", "/bluetooth/devices", None),
            ("POST", "/bluetooth/scan", Some(serde_json::json!({}))),
            (
                "POST",
                "/bluetooth/pair",
                Some(serde_json::json!({"address": "AA:BB:CC:DD:EE:FF"})),
            ),
            (
                "POST",
                "/bluetooth/connect",
                Some(serde_json::json!({"address": "AA:BB:CC:DD:EE:FF"})),
            ),
            (
                "POST",
                "/bluetooth/disconnect",
                Some(serde_json::json!({"address": "AA:BB:CC:DD:EE:FF"})),
            ),
            (
                "POST",
                "/bluetooth/forget",
                Some(serde_json::json!({"address": "AA:BB:CC:DD:EE:FF"})),
            ),
            ("GET", "/serial/status", None),
            (
                "POST",
                "/serial/write",
                Some(serde_json::json!({"text": "hi"})),
            ),
            ("GET", "/serial/read?max=16", None),
            ("GET", "/serial/stream?idle_ms=100", None),
            ("GET", "/api/v1/rpa/screen", None),
            (
                "POST",
                "/api/v1/rpa/type",
                Some(serde_json::json!({"text": "hi"})),
            ),
            (
                "POST",
                "/api/v1/rpa/key",
                Some(serde_json::json!({"key": "enter"})),
            ),
            ("GET", "/api/v1/rpa/screenshot", None),
            ("POST", "/api/v1/rpa/ocr", Some(serde_json::json!({}))),
            (
                "POST",
                "/api/v1/rpa/script",
                Some(serde_json::json!({"script": "key enter"})),
            ),
            ("GET", "/backup/snapshots", None),
            ("POST", "/backup/run", Some(serde_json::json!({}))),
            ("GET", "/rgb/state", None),
            ("GET", "/current", None),
            ("GET", "/serial/state", None),
            ("GET", "/display/geometry", None),
            (
                "POST",
                "/display/geometry",
                Some(serde_json::json!({"output": "DP-1", "mode": "1280x720"})),
            ),
            ("GET", "/display/layout", None),
            (
                "POST",
                "/display/layout",
                Some(serde_json::json!({"outputs": [{"output": "DP-1", "mode": "1280x720"}]})),
            ),
            ("GET", "/clipboard", None),
            (
                "POST",
                "/clipboard",
                Some(serde_json::json!({"text": "hi"})),
            ),
            ("GET", "/wallpaper", None),
            (
                "POST",
                "/wallpaper",
                Some(serde_json::json!({"path": "/tmp/a.png"})),
            ),
        ] {
            let builder = Request::builder().method(method).uri(uri);
            let request = match body {
                Some(body) => builder
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
                None => builder.body(Body::empty()).unwrap(),
            };
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must be bearer-guarded"
            );
        }

        let authorized = |method: &str, uri: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", "Bearer secret-token")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        };

        let response = app
            .clone()
            .oneshot(authorized("GET", "/codec"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The desktop routes are behind the same guard; an authorized request
        // on a non-desktop state reaches the typed profile error.
        let response = app
            .clone()
            .oneshot(authorized("GET", "/display/geometry"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);

        let response = app
            .clone()
            .oneshot(authorized("POST", "/audio/apply"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);

        let response = app
            .clone()
            .oneshot(authorized("POST", "/codec"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/input/key")
                    .header("authorization", "Bearer secret-token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"keycode":30,"down":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ozma-usb-{label}-{}-{unique}", std::process::id()))
    }

    fn temp_file(label: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = temp_path(label);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn auto_media_config(
        path: &std::path::Path,
        restart: MediaRestartPolicy,
    ) -> UsbMediaAutoStartConfig {
        auto_media_config_with(path, restart, false, None)
    }

    fn auto_media_config_with(
        path: &std::path::Path,
        restart: MediaRestartPolicy,
        read_write: bool,
        persist: Option<&std::path::Path>,
    ) -> UsbMediaAutoStartConfig {
        UsbMediaAutoStartConfig {
            path: path.to_path_buf(),
            format: UsbMediaFormat::Raw,
            read_only: !read_write,
            persist_path: persist.map(std::path::Path::to_path_buf),
            persist_on_flush: true,
            persist_on_stop: true,
            reload: false,
            timeout_ms: None,
            max_transfer_bytes: None,
            restart,
            functionfs_mount: None,
        }
    }

    fn usb_fake_state() -> (ConnectionState, Arc<FakeUsbGadgetController>) {
        let fake = Arc::new(FakeUsbGadgetController::new());
        let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::keyboard("kbd"))
            .with_function(UsbFunction::mouse("mouse"))
            .with_function(UsbFunction::mass_storage("usb0", None));
        fake.create_gadget(&config).unwrap();
        fake.bind("ozma", "dummy_udc.0").unwrap();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::configured(fake.clone(), "ozma")));
        (state, fake)
    }

    #[tokio::test]
    async fn usb_status_reports_uac2_bridge_and_functionfs_readiness_absence() {
        let fake = Arc::new(FakeUsbGadgetController::new());
        let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::uac2("audio", 2, 1, 48_000, 16))
            .with_function(UsbFunction::vendor("vendor0", 0x1d6b, 0x0106));
        fake.create_gadget(&config).unwrap();
        fake.bind("ozma", "dummy_udc.0").unwrap();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::configured(fake, "ozma")));

        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        let functions = body["gadget"]["functions"].as_array().unwrap();
        let audio = functions
            .iter()
            .find(|function| function["name"] == "uac2.audio")
            .unwrap();
        assert_eq!(audio["subtype"], "audio-uac2");
        assert_eq!(audio["uac2"]["bridge"], "unsupported");
        assert_eq!(audio["uac2"]["mode"], "headset");
        assert_eq!(audio["uac2"]["descriptors"]["endpoints"], 2);
        let vendor = functions
            .iter()
            .find(|function| function["name"] == "ffs.vendor0")
            .unwrap();
        assert_eq!(vendor["subtype"], "vendor");
        assert_eq!(vendor["requires_userspace"], true);
        assert_eq!(vendor["function_fs"]["ready"], false);
        assert_eq!(vendor["function_fs"]["readiness"], "daemon_not_configured");
    }

    #[tokio::test]
    async fn usb_status_defaults_usbip_to_kernel_required_without_counters() {
        let (state, _) = usb_fake_state();
        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["usbip"]["state"], "not-configured");
        assert_eq!(body["usbip"]["adapter"], "kernel-required");
        assert_eq!(body["usbip"]["kernel_required"], true);
        assert_eq!(body["usbip"]["data_sessions"], 0);
    }

    #[tokio::test]
    async fn media_auto_start_selects_the_polling_functionfs_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base.img");
        std::fs::write(&base, vec![0u8; SCSI_SECTOR_BYTES * 4]).unwrap();
        let mountpoint = temp.path().join("dev/ffs-vendor0");

        let fake = Arc::new(FakeUsbGadgetController::new());
        let gadget = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::vendor("vendor0", 0x1d6b, 0x0106));
        let mut handoff =
            FunctionFsHandoff::new(fake.clone(), "ozma", "vendor0", "dummy_udc.0", &mountpoint)
                .unwrap()
                .with_mounter(FakeFunctionFsMounter::new().with_endpoints([1, 2]));
        handoff.prepare(&gadget).unwrap();
        handoff.mount().unwrap();
        let mut control = UsbControl::configured(fake.clone(), "ozma");
        control.set_functionfs_handoff(Some(handoff));
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(control.clone()));

        let auto = UsbMediaAutoStartConfig {
            path: base,
            format: UsbMediaFormat::Raw,
            read_only: true,
            persist_path: None,
            persist_on_flush: true,
            persist_on_stop: true,
            reload: false,
            timeout_ms: Some(50),
            max_transfer_bytes: None,
            restart: MediaRestartPolicy::default(),
            functionfs_mount: Some(mountpoint.clone()),
        };
        control.auto_start_media_export(&auto);
        assert!(fake.is_bound("ozma"), "the daemon wrote ep0 before bind");
        assert_eq!(
            control.functionfs_phase(),
            Some(FunctionFsHandoffPhase::Bound)
        );

        // The worker writes descriptors and waits for ENABLE; no fake transport
        // may report `running` before a real command cycle.
        let mut body = Value::Null;
        for _ in 0..100 {
            let (status, candidate) = get_json(&usb_router(state.clone()), "/usb/media").await;
            assert_eq!(status, StatusCode::OK);
            body = candidate;
            if body["state"] == "waiting-for-enable" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(body["state"], "waiting-for-enable");
        assert_eq!(body["running"], false);
        assert_eq!(body["stats"]["commands"], 0);
        assert_eq!(body["transport"]["kind"], "polling-file");
        assert_eq!(body["transport"]["attached"], false);
        assert_eq!(body["transport"]["live"], false);

        let router = usb_router(state.clone());
        let (status, body) = post_json(&router, "/usb/media/stop", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["running"], false);
        let (_, body) = get_json(&usb_router(state), "/usb/media").await;
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["running"], false);
        assert_eq!(
            fake.unbind_calls(),
            1,
            "shutdown must unbind once and not require a second external unbind"
        );
        assert!(
            fake.status("ozma").is_err(),
            "the function and gadget were removed after shutdown"
        );
        assert!(!mountpoint.exists(), "the FunctionFS mount was removed");
        assert_eq!(control.functionfs_phase(), None);
        control.shutdown_media_export().unwrap();
    }

    #[tokio::test]
    async fn usb_status_overlays_configured_bridge_without_claiming_live_audio() {
        use ozma_usb::{
            Uac2BridgeCounters, Uac2BridgeReport, Uac2BridgeStatus, Uac2BridgeStreamReport,
            Uac2Direction, Uac2PcmFormat, Uac2SampleSize, UAC2_PLAYBACK_ENDPOINT,
        };

        let format = Uac2PcmFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_size: Uac2SampleSize::S16,
        };
        let report = Uac2BridgeReport {
            status: Uac2BridgeStatus::Configured,
            playback: Some(Uac2BridgeStreamReport {
                endpoint: UAC2_PLAYBACK_ENDPOINT,
                direction: Uac2Direction::Out,
                format,
                packet_interval: 1,
                max_packet_size: 1024,
                maximum_packet_bytes: 24,
            }),
            capture: None,
            counters: Uac2BridgeCounters::default(),
            missing_paths: Vec::new(),
            reason: None,
        };
        let state = uac2_usb_state(Some(crate::usb_uac2::Uac2BridgeShared::from_report(report)));
        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        let audio = body["gadget"]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|function| function["name"] == "uac2.audio")
            .unwrap();
        assert_eq!(audio["uac2"]["bridge"], "configured");
        assert_eq!(audio["uac2"]["bridge_report"]["status"], "configured");
        assert_eq!(
            audio["uac2"]["bridge_report"]["playback"]["endpoint"],
            UAC2_PLAYBACK_ENDPOINT
        );
        assert_eq!(
            audio["uac2"]["bridge_report"]["playback"]["format"]["sample_rate"],
            48_000
        );
        assert_eq!(
            audio["uac2"]["bridge_report"]["playback"]["max_packet_size"],
            1024
        );
        assert_eq!(audio["uac2"]["bridge_report"]["counters"]["sink_bytes"], 0);
        assert_ne!(audio["uac2"]["bridge"], "running");
    }

    #[tokio::test]
    async fn usb_status_reports_running_only_with_pcm_counters() {
        use ozma_usb::{
            Uac2BridgeCounters, Uac2BridgeReport, Uac2BridgeStatus, Uac2BridgeStreamReport,
            Uac2Direction, Uac2PcmFormat, Uac2SampleSize, UAC2_CAPTURE_ENDPOINT,
        };

        let format = Uac2PcmFormat {
            sample_rate: 48_000,
            channels: 2,
            sample_size: Uac2SampleSize::S16,
        };
        let mut counters = Uac2BridgeCounters::default();
        counters.sink_bytes = 1_920;
        counters.sink_frames = 480;
        counters.capture_bytes_out = 1_920;
        counters.capture_frames_out = 480;
        let report = Uac2BridgeReport {
            status: Uac2BridgeStatus::Running,
            playback: Some(Uac2BridgeStreamReport {
                endpoint: 0x01,
                direction: Uac2Direction::Out,
                format,
                packet_interval: 1,
                max_packet_size: 1024,
                maximum_packet_bytes: 24,
            }),
            capture: Some(Uac2BridgeStreamReport {
                endpoint: UAC2_CAPTURE_ENDPOINT,
                direction: Uac2Direction::In,
                format,
                packet_interval: 1,
                max_packet_size: 1024,
                maximum_packet_bytes: 24,
            }),
            counters,
            missing_paths: Vec::new(),
            reason: None,
        };
        let state = uac2_usb_state(Some(crate::usb_uac2::Uac2BridgeShared::from_report(report)));
        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        let audio = body["gadget"]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|function| function["name"] == "uac2.audio")
            .unwrap();
        assert_eq!(audio["uac2"]["bridge"], "running");
        assert_eq!(
            audio["uac2"]["bridge_report"]["counters"]["sink_bytes"],
            1_920
        );
        assert_eq!(audio["uac2"]["bridge_report"]["capture"]["direction"], "in");
        assert!(body.get("uac2_bridge_error").is_none());
    }

    #[tokio::test]
    async fn usb_status_reports_unsupported_and_typed_bridge_errors() {
        use ozma_usb::{Uac2BridgeCounters, Uac2BridgeReport, Uac2BridgeStatus};

        let unsupported = Uac2BridgeReport::unsupported("uac2-missing-path: no gadget ALSA card");
        let state = uac2_usb_state(Some(crate::usb_uac2::Uac2BridgeShared::from_report(
            unsupported,
        )));
        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        let audio = body["gadget"]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|function| function["name"] == "uac2.audio")
            .unwrap();
        assert_eq!(audio["uac2"]["bridge"], "unsupported");
        assert_eq!(
            audio["uac2"]["bridge_report"]["reason"],
            "uac2-missing-path: no gadget ALSA card"
        );
        assert_ne!(audio["uac2"]["bridge"], "running");

        let stopped = Uac2BridgeReport {
            status: Uac2BridgeStatus::Stopped,
            playback: None,
            capture: None,
            counters: Uac2BridgeCounters {
                sink_bytes: 64,
                sink_frames: 16,
                ..Uac2BridgeCounters::default()
            },
            missing_paths: Vec::new(),
            reason: None,
        };
        let shared = crate::usb_uac2::Uac2BridgeShared::from_report_with_error(
            stopped,
            crate::usb_uac2::Uac2BridgeError {
                kind: "uac2-endpoint".to_owned(),
                message: "backpressure".to_owned(),
            },
        );
        let state = uac2_usb_state(Some(shared));
        let (status, body) = get_json(&usb_router(state), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        let audio = body["gadget"]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|function| function["name"] == "uac2.audio")
            .unwrap();
        assert_eq!(audio["uac2"]["bridge"], "stopped");
        assert_eq!(body["uac2_bridge_error"]["kind"], "uac2-endpoint");
        assert_eq!(body["uac2_bridge_error"]["message"], "backpressure");
    }

    fn uac2_usb_state(bridge: Option<crate::usb_uac2::Uac2BridgeShared>) -> ConnectionState {
        let fake = Arc::new(FakeUsbGadgetController::new());
        let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::uac2("audio", 2, 1, 48_000, 16));
        fake.create_gadget(&config).unwrap();
        fake.bind("ozma", "dummy_udc.0").unwrap();
        ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(
            UsbControl::configured(fake, "ozma").with_uac2_bridge(bridge),
        ))
    }

    #[tokio::test]
    async fn usb_status_reports_a_bound_fake_gadget_and_connection_state() {
        let (state, _) = usb_fake_state();
        let (status, body) = get_json(&usb_router(state.clone()), "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["available"], true);
        assert_eq!(body["status"], "running");
        assert_eq!(body["gadget"]["name"], "ozma");
        assert_eq!(body["gadget"]["active"], true);
        assert_eq!(body["gadget"]["bound"], true);
        assert_eq!(body["gadget"]["udc"], "dummy_udc.0");
        assert_eq!(body["capabilities"]["gadget"], true);
        assert_eq!(body["capabilities"]["virtual_media"], true);
        assert_eq!(body["capabilities"]["mass_storage"], true);
        assert_eq!(body["attached"], serde_json::json!([]));
        let functions = body["gadget"]["functions"].as_array().unwrap();
        assert_eq!(functions.len(), 3);
        assert_eq!(functions[0]["name"], "hid.kbd");
        assert_eq!(functions[2]["name"], "mass_storage.usb0");
        assert_eq!(functions[2]["subtype"], "mass-storage");

        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(connection["usb"]["status"], "running");
        assert_eq!(connection["usb"]["available"], true);
        assert_eq!(connection["usb"]["bound"], true);
        assert_eq!(connection["usb"]["udc"], "dummy_udc.0");
        assert!(connection["usb"]["error"].is_null());
    }

    #[tokio::test]
    async fn usb_media_attach_and_detach_are_read_only_and_do_not_echo_paths() {
        let (state, _) = usb_fake_state();
        let app = usb_router(state);
        let image = temp_file("media-attach", &vec![0u8; 4096]);

        let (status, body) = post_json(
            &app,
            "/usb/media/attach",
            serde_json::json!({"path": image.to_string_lossy(), "read_only": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["function"], "usb0");
        assert_eq!(body["media"]["state"], "attached");
        assert_eq!(body["media"]["mode"], "disk");
        assert_eq!(body["media"]["read_only"], true);
        assert_eq!(body["media"]["size_bytes"], 4096);
        assert!(
            !body.to_string().contains(image.to_string_lossy().as_ref()),
            "attach response must not echo the backing path: {body}"
        );

        let (status, usb) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(usb["media"][0]["state"], "attached");
        assert_eq!(usb["media"][0]["function"], "usb0");
        assert!(
            !usb.to_string().contains(image.to_string_lossy().as_ref()),
            "status must not expose the backing path: {usb}"
        );

        let (status, body) = post_json(
            &app,
            "/usb/media/detach",
            serde_json::json!({"function": "mass_storage.usb0"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media"]["state"], "detached");
        let (_, usb) = get_json(&app, "/usb").await;
        assert_eq!(usb["media"][0]["state"], "detached");

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_attach_validates_paths_and_cdrom_mode() {
        let (state, _) = usb_fake_state();
        let app = usb_router(state);
        let image = temp_file("media-cdrom", &vec![0u8; 2048]);

        let (status, body) = post_json(
            &app,
            "/usb/media/attach",
            serde_json::json!({"path": "relative.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media-path");

        let (status, body) = post_json(
            &app,
            "/usb/media/attach",
            serde_json::json!({"path": "/does/not/exist/ozma.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-media-not-found");
        assert!(
            !body.to_string().contains("/does/not/exist/ozma.img"),
            "media errors must not echo the path: {body}"
        );

        let (status, body) = post_json(
            &app,
            "/usb/media/attach",
            serde_json::json!({"path": image.to_string_lossy(), "cdrom": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media"]["mode"], "cdrom");
        assert_eq!(body["media"]["read_only"], true);
        assert_eq!(body["media"]["size_bytes"], 2048);

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_nbd_start_status_and_stop_are_opt_in_and_path_free() {
        let (state, _) = usb_fake_state();
        let app = usb_router(state);
        let image = temp_file("nbd-media", &vec![0x5Au8; 4096]);

        let (status, body) = get_json(&app, "/usb/nbd").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["running"], false);

        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({"path": image.to_string_lossy(), "format": "raw"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["running"], true);
        assert_eq!(body["export"], "media");
        assert_eq!(body["size_bytes"], 4096);
        assert_eq!(body["read_only"], true);
        let url = body["url"].as_str().unwrap();
        assert!(url.starts_with("nbd://127.0.0.1:"), "{url}");
        assert!(url.ends_with("/media"), "{url}");
        assert!(
            !body.to_string().contains(image.to_string_lossy().as_ref()),
            "NBD start response must not echo the backing path: {body}"
        );

        let (status, body) = get_json(&app, "/usb/nbd").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["running"], true);
        assert_eq!(body["export"], "media");

        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({"path": image.to_string_lossy()}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "usb-nbd-already-running");

        let (status, body) = post_json(&app, "/usb/nbd/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["running"], false);
        assert_eq!(body["stats"]["connections_accepted"], 0);

        let (status, body) = post_json(&app, "/usb/nbd/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "usb-nbd-not-running");

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_nbd_start_rejects_bad_paths_and_writable_iso_or_fat32() {
        let (state, _) = usb_fake_state();
        let app = usb_router(state);
        let image = temp_file("nbd-bad-format", &vec![0u8; 4096]);

        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({"path": "relative.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media-path");

        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({"path": "/does/not/exist/ozma.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-media-not-found");
        assert!(!body.to_string().contains("/does/not/exist/ozma.img"));

        for format in ["iso9660", "fat32"] {
            let (status, body) = post_json(
                &app,
                "/usb/nbd/start",
                serde_json::json!({
                    "path": image.to_string_lossy(),
                    "format": format,
                    "read_only": false,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{format}");
            assert_eq!(body["code"], "usb-read-only-media", "{format}");
        }

        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({"path": image.to_string_lossy(), "format": "vhd"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media-format");

        // A file that is not really ISO 9660 is rejected by the bounded probe.
        let (status, body) = post_json(
            &app,
            "/usb/nbd/start",
            serde_json::json!({
                "path": image.to_string_lossy(),
                "format": "iso9660",
                "read_only": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media");

        std::fs::remove_file(image).unwrap();
    }

    fn media_cbw(opcode: u8, cdb: &[u8], transfer_length: u32, direction_in: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; 31];
        bytes[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&transfer_length.to_le_bytes());
        bytes[12] = if direction_in { 0x80 } else { 0x00 };
        bytes[13] = 0;
        bytes[14] = cdb.len() as u8;
        bytes[15..15 + cdb.len()].copy_from_slice(cdb);
        bytes[15] = opcode;
        bytes
    }

    fn media_cdb10(lba: u32, count: u16) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&count.to_be_bytes());
        cdb
    }

    fn media_read10(lba: u32, count: u16) -> Vec<u8> {
        let mut cdb = media_cdb10(lba, count);
        cdb[0] = 0x28;
        media_cbw(0x28, &cdb, count as u32 * SCSI_SECTOR_BYTES as u32, true)
    }

    fn media_write10(lba: u32, count: u16, data: &[u8]) -> Vec<u8> {
        let mut cdb = media_cdb10(lba, count);
        cdb[0] = 0x2a;
        media_cbw(0x2a, &cdb, data.len() as u32, false)
    }

    fn media_sync_cache() -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x35;
        media_cbw(0x35, &cdb, 0, false)
    }

    fn media_test_unit_ready() -> Vec<u8> {
        media_cbw(0x00, &[0u8; 6], 0, false)
    }

    #[tokio::test]
    async fn usb_media_export_reports_configured_running_and_stopped() {
        let bare = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        );
        let (status, body) = get_json(&usb_router(bare), "/usb/media").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "usb.unsupported");

        let (state, _fake) = usb_fake_state();
        let control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-export", &vec![0u8; 4096]);

        let (status, body) = get_json(&app, "/usb/media").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "not-configured");
        assert_eq!(body["running"], false);

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({
                "path": image.to_string_lossy(),
                "read_only": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "configured");
        assert_eq!(body["running"], false);
        assert_eq!(body["read_only"], true);
        assert_eq!(body["capacity"]["sector_size"], SCSI_SECTOR_BYTES);
        assert_eq!(body["capacity"]["sectors"], 8);
        assert_eq!(body["capacity"]["bytes"], 4096);
        assert_eq!(body["stats"]["commands"], 0);
        assert!(
            !body.to_string().contains(image.to_string_lossy().as_ref()),
            "media export start must not echo the backing path: {body}"
        );

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "configured");
        assert_eq!(body["media_export"]["running"], false);

        // A configured export is not running until a command cycle completes.
        let mut transport = MemoryBulkTransport::new();
        transport.push_host_data(&media_test_unit_ready()).unwrap();
        control.media_attach_test_transport(transport).unwrap();
        let outcome = control.media_step_test().unwrap().unwrap();
        assert_eq!(outcome.status, ScsiStatus::Good);

        let (status, body) = get_json(&app, "/usb/media").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "running");
        assert_eq!(body["running"], true);
        assert_eq!(body["stats"]["commands"], 1);
        assert_eq!(body["stats"]["csws"], 1);

        let (status, body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["running"], false);

        let (status, body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "usb-media-not-running");

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_export_does_not_require_a_kernel_mass_storage_function() {
        let fake = Arc::new(FakeUsbGadgetController::new());
        let config =
            UsbGadgetConfig::new("ozma", "dummy_udc.0").with_function(UsbFunction::keyboard("kbd"));
        fake.create_gadget(&config).unwrap();
        fake.bind("ozma", "dummy_udc.0").unwrap();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::configured(fake, "ozma")));
        let app = usb_router(state);
        let image = temp_file("media-no-kernel-lun", &vec![0u8; 4096]);

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "read_only": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "configured");
        assert_eq!(body["function"], "usb0");

        let (status, _body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_export_writable_overlay_persists_and_reloads() {
        let (state, _fake) = usb_fake_state();
        let control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let original = vec![0x11u8; 4096];
        let base = temp_file("media-overlay-base", &original);
        let persistence = temp_path("media-overlay-persist");

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({
                "path": base.to_string_lossy(),
                "read_only": false,
                "persist_path": persistence.to_string_lossy(),
                "persist_on_stop": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "configured");
        assert_eq!(body["read_only"], false);
        assert_eq!(body["overlay"]["persistence"], true);
        assert_eq!(body["overlay"]["dirty"], false);

        let mut payload = vec![0x5au8; SCSI_SECTOR_BYTES];
        payload[7] = 0x42;
        let mut transport = MemoryBulkTransport::new();
        transport
            .push_host_data(&media_write10(0, 1, &payload))
            .unwrap();
        transport.push_host_data(&payload).unwrap();
        transport.push_host_data(&media_read10(0, 1)).unwrap();
        transport.push_host_data(&media_sync_cache()).unwrap();
        control.media_attach_test_transport(transport).unwrap();

        let write = control.media_step_test().unwrap().unwrap();
        assert_eq!(write.status, ScsiStatus::Good);
        let read = control.media_step_test().unwrap().unwrap();
        assert_eq!(read.status, ScsiStatus::Good);
        assert_eq!(read.data_in_bytes, SCSI_SECTOR_BYTES);
        let flush = control.media_step_test().unwrap().unwrap();
        assert_eq!(flush.status, ScsiStatus::Good);

        let (status, body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["overlay"]["dirty"], false);
        assert_eq!(body["overlay"]["allocated_blocks"], 1);
        assert!(persistence.is_file(), "stop must persist the overlay");
        assert_eq!(std::fs::read(&base).unwrap(), original);

        let mut reloaded = MassStorageDevice::load_writable_file(
            &base,
            OverlayConfig::default().with_persistence(&persistence),
            ozma_usb::MassStoragePersistPolicy::manual(),
            &persistence,
        )
        .unwrap();
        let mut sector = vec![0u8; SCSI_SECTOR_BYTES];
        reloaded.read_at(0, &mut sector).unwrap();
        assert_eq!(sector, payload);

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({
                "path": base.to_string_lossy(),
                "read_only": false,
                "persist_path": persistence.to_string_lossy(),
                "reload": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "configured");
        assert_eq!(body["overlay"]["allocated_blocks"], 1);

        std::fs::remove_file(base).unwrap();
        std::fs::remove_file(persistence).unwrap();
    }

    #[tokio::test]
    async fn usb_media_export_rejects_bad_requests_and_reports_typed_errors() {
        let (state, _fake) = usb_fake_state();
        let control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-bad", &vec![0u8; 4096]);

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": "relative.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media-path");

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": "/does/not/exist/ozma.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-media-not-found");
        assert!(!body.to_string().contains("/does/not/exist/ozma.img"));

        for format in ["iso9660", "fat32"] {
            let (status, body) = post_json(
                &app,
                "/usb/media/start",
                serde_json::json!({
                    "path": image.to_string_lossy(),
                    "format": format,
                    "read_only": false,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{format}");
            assert_eq!(body["code"], "usb-read-only-media", "{format}");
        }

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "format": "vhd"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media-format");

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "format": "iso9660"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "usb-invalid-media");

        let (status, _body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "read_only": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "read_only": true}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "usb-media-already-running");

        let mut transport = MemoryBulkTransport::new();
        transport.set_read_fault(Some(BulkTransportFault::Timeout));
        control.media_attach_test_transport(transport).unwrap();
        let error = control.media_step_test().expect_err("stalled endpoint");
        assert_eq!(error.kind(), "mass-storage-endpoint-timeout");

        let (status, body) = get_json(&app, "/usb/media").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "failed");
        assert_eq!(body["running"], false);
        assert_eq!(body["last_error"]["kind"], "mass-storage-endpoint-timeout");

        let (status, body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "stopped");

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_hotplug_view_is_bounded_and_never_fabricates_events() {
        let (state, fake) = usb_fake_state();
        let app = usb_router(state);
        let image = temp_file("media-hotplug", &vec![0u8; 4096]);

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
        assert_eq!(body["capacity"], 64);
        assert_eq!(body["next_sequence"], 1);

        let (status, _body) = post_json(
            &app,
            "/usb/media/attach",
            serde_json::json!({"path": image.to_string_lossy()}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _body) = post_json(&app, "/usb/media/detach", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        let kinds: Vec<&str> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["attach", "detach"]);

        let (status, _body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": image.to_string_lossy(), "read_only": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _body) = post_json(&app, "/usb/media/stop", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = get_json(&app, "/usb/hotplug?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "export_stop");
        let last_sequence = events[0]["sequence"].as_u64().unwrap();

        let (status, body) = get_json(&app, "/usb/hotplug?since=0&limit=2").await;
        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], "export_start");
        assert_eq!(events[1]["kind"], "export_stop");

        let (status, body) = get_json(&app, &format!("/usb/hotplug?since={last_sequence}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["events"].as_array().unwrap().len(), 0);

        fake.unbind("ozma").unwrap();
        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        let last = body["events"].as_array().unwrap().last().unwrap();
        assert_eq!(last["kind"], "unbind");
        assert_eq!(last["state"], "unbound");

        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_auto_start_reports_not_configured_configured_and_no_fake_running() {
        let (state, _fake) = usb_fake_state();
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-auto-start", &vec![0u8; 4096]);

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "not-configured");
        assert_eq!(body["media_export"]["auto_start"], false);
        assert_eq!(body["media_export"]["running"], false);

        control.auto_start_media_export(&auto_media_config(&image, MediaRestartPolicy::default()));

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "configured");
        assert_eq!(body["media_export"]["configured"], true);
        assert_eq!(body["media_export"]["auto_start"], true);
        assert_eq!(
            body["media_export"]["running"], false,
            "auto-start must not claim running without a completed transfer"
        );
        assert_eq!(body["media_export"]["stats"]["commands"], 0);
        assert_eq!(body["media_export"]["restart"]["max_attempts"], 3);
        assert!(
            !body.to_string().contains(image.to_string_lossy().as_ref()),
            "auto-start status must not echo the backing path: {body}"
        );

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "export_start");
        assert_eq!(events[0]["state"], "auto-start");

        control.shutdown_media_export().unwrap();
        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_auto_start_failure_is_typed_and_not_running() {
        let fake = Arc::new(FakeUsbGadgetController::new());
        let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::mass_storage("usb0", None));
        fake.create_gadget(&config).unwrap();
        fake.bind("ozma", "dummy_udc.0").unwrap();
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::configured(fake, "ozma")));
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let missing = temp_path("media-auto-missing");

        control
            .auto_start_media_export(&auto_media_config(&missing, MediaRestartPolicy::default()));

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "failed");
        assert_eq!(body["media_export"]["running"], false);
        assert_eq!(body["media_export"]["configured"], false);
        assert_eq!(body["media_export"]["auto_start"], true);
        assert_eq!(
            body["media_export"]["last_error"]["kind"],
            "media-not-found"
        );
        assert!(
            !body
                .to_string()
                .contains(missing.to_string_lossy().as_ref()),
            "auto-start failure must not echo the backing path: {body}"
        );

        let (status, body) = get_json(&app, "/usb/media").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "failed");
        assert_eq!(body["running"], false);
    }

    #[tokio::test]
    async fn usb_media_transport_error_restarts_with_backoff_and_recovers() {
        let (state, _fake) = usb_fake_state();
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-restart-recover", &vec![0u8; 4096]);

        control.auto_start_media_export(&auto_media_config(
            &image,
            MediaRestartPolicy::new(2, 25, 100),
        ));

        let mut fault = MemoryBulkTransport::new();
        fault.set_read_fault(Some(BulkTransportFault::Timeout));
        control.media_attach_test_transport(fault).unwrap();
        assert!(control.media_step_test().is_err());

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "failed");
        assert_eq!(body["media_export"]["running"], false);
        assert_eq!(body["media_export"]["restart"]["attempts"], 0);
        assert_eq!(
            body["media_export"]["last_error"]["kind"],
            "mass-storage-endpoint-timeout"
        );
        let transport_errors = body["media_export"]["stats"]["timeout_errors"]
            .as_u64()
            .unwrap();
        assert_eq!(transport_errors, 1);

        assert_eq!(
            control.restart_media_export(MediaRestartReason::TransportError),
            MediaRestartOutcome::Restarted {
                attempt: 1,
                backoff_ms: 25
            }
        );

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "configured");
        assert_eq!(body["media_export"]["running"], false);
        assert_eq!(body["media_export"]["restart"]["attempts"], 1);
        assert!(body["media_export"]["restart"]["pending_backoff_ms"].is_null());
        assert_eq!(
            body["media_export"]["stats"]["timeout_errors"], transport_errors,
            "counters must survive the supervised restart"
        );

        let mut healthy = MemoryBulkTransport::new();
        healthy.push_host_data(&media_test_unit_ready()).unwrap();
        control.media_attach_test_transport(healthy).unwrap();
        let outcome = control.media_step_test().unwrap().unwrap();
        assert_eq!(outcome.status, ScsiStatus::Good);

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "running");
        assert_eq!(body["media_export"]["running"], true);
        assert_eq!(
            body["media_export"]["restart"]["attempts"], 0,
            "a real transfer resets the failure streak"
        );

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().unwrap();
        assert!(events
            .iter()
            .any(|event| event["kind"] == "restart" && event["state"] == "configured"));

        control.shutdown_media_export().unwrap();
        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_repeated_failures_hit_the_attempt_cap_and_stay_failed() {
        let (state, _fake) = usb_fake_state();
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-restart-cap", &vec![0u8; 4096]);
        control.auto_start_media_export(&auto_media_config(
            &image,
            MediaRestartPolicy::new(2, 5, 20),
        ));

        for expected in 1..=2 {
            let mut fault = MemoryBulkTransport::new();
            fault.set_read_fault(Some(BulkTransportFault::Io));
            control.media_attach_test_transport(fault).unwrap();
            assert!(control.media_step_test().is_err());
            assert_eq!(
                control.restart_media_export(MediaRestartReason::TransportError),
                MediaRestartOutcome::Restarted {
                    attempt: expected,
                    backoff_ms: 5 * u64::from(expected)
                }
            );
        }

        let mut fault = MemoryBulkTransport::new();
        fault.set_read_fault(Some(BulkTransportFault::Io));
        control.media_attach_test_transport(fault).unwrap();
        assert!(control.media_step_test().is_err());
        assert_eq!(
            control.restart_media_export(MediaRestartReason::TransportError),
            MediaRestartOutcome::Exhausted { max_attempts: 2 }
        );

        // A repeated request cannot exceed the cap or start a new crash loop.
        assert_eq!(
            control.restart_media_export(MediaRestartReason::TransportError),
            MediaRestartOutcome::Exhausted { max_attempts: 2 }
        );

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "failed");
        assert_eq!(body["media_export"]["running"], false);
        assert_eq!(body["media_export"]["restart"]["attempts"], 2);
        assert_eq!(body["media_export"]["restart"]["exhausted"], true);
        assert!(body["media_export"]["last_error"]["kind"]
            .as_str()
            .unwrap()
            .starts_with("mass-storage"));

        control.shutdown_media_export().unwrap();
        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_overlay_survives_restart_and_shutdown_flush() {
        let (state, _fake) = usb_fake_state();
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let base = temp_file("media-restart-overlay", &vec![0u8; 4096]);
        let persistence = temp_path("media-restart-overlay-persist");
        control.auto_start_media_export(&auto_media_config_with(
            &base,
            MediaRestartPolicy::default(),
            true,
            Some(&persistence),
        ));

        let mut payload = vec![0x5au8; SCSI_SECTOR_BYTES];
        payload[3] = 0x42;
        let mut transport = MemoryBulkTransport::new();
        transport
            .push_host_data(&media_write10(0, 1, &payload))
            .unwrap();
        transport.push_host_data(&payload).unwrap();
        control.media_attach_test_transport(transport).unwrap();
        assert_eq!(
            control.media_step_test().unwrap().unwrap().status,
            ScsiStatus::Good
        );

        let mut fault = MemoryBulkTransport::new();
        fault.set_read_fault(Some(BulkTransportFault::Io));
        control.media_attach_test_transport(fault).unwrap();
        assert!(control.media_step_test().is_err());
        assert!(matches!(
            control.restart_media_export(MediaRestartReason::TransportError),
            MediaRestartOutcome::Restarted { attempt: 1, .. }
        ));

        // The restart committed and reloaded the overlay, so the bytes survive.
        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "configured");
        assert_eq!(body["media_export"]["overlay"]["allocated_blocks"], 1);
        assert_eq!(body["media_export"]["overlay"]["dirty"], false);

        let mut reader = MemoryBulkTransport::new();
        reader.push_host_data(&media_read10(0, 1)).unwrap();
        control.media_attach_test_transport(reader).unwrap();
        let read = control.media_step_test().unwrap().unwrap();
        assert_eq!(read.status, ScsiStatus::Good);
        assert_eq!(read.data_in_bytes, SCSI_SECTOR_BYTES);
        let mut slot = lock(&control.media);
        let runtime = slot.as_mut().unwrap();
        let mut sector = vec![0u8; SCSI_SECTOR_BYTES];
        runtime.runner.device_mut().read_at(0, &mut sector).unwrap();
        assert_eq!(sector, payload);
        drop(slot);

        // A second unflushed write must be committed by shutdown.
        let mut payload2 = vec![0xa5u8; SCSI_SECTOR_BYTES];
        payload2[9] = 0x17;
        let mut writer = MemoryBulkTransport::new();
        writer
            .push_host_data(&media_write10(0, 1, &payload2))
            .unwrap();
        writer.push_host_data(&payload2).unwrap();
        control.media_attach_test_transport(writer).unwrap();
        assert_eq!(
            control.media_step_test().unwrap().unwrap().status,
            ScsiStatus::Good
        );

        control.shutdown_media_export().unwrap();
        assert!(persistence.is_file(), "shutdown must flush per policy");
        assert_eq!(std::fs::read(&base).unwrap(), vec![0u8; 4096]);

        let mut reloaded = MassStorageDevice::load_writable_file(
            &base,
            OverlayConfig::default().with_persistence(&persistence),
            ozma_usb::MassStoragePersistPolicy::manual(),
            &persistence,
        )
        .unwrap();
        let mut sector = vec![0u8; SCSI_SECTOR_BYTES];
        reloaded.read_at(0, &mut sector).unwrap();
        assert_eq!(sector, payload2);

        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "stopped");
        assert_eq!(body["media_export"]["running"], false);

        std::fs::remove_file(base).unwrap();
        std::fs::remove_file(persistence).unwrap();
    }

    #[tokio::test]
    async fn usb_media_functionfs_unbind_reloads_and_is_logged() {
        let (state, fake) = usb_fake_state();
        let mut control = state.usb.as_ref().unwrap().clone();
        let app = usb_router(state);
        let image = temp_file("media-rebind", &vec![0u8; 4096]);
        control.auto_start_media_export(&auto_media_config(&image, MediaRestartPolicy::default()));

        fake.unbind("ozma").unwrap();
        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media_export"]["state"], "configured");
        assert_eq!(body["media_export"]["running"], false);
        assert_eq!(body["media_export"]["restart"]["attempts"], 1);

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"unbind"));
        assert!(kinds.contains(&"rebind"));
        let rebind = events
            .iter()
            .find(|event| event["kind"] == "rebind")
            .unwrap();
        assert_eq!(rebind["state"], "configured");

        control.shutdown_media_export().unwrap();
        std::fs::remove_file(image).unwrap();
    }

    #[tokio::test]
    async fn usb_media_export_requires_a_configured_gadget() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::failed("missing-configfs-root: absent")));
        let app = usb_router(state);

        let (status, body) = get_json(&app, "/usb/media").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "usb-unavailable");

        let (status, body) = post_json(
            &app,
            "/usb/media/start",
            serde_json::json!({"path": "/srv/media.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "usb-unavailable");

        let (status, body) = get_json(&app, "/usb/hotplug").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "usb-unavailable");
    }

    #[tokio::test]
    async fn usb_unavailable_is_a_typed_503_not_a_fake_gadget() {
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::failed(
            "missing-configfs-root: /sys/kernel/config/usb_gadget does not exist",
        )));

        let (status, body) = get_json(&usb_router(state.clone()), "/usb").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "usb-unavailable");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("missing-configfs-root"));
        assert!(body["remediation"].is_string());

        let (status, body) = post_json(
            &usb_router(state.clone()),
            "/usb/media/attach",
            serde_json::json!({"path": "/srv/media.img"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "usb-unavailable");

        let (status, connection) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(connection["usb"]["status"], "failed");
        assert_eq!(connection["usb"]["available"], false);
        assert!(connection["usb"]["error"]
            .as_str()
            .unwrap()
            .contains("missing-configfs-root"));
    }

    #[tokio::test]
    async fn configfs_temp_root_creates_binds_and_reports_usb_status() {
        let root = temp_path("configfs-root");
        std::fs::create_dir_all(&root).unwrap();
        let image = temp_file("configfs-media", &vec![0u8; 4096]);
        let controller: Arc<dyn UsbGadgetController> = Arc::new(ConfigFsUsbController::new(&root));
        let gadget = UsbGadgetConfig::new("ozma", "dummy_udc.0")
            .with_function(UsbFunction::keyboard("kbd"))
            .with_function(UsbFunction::mouse("mouse"))
            .with_function(UsbFunction::mass_storage(
                "usb0",
                Some(MassStorageImage::disk(&image)),
            ));
        let created = controller.create_and_bind(&gadget).unwrap();
        assert!(created.bound);
        assert_eq!(created.udc.as_deref(), Some("dummy_udc.0"));

        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "hardware",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_usb(Some(UsbControl::configured(controller, "ozma")));

        let app = usb_router(state.clone());
        let (status, body) = get_json(&app, "/usb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "running");
        assert_eq!(body["gadget"]["bound"], true);
        assert_eq!(body["capabilities"]["configfs"], true);
        assert_eq!(body["media"][0]["state"], "attached");
        assert_eq!(body["media"][0]["size_bytes"], 4096);
        assert!(!body.to_string().contains(&*image.to_string_lossy()));

        let (status, body) = post_json(
            &app,
            "/usb/media/detach",
            serde_json::json!({"function": "usb0"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["media"]["state"], "detached");
        let lun_file = root.join("ozma/functions/mass_storage.usb0/lun.0/file");
        assert_eq!(std::fs::read_to_string(lun_file).unwrap().trim(), "");

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(image).unwrap();
    }

    struct RouteWebRtcSession {
        status: WebRtcStatus,
        running: Arc<AtomicBool>,
        socket: std::path::PathBuf,
        shm: std::path::PathBuf,
    }

    impl RouteWebRtcSession {
        fn new(console: u32) -> Self {
            let status = WebRtcStatus::starting_for_test();
            status.mark_running_for_test(Some(8000 + console));
            Self {
                status,
                running: Arc::new(AtomicBool::new(true)),
                socket: std::path::PathBuf::from(format!("/tmp/route-{console}.sock")),
                shm: std::path::PathBuf::from(format!("/tmp/route-{console}.bgra")),
            }
        }
    }

    #[async_trait]
    impl crate::multi_webrtc::ConsoleWebRtcSession for RouteWebRtcSession {
        fn status(&self) -> WebRtcStatus {
            self.status.clone()
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }

        fn pid(&self) -> Option<u32> {
            self.status.helper_pid()
        }

        fn socket_path(&self) -> std::path::PathBuf {
            self.socket.clone()
        }

        fn shm_path(&self) -> std::path::PathBuf {
            self.shm.clone()
        }

        async fn offer_answer(
            &self,
            _sdp: &str,
        ) -> Result<crate::webrtc::WebRtcAnswer, WebRtcError> {
            Ok(crate::webrtc::WebRtcAnswer {
                sdp: format!("answer for {}", self.socket.display()),
                session: Some(format!("route-{}", self.socket.display())),
            })
        }

        async fn add_ice_candidate(
            &self,
            _session: Option<&str>,
            _candidate: &str,
            _sdp_mline_index: u32,
        ) -> Result<(), WebRtcError> {
            Ok(())
        }

        async fn set_bitrate(&self, _bitrate_kbps: u32) -> Result<(), WebRtcError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), String> {
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    struct RouteWebRtcFactory {
        provider: Option<ozma_display::vm::VmConsoleBackend>,
        audio_template: Option<String>,
    }

    #[async_trait]
    impl crate::multi_webrtc::ConsoleWebRtcFactory for RouteWebRtcFactory {
        fn provider(&self) -> Option<ozma_display::vm::VmConsoleBackend> {
            self.provider
        }

        fn audio_configured(&self) -> bool {
            self.audio_template.is_some()
        }

        fn audio_source_for(&self, console: u32) -> Option<String> {
            self.audio_template.as_ref().and_then(|template| {
                crate::multi_webrtc::expand_audio_source(template, console).ok()
            })
        }

        async fn start(
            &self,
            console: u32,
            _geometry: Option<(u32, u32)>,
        ) -> Result<Arc<dyn crate::multi_webrtc::ConsoleWebRtcSession>, String> {
            Ok(Arc::new(RouteWebRtcSession::new(console))
                as Arc<dyn crate::multi_webrtc::ConsoleWebRtcSession>)
        }
    }

    async fn route_webrtc_manager(
        provider: ozma_display::vm::VmConsoleBackend,
        active: u32,
        consoles: &[u32],
    ) -> MultiConsoleWebrtc {
        route_webrtc_manager_with_audio(provider, active, consoles, None).await
    }

    async fn route_webrtc_manager_with_audio(
        provider: ozma_display::vm::VmConsoleBackend,
        active: u32,
        consoles: &[u32],
        audio_template: Option<&str>,
    ) -> MultiConsoleWebrtc {
        let specs: Vec<String> = consoles
            .iter()
            .map(|index| format!("{index}:Console_{index}:1280x800:dbus"))
            .collect();
        let selection = crate::display_outputs::outputs_from_specs(
            &specs,
            ozma_display::vm::VmConsoleBackend::Dbus,
            None,
            Some(active),
        )
        .expect("selection");
        let manager = MultiConsoleWebrtc::new(
            Arc::new(RouteWebRtcFactory {
                provider: Some(provider),
                audio_template: audio_template.map(str::to_owned),
            }),
            crate::multi_webrtc::MultiConsoleWebrtcConfig {
                max_helpers: 2,
                idle_timeout: Duration::from_secs(60),
                audio_source_template: audio_template.map(str::to_owned),
            },
        );
        manager.sync(&selection).await;
        manager
    }

    #[tokio::test]
    async fn per_console_webrtc_offer_selects_console_and_reports_status() {
        let manager =
            route_webrtc_manager(ozma_display::vm::VmConsoleBackend::Dbus, 0, &[0, 1]).await;
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_multi_console_webrtc(Some(manager));
        let app = webrtc_router(state.clone());

        let app_zero = app.clone();
        let app_one = app.clone();
        let (zero, one) = tokio::join!(
            post_json(
                &app_zero,
                "/webrtc/offer?console=0",
                serde_json::json!({"sdp": "v=0", "type": "offer"}),
            ),
            post_json(
                &app_one,
                "/webrtc/offer",
                serde_json::json!({"sdp": "v=0", "type": "offer", "console": 1}),
            ),
        );
        assert_eq!(zero.0, StatusCode::OK);
        assert_eq!(zero.1["type"], "answer");
        assert_eq!(zero.1["console"], 0);
        assert!(zero.1["session"].is_string());
        assert_eq!(one.0, StatusCode::OK);
        assert_eq!(one.1["console"], 1);
        assert!(one.1["answer"].is_null());
        assert!(one.1["sdp"].as_str().unwrap().contains("/tmp/route-1.sock"));

        let (status, body) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["webrtc"], "running");
        assert_eq!(body["multi_console_webrtc"]["enabled"], true);
        assert_eq!(body["multi_console_webrtc"]["max_helpers"], 2);
        assert_eq!(
            body["multi_console_webrtc"]["consoles"][0]["status"],
            "running"
        );
        assert_eq!(
            body["multi_console_webrtc"]["consoles"][1]["status"],
            "running"
        );
        assert_eq!(body["multi_console_webrtc"]["consoles"][1]["sessions"], 1);
    }

    #[tokio::test]
    async fn per_console_webrtc_status_reports_configured_audio_without_samples() {
        let manager = route_webrtc_manager_with_audio(
            ozma_display::vm::VmConsoleBackend::Dbus,
            0,
            &[0, 1],
            Some("ozma-vm{console}.monitor"),
        )
        .await;
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_multi_console_webrtc(Some(manager));
        let (status, body) = get_json(&connection_router(state), "/api/v1/connection").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["webrtc_audio_configured"], true);
        assert_eq!(
            body["multi_console_webrtc"]["consoles"][0]["audio_configured"],
            true
        );
        assert_eq!(
            body["multi_console_webrtc"]["consoles"][0]["audio_source"],
            "ozma-vm0.monitor"
        );
        assert_eq!(
            body["multi_console_webrtc"]["consoles"][1]["audio_source"],
            "ozma-vm1.monitor"
        );
        // Configuration intent only: these status fields never claim samples.
        assert!(body["multi_console_webrtc"]["consoles"][0]["audio_samples"].is_null());
    }

    #[tokio::test]
    async fn per_console_webrtc_single_head_rejects_explicit_console() {
        let manager = route_webrtc_manager(ozma_display::vm::VmConsoleBackend::Qmp, 0, &[0]).await;
        let state = ConnectionState::new(
            ConnectionInfo::new(
                "node-a._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                SINK_FAKE_RECORDING,
            ),
            CompatStats::default(),
        )
        .with_multi_console_webrtc(Some(manager));
        let app = webrtc_router(state);

        let (status, body) = post_json(
            &app,
            "/webrtc/offer?console=0",
            serde_json::json!({"sdp": "v=0", "type": "offer"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["code"], "webrtc-console-unsupported");
        assert!(body["remediation"].is_string());
    }

    async fn wait_for(mut predicate: impl FnMut() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition was not met within 2 seconds");
    }
}
