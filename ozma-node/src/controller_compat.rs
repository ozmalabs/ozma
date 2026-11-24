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
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ozma_audio::{AudioError, AudioPlaybackSpec, AudioProfile, AudioRouter, AudioSourceProfile};
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
    is_safe_identifier, FunctionStatus, GadgetStatus, MassStorageImage, MediaState,
    NbdExportCatalog, NbdExportSource, NbdServer, NbdServerConfig, NbdServerHandle, NbdServerStats,
    UsbError, UsbFunctionKind, UsbGadgetCapabilities, UsbGadgetController, VirtualMedia,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::display_compat::{DisplayInfo, SnapshotControl, SnapshotError};
use crate::display_control::{DisplayControl, DisplayStatus, SelectConsoleRequest};
use crate::hls::{HlsController, HlsEncoder, HlsError, HlsStatus};
use crate::keyboard_layout::{
    key_event, resolve_compose_sequence, resolve_dead_key_sequence, CharOutcome, KeyChord,
    KeyboardLayout,
};
use crate::webrtc::{WebRtcControl, WebRtcError};

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
/// `current_encoder` and `available` describe the encoders the node can
/// actually run. `override` is the accepted request, `receipt` is the policy
/// evaluation that was stored with it, and `pending_restart` says the running
/// HLS pipeline has **not** restarted into that selection yet. `applied` is
/// true only after a real HLS restart succeeded. WebRTC keeps its existing
/// encoder; neither `applied` nor `current_encoder` claims a WebRTC switch.
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
    /// True only when the HLS pipeline restarted with the selected encoder.
    pub applied: bool,
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
}

#[derive(Clone, Debug, Default)]
struct StoredCodecState {
    override_state: CodecOverrideState,
    /// The HLS encoder the running pipeline was actually restarted into.
    applied_encoder: Option<HlsEncoder>,
}

impl CodecControl {
    /// Build a control from a probed/registered encoder set.
    pub fn new(registry: EncoderRegistry, format: MediaFormat) -> Self {
        Self {
            registry,
            state: Arc::new(Mutex::new(StoredCodecState::default())),
            format,
            hls: None,
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
            applied: applied.is_some(),
        }
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
        let running = self.hls.as_ref().is_some_and(HlsController::is_running);
        if !running {
            let hls_status = self
                .hls
                .as_ref()
                .map(|hls| hls.status().status_name())
                .unwrap_or("not-configured");
            *lock(&self.state) = StoredCodecState {
                override_state: CodecOverrideState::from_receipt(request, receipt.clone()),
                applied_encoder: None,
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
            }));
        }

        let selected = selected_capability.ok_or_else(|| {
            CodecApplyError::pipeline(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                "the codec policy accepted the request without selecting an encoder",
            ))
        })?;
        let encoder = hls_encoder_for_selection(selected).map_err(CodecApplyError::pipeline)?;
        let hls = self
            .hls
            .as_ref()
            .expect("running implies an HLS controller");
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
            last_profile: Arc::new(Mutex::new(None)),
        }
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
}

struct NbdRuntime {
    handle: NbdServerHandle,
    url: String,
    export: String,
    size_bytes: u64,
    read_only: bool,
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
        }
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
        controller
            .status(&self.gadget)
            .map(UsbGadgetSnapshot::from_status)
            .map_err(|error| format!("{}: {error}", error.kind()))
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
        }
    }
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
    /// Bounded host sampler for the public `/agent/metrics` document.
    pub metrics: crate::agent_metrics::AgentMetricsSampler,
    /// Controller-facing display outputs (VM consoles or raw host outputs).
    pub display_outputs: serde_json::Value,
    /// Live capture console/provider and any multi-console limitation.
    pub display_capture: DisplayCaptureStatus,
    /// Runtime console selector. When present it is the source of truth for
    /// `/api/v1/connection.display_outputs`/`display_capture` and backs
    /// `POST /display/select`.
    pub display_control: Option<DisplayControl>,
    /// Live capture transport/source for `/api/v1/connection`.
    pub capture: CaptureStatus,
}

