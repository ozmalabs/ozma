// SPDX-License-Identifier: AGPL-3.0-only
//! Named capture targets for PipeWire and other audio backends.
//!
//! [`AudioTarget`] is the backend-neutral way to say *what* to capture: the
//! session default, a named PipeWire object, a graph node id, or the monitor
//! of a named sink.  It deliberately stops at selection: a `SinkMonitor`
//! names an object that must already exist in the graph, and does not create
//! a null sink, move streams, or provision per-VM routing.
//!
//! The textual form is stable and round-trips through [`FromStr`],
//! [`Display`](std::fmt::Display), and serde:
//!
//! | Value | Example |
//! |---|---|
//! | default | `default` |
//! | named object | `object:alsa_input.usb-mic` |
//! | graph node | `node:42` |
//! | sink monitor | `monitor:ozma-vm1` |
//!
//! A sink monitor is represented by the *sink* name.  The conventional
//! PipeWire monitor object name is `<sink>.monitor`; the conversion to
//! [`ozma_pipewire::StreamTarget`] applies that suffix, so callers never
//! build the string themselves.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::AudioError;

/// Prefix used by the stable textual form of [`AudioTarget::Object`].
pub const OBJECT_PREFIX: &str = "object:";
/// Prefix used by the stable textual form of [`AudioTarget::NodeId`].
pub const NODE_PREFIX: &str = "node:";
/// Prefix used by the stable textual form of [`AudioTarget::SinkMonitor`].
pub const MONITOR_PREFIX: &str = "monitor:";

/// A named audio capture target.
///
/// The target is a *selection*, not a provisioning operation.  In particular
/// [`AudioTarget::SinkMonitor`] requires the sink and its monitor node to
/// already exist in the PipeWire graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AudioTarget {
    /// Let the backend choose its default source.
    Default,
    /// A named graph object, for example `alsa_input.usb-mic`.
    Object(String),
    /// A numeric PipeWire graph node id.
    NodeId(u32),
    /// The monitor of a named sink, for example `ozma-vm1`.
    ///
    /// The PipeWire object name is `<sink>.monitor`.
    SinkMonitor(String),
}

impl AudioTarget {
    /// Validate a named target and build it.
    pub fn object(name: impl Into<String>) -> Result<Self, AudioError> {
        let name = name.into();
        validate_name("object", &name)?;
        Ok(Self::Object(name))
    }

    /// Build the monitor target for a named sink.
    ///
    /// The sink name is validated.  The monitor object name itself is
    /// constructed later by [`AudioTarget::to_stream_target`] (Linux), so the
    /// stable representation stays tied to the sink.
    pub fn sink_monitor(sink: impl Into<String>) -> Result<Self, AudioError> {
        let sink = sink.into();
        validate_name("sink", &sink)?;
        Ok(Self::SinkMonitor(sink))
    }

    /// Convenience form of [`AudioTarget::sink_monitor`].
    ///
    /// # Panics
    ///
    /// Panics if `sink` is empty, whitespace-only, or contains control
    /// characters.  Use [`AudioTarget::sink_monitor`] when the sink name comes
    /// from untrusted input.
    pub fn monitor_for_sink(sink: &str) -> AudioTarget {
        Self::sink_monitor(sink)
            .expect("sink name must be non-empty and contain no control characters")
    }

    /// The conventional PipeWire monitor object name for `sink`.
    ///
    /// This is a pure string helper: it does not check that the sink exists.
    pub fn monitor_name(sink: &str) -> String {
        format!("{sink}.monitor")
    }

    /// Validate a target built from its public variants.
    ///
    /// Direct enum construction bypasses the validated constructors, so
    /// callers that accept an `AudioTarget` over an API boundary should call
    /// this before opening a stream.
    pub fn validate(&self) -> Result<(), AudioError> {
        match self {
            Self::Default | Self::NodeId(_) => Ok(()),
            Self::Object(name) => validate_name("object", name),
            Self::SinkMonitor(sink) => validate_name("sink", sink),
        }
    }

