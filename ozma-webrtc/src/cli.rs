// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Command line accepted by the native helper.
//!
//! The flag set deliberately mirrors `scripts/webrtc/gst_webrtc_worker.py` so
//! `ozma-node --webrtc-helper /path/to/ozma-webrtc-native` can spawn this
//! binary with the existing argv builder. `ozma-node` sends the bitrate over
//! the signaling socket, but `--bitrate` is accepted here for standalone runs.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Audio capture backend token shared with the GStreamer worker.
///
/// `auto` tries the native PipeWire stream first and falls back to Pulse. The
/// explicit tokens never use the other backend. Pulse capture uses `parec`
/// (the PulseAudio/PipeWire compatibility server), always with an explicit
/// device name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum AudioBackend {
    /// Prefer native PipeWire capture, fall back to PulseAudio.
    #[default]
    Auto,
    /// PulseAudio via `parec`.
    Pulse,
    /// Native PipeWire capture.
    #[value(name = "pipewire")]
    PipeWire,
}

/// Native WebRTC helper speaking the node's Unix-socket protocol.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "ozma-webrtc-native",
    version,
    about = "Native Rust WebRTC helper (H.264 from packed BGRA shared memory)"
)]
pub struct Args {
    /// Signaling Unix socket the helper creates and serves.
    #[arg(long)]
    pub sock: PathBuf,

    /// Shared-memory file receiving packed BGRA frames.
    #[arg(long)]
    pub shm: PathBuf,

    /// Frame width written to shared memory.
    #[arg(long, default_value_t = 640)]
    pub width: u32,

    /// Frame height written to shared memory.
    #[arg(long, default_value_t = 480)]
    pub height: u32,

    /// Source frame rate used for encode pacing and the RTP sample duration.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,

    /// Initial H.264 bitrate in kbps (500..=50000). The node normally changes
    /// this at runtime over signaling instead.
    #[arg(long, default_value_t = 4_000)]
    pub bitrate: u32,

    /// Optional STUN server (`stun:host:port`, `stun://host:port`, or bare
    /// `host:port`).
    #[arg(long)]
    pub stun_server: Option<String>,

    /// Optional TURN server without credentials (`turn:host:port`,
    /// `turn://host:port`, or bare `host:port`).
    #[arg(long)]
    pub turn_server: Option<String>,

    /// TURN username; required with `--turn-server`.
    #[arg(long)]
    pub turn_user: Option<String>,

    /// TURN password; required with `--turn-server` and never logged.
    #[arg(long)]
    pub turn_password: Option<String>,

    /// Opt-in audio source: a PipeWire object/monitor name, `object:<name>`,
    /// `monitor:<sink>`, `node:<id>`, or an explicit `default`. Without this
    /// flag the helper is video-only and an offered Opus m-line stays inactive.
    #[arg(long)]
    pub audio_source: Option<String>,

    /// Audio capture backend: `auto` (PipeWire, then Pulse), `pipewire`, or
    /// `pulse`. Only meaningful with `--audio-source`.
    #[arg(long, value_enum, default_value_t = AudioBackend::Auto)]
    pub audio_backend: AudioBackend,

    /// Bounded wait for local ICE gathering before the answer is sent.
    #[arg(long, default_value_t = 5)]
    pub gather_timeout_seconds: u64,

    /// Maximum simultaneous peer connections (each has its own DTLS/SRTP
    /// state but shares one encoded video track).
    #[arg(long, default_value_t = 4)]
    pub max_sessions: usize,
}

impl Args {
    /// Validate ranges the encoder and shared-memory layout cannot use.
    pub fn validate(&self) -> Result<(), String> {
        if self.sock.as_os_str().is_empty() {
            return Err("--sock must not be empty".to_owned());
        }
        if self.shm.as_os_str().is_empty() {
            return Err("--shm must not be empty".to_owned());
        }
        if self.width < 2 || self.height < 2 {
            return Err(format!(
                "--width/--height must be at least 2x2, got {}x{}",
                self.width, self.height
            ));
        }
        if self.fps == 0 {
            return Err("--fps must be greater than zero".to_owned());
        }
        if self.bitrate == 0 {
            return Err("--bitrate must be greater than zero kbps".to_owned());
        }
        if self.gather_timeout_seconds == 0 {
            return Err("--gather-timeout-seconds must be greater than zero".to_owned());
        }
        if self.max_sessions == 0 {
            return Err("--max-sessions must be greater than zero".to_owned());
        }
        if self.audio_source.as_deref().is_some_and(str::is_empty) {
            return Err("--audio-source must not be empty when configured".to_owned());
        }
        if self.turn_server.is_some() {
            if self.turn_user.as_deref().unwrap_or("").is_empty()
                || self.turn_password.as_deref().unwrap_or("").is_empty()
            {
                return Err(
                    "--turn-server requires non-empty --turn-user and --turn-password".to_owned(),
                );
            }
        } else if self.turn_user.is_some() || self.turn_password.is_some() {
            return Err("--turn-user/--turn-password require --turn-server".to_owned());
        }
        for value in [&self.turn_user, &self.turn_password].into_iter().flatten() {
            if value.chars().any(char::is_control) {
                return Err("TURN credentials must not contain control characters".to_owned());
            }
        }
        Ok(())
    }
}