impl ConnectionState {
    pub fn new(info: ConnectionInfo, stats: CompatStats) -> Self {
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
            metrics: crate::agent_metrics::AgentMetricsSampler::default(),
            display_outputs: serde_json::json!([]),
            display_capture: DisplayCaptureStatus::default(),
            display_control: None,
            capture: CaptureStatus::default(),
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

    /// Attach the one-shot display snapshot control.
    ///
    /// `None` keeps `GET /display/snapshot` at a typed 503; attaching an empty
    /// registry is also honest (the route reports no source) and lets
    /// `GET /display/info` describe the node.
    pub fn with_snapshot(mut self, snapshot: Option<SnapshotControl>) -> Self {
        self.snapshot = snapshot;
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
    pub webrtc: &'static str,
    /// Why WebRTC is not `running`, when it is configured and failing.
    pub webrtc_error: Option<String>,
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
    /// Actual capture transport and enumerated source, or `available:false`.
    pub capture: CaptureStatus,
}

/// Build the control compatibility router. Mount it before the node API auth
/// layer so these endpoints are covered by the same bearer-token guard.
///
/// This router deliberately excludes the controller's read-only probes
/// (`/display/snapshot`, `/display/info`, `/node/status`, `/ui/hints`,
/// `/agent/metrics`, `/audio/nodes`) and the always-unsupported
/// `POST /audio/sweep` stub. Those live in [`public_router`] and must be merged
/// **after** the guard layer so the unmodified controller can poll them without
/// a bearer token. Control surfaces (`/input/*`, `/power/*`, `/audio/*`,
/// `/codec`, `/webrtc/*`, `/display/mjpeg`, and the legacy mutating aliases)
/// stay here and stay guarded.
pub fn compat_router(state: ConnectionState) -> Router {
    connection_router(state.clone())
        .merge(power_router(state.clone()))
        .merge(input_router(state.clone()))
        .merge(codec_router(state.clone()))
        .merge(webrtc_router(state.clone()))
        .merge(audio_router(state.clone()))
        .merge(usb_router(state.clone()))
        .merge(display_router(state.clone()))
        .merge(legacy_router(state))
}

/// Build the controller's unauthenticated read-only compatibility router.
///
/// The unmodified Python controller fetches these routes without a node bearer
/// token:
///
/// * `GET /display/snapshot` (`controller/stream.py`,
///   `controller/terminal_bridge.py`, `controller/api.py`)
/// * `GET /ui/hints` (`controller/agent_engine.py`, `controller/api.py`)
/// * `GET /node/status` (`controller/mesh_intelligence.py`)
/// * `GET /agent/metrics` (`controller/device_metrics.py`)
///
/// `GET /display/info`, `GET /audio/nodes`, and `POST /audio/sweep` are mounted
/// here as well: the first is read-only metadata, and the audio routes are
/// typed 501 stubs that never change state. Merge this router **after** the
/// bearer layer; see [`crate::api::serve_with_scene_replay_and_sink`].
pub fn public_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/display/snapshot", get(display_snapshot))
        .route("/snapshot", get(display_snapshot))
        .route("/display/info", get(display_info))
        .route("/node/status", get(node_status))
        .route("/status", get(node_status))
        .route("/agent/metrics", get(agent_metrics))
        .route("/ui/hints", get(ui_hints))
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
/// typed 200/400/502/503 results. Without a router every route answers a typed
/// 501 rather than a success-shaped no-op.
pub fn audio_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/audio/apply", post(audio_apply))
        .route("/audio/remove", post(audio_remove))
        .route("/audio/play", post(audio_play))
        .route("/audio/stop", post(audio_stop))
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
        .route("/usb/media/attach", post(usb_media_attach))
        .route("/usb/media/detach", post(usb_media_detach))
        .route("/usb/nbd", get(usb_nbd_status))
        .route("/usb/nbd/start", post(usb_nbd_start))
        .route("/usb/nbd/stop", post(usb_nbd_stop))
        .with_state(state)
}

/// Display compatibility control routes.
///
/// `GET /display/snapshot` and `GET /display/info` are read-only and live in
/// [`public_router`] so the controller's unauthenticated preview pollers work
/// with `--api-token`. `GET /display/mjpeg` stays here as a guarded typed 501
/// because no controller caller depends on it and it is not a media surface
/// this node implements.
pub fn display_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/display/mjpeg", get(display_mjpeg))
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
        .route("/current", get(legacy_stub))
        .route("/session/init", post(legacy_stub))
        .route("/serial/*rest", get(legacy_stub).post(legacy_stub))
        .with_state(state)
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
                    serde_json::json!({"status": "unknown", "available": false})
                }
            }
        }
    };
    let (hls, hls_error) = match (&state.hls_control, &state.hls) {
        (Some(controller), _) => {
            let status = controller.status();
            (status.status_name(), status.error())
        }
        (None, None) => ("not-configured", None),
        (None, Some(status)) => (status.status_name(), status.error()),
    };
    let (webrtc, webrtc_error) = match &state.webrtc {
        None => ("not-configured", None),
        Some(control) => (control.status_name(), control.error()),
    };
    let (webrtc_audio_configured, webrtc_audio_source) = match &state.webrtc {
        None => (false, None),
        Some(control) => (control.audio_configured(), control.audio_source()),
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
    let codec = state
        .codec_control
        .as_ref()
        .map(CodecControl::status)
        .unwrap_or_else(|| state.codecs.clone());
    let display = state.display_status();
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
        webrtc,
        webrtc_error,
        audio,
        audio_error,
        usb,
        webrtc_audio_configured,
        webrtc_audio_source,
        display_outputs: display.display_outputs,
        display_capture: display.display_capture,
        capture: state.capture.clone(),
    })
}