    /// Human-readable description for diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::Default => "default PipeWire source".to_owned(),
            Self::Object(name) => format!("PipeWire object `{name}`"),
            Self::NodeId(id) => format!("PipeWire node {id}"),
            Self::SinkMonitor(sink) => {
                format!("monitor `{}` of sink `{sink}`", Self::monitor_name(sink))
            }
        }
    }

    /// Convert to the native PipeWire stream target.
    ///
    /// `SinkMonitor(sink)` becomes `Object("<sink>.monitor")`.
    #[cfg(target_os = "linux")]
    pub fn to_stream_target(&self) -> ozma_pipewire::StreamTarget {
        use ozma_pipewire::StreamTarget;

        match self {
            Self::Default => StreamTarget::Default,
            Self::Object(name) => StreamTarget::Object(name.clone()),
            Self::NodeId(id) => StreamTarget::NodeId(*id),
            Self::SinkMonitor(sink) => StreamTarget::monitor_for_sink(sink),
        }
    }
}

impl Default for AudioTarget {
    fn default() -> Self {
        Self::Default
    }
}

impl fmt::Display for AudioTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Object(name) => write!(f, "{OBJECT_PREFIX}{name}"),
            Self::NodeId(id) => write!(f, "{NODE_PREFIX}{id}"),
            Self::SinkMonitor(sink) => write!(f, "{MONITOR_PREFIX}{sink}"),
        }
    }
}

impl FromStr for AudioTarget {
    type Err = AudioError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim();
        if raw == "default" {
            return Ok(Self::Default);
        }
        if let Some(name) = raw.strip_prefix(OBJECT_PREFIX) {
            return Self::object(name);
        }
        if let Some(id) = raw.strip_prefix(NODE_PREFIX) {
            let id = id.parse::<u32>().map_err(|_| {
                AudioError::InvalidTarget(format!(
                    "`{raw}` is not a valid target: `{NODE_PREFIX}` must be followed by a u32 node id"
                ))
            })?;
            return Ok(Self::NodeId(id));
        }
        if let Some(sink) = raw.strip_prefix(MONITOR_PREFIX) {
            return Self::sink_monitor(sink);
        }
        Err(AudioError::InvalidTarget(format!(
            "`{raw}` is not a valid audio target; expected `default`, `{OBJECT_PREFIX}<name>`, \
             `{NODE_PREFIX}<id>`, or `{MONITOR_PREFIX}<sink>`"
        )))
    }
}

impl Serialize for AudioTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AudioTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(target_os = "linux")]
impl From<&AudioTarget> for ozma_pipewire::StreamTarget {
    fn from(target: &AudioTarget) -> Self {
        target.to_stream_target()
    }
}

#[cfg(target_os = "linux")]
impl From<AudioTarget> for ozma_pipewire::StreamTarget {
    fn from(target: AudioTarget) -> Self {
        target.to_stream_target()
    }
}

