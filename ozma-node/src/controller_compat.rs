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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ozma_audio::{AudioError, AudioPlaybackSpec, AudioProfile, AudioRouter};
use ozma_encode::{
    CodecOverrideState, CodecPreference, EncoderOverride, EncoderRegistry, OverrideReceipt,
};
use ozma_input::{linux_keycode_to_hid_usage, SharedSink, SubmitReport};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton};
use ozma_protocol::{codes, ApiError, Codec, EncoderBackend, MediaFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::hls::HlsStatus;
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
/// HLS/WebRTC pipeline has **not** restarted into that selection yet. The node
/// never reports an override as live.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CodecStatus {
    /// The encoder currently used by the pipeline, or `null` with no video.
    pub current_encoder: Option<String>,
    /// The controller's accepted override, or `null` for automatic selection.
    #[serde(rename = "override")]
    pub override_setting: Option<EncoderOverride>,
    /// Video codecs the node advertises, in capability order.
    pub available: Vec<String>,
    /// The last stored policy receipt, or `null` when none was evaluated.
    pub receipt: Option<OverrideReceipt>,
    /// True while a stored override has not been applied by a pipeline restart.
    pub pending_restart: bool,
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
            receipt: None,
            pending_restart: false,
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

/// Runtime codec override control for `GET`/`POST /codec`.
///
/// Holds the real [`EncoderRegistry`] and the last accepted
/// [`CodecOverrideState`]. Applying an override stores the request and its
/// receipt; it deliberately does **not** restart the HLS/WebRTC pipeline, so
/// every mutation response reports `applied: false` and
/// `pending_restart: true`.
#[derive(Clone)]
pub struct CodecControl {
    registry: EncoderRegistry,
    state: Arc<Mutex<CodecOverrideState>>,
    format: MediaFormat,
}

impl CodecControl {
    /// Build a control from a probed/registered encoder set.
    pub fn new(registry: EncoderRegistry, format: MediaFormat) -> Self {
        Self {
            registry,
            state: Arc::new(Mutex::new(CodecOverrideState::empty())),
            format,
        }
    }

    /// A control with no registry and no video capability.
    pub fn not_configured() -> Self {
        Self::new(
            EncoderRegistry::new(),
            MediaFormat::new(Codec::H264, 1920, 1080, 30, 4_000),
        )
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

    /// Status derived from the registry plus the stored override state.
    pub fn status(&self) -> CodecStatus {
        let available = video_codec_names(
            self.registry
                .capabilities()
                .iter()
                .map(|capability| capability.codec),
        );
        let current_encoder = available.first().cloned();
        let state = lock(&self.state).clone();
        let pending_restart = state.requested.is_some();
        CodecStatus {
            current_encoder,
            override_setting: state.requested,
            available,
            receipt: state.receipt,
            pending_restart,
        }
    }

    /// Evaluate and store a controller codec request.
    ///
    /// # Errors
    ///
    /// A malformed request, an unknown codec/backend/bitrate, or a selection
    /// the registry cannot honour returns a typed [`ApiError`] whose `details`
    /// list the real capabilities. No state is stored on error.
    pub fn apply(&self, request: &CodecRequest) -> Result<serde_json::Value, ApiError> {
        let preference = self.preference_from_request(request)?;
        let request = EncoderOverride::new(preference).with_source("controller");
        let receipt = self.registry.override_receipt(&request, &self.format)?;
        let state = CodecOverrideState::from_receipt(request, receipt.clone());
        *lock(&self.state) = state;
        Ok(serde_json::json!({
            "ok": true,
            "applied": false,
            "pending_restart": true,
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
        sink.map(|sink| sink.trim().to_owned())
            .filter(|sink| !sink.is_empty())
            .or_else(|| self.default_sink.clone())
            .ok_or_else(|| {
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

    fn remember_profile(&self, profile_id: &str) {
        *lock(&self.last_profile) = Some(profile_id.to_owned());
    }

    fn last_profile(&self) -> Option<String> {
        lock(&self.last_profile).clone()
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
    /// The one sink shared with session input and the UDP HID listener.
    pub input_sink: Option<SharedSink>,
    /// Base status derived from the advertised capabilities. When a
    /// [`CodecControl`] is attached its live status replaces this value.
    pub codecs: CodecStatus,
    /// Runtime codec override control, when the node has a video encoder.
    pub codec_control: Option<CodecControl>,
    /// Runtime audio routing control. `None` leaves `/audio/*` at typed 501.
    pub audio: Option<AudioControl>,
    /// External WebRTC helper. `None` means no `--webrtc-helper` was given;
    /// `Some(control)` may be a configured-but-failed helper.
    pub webrtc: Option<WebRtcControl>,
}

impl ConnectionState {
    pub fn new(info: ConnectionInfo, stats: CompatStats) -> Self {
        Self {
            info,
            stats,
            power: None,
            hls: None,
            input_sink: None,
            codecs: CodecStatus::not_configured(),
            codec_control: None,
            audio: None,
            webrtc: None,
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

    /// Attach the shared input sink used by session input and UDP HID.
    ///
    /// This must be the same [`SharedSink`] passed to the UDP listener; the
    /// compatibility `/input/*` routes never open a second backend.
    pub fn with_input_sink(mut self, sink: Option<SharedSink>) -> Self {
        self.input_sink = sink;
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

    /// Attach the external WebRTC helper control surface.
    ///
    /// `None` means WebRTC was not configured and reports `not-configured`.
    /// `Some(WebRtcControl::failed(...))` is a configured helper that could
    /// not start and reports `failed` plus `webrtc_error`.
    pub fn with_webrtc(mut self, webrtc: Option<WebRtcControl>) -> Self {
        self.webrtc = webrtc;
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
}

/// Build the compatibility router. Mount it before the node API auth layer so
/// the endpoints are covered by the same bearer-token guard.
pub fn compat_router(state: ConnectionState) -> Router {
    connection_router(state.clone())
        .merge(power_router(state.clone()))
        .merge(input_router(state.clone()))
        .merge(codec_router(state.clone()))
        .merge(webrtc_router(state.clone()))
        .merge(audio_router(state))
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

/// Codec status routes. `POST /codec` is explicit about not being wired to the
/// encode pipeline yet.
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
/// `{"sdp": "...", "type": "answer"}`. `POST /webrtc/bitrate` accepts
/// `{"bitrate": <bps>}`. Both are mounted through [`compat_router`] before the
/// node API's bearer guard. A node with no helper answers 503; a helper that
/// does not acknowledge a bitrate change answers a typed 501/502 rather than a
/// fake success.
pub fn webrtc_router(state: ConnectionState) -> Router {
    Router::new()
        .route("/webrtc/offer", post(webrtc_offer))
        .route("/webrtc/bitrate", post(webrtc_bitrate))
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
    let (hls, hls_error) = match &state.hls {
        None => ("not-configured", None),
        Some(status) => (status.status_name(), status.error()),
    };
    let (webrtc, webrtc_error) = match &state.webrtc {
        None => ("not-configured", None),
        Some(control) => (control.status_name(), control.error()),
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
    let codec = state
        .codec_control
        .as_ref()
        .map(CodecControl::status)
        .unwrap_or_else(|| state.codecs.clone());
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
    })
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
    /// A character has no unambiguous US-layout HID keyboard usage.
    #[error(
        "unsupported character {character:?} at index {index}; only printable ASCII, newline, and tab are supported"
    )]
    Unsupported {
        /// The offending character.
        character: char,
        /// Zero-based character (not byte) index.
        index: usize,
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

/// Map a printable-ASCII character to a US-layout HID usage and shift state.
fn ascii_hid_usage(character: char) -> Option<(u8, bool)> {
    match character {
        'a'..='z' => Some(((character as u8 - b'a') + 0x04, false)),
        'A'..='Z' => Some(((character as u8 - b'A') + 0x04, true)),
        '1'..='9' => Some(((character as u8 - b'1') + 0x1E, false)),
        '0' => Some((0x27, false)),
        '\n' | '\r' => Some((0x28, false)), // Enter
        '\t' => Some((0x2B, false)),        // Tab
        ' ' => Some((0x2C, false)),         // Space
        '-' => Some((0x2D, false)),
        '_' => Some((0x2D, true)),
        '=' => Some((0x2E, false)),
        '+' => Some((0x2E, true)),
        '[' => Some((0x2F, false)),
        '{' => Some((0x2F, true)),
        ']' => Some((0x30, false)),
        '}' => Some((0x30, true)),
        '\\' => Some((0x31, false)),
        '|' => Some((0x31, true)),
        ';' => Some((0x33, false)),
        ':' => Some((0x33, true)),
        '\'' => Some((0x34, false)),
        '"' => Some((0x34, true)),
        '`' => Some((0x35, false)),
        '~' => Some((0x35, true)),
        ',' => Some((0x36, false)),
        '<' => Some((0x36, true)),
        '.' => Some((0x37, false)),
        '>' => Some((0x37, true)),
        '/' => Some((0x38, false)),
        '?' => Some((0x38, true)),
        '!' => Some((0x1E, true)),
        '@' => Some((0x1F, true)),
        '#' => Some((0x20, true)),
        '$' => Some((0x21, true)),
        '%' => Some((0x22, true)),
        '^' => Some((0x23, true)),
        '&' => Some((0x24, true)),
        '*' => Some((0x25, true)),
        '(' => Some((0x26, true)),
        ')' => Some((0x27, true)),
        _ => None,
    }
}

fn key_event(code: u32, down: bool) -> InputEvent {
    InputEvent::Key {
        code,
        down,
        modifiers: KeyModifiers(0),
    }
}

/// Convert text into per-character key event chunks.
///
/// Each chunk is a key-down/up pair (plus left-shift down/up for shifted
/// characters). The handler sleeps between chunks so the guest sees separate
/// keystrokes; [`text_to_input_events`] flattens the same mapping for callers
/// and tests that only need the event sequence.
fn text_to_key_chunks(text: &str) -> Result<Vec<Vec<InputEvent>>, TextMappingError> {
    let length = text.chars().count();
    if length > MAX_TYPE_TEXT_CHARS {
        return Err(TextMappingError::TooLong {
            actual: length,
            limit: MAX_TYPE_TEXT_CHARS,
        });
    }

    let mut chunks = Vec::with_capacity(length);
    for (index, character) in text.chars().enumerate() {
        let Some((usage, shift)) = ascii_hid_usage(character) else {
            return Err(TextMappingError::Unsupported { character, index });
        };
        let usage = u32::from(usage);
        let mut events = Vec::with_capacity(if shift { 4 } else { 2 });
        if shift {
            events.push(key_event(0xE1, true)); // left shift
        }
        events.push(key_event(usage, true));
        events.push(key_event(usage, false));
        if shift {
            events.push(key_event(0xE1, false));
        }
        chunks.push(events);
    }
    Ok(chunks)
}

/// Map text to normalized key events without touching a sink.
///
/// This is the pure, testable core of `POST /input/type`. Unsupported
/// characters are rejected rather than silently dropped or mapped to an
/// invented keycode.
pub fn text_to_input_events(text: &str) -> Result<Vec<InputEvent>, TextMappingError> {
    Ok(text_to_key_chunks(text)?.into_iter().flatten().collect())
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
    sink.submit_report(event).map_err(|error| {
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
    })
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

async fn input_type(
    State(state): State<ConnectionState>,
    payload: Result<Json<TypeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(error) => return json_rejection(error),
    };

    let chunks = match text_to_key_chunks(&request.text) {
        Ok(chunks) => chunks,
        Err(error) => {
            let code = match error {
                TextMappingError::TooLong { .. } => "text-too-long",
                TextMappingError::Unsupported { .. } => "unsupported-character",
            };
            return compat_error(StatusCode::BAD_REQUEST, code, error.to_string());
        }
    };

    let sink = match require_input_sink(&state) {
        Ok(sink) => sink,
        Err(response) => return response,
    };

    let mut submitted = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        for event in chunk {
            if let Err(response) = submit_input(&sink, event) {
                return response;
            }
            submitted += 1;
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
    match control.apply(&request) {
        Ok(body) => Json(body).into_response(),
        Err(error) => api_error_response(StatusCode::BAD_REQUEST, error),
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
    if request
        .source
        .as_deref()
        .is_some_and(|source| !source.trim().is_empty())
    {
        return compat_error_with_remediation(
            StatusCode::NOT_IMPLEMENTED,
            "audio-source-unsupported",
            "audio source routing unsupported",
            Some(
                "This node routes playback sinks only. Microphone/source routing needs an \
                 explicit source-output index and is not implemented.",
            ),
        );
    }
    let profile_id = match required_profile_id(request.profile_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let sink = match control.sink_or_default(request.sink) {
        Ok(sink) => sink,
        Err(response) => return response,
    };
    let mut profile = match AudioProfile::new(profile_id.clone(), sink.clone()) {
        Ok(profile) => profile,
        Err(error) => return audio_error_response(error),
    };
    if let Some(volume) = request.volume {
        if !(0..=100).contains(&volume) {
            return compat_error(
                StatusCode::BAD_REQUEST,
                "audio-invalid-volume",
                format!("volume {volume} is outside 0..=100"),
            );
        }
        profile = match profile.with_volume(volume as u8) {
            Ok(profile) => profile,
            Err(error) => return audio_error_response(error),
        };
    }
    if let Some(muted) = request.muted {
        profile = profile.with_muted(muted);
    }

    match router.apply_profile(profile).await {
        Ok(()) => {
            control.remember_profile(&profile_id);
            Json(serde_json::json!({
                "ok": true,
                "profile_id": profile_id,
                "sink": sink,
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

    match control.offer(&request.sdp).await {
        Ok(sdp) => Json(WebrtcOfferResponse {
            sdp,
            kind: "answer",
        })
        .into_response(),
        Err(error) => webrtc_offer_failure(error),
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
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use ozma_input::{FakeInputSink, InputSink, RecordingSink};
    use ozma_power::{PowerBackend, PowerError, PowerState};
    use serde_json::Value;
    use std::time::Duration;
    use tower::ServiceExt;

    struct FakePower {
        status: String,
        fail: bool,
        calls: Vec<&'static str>,
    }

    impl FakePower {
        fn running() -> Self {
            Self {
                status: "running".into(),
                fail: false,
                calls: Vec::new(),
            }
        }

        fn failing() -> Self {
            Self {
                status: "unknown".into(),
                fail: true,
                calls: Vec::new(),
            }
        }

        fn shared(self) -> SharedPower {
            Arc::new(tokio::sync::Mutex::new(
                Box::new(self) as Box<dyn PowerBackend>
            ))
        }

        fn record(&mut self, action: &'static str) -> Result<(), PowerError> {
            if self.fail {
                return Err(PowerError::NotConnected);
            }
            self.calls.push(action);
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

    use ozma_audio::{audio_codes, AudioSinkSpec, AudioSinkState, AudioStreamRef};

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

        async fn apply_profile(&self, profile: AudioProfile) -> Result<(), AudioError> {
            self.record(format!(
                "apply:{}:{}:{:?}:{:?}",
                profile.id, profile.sink, profile.volume, profile.muted
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
        .with_input_sink(Some(sink));
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
        let events = |text: &str| text_to_input_events(text).unwrap();

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

    #[test]
    fn text_mapping_rejects_unsupported_characters_and_oversized_text() {
        assert_eq!(
            text_to_input_events("aé").unwrap_err(),
            TextMappingError::Unsupported {
                character: 'é',
                index: 1,
            }
        );
        assert_eq!(
            text_to_input_events("日本").unwrap_err(),
            TextMappingError::Unsupported {
                character: '日',
                index: 0,
            }
        );

        let oversized = "a".repeat(MAX_TYPE_TEXT_CHARS + 1);
        assert_eq!(
            text_to_input_events(&oversized).unwrap_err(),
            TextMappingError::TooLong {
                actual: MAX_TYPE_TEXT_CHARS + 1,
                limit: MAX_TYPE_TEXT_CHARS,
            }
        );
        // The exact bound is accepted.
        let at_limit = "a".repeat(MAX_TYPE_TEXT_CHARS);
        assert_eq!(
            text_to_input_events(&at_limit).unwrap().len(),
            MAX_TYPE_TEXT_CHARS * 2
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
    async fn input_type_route_rejects_unsupported_and_oversized_text() {
        let (state, recording) = input_state(SINK_FAKE_RECORDING);
        let app = input_router(state);

        let (status, body) = post_json(&app, "/input/type", serde_json::json!({"text": "é"})).await;
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

    fn codec_control(registry: EncoderRegistry) -> CodecControl {
        CodecControl::new(
            registry,
            MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
        )
    }

    fn h264_registry() -> EncoderRegistry {
        EncoderRegistry::from_capabilities([ozma_encode::H264SoftwareEncoder::capability()])
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

        let (status, body) = post_json(
            &app,
            "/codec",
            serde_json::json!({"codec": "h264", "bitrate": "8M", "hw_accel": "software"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        // The pipeline did not restart: never claim the override is live.
        assert_eq!(body["applied"], false);
        assert_eq!(body["pending_restart"], true);
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
        assert_eq!(body["available"], serde_json::json!(["h264"]));
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
                "apply:profile-a:ozma-vm1:Some(42):Some(true)",
                "apply:profile-b:ozma-default:Some(0):None",
                "remove:profile-a",
                "remove:profile-b",
                "play:sweep.flac:ozma-vm1",
                "stop",
            ]
        );
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
                serde_json::json!({"profile_id": "p", "sink": "s", "source": "mic"}),
                "audio-source-unsupported",
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
            assert_eq!(
                status,
                if code == "audio-source-unsupported" {
                    StatusCode::NOT_IMPLEMENTED
                } else {
                    StatusCode::BAD_REQUEST
                },
                "{path}: {response}"
            );
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