async fn display_snapshot(State(state): State<ConnectionState>) -> Response {
    let Some(control) = state.snapshot else {
        return snapshot_error_response(SnapshotError::NotConfigured);
    };
    match control.capture().await {
        Ok(snapshot) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            snapshot.jpeg,
        )
            .into_response(),
        Err(error) => snapshot_error_response(error),
    }
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

async fn display_info(State(state): State<ConnectionState>) -> Json<DisplayInfo> {
    let info = match state.snapshot {
        Some(control) => control.info().await,
        None => DisplayInfo {
            ok: true,
            source_id: None,
            source_name: None,
            width: 0,
            height: 0,
            pixel_format: None,
            snapshot_available: false,
            snapshot_encoding: "jpeg",
            dimensions_source: "none",
            last_error: Some("display snapshot support is not configured".to_owned()),
        },
    };
    Json(info)
}

async fn display_mjpeg(State(_state): State<ConnectionState>) -> Response {
    compat_error_with_remediation(
        StatusCode::NOT_IMPLEMENTED,
        "display.mjpeg_unsupported",
        "This node does not provide a multipart MJPEG stream.",
        Some(
            "Use GET /display/snapshot for a single JPEG frame; the controller polls that \
             endpoint for previews. A real MJPEG stream is not implemented.",
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
        Ok(response) => Json(response).into_response(),
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
    let document = state.metrics.collect(&state.info.node_id);
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

async fn legacy_stub(State(_state): State<ConnectionState>, uri: Uri) -> Response {
    let (code, error, remediation) = match uri.path() {
        "/rgb/state" | "/rgb/effect" | "/rgb/set" => (
            "rgb.unsupported",
            "RGB lighting control is not implemented on this node.",
            "Configure RGB control on the host that owns the lighting controller; this node \
             does not claim an effect was applied.",
        ),
        "/current" => (
            "power.current_unsupported",
            "Power-rail current/voltage telemetry is not implemented on this node.",
            "Attach a sensor driver that exposes /current before enabling the controller's \
             USB power panel.",
        ),
        "/session/init" => (
            "session.init_unsupported",
            "The controller's encrypted /session/init handshake is not implemented on this node.",
            "Use the node's native QUIC/WebTransport session API; do not treat this 501 as an \
             established encrypted session.",
        ),
        path if path.starts_with("/serial") => (
            "serial.unsupported",
            "A serial console buffer is not implemented on this node.",
            "Attach a serial console provider before enabling the controller's serial panel; \
             no buffer is fabricated.",
        ),
        _ => (
            "legacy.unsupported",
            "This legacy controller route is not implemented on this node.",
            "Use the native /api/v1 surface or a provider that implements this feature.",
        ),
    };
    compat_error_with_remediation(StatusCode::NOT_IMPLEMENTED, code, error, Some(remediation))
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
            let body = serde_json::json!({
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
            Json(body).into_response()
        }
        Err(error) => usb_status_unavailable(&error),
    }
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
        Ok(media_state) => Json(serde_json::json!({
            "ok": true,
            "function": function,
            "media": UsbMediaStatus::from_state(&function, &media_state),
        }))
        .into_response(),
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
        Ok(media_state) => Json(serde_json::json!({
            "ok": true,
            "function": function,
            "media": UsbMediaStatus::from_state(&function, &media_state),
        }))
        .into_response(),
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
    let source = match NbdExportSource::from_media(export_name.clone(), &media) {
        Ok(source) => source,
        Err(error) => return usb_error_response(error),
    };
    let read_only = source.is_read_only();
    let size_bytes = source.export.size_bytes;
    let catalog = match NbdExportCatalog::new(vec![source]) {
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
            (
                axum::http::StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "status": "unknown",
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
        PowerAction::Start => backend.cont().await,
        PowerAction::Shutdown => backend.system_powerdown().await,
        PowerAction::Reset => backend.system_reset().await,
        PowerAction::Pause => backend.pause().await,
        PowerAction::Resume => backend.resume().await,
        PowerAction::Stop => backend.stop().await,
    };

    match result {
        Ok(()) => Json(serde_json::json!({"ok": true, "action": action.as_str()})).into_response(),
        Err(error) => {
            warn!(action = %action, error = %error, "Power action failed");
            (
                axum::http::StatusCode::BAD_GATEWAY,
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

async fn input_type(
    State(state): State<ConnectionState>,
    payload: Result<Json<TypeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    // Validate the complete request against the configured layout before the
    // sink is consulted, so a bad character never produces a partial keystroke
    // sequence.
    let chunks = match text_to_key_chunks(&request.text, state.keyboard_layout) {
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
            return compat_error_with_remediation(
                StatusCode::BAD_REQUEST,
                code,
                error.to_string(),
                remediation,
            );
        }
    };

    let sink = match require_input_sink(&state) {
        Ok(sink) => sink,
        Err(response) => return response,
    };

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
                return response;
            }
        }
        if index + 1 < chunks.len() {
            tokio::time::sleep(TYPE_CHAR_DELAY).await;
        }
    }

    Json(serde_json::json!({"ok": true, "events": submitted})).into_response()
}

// ---------------------------------------------------------------------------
// Codec and audio compatibility routes
// ---------------------------------------------------------------------------

async fn codec_get(State(state): State<ConnectionState>) -> Json<CodecStatus> {
    let status = state
        .codec_control
        .as_ref()
        .map(CodecControl::status)
        .unwrap_or_else(|| state.codecs.clone());
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

/// `GET /audio/nodes` — typed unsupported response with the soft-node shape.
///
/// The Python soft node returns `{"nodes": [...], "default_sink": "...",
/// "default_source": "..."}` from its PipeWire watcher. The Rust
/// [`AudioRouter`] controls an existing graph but has no node-enumeration
/// operation, so this node cannot honestly fill that list. The controller
/// proxies the body to the dashboard, which can then show the exact reason
/// instead of an empty fake graph. The route is public because the controller's
/// proxy sends no node token; it never mutates state.
async fn audio_nodes(State(_state): State<ConnectionState>) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "ok": false,
            "available": false,
            "code": "audio-nodes-unsupported",
            "error": "This node does not enumerate PipeWire/PulseAudio graph nodes.",
            "remediation": "Use a soft node with a PipeWire watcher for room-correction device \
                            discovery, or choose a sink by name with --audio-sink. The Rust \
                            AudioRouter controls an existing graph and does not expose a node \
                            listing; no nodes are fabricated.",
            "nodes": [],
            "links": [],
            "default_sink": serde_json::Value::Null,
            "default_source": serde_json::Value::Null,
        })),
    )
        .into_response()
}