fn validate_name(kind: &str, name: &str) -> Result<(), AudioError> {
    if name.is_empty() {
        return Err(AudioError::InvalidTarget(format!(
            "{kind} name must not be empty"
        )));
    }
    if name.trim().is_empty() {
        return Err(AudioError::InvalidTarget(format!(
            "{kind} name must not be whitespace-only"
        )));
    }
    if let Some(control) = name.chars().find(|c| c.is_control()) {
        return Err(AudioError::InvalidTarget(format!(
            "{kind} name must not contain control character {control:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_name_uses_the_conventional_suffix() {
        assert_eq!(AudioTarget::monitor_name("ozma-vm1"), "ozma-vm1.monitor");
        assert_eq!(
            AudioTarget::monitor_name("alsa_output.pci-0000_00_1f.3.analog-stereo"),
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        );
    }

    #[test]
    fn monitor_for_sink_keeps_the_sink_name_stable() {
        let target = AudioTarget::monitor_for_sink("ozma-vm1");
        assert_eq!(target, AudioTarget::SinkMonitor("ozma-vm1".to_owned()));
        assert_eq!(target.to_string(), "monitor:ozma-vm1");
        assert!(target.describe().contains("ozma-vm1.monitor"));
    }

    #[test]
    fn object_and_sink_names_are_validated() {
        assert!(AudioTarget::object("mic").is_ok());
        assert!(AudioTarget::sink_monitor("ozma-vm1").is_ok());

        for invalid in ["", " ", "\t", "\n"] {
            assert!(
                AudioTarget::object(invalid).is_err(),
                "object {invalid:?} should be rejected"
            );
            assert!(
                AudioTarget::sink_monitor(invalid).is_err(),
                "sink {invalid:?} should be rejected"
            );
        }

        assert!(AudioTarget::object("mic\u{7}").is_err());
        assert!(AudioTarget::sink_monitor("ozma\u{1b}[31m").is_err());

        // Direct construction can bypass the constructors, so validation is
        // available at the API boundary too.
        assert!(AudioTarget::Object(String::new()).validate().is_err());
        assert!(AudioTarget::SinkMonitor("  ".to_owned())
            .validate()
            .is_err());
        assert!(AudioTarget::Object("mic".to_owned()).validate().is_ok());
    }

    #[test]
    #[should_panic(expected = "sink name must be non-empty")]
    fn panic_convenience_rejects_an_invalid_sink() {
        let _ = AudioTarget::monitor_for_sink("");
    }

    #[test]
    fn display_and_parse_round_trip() {
        assert_eq!(AudioTarget::default(), AudioTarget::Default);
        let targets = [
            AudioTarget::Default,
            AudioTarget::Object("alsa_input.usb-mic".to_owned()),
            AudioTarget::NodeId(42),
            AudioTarget::SinkMonitor("ozma-vm1".to_owned()),
        ];
        for target in targets {
            let text = target.to_string();
            let parsed: AudioTarget = text.parse().expect("round trip");
            assert_eq!(parsed, target, "{text}");
        }
    }

    #[test]
    fn parse_rejects_unknown_and_malformed_values() {
        for raw in [
            "",
            "speaker",
            "object:",
            "object: \t",
            "node:",
            "node:not-a-number",
            "monitor:",
            "monitor:\u{7}",
        ] {
            assert!(raw.parse::<AudioTarget>().is_err(), "{raw:?}");
        }
    }

    #[test]
    fn validation_errors_carry_actionable_context() {
        let error = "object:".parse::<AudioTarget>().unwrap_err();
        assert!(matches!(error, AudioError::InvalidTarget(_)));
        assert!(error.to_string().contains("object"));
        let error = AudioTarget::SinkMonitor("\u{1b}".to_owned())
            .validate()
            .unwrap_err();
        assert!(matches!(error, AudioError::InvalidTarget(_)));
        assert!(error.to_string().contains("control"));
    }

    #[test]
    fn serde_uses_the_stable_text_form() {
        let target = AudioTarget::SinkMonitor("ozma-vm1".to_owned());
        let json = serde_json::to_string(&target).unwrap();
        assert_eq!(json, "\"monitor:ozma-vm1\"");
        let parsed: AudioTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, target);

        let error = serde_json::from_str::<AudioTarget>("\"object:\"");
        assert!(error.is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn targets_map_to_pipewire_stream_targets() {
        use ozma_pipewire::StreamTarget;

        assert_eq!(
            AudioTarget::Default.to_stream_target(),
            StreamTarget::Default
        );
        assert_eq!(
            AudioTarget::Object("mic".to_owned()).to_stream_target(),
            StreamTarget::Object("mic".to_owned())
        );
        assert_eq!(
            AudioTarget::NodeId(42).to_stream_target(),
            StreamTarget::NodeId(42)
        );
        assert_eq!(
            AudioTarget::SinkMonitor("ozma-vm1".to_owned()).to_stream_target(),
            StreamTarget::Object("ozma-vm1.monitor".to_owned())
        );
        assert_eq!(
            StreamTarget::from(AudioTarget::SinkMonitor("ozma-vm1".to_owned())),
            StreamTarget::Object("ozma-vm1.monitor".to_owned())
        );
    }
}
