//! Typed errors for the native PipeWire stream layer.
//!
//! Every error carries a stable `pipewire.*` code so callers (diagnostics,
//! the node API, the controller) can branch on it instead of matching
//! human-readable strings.  [`PipeWireError::remediation`] provides the
//! actionable half of the message.

use std::fmt;

/// Errors produced while probing, capturing from, or playing to PipeWire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeWireError {
    /// No PipeWire daemon/session could be reached.
    Unavailable { detail: String },
    /// The stream negotiated a sample format/rate/layout we cannot represent.
    FormatMismatch { got: String, expected: String },
    /// The stream negotiated a raw video layout OZMA cannot carry.
    VideoFormatUnsupported { got: String, expected: String },
    /// The graph offered a compressed video stream, which raw capture cannot use.
    CompressedVideoUnsupported { format: String },
    /// No `Video/Source` node matched the requested target.
    NoVideoSource { detail: String },
    /// A stream or probe did not reach its ready state within the timeout.
    StartTimeout { detail: String },
    /// A status probe timed out without a definitive answer from the server.
    ProbeTimeout { detail: String },
    /// The stream was closed (server shutdown, node removed, or receiver gone).
    Disconnected { detail: String },
    /// The bounded playback queue had no room for the requested samples.
    QueueFull {
        capacity: usize,
        requested: usize,
        available: usize,
    },
    /// A PipeWire operation failed.
    Stream { detail: String },
    /// The PipeWire OS thread could not be created.
    Thread { detail: String },
    /// A shared queue mutex was poisoned by a panicking thread.
    Poisoned,
}

impl PipeWireError {
    /// Stable machine-readable code, matching the node's error convention.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "pipewire.unavailable",
            Self::FormatMismatch { .. } => "pipewire.format_mismatch",
            Self::VideoFormatUnsupported { .. } => "pipewire.video_format_unsupported",
            Self::CompressedVideoUnsupported { .. } => "pipewire.compressed_video",
            Self::NoVideoSource { .. } => "pipewire.no_video_source",
            Self::StartTimeout { .. } => "pipewire.start_timeout",
            Self::ProbeTimeout { .. } => "pipewire.probe_timeout",
            Self::Disconnected { .. } => "pipewire.disconnected",
            Self::QueueFull { .. } => "pipewire.queue_full",
            Self::Stream { .. } => "pipewire.stream_error",
            Self::Thread { .. } => "pipewire.thread_error",
            Self::Poisoned => "pipewire.internal",
        }
    }

    /// Actionable next step for a human or the diagnostics UI.
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => {
                "Start PipeWire (systemctl --user start pipewire) and check that \
                 XDG_RUNTIME_DIR (or PIPEWIRE_REMOTE) points at the live session."
            }
            Self::FormatMismatch { .. } => {
                "Ask the graph for a format the pipeline carries (audio: 48 kHz \
                 stereo S16LE/F32LE; video: raw BGRA/BGRx/RGBx/I420/NV12/YUYV), or let \
                 PipeWire insert its converter by not using the NO_CONVERT flag."
            }
            Self::VideoFormatUnsupported { .. } => {
                "Select a raw video layout (BGRA, BGRx, RGBx, I420, NV12, or YUYV). \
                 Compressed camera formats such as MJPEG/H.264 must be decoded by \
                 a codec path, not opened as a raw PipeWire video stream."
            }
            Self::CompressedVideoUnsupported { .. } => {
                "This stream only offers compressed video. Capture through the \
                 V4L2/MJPEG path or add a decoder node before the raw capture stream."
            }
            Self::NoVideoSource { .. } => {
                "Start a screen-cast portal session or connect a camera so the \
                 graph exposes a Video/Source node, then refresh capture discovery."
            }
            Self::StartTimeout { .. } => {
                "The server accepted the connection but no stream format was \
                 negotiated. Check `pw-cli info 0` and that an audio source/sink exists."
            }
            Self::ProbeTimeout { .. } => {
                "The PipeWire server did not answer a sync round-trip. Check the \
                 daemon and the configured remote."
            }
            Self::Disconnected { .. } => {
                "Reconnect when PipeWire is available again; the target node may \
                 have been removed from the graph."
            }
            Self::QueueFull { .. } => {
                "Drain playback (or call flush), write smaller blocks, or open the \
                 sink with a larger queue capacity."
            }
            Self::Stream { .. } => {
                "Check `pw-cli info`/`pw-dump` for the target node and the server log."
            }
            Self::Thread { .. } => "Check process thread limits and retry.",
            Self::Poisoned => "A previous panic poisoned shared audio state; restart the stream.",
        }
    }

    pub(crate) fn unavailable(detail: impl Into<String>) -> Self {
        Self::Unavailable {
            detail: detail.into(),
        }
    }

    pub(crate) fn stream(detail: impl Into<String>) -> Self {
        Self::Stream {
            detail: detail.into(),
        }
    }

    /// Classify server-side stream errors: transport-level failures are
    /// reported as `pipewire.unavailable`, everything else as a stream error.
    pub(crate) fn stream_or_unavailable(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let lower = detail.to_ascii_lowercase();
        let transport = [
            "host is down",
            "connection refused",
            "connection reset",
            "no such file",
            "permission denied",
            "not permitted",
            "could not connect",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if transport {
            Self::Unavailable { detail }
        } else {
            Self::Stream { detail }
        }
    }

    pub(crate) fn disconnected(detail: impl Into<String>) -> Self {
        Self::Disconnected {
            detail: detail.into(),
        }
    }

    pub(crate) fn format_mismatch(got: impl Into<String>, expected: impl Into<String>) -> Self {
        Self::FormatMismatch {
            got: got.into(),
            expected: expected.into(),
        }
    }

    pub(crate) fn video_format_unsupported(
        got: impl Into<String>,
        expected: impl Into<String>,
    ) -> Self {
        Self::VideoFormatUnsupported {
            got: got.into(),
            expected: expected.into(),
        }
    }

    pub(crate) fn compressed_video(format: impl Into<String>) -> Self {
        Self::CompressedVideoUnsupported {
            format: format.into(),
        }
    }
}