/// `POST /audio/sweep` — typed unsupported response with the soft-node shape.
///
/// `controller/api.py::run_room_sweep` proxies a sweep to the node when the
/// request names one. The Rust node has no room-correction engine or sweep
/// transport, so it never claims a profile. The route is public because that
/// proxy sends no node token; the 501 is the whole behaviour, so the public
/// surface cannot change state.
async fn audio_sweep() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "ok": false,
            "available": false,
            "code": "audio-sweep-unsupported",
            "error": "Room-correction sweeps are not implemented on this node.",
            "remediation": "Run the sweep on the controller host or a soft node with \
                            room_correction. This node has no sweep engine and returns no \
                            fabricated profile.",
            "profile": serde_json::Value::Null,
        })),
    )
        .into_response()
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

/// `POST /webrtc/offer` body, matching the controller/soft-node shape.
#[derive(Debug, Deserialize)]
struct WebrtcOfferRequest {
    sdp: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
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
}

/// `POST /webrtc/candidate` body; a late remote ICE candidate.
#[derive(Debug, Deserialize)]
struct WebrtcCandidateRequest {
    #[serde(default)]
    session: Option<String>,
    candidate: String,
    #[serde(default, rename = "sdpMLineIndex")]
    sdp_mline_index: u32,
}

/// `POST /webrtc/bitrate` body; `bitrate` is bits per second.
#[derive(Debug, Deserialize)]
struct WebrtcBitrateRequest {
    #[serde(default = "default_webrtc_bitrate_bps")]
    bitrate: u64,
}

