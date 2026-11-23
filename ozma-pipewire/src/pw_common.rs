//! Small pieces shared by the native PipeWire capture/sink/status threads.
//!
//! Everything here is Linux-only and safe Rust.

use std::sync::Mutex;

use pipewire as pw;
use pw::spa::{
    param::{
        audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS},
        format::{MediaSubtype, MediaType},
        ParamType,
    },
    pod::{serialize::PodSerializer, Object, Pod, Value},
    sys as spa_sys,
    utils::SpaTypes,
};
use tokio::sync::oneshot;

use crate::error::PipeWireError;
use crate::pcm::{NegotiatedFormat, SampleFormat};

/// Where a stream should connect.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StreamTarget {
    /// Let PipeWire autoconnect to the session default source/sink.
    #[default]
    Default,
    /// Connect to a named node/object (`target.object`), e.g. `alsa_input.foo`.
    Object(String),
    /// Connect directly to a graph node id.
    NodeId(u32),
}

impl StreamTarget {
    /// Value for the `target.object` property, when applicable.
    pub(crate) fn object_name(&self) -> Option<&str> {
        match self {
            Self::Object(name) => Some(name.as_str()),
            _ => None,
        }
    }

    /// Node id passed to `Stream::connect`, when applicable.
    pub(crate) fn node_id(&self) -> Option<u32> {
        match self {
            Self::NodeId(id) => Some(*id),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Default => "default".to_string(),
            Self::Object(name) => format!("object:{name}"),
            Self::NodeId(id) => format!("node:{id}"),
        }
    }

    /// Target the monitor of a named sink.
    ///
    /// PipeWire exposes a sink's monitor as `<sink>.monitor`.  This is a pure
    /// name construction; the sink and monitor must already exist in the graph.
    pub fn monitor_for_sink(sink: &str) -> Self {
        Self::Object(format!("{sink}.monitor"))
    }
}

/// Oneshot slot that reports the first negotiation result exactly once.
pub(crate) struct ReadySlot {
    inner: Mutex<Option<oneshot::Sender<Result<NegotiatedFormat, PipeWireError>>>>,
}

impl ReadySlot {
    pub(crate) fn new(
        tx: oneshot::Sender<Result<NegotiatedFormat, PipeWireError>>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: Mutex::new(Some(tx)),
        })
    }

    /// Send the outcome if nobody has sent it yet.  Never blocks on the
    /// receiver: `oneshot::Sender::send` is non-blocking.
    pub(crate) fn send(&self, result: Result<NegotiatedFormat, PipeWireError>) {
        let sender = self.inner.lock().ok().and_then(|mut slot| slot.take());
        if let Some(tx) = sender {
            let _ = tx.send(result);
        }
    }
}

/// Last fatal stream error, readable from the async side after the loop exits.
#[derive(Default)]
pub(crate) struct SharedError {
    inner: Mutex<Option<PipeWireError>>,
}

impl SharedError {
    pub(crate) fn set(&self, error: PipeWireError) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(error);
        }
    }

    pub(crate) fn take(&self) -> Option<PipeWireError> {
        self.inner.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Build a raw SPA EnumFormat pod for interleaved PCM at a fixed rate/layout.
pub(crate) fn build_format_pod(
    format: SampleFormat,
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<u8>, PipeWireError> {
    if channels == 0 || channels as usize > MAX_CHANNELS {
        return Err(PipeWireError::format_mismatch(
            format!("{channels} channels"),
            format!("1..{MAX_CHANNELS} channels"),
        ));
    }

    let mut info = AudioInfoRaw::new();
    info.set_format(match format {
        SampleFormat::S16Le => AudioFormat::S16LE,
        SampleFormat::F32Le => AudioFormat::F32LE,
    });
    info.set_rate(sample_rate);
    info.set_channels(channels as u32);
    if channels == 2 {
        let mut position = [0u32; MAX_CHANNELS];
        position[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(position);
    }

    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let serialized =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|e| PipeWireError::stream(format!("serialize audio format: {e:?}")))?;
    Ok(serialized.0.into_inner())
}

/// Parse an emitted Format parameter into a supported [`NegotiatedFormat`].
pub(crate) fn parse_negotiated(param: &Pod) -> Result<NegotiatedFormat, PipeWireError> {
    let (media_type, media_subtype) = pw::spa::param::format_utils::parse_format(param)
        .map_err(|e| PipeWireError::stream(format!("parse format: {e:?}")))?;
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return Err(PipeWireError::format_mismatch(
            format!("{media_type:?}/{media_subtype:?}"),
            "Audio/Raw",
        ));
    }

    let mut info = AudioInfoRaw::new();
    info.parse(param)
        .map_err(|e| PipeWireError::stream(format!("parse raw audio info: {e:?}")))?;

    let format = if info.format() == AudioFormat::S16LE {
        SampleFormat::S16Le
    } else if info.format() == AudioFormat::F32LE {
        SampleFormat::F32Le
    } else {
        return Err(PipeWireError::format_mismatch(
            format!("{:?}", info.format()),
            "S16LE or F32LE",
        ));
    };

    NegotiatedFormat::new(info.rate(), info.channels() as u16, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_helpers_map_to_pipewire_fields() {
        assert_eq!(StreamTarget::Default.object_name(), None);
        assert_eq!(StreamTarget::Default.node_id(), None);
        assert_eq!(
            StreamTarget::Object("mic".into()).object_name(),
            Some("mic")
        );
        assert_eq!(StreamTarget::Object("mic".into()).node_id(), None);
        assert_eq!(StreamTarget::NodeId(42).object_name(), None);
        assert_eq!(StreamTarget::NodeId(42).node_id(), Some(42));
        assert_eq!(StreamTarget::Object("mic".into()).describe(), "object:mic");
        assert_eq!(
            StreamTarget::monitor_for_sink("ozma-vm1"),
            StreamTarget::Object("ozma-vm1.monitor".into())
        );
        assert_eq!(
            StreamTarget::monitor_for_sink("alsa_output.pci-0000_00_1f.3.analog-stereo"),
            StreamTarget::Object("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor".into())
        );
        assert_eq!(
            StreamTarget::monitor_for_sink("ozma-vm1").describe(),
            "object:ozma-vm1.monitor"
        );
    }

    #[test]
    fn format_pods_round_trip_through_spa() {
        for (format, label) in [
            (SampleFormat::S16Le, "S16LE"),
            (SampleFormat::F32Le, "F32LE"),
        ] {
            let bytes = build_format_pod(format, 48_000, 2).expect("build pod");
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let negotiated = parse_negotiated(pod).expect("parse pod");
            assert_eq!(negotiated.sample_rate, 48_000);
            assert_eq!(negotiated.channels, 2);
            assert_eq!(negotiated.sample_format, format, "{label}");
        }
    }

    #[test]
    fn invalid_channel_count_is_a_typed_error() {
        let error = build_format_pod(SampleFormat::S16Le, 48_000, 0).unwrap_err();
        assert_eq!(error.code(), "pipewire.format_mismatch");
    }
}