impl fmt::Display for PipeWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { detail } => write!(f, "PipeWire is unavailable: {detail}"),
            Self::FormatMismatch { got, expected } => {
                write!(f, "unsupported media format {got} (expected {expected})")
            }
            Self::VideoFormatUnsupported { got, expected } => {
                write!(f, "unsupported video format {got} (expected {expected})")
            }
            Self::CompressedVideoUnsupported { format } => {
                write!(f, "compressed video stream {format} cannot be captured raw")
            }
            Self::NoVideoSource { detail } => {
                write!(f, "no PipeWire video source: {detail}")
            }
            Self::StartTimeout { detail } => {
                write!(f, "PipeWire stream startup timed out: {detail}")
            }
            Self::ProbeTimeout { detail } => write!(f, "PipeWire probe timed out: {detail}"),
            Self::Disconnected { detail } => write!(f, "PipeWire stream disconnected: {detail}"),
            Self::QueueFull {
                capacity,
                requested,
                available,
            } => write!(
                f,
                "playback queue full: requested {requested} samples, {available} free of {capacity}"
            ),
            Self::Stream { detail } => write!(f, "PipeWire stream error: {detail}"),
            Self::Thread { detail } => write!(f, "could not start PipeWire thread: {detail}"),
            Self::Poisoned => f.write_str("shared audio queue lock poisoned"),
        }
    }
}

impl std::error::Error for PipeWireError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_distinct() {
        let errors = [
            PipeWireError::unavailable("x"),
            PipeWireError::format_mismatch("u8", "s16le"),
            PipeWireError::video_format_unsupported("mjpeg", "bgra"),
            PipeWireError::compressed_video("video/encoded"),
            PipeWireError::NoVideoSource {
                detail: "x".to_owned(),
            },
            PipeWireError::StartTimeout { detail: "x".into() },
            PipeWireError::ProbeTimeout { detail: "x".into() },
            PipeWireError::disconnected("x"),
            PipeWireError::QueueFull {
                capacity: 4,
                requested: 8,
                available: 0,
            },
            PipeWireError::stream("x"),
            PipeWireError::Thread { detail: "x".into() },
            PipeWireError::Poisoned,
        ];
        let mut codes: Vec<_> = errors.iter().map(PipeWireError::code).collect();
        assert!(codes.iter().all(|c| c.starts_with("pipewire.")));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len(), "codes must be unique");
        assert!(errors.iter().all(|e| !e.remediation().is_empty()));
    }

    #[test]
    fn display_mentions_context() {
        let e = PipeWireError::format_mismatch("S32LE", "S16LE/F32LE");
        let s = e.to_string();
        assert!(s.contains("S32LE"));
        assert!(s.contains("S16LE/F32LE"));
    }
}