fn default_webrtc_bitrate_bps() -> u64 {
    4_000_000
}

const WEBRTC_BITRATE_MIN_BPS: u64 = 500_000;
const WEBRTC_BITRATE_MAX_BPS: u64 = 50_000_000;

async fn webrtc_offer(
    State(state): State<ConnectionState>,
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
        })
        .into_response(),
        Err(error) => webrtc_offer_failure(error),
    }
}

async fn webrtc_candidate(
    State(state): State<ConnectionState>,
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

fn webrtc_offer_failure(error: WebRtcError) -> Response {
    match &error {
        WebRtcError::InvalidConfig(_) => {
            compat_error(StatusCode::BAD_REQUEST, "invalid-sdp", error.to_string())
        }
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
                 softnode/gst_webrtc_worker.py ignores it today, so this is an unsupported \
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
    use crate::hls::{
        ffmpeg_argv, EncoderAvailability, EncoderProbe, FfmpegSpawner, FrameSink, HlsConfig,
        HlsController, HlsEncoder, HlsError, HlsSourceFactory, SpawnedFfmpeg, StaticEncoderProbe,
    };
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use ozma_capture::{CaptureRegistry, FakeCaptureProvider};
    use ozma_input::{FakeInputSink, InputSink, RecordingSink};
    use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
    use ozma_power::{PowerBackend, PowerError, PowerState};
    use ozma_usb::{
        ConfigFsUsbController, FakeUsbGadgetController, MassStorageImage, UsbFunction,
        UsbGadgetConfig,
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

    use ozma_audio::{
        audio_codes, AudioSinkSpec, AudioSinkState, AudioSourceProfile, AudioSourceRef,
        AudioSourceSpec, AudioSourceState, AudioStreamRef,
    };

    struct FakeAudioRouter {
        calls: Arc<Mutex<Vec<String>>>,
        failure: Option<AudioError>,
    }

    impl FakeAudioRouter {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                failure: None,
            }
        }

        fn failing(error: AudioError) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                failure: Some(error),
            }
        }

        fn calls(&self) -> Vec<String> {
            lock(&self.calls).clone()
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
            FakeCaptureProvider::new().with_format(FrameFormat::new(PixelFormat::Nv12, 4, 4)),
        ))));
        let (status, body) = get_json(&public_router(state), "/display/snapshot").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "display.unsupported_format");
        assert!(body["error"].as_str().unwrap().contains("nv12"));
        assert!(body["remediation"].as_str().unwrap().contains("rgb24"));
    }

    #[tokio::test]
    async fn display_mjpeg_is_a_typed_501_not_a_fake_stream() {
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
        let (status, body) = get_json(&display_router(state), "/display/mjpeg").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["ok"], false);
        assert_eq!(body["code"], "display.mjpeg_unsupported");
        assert!(body["remediation"].is_string());
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
            ("/current", "GET", "power.current_unsupported"),
            ("/session/init", "POST", "session.init_unsupported"),
            ("/serial/buffer?lines=50", "GET", "serial.unsupported"),
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
