// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Node configuration derived from the CLI. The node deliberately keeps
//! configuration small: capability detection and profiles do the rest.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::ValueEnum;
use ozma_auth::MachineClass;
use ozma_input::SinkKind;
use ozma_protocol::NodeId;
use ozma_qmp::QmpConfig;
use serde::{Deserialize, Serialize};

use crate::webrtc::WebRtcAudioBackend;

/// Default QMP connect/greeting timeout, matching `ozma-qmp`.
pub const DEFAULT_QMP_CONNECT_TIMEOUT_MS: u64 = 3_000;
/// Default QMP control-command timeout, matching `ozma-qmp`.
pub const DEFAULT_QMP_COMMAND_TIMEOUT_MS: u64 = 5_000;

/// Which deployment shape this node is running as.
///
/// Profiles select honest provider defaults. The hardware runtime and the VM
/// profile's QMP capture/input/power wiring are implemented. Local HLS is
/// opt-in via `--hls`; WebRTC runs through an external helper when
/// `--webrtc-helper` is configured. The per-VM supervisor remains a later
/// phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NodeProfile {
    /// Physical KVM hardware: V4L2 capture, HID gadget input.
    #[default]
    Hardware,
    /// QEMU/libvirt guest: QMP capture/power, uinput input.
    Vm,
    /// Desktop seat: PipeWire capture, uinput input.
    Desktop,
}

impl NodeProfile {
    /// Stable name used by the CLI, serialized configuration, and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Vm => "vm",
            Self::Desktop => "desktop",
        }
    }
}

impl fmt::Display for NodeProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NodeProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hardware" => Ok(Self::Hardware),
            "vm" => Ok(Self::Vm),
            "desktop" => Ok(Self::Desktop),
            other => Err(format!(
                "unknown node profile {other:?}; expected hardware, vm, or desktop"
            )),
        }
    }
}

/// Provider chosen for each node subsystem.
///
/// These are nominal names; [`VmProviderConfig`] resolves whether a provider is
/// actually usable from the configured flags and turns absent flags into
/// `None`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSelection {
    /// Video capture backend (`v4l2`, `pipewire`, `qmp`, ...).
    pub capture: String,
    /// Input injection backend (`hid-gadget`, `uinput`, `qmp`, ...).
    pub input: String,
    /// Audio backend (`pipewire`, `alsa`, ...).
    pub audio: String,
    /// Virtual display backend (`fake`, `evdi`, ...).
    pub display: String,
    /// Power backend (`none`, `qmp`, `libvirt`).
    pub power: String,
}

impl Default for ProviderSelection {
    fn default() -> Self {
        Self::for_profile(NodeProfile::default())
    }
}

impl ProviderSelection {
    /// Explicit, honest provider defaults for a profile.
    ///
    /// `fake` display and `none` power mean "no provider yet", not "working".
    pub fn for_profile(profile: NodeProfile) -> Self {
        match profile {
            NodeProfile::Hardware => Self {
                capture: "v4l2".into(),
                input: "hid-gadget".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "none".into(),
            },
            NodeProfile::Vm => Self {
                capture: "qmp".into(),
                input: "uinput".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "qmp".into(),
            },
            NodeProfile::Desktop => Self {
                capture: "pipewire".into(),
                input: "uinput".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "none".into(),
            },
        }
    }

    /// Apply CLI overrides on top of the profile defaults.
    ///
    /// An override is applied only when it is `Some`; empty CLI strings are
    /// translated to `None` by [`ProviderOverrides::from_cli`].
    pub fn with_overrides(mut self, overrides: &ProviderOverrides) -> Self {
        if let Some(value) = &overrides.capture {
            self.capture = value.clone();
        }
        if let Some(value) = &overrides.input {
            self.input = value.clone();
        }
        if let Some(value) = &overrides.audio {
            self.audio = value.clone();
        }
        if let Some(value) = &overrides.display {
            self.display = value.clone();
        }
        if let Some(value) = &overrides.power {
            self.power = value.clone();
        }
        self
    }
}

/// Optional CLI overrides for [`ProviderSelection`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderOverrides {
    pub capture: Option<String>,
    pub input: Option<String>,
    pub audio: Option<String>,
    pub display: Option<String>,
    pub power: Option<String>,
}

impl ProviderOverrides {
    /// Build overrides from the five CLI strings. Empty or whitespace-only
    /// values mean "not set".
    pub fn from_cli(capture: &str, input: &str, audio: &str, display: &str, power: &str) -> Self {
        Self {
            capture: non_empty(capture),
            input: non_empty(input),
            audio: non_empty(audio),
            display: non_empty(display),
            power: non_empty(power),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// A power backend the VM profile can actually start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowerProvider {
    /// QEMU power control over the control QMP socket.
    Qmp,
    /// libvirt/virsh domain power control.
    Libvirt,
}

impl PowerProvider {
    /// Stable configuration/API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Qmp => "qmp",
            Self::Libvirt => "libvirt",
        }
    }
}

impl fmt::Display for PowerProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Audio routing backend selected by `--audio-router`.
///
/// `None` keeps the controller compatibility `/audio/*` routes at their honest
/// typed 501. The two real options construct a router over an **existing**
/// PipeWire/PulseAudio server; they do not start an audio server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AudioRouterKind {
    /// No audio router; `/audio/*` reports unsupported.
    #[default]
    #[value(name = "none")]
    None,
    /// `pactl` graph control plus `pw-play` reference tracks.
    #[value(name = "pipewire")]
    PipeWire,
    /// `pactl` graph control plus `paplay` reference tracks.
    #[value(name = "pulseaudio")]
    PulseAudio,
}

impl AudioRouterKind {
    /// Stable configuration/API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PipeWire => "pipewire",
            Self::PulseAudio => "pulseaudio",
        }
    }

    /// Whether a router is configured.
    pub const fn is_configured(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Display for AudioRouterKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The VM profile's resolved capture/input/power selection.
///
/// This is deliberately separate from [`ProviderSelection`]: the nominal
/// profile names are not enough to know whether a provider is usable. A VM node
/// has no hardware capture device, and QMP/libvirt need a configured socket or
/// domain. Resolution therefore returns `None` rather than advertising a
/// backend whose flag is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmProviderConfig {
    /// QEMU control QMP socket (capture and power).
    pub qmp_socket: Option<PathBuf>,
    /// Dedicated QMP input socket.
    pub qmp_input_socket: Option<PathBuf>,
    /// libvirt domain name for power control.
    pub libvirt_domain: Option<String>,
    /// QMP connect/handshake timeout.
    pub connect_timeout: Duration,
    /// QMP command timeout.
    pub command_timeout: Duration,
    /// Capture provider name, or `None` when no source is configured.
    pub capture: Option<String>,
    /// Ordered input sinks to try. Never empty.
    pub input_chain: Vec<SinkKind>,
    /// Power backend, or `None` when none is configured.
    pub power: Option<PowerProvider>,
}

impl VmProviderConfig {
    /// Resolve the VM profile's providers from the profile and CLI flags.
    ///
    /// This is pure: it performs no I/O and never opens a socket. A provider is
    /// only selected when the flag it needs is present. `hardware` and
    /// `desktop` keep their existing behavior, except that an explicit input
    /// provider override is honored.
    pub fn resolve(config: &NodeConfig) -> Result<Self, String> {
        config.validate()?;

        let capture = resolve_capture(config)?;
        let input_chain = resolve_input_chain(config)?;
        let power = resolve_power(config)?;

        Ok(Self {
            qmp_socket: config.qmp_socket.clone(),
            qmp_input_socket: config.qmp_input_socket.clone(),
            libvirt_domain: config.libvirt_domain.clone(),
            connect_timeout: config.qmp_connect_timeout,
            command_timeout: config.qmp_command_timeout,
            capture,
            input_chain,
            power,
        })
    }

    /// True when a capture source is configured.
    pub fn has_capture(&self) -> bool {
        self.capture.is_some()
    }

    /// True when power control is configured.
    pub fn has_power(&self) -> bool {
        self.power.is_some()
    }

    /// Human-readable input chain (`uinput -> qmp -> hid-gadget -> recording`).
    pub fn input_chain_names(&self) -> Vec<&'static str> {
        self.input_chain.iter().map(|kind| kind.as_str()).collect()
    }

    /// QMP configuration for the control socket (capture/power).
    ///
    /// Uses a single socket: the dedicated input socket must not be a
    /// prerequisite for capture or power.
    pub fn qmp_control_config(&self) -> Option<QmpConfig> {
        let control = self.qmp_socket.as_ref()?;
        Some(
            QmpConfig::new(control)
                .with_connect_timeout(self.connect_timeout)
                .with_command_timeout(self.command_timeout),
        )
    }

    /// QMP configuration for the input sink.
    ///
    /// When both a control and an input socket are configured the client opens
    /// both. With only the control socket, input uses the same connection in
    /// single-socket mode (matching `ozma-qmp`'s legacy fallback). With only an
    /// input socket, that socket is used as the control socket.
    ///
    /// The node normally prefers [`VmProviderConfig::qmp_dedicated_input_config`]
    /// when `--qmp-input-socket` is set, and a shared control client otherwise.
    pub fn qmp_input_config(&self) -> Option<QmpConfig> {
        let config = match (&self.qmp_socket, &self.qmp_input_socket) {
            (Some(control), Some(input)) => QmpConfig::dual(control, input),
            (Some(control), None) => QmpConfig::new(control),
            (None, Some(input)) => QmpConfig::new(input),
            (None, None) => return None,
        };
        Some(
            config
                .with_connect_timeout(self.connect_timeout)
                .with_command_timeout(self.command_timeout),
        )
    }

    /// QMP configuration that uses only the dedicated input socket.
    ///
    /// Returns `None` when `--qmp-input-socket` is absent. The returned config
    /// is dual-shaped so `QmpSink::spawn_dedicated_input` keeps fire-and-forget
    /// writes and connects only the input half; the control path is a required
    /// config field but is never opened by that path.
    pub fn qmp_dedicated_input_config(&self) -> Option<QmpConfig> {
        let input = self.qmp_input_socket.as_ref()?;
        let control = self.qmp_socket.as_ref().unwrap_or(input);
        Some(
            QmpConfig::dual(control, input)
                .with_connect_timeout(self.connect_timeout)
                .with_command_timeout(self.command_timeout),
        )
    }
}

fn resolve_capture(config: &NodeConfig) -> Result<Option<String>, String> {
    let vnc_requested = config.vnc_host.is_some() || config.vnc_port.is_some();
    let dbus_requested = config.dbus_requested;

    if let Some(value) = &config.provider_overrides.capture {
        let name = value.trim().to_ascii_lowercase();
        return match name.as_str() {
            "qmp" => {
                if config.qmp_socket.is_none() {
                    return Err("--capture-provider qmp requires --qmp-socket".to_owned());
                }
                Ok(Some("qmp".to_owned()))
            }
            "vnc" => {
                if config.profile != NodeProfile::Vm {
                    return Err("--capture-provider vnc requires --profile vm".to_owned());
                }
                validate_vnc_flags(config, true)?;
                Ok(Some("vnc".to_owned()))
            }
            "dbus" => {
                if config.profile != NodeProfile::Vm {
                    return Err("--capture-provider dbus requires --profile vm".to_owned());
                }
                if config.qmp_socket.is_none() {
                    return Err("--capture-provider dbus requires --qmp-socket".to_owned());
                }
                if vnc_requested {
                    return Err(
                        "--capture-provider dbus conflicts with --vnc-host/--vnc-port; \
                         select one VM display capture provider"
                            .to_owned(),
                    );
                }
                Ok(Some("dbus".to_owned()))
            }
            _ if config.profile == NodeProfile::Vm => Err(format!(
                "unknown capture provider {value:?} for the vm profile; expected qmp, dbus, or vnc"
            )),
            _ if vnc_requested => Err(format!(
                "--capture-provider {value:?} conflicts with --vnc-host/--vnc-port; \
                 select --capture-provider vnc or remove the VNC flags"
            )),
            _ if dbus_requested => Err(format!(
                "--capture-provider {value:?} conflicts with the D-Bus display flags; \
                 select --capture-provider dbus or remove --dbus-console/--dbus-width/\
                 --dbus-height/--dbus-fps"
            )),
            _ => Ok(Some(value.clone())),
        };
    }

    if config.profile == NodeProfile::Vm {
        if vnc_requested && dbus_requested {
            return Err(
                "--vnc-host/--vnc-port and the D-Bus display flags are both set; \
                 select exactly one VM display capture provider"
                    .to_owned(),
            );
        }
        if dbus_requested {
            if config.qmp_socket.is_none() {
                return Err(
                    "the QEMU D-Bus display flags (--dbus-console/--dbus-width/--dbus-height/\
                     --dbus-fps) require --qmp-socket"
                        .to_owned(),
                );
            }
            return Ok(Some("dbus".to_owned()));
        }
        if vnc_requested {
            validate_vnc_flags(config, false)?;
            return Ok(Some("vnc".to_owned()));
        }
        // QMP keeps precedence when no VNC/D-Bus endpoint and no provider was
        // asked for. A VM with neither configured gets no capture source.
        return Ok(if config.qmp_socket.is_some() {
            Some("qmp".to_owned())
        } else {
            None
        });
    }

    if vnc_requested {
        return Err(
            "--vnc-host/--vnc-port select VNC capture, which requires --profile vm".to_owned(),
        );
    }
    if dbus_requested {
        return Err(
            "the QEMU D-Bus display flags select D-Bus capture, which requires --profile vm"
                .to_owned(),
        );
    }

    Ok(match config.profile {
        NodeProfile::Hardware => Some("v4l2".to_owned()),
        NodeProfile::Desktop => Some("pipewire".to_owned()),
        NodeProfile::Vm => unreachable!("vm profile handled above"),
    })
}

/// VNC capture needs both endpoint flags. A partial endpoint is an error rather
/// than an invitation to invent a host or port from the defaults.
fn validate_vnc_flags(config: &NodeConfig, explicit_provider: bool) -> Result<(), String> {
    match (&config.vnc_host, config.vnc_port) {
        (Some(_), Some(_)) => Ok(()),
        (None, None) if explicit_provider => {
            Err("--capture-provider vnc requires --vnc-host and --vnc-port".to_owned())
        }
        _ => Err("--vnc-host and --vnc-port must be provided together for VNC capture".to_owned()),
    }
}

fn resolve_input_chain(config: &NodeConfig) -> Result<Vec<SinkKind>, String> {
    if let Some(value) = &config.provider_overrides.input {
        let kind = parse_sink_kind(value)?;
        if kind == SinkKind::Qmp && config.qmp_socket.is_none() && config.qmp_input_socket.is_none()
        {
            return Err(
                "--input-provider qmp requires --qmp-socket or --qmp-input-socket".to_owned(),
            );
        }
        return Ok(vec![kind]);
    }

    Ok(match config.profile {
        NodeProfile::Hardware => vec![SinkKind::HidGadget, SinkKind::Recording],
        NodeProfile::Vm | NodeProfile::Desktop => {
            let mut chain = vec![SinkKind::Uinput];
            if config.qmp_socket.is_some() || config.qmp_input_socket.is_some() {
                chain.push(SinkKind::Qmp);
            }
            chain.push(SinkKind::HidGadget);
            chain.push(SinkKind::Recording);
            chain
        }
    })
}

fn parse_sink_kind(value: &str) -> Result<SinkKind, String> {
    // `fake-recording` is the controller-facing name for the recording sink.
    if value.eq_ignore_ascii_case("fake-recording") || value.eq_ignore_ascii_case("fake_recording")
    {
        return Ok(SinkKind::Recording);
    }
    SinkKind::from_str(value).map_err(|error| error.to_string())
}

fn resolve_power(config: &NodeConfig) -> Result<Option<PowerProvider>, String> {
    if let Some(value) = &config.provider_overrides.power {
        return match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(None),
            "qmp" => {
                if config.qmp_socket.is_none() {
                    return Err("--power-provider qmp requires --qmp-socket".to_owned());
                }
                Ok(Some(PowerProvider::Qmp))
            }
            "libvirt" => {
                if config.libvirt_domain.is_none() {
                    return Err("--power-provider libvirt requires --libvirt-domain".to_owned());
                }
                Ok(Some(PowerProvider::Libvirt))
            }
            other => Err(format!(
                "unknown power provider {other:?}; expected none, qmp, or libvirt"
            )),
        };
    }

    if config.profile != NodeProfile::Vm {
        return Ok(None);
    }

    Ok(if config.qmp_socket.is_some() {
        Some(PowerProvider::Qmp)
    } else if config.libvirt_domain.is_some() {
        Some(PowerProvider::Libvirt)
    } else {
        None
    })
}

/// Effective node configuration.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub name: String,
    pub node_id: NodeId,
    pub hid_udp_port: u16,
    pub http_port: u16,
    pub rtsp_port: u16,
    pub capabilities: Vec<String>,
    pub hw: String,
    pub fw: String,
    pub register_url: Option<String>,
    pub register_host: Option<String>,
    pub heartbeat_interval_secs: u64,
    pub role: String,
    pub machine_class: MachineClass,
    pub workstation_consent: bool,
    pub api_token: Option<String>,
    pub profile: NodeProfile,
    pub providers: ProviderSelection,
    /// CLI overrides applied on top of the profile defaults.
    pub provider_overrides: ProviderOverrides,
    /// V4L2/other capture device path advertised to the controller.
    pub capture_device: Option<String>,
    /// QEMU control QMP socket (VM capture and power).
    pub qmp_socket: Option<PathBuf>,
    /// Dedicated QEMU QMP input socket.
    pub qmp_input_socket: Option<PathBuf>,
    /// libvirt domain name used for power control.
    pub libvirt_domain: Option<String>,
    /// QMP connect/handshake timeout.
    pub qmp_connect_timeout: Duration,
    /// QMP command timeout.
    pub qmp_command_timeout: Duration,
    /// QEMU VNC host for VM display capture.
    pub vnc_host: Option<String>,
    /// QEMU VNC port for VM display capture.
    pub vnc_port: Option<u16>,
    /// Scaled VNC output width.
    pub vnc_width: u32,
    /// Scaled VNC output height.
    pub vnc_height: u32,
    /// VNC output frame rate.
    pub vnc_fps: u32,
    /// Effective D-Bus console index (`Console_N`).
    pub dbus_console: u32,
    /// Largest D-Bus scanout width accepted.
    pub dbus_width: u32,
    /// Largest D-Bus scanout height accepted.
    pub dbus_height: u32,
    /// D-Bus frame-rate ceiling.
    pub dbus_fps: u32,
    /// True when any D-Bus display flag was supplied explicitly. A default
    /// console index of 0 must not select D-Bus capture on its own.
    pub dbus_requested: bool,
    /// Audio routing backend. `none` leaves `/audio/*` at typed 501.
    pub audio_router: AudioRouterKind,
    /// Directory containing reference tracks for `POST /audio/play`.
    pub audio_track_root: Option<PathBuf>,
    /// ffmpeg executable used by VNC capture and HLS.
    pub ffmpeg_bin: PathBuf,
    /// Audio sink name advertised to the controller.
    pub audio_sink: Option<String>,
    /// Optional PulseAudio/PipeWire monitor source for WebRTC audio.
    pub webrtc_audio_source: Option<String>,
    /// Capture backend requested for the WebRTC audio source.
    pub webrtc_audio_backend: WebRtcAudioBackend,
    /// Guest IP advertised by VM soft nodes.
    pub vm_guest_ip: Option<String>,
    /// JSON-encoded display output list sent to the controller.
    pub display_outputs_json: String,
    /// JSON-encoded PCI passthrough device list sent to the controller.
    pub pci_devices_json: String,
}

impl NodeConfig {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|value| value.trim() == capability)
    }

    pub fn node_id_from_name(name: &str) -> NodeId {
        NodeId::new(format!("{name}._ozma._udp.local."))
    }

    /// The local consent policy for this machine.
    pub fn consent_policy(&self) -> ozma_auth::ConsentPolicy {
        ozma_auth::ConsentPolicy::new(self.machine_class, self.workstation_consent)
    }

    /// Reject values the controller would fail to decode.
    ///
    /// `DirectRegisterRequest` parses `display_outputs` and `pci_devices` with
    /// `json.loads(...)` and expects lists. Catching malformed JSON here turns a
    /// controller-side 500 into an actionable startup error.
    pub fn validate(&self) -> Result<(), String> {
        validate_json_array("display_outputs", &self.display_outputs_json)?;
        validate_json_array("pci_devices", &self.pci_devices_json)?;
        validate_path("--qmp-socket", self.qmp_socket.as_deref())?;
        validate_path("--qmp-input-socket", self.qmp_input_socket.as_deref())?;
        validate_path("--ffmpeg-bin", Some(self.ffmpeg_bin.as_path()))?;
        if let Some(host) = &self.vnc_host {
            if host.trim().is_empty() {
                return Err("--vnc-host must not be empty".to_owned());
            }
        }
        if self.vnc_port == Some(0) {
            return Err("--vnc-port must be greater than zero".to_owned());
        }
        if self.vnc_width == 0 {
            return Err("--vnc-width must be greater than zero".to_owned());
        }
        if self.vnc_height == 0 {
            return Err("--vnc-height must be greater than zero".to_owned());
        }
        if self.vnc_fps == 0 {
            return Err("--vnc-fps must be greater than zero".to_owned());
        }
        if self.dbus_width == 0 {
            return Err("--dbus-width must be greater than zero".to_owned());
        }
        if self.dbus_height == 0 {
            return Err("--dbus-height must be greater than zero".to_owned());
        }
        if self.dbus_fps == 0 {
            return Err("--dbus-fps must be greater than zero".to_owned());
        }
        if self.dbus_width > ozma_capture::MAX_DBUS_DIMENSION
            || self.dbus_height > ozma_capture::MAX_DBUS_DIMENSION
        {
            return Err(format!(
                "--dbus-width/--dbus-height exceed the {}-pixel bound",
                ozma_capture::MAX_DBUS_DIMENSION
            ));
        }
        validate_path("--audio-track-root", self.audio_track_root.as_deref())?;
        if self
            .webrtc_audio_source
            .as_deref()
            .is_some_and(|source| source.trim().is_empty())
        {
            return Err("--webrtc-audio-source must not be empty or whitespace".to_owned());
        }
        if let Some(domain) = &self.libvirt_domain {
            if domain.trim().is_empty() {
                return Err("--libvirt-domain must not be empty".to_owned());
            }
        }
        if self.qmp_connect_timeout.is_zero() {
            return Err("--qmp-connect-timeout-ms must be greater than zero".to_owned());
        }
        if self.qmp_command_timeout.is_zero() {
            return Err("--qmp-command-timeout-ms must be greater than zero".to_owned());
        }
        Ok(())
    }

    /// Resolve the profile's capture/input/power providers from flags.
    pub fn vm_providers(&self) -> Result<VmProviderConfig, String> {
        VmProviderConfig::resolve(self)
    }
}

fn validate_path(label: &str, path: Option<&std::path::Path>) -> Result<(), String> {
    if let Some(path) = path {
        if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
            return Err(format!("{label} must not be empty"));
        }
    }
    Ok(())
}

fn validate_json_array(label: &str, value: &str) -> Result<(), String> {
    let flag = format!("--{}-json", label.replace('_', "-"));
    let parsed: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| format!("{flag} is not valid JSON: {error}"))?;
    if !parsed.is_array() {
        return Err(format!("{flag} must be a JSON array"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_hardware_and_names_round_trip() {
        assert_eq!(NodeProfile::default(), NodeProfile::Hardware);

        for (profile, name) in [
            (NodeProfile::Hardware, "hardware"),
            (NodeProfile::Vm, "vm"),
            (NodeProfile::Desktop, "desktop"),
        ] {
            assert_eq!(profile.to_string(), name);
            assert_eq!(profile.as_str(), name);
            assert_eq!(
                <NodeProfile as std::str::FromStr>::from_str(name).unwrap(),
                profile
            );
        }

        // clap's ValueEnum parser must accept exactly the same names.
        assert_eq!(
            <NodeProfile as ValueEnum>::from_str("hardware", false).unwrap(),
            NodeProfile::Hardware
        );
        assert_eq!(
            <NodeProfile as ValueEnum>::from_str("vm", false).unwrap(),
            NodeProfile::Vm
        );
        assert_eq!(
            <NodeProfile as ValueEnum>::from_str("desktop", false).unwrap(),
            NodeProfile::Desktop
        );
        assert!(<NodeProfile as ValueEnum>::from_str("bogus", false).is_err());
    }

    #[test]
    fn provider_defaults_are_explicit_per_profile() {
        assert_eq!(
            ProviderSelection::for_profile(NodeProfile::Hardware),
            ProviderSelection {
                capture: "v4l2".into(),
                input: "hid-gadget".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "none".into(),
            }
        );
        assert_eq!(
            ProviderSelection::for_profile(NodeProfile::Vm),
            ProviderSelection {
                capture: "qmp".into(),
                input: "uinput".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "qmp".into(),
            }
        );
        assert_eq!(
            ProviderSelection::for_profile(NodeProfile::Desktop),
            ProviderSelection {
                capture: "pipewire".into(),
                input: "uinput".into(),
                audio: "pipewire".into(),
                display: "fake".into(),
                power: "none".into(),
            }
        );

        // `Default` tracks the default (hardware) profile.
        assert_eq!(
            ProviderSelection::default(),
            ProviderSelection::for_profile(NodeProfile::Hardware)
        );
    }

    #[test]
    fn empty_cli_overrides_keep_profile_defaults() {
        let overrides = ProviderOverrides::from_cli("", "   ", "", "", "");
        assert_eq!(overrides, ProviderOverrides::default());

        let selected = ProviderSelection::for_profile(NodeProfile::Vm).with_overrides(&overrides);
        assert_eq!(selected, ProviderSelection::for_profile(NodeProfile::Vm));
    }

    #[test]
    fn registration_json_fields_must_be_arrays() {
        assert!(validate_json_array("display_outputs", "[]").is_ok());
        assert!(validate_json_array("display_outputs", r#"[{"name":"HDMI-1"}]"#).is_ok());
        assert!(validate_json_array("pci_devices", r#"["0000:01:00.0"]"#).is_ok());

        let invalid = validate_json_array("display_outputs", "not-json").unwrap_err();
        assert!(invalid.contains("not valid JSON"), "{invalid}");
        let object = validate_json_array("pci_devices", "{}").unwrap_err();
        assert!(object.contains("must be a JSON array"), "{object}");
    }

    #[test]
    fn cli_overrides_take_precedence_over_profile_defaults() {
        let overrides = ProviderOverrides::from_cli(" qmp ", "", "alsa", "", "libvirt");
        let selected =
            ProviderSelection::for_profile(NodeProfile::Desktop).with_overrides(&overrides);

        assert_eq!(selected.capture, "qmp");
        assert_eq!(selected.input, "uinput"); // desktop default kept
        assert_eq!(selected.audio, "alsa");
        assert_eq!(selected.display, "fake"); // desktop default kept
        assert_eq!(selected.power, "libvirt");

        let partial = ProviderOverrides::from_cli("v4l2", "uinput", "", "", "");
        let selected = ProviderSelection::for_profile(NodeProfile::Vm).with_overrides(&partial);
        assert_eq!(selected.capture, "v4l2");
        assert_eq!(selected.input, "uinput");
        assert_eq!(selected.audio, "pipewire"); // vm default kept
        assert_eq!(selected.power, "qmp"); // vm default kept
    }

    fn test_config(profile: NodeProfile) -> NodeConfig {
        NodeConfig {
            name: "node".into(),
            node_id: NodeId::new("node._ozma._udp.local."),
            hid_udp_port: 7331,
            http_port: 7382,
            rtsp_port: 0,
            capabilities: vec!["hid".into(), "video".into(), "audio".into()],
            hw: "test".into(),
            fw: "0.1.0".into(),
            register_url: None,
            register_host: None,
            heartbeat_interval_secs: 30,
            role: "compute".into(),
            machine_class: MachineClass::Server,
            workstation_consent: false,
            api_token: None,
            profile,
            providers: ProviderSelection::for_profile(profile),
            provider_overrides: ProviderOverrides::default(),
            capture_device: None,
            qmp_socket: None,
            qmp_input_socket: None,
            libvirt_domain: None,
            qmp_connect_timeout: Duration::from_millis(DEFAULT_QMP_CONNECT_TIMEOUT_MS),
            qmp_command_timeout: Duration::from_millis(DEFAULT_QMP_COMMAND_TIMEOUT_MS),
            vnc_host: None,
            vnc_port: None,
            vnc_width: ozma_capture::VNC_DEFAULT_WIDTH,
            vnc_height: ozma_capture::VNC_DEFAULT_HEIGHT,
            vnc_fps: ozma_capture::VNC_DEFAULT_FPS,
            dbus_console: ozma_capture::DBUS_DEFAULT_CONSOLE_INDEX,
            dbus_width: ozma_capture::DBUS_DEFAULT_WIDTH,
            dbus_height: ozma_capture::DBUS_DEFAULT_HEIGHT,
            dbus_fps: ozma_capture::DBUS_DEFAULT_FPS,
            dbus_requested: false,
            audio_router: AudioRouterKind::None,
            audio_track_root: None,
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            audio_sink: None,
            webrtc_audio_source: None,
            webrtc_audio_backend: WebRtcAudioBackend::Auto,
            vm_guest_ip: None,
            display_outputs_json: "[]".into(),
            pci_devices_json: "[]".into(),
        }
    }

    fn vm_config_with_overrides(capture: &str, input: &str, power: &str) -> NodeConfig {
        let mut config = test_config(NodeProfile::Vm);
        config.provider_overrides = ProviderOverrides::from_cli(capture, input, "", "", power);
        config.providers = ProviderSelection::for_profile(NodeProfile::Vm)
            .with_overrides(&config.provider_overrides);
        config
    }

    #[test]
    fn vm_without_flags_selects_no_capture_or_power() {
        let config = test_config(NodeProfile::Vm);
        let resolved = config.vm_providers().unwrap();

        assert_eq!(resolved.capture, None);
        assert_eq!(
            resolved.input_chain,
            vec![SinkKind::Uinput, SinkKind::HidGadget, SinkKind::Recording]
        );
        assert_eq!(resolved.power, None);
        assert!(!resolved.has_capture());
        assert!(!resolved.has_power());
    }

    #[test]
    fn vm_with_qmp_socket_selects_qmp_capture_power_and_input() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        config.qmp_input_socket = Some(PathBuf::from("/run/ozma/input.qmp"));

        let resolved = config.vm_providers().unwrap();
        assert_eq!(resolved.capture.as_deref(), Some("qmp"));
        assert_eq!(
            resolved.input_chain,
            vec![
                SinkKind::Uinput,
                SinkKind::Qmp,
                SinkKind::HidGadget,
                SinkKind::Recording
            ]
        );
        assert_eq!(resolved.power, Some(PowerProvider::Qmp));

        let control = resolved.qmp_control_config().unwrap();
        assert_eq!(control.control_path, PathBuf::from("/run/ozma/ctrl.qmp"));
        assert!(!control.is_dual());

        let input = resolved.qmp_input_config().unwrap();
        assert_eq!(input.control_path, PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(input.input_path, Some(PathBuf::from("/run/ozma/input.qmp")));
        assert!(input.is_dual());
    }

    #[test]
    fn vm_with_only_libvirt_domain_selects_libvirt_power() {
        let mut config = test_config(NodeProfile::Vm);
        config.libvirt_domain = Some("windows-gpu".into());

        let resolved = config.vm_providers().unwrap();
        assert_eq!(resolved.capture, None);
        assert_eq!(resolved.power, Some(PowerProvider::Libvirt));
    }

    #[test]
    fn hardware_and_desktop_keep_their_existing_input_chains() {
        let hardware = test_config(NodeProfile::Hardware);
        assert_eq!(
            hardware.vm_providers().unwrap().input_chain,
            vec![SinkKind::HidGadget, SinkKind::Recording]
        );
        assert_eq!(
            hardware.vm_providers().unwrap().capture.as_deref(),
            Some("v4l2")
        );

        let desktop = test_config(NodeProfile::Desktop);
        assert_eq!(
            desktop.vm_providers().unwrap().input_chain,
            vec![SinkKind::Uinput, SinkKind::HidGadget, SinkKind::Recording]
        );
        assert_eq!(
            desktop.vm_providers().unwrap().capture.as_deref(),
            Some("pipewire")
        );
        assert_eq!(desktop.vm_providers().unwrap().power, None);
    }

    #[test]
    fn input_provider_qmp_requires_a_qmp_socket() {
        let config = vm_config_with_overrides("", "qmp", "");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--qmp-socket"), "{error}");

        // The control socket is enough: `ozma-qmp` falls back to single-socket
        // input when no dedicated input socket is configured.
        let mut config = vm_config_with_overrides("", "qmp", "");
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().input_chain,
            vec![SinkKind::Qmp]
        );

        let mut config = vm_config_with_overrides("", "qmp", "");
        config.qmp_input_socket = Some(PathBuf::from("/run/ozma/input.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().input_chain,
            vec![SinkKind::Qmp]
        );
    }

    #[test]
    fn explicit_power_and_capture_providers_require_their_flags() {
        let mut config = vm_config_with_overrides("qmp", "", "");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--qmp-socket"), "{error}");

        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("qmp")
        );

        let config = vm_config_with_overrides("", "", "libvirt");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--libvirt-domain"), "{error}");

        let config = vm_config_with_overrides("", "", "none");
        assert_eq!(config.vm_providers().unwrap().power, None);

        let config = vm_config_with_overrides("", "", "warp-drive");
        assert!(config.vm_providers().is_err());
    }

    #[test]
    fn empty_paths_domains_and_zero_timeouts_are_rejected() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::new());
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.qmp_input_socket = Some(PathBuf::from("   "));
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.libvirt_domain = Some("  ".into());
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.qmp_connect_timeout = Duration::ZERO;
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.qmp_command_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn vm_with_vnc_flags_selects_vnc_capture() {
        let mut config = test_config(NodeProfile::Vm);
        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);

        let resolved = config.vm_providers().unwrap();
        assert_eq!(resolved.capture.as_deref(), Some("vnc"));
        assert!(resolved.has_capture());

        // An explicit provider is accepted with a complete endpoint.
        let mut config = vm_config_with_overrides("vnc", "", "");
        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("vnc")
        );

        // Either flag alone is enough to select VNC, but then the endpoint is
        // incomplete and resolution must fail instead of inventing a value.
        let mut config = test_config(NodeProfile::Vm);
        config.vnc_port = Some(5901);
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--vnc-host"), "{error}");

        let mut config = test_config(NodeProfile::Vm);
        config.vnc_host = Some("127.0.0.1".into());
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--vnc-port"), "{error}");
    }

    #[test]
    fn explicit_vnc_provider_requires_both_endpoint_flags() {
        let config = vm_config_with_overrides("vnc", "", "");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--vnc-host"), "{error}");
        assert!(error.contains("--vnc-port"), "{error}");
    }

    #[test]
    fn vnc_flags_take_precedence_over_qmp_when_no_provider_is_explicit() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);

        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("vnc")
        );

        // Without VNC flags the QMP socket keeps its existing precedence.
        config.vnc_host = None;
        config.vnc_port = None;
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("qmp")
        );
    }

    #[test]
    fn vnc_capture_is_vm_only_and_vm_rejects_unknown_providers() {
        let mut hardware = test_config(NodeProfile::Hardware);
        hardware.vnc_host = Some("127.0.0.1".into());
        hardware.vnc_port = Some(5901);
        let error = hardware.vm_providers().unwrap_err();
        assert!(error.contains("--profile vm"), "{error}");

        let mut hardware = vm_config_with_overrides("vnc", "", "");
        hardware.profile = NodeProfile::Hardware;
        hardware.vnc_host = Some("127.0.0.1".into());
        hardware.vnc_port = Some(5901);
        let error = hardware.vm_providers().unwrap_err();
        assert!(error.contains("--profile vm"), "{error}");

        let config = vm_config_with_overrides("v4l2", "", "");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("expected qmp, dbus, or vnc"), "{error}");
    }

    #[test]
    fn invalid_vnc_values_are_rejected() {
        let mut config = test_config(NodeProfile::Vm);
        config.vnc_host = Some("   ".into());
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.vnc_port = Some(0);
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.vnc_width = 0;
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.vnc_height = 0;
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.vnc_fps = 0;
        assert!(config.validate().is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.ffmpeg_bin = PathBuf::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn dbus_flags_select_dbus_only_on_a_vm_with_a_qmp_socket() {
        let mut config = test_config(NodeProfile::Vm);
        config.dbus_requested = true;
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--qmp-socket"), "{error}");

        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("dbus")
        );

        // A non-VM profile must not turn host hardware into D-Bus capture.
        let mut hardware = test_config(NodeProfile::Hardware);
        hardware.dbus_requested = true;
        let error = hardware.vm_providers().unwrap_err();
        assert!(error.contains("--profile vm"), "{error}");
    }

    #[test]
    fn explicit_dbus_provider_requires_vm_profile_and_qmp_socket() {
        let config = vm_config_with_overrides("dbus", "", "");
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("--qmp-socket"), "{error}");

        let mut config = vm_config_with_overrides("dbus", "", "");
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("dbus")
        );

        let mut hardware = vm_config_with_overrides("dbus", "", "");
        hardware.profile = NodeProfile::Hardware;
        hardware.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        let error = hardware.vm_providers().unwrap_err();
        assert!(error.contains("--profile vm"), "{error}");
    }

    #[test]
    fn dbus_and_vnc_flags_are_ambiguous_without_an_explicit_provider() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        config.dbus_requested = true;
        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("exactly one"), "{error}");

        // An explicit provider is accepted, but the conflicting VNC flags are
        // rejected rather than silently ignored.
        let mut config = vm_config_with_overrides("dbus", "", "");
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);
        let error = config.vm_providers().unwrap_err();
        assert!(error.contains("conflicts"), "{error}");
    }

    #[test]
    fn qmp_remains_the_default_and_vnc_still_wins_over_qmp() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("qmp")
        );

        config.vnc_host = Some("127.0.0.1".into());
        config.vnc_port = Some(5901);
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("vnc")
        );
    }

    #[test]
    fn webrtc_audio_source_and_backend_validate() {
        let mut config = test_config(NodeProfile::Vm);
        assert!(config.webrtc_audio_source.is_none());
        assert_eq!(config.webrtc_audio_backend, WebRtcAudioBackend::Auto);
        assert!(config.validate().is_ok());

        config.webrtc_audio_source = Some("   ".into());
        assert!(config.validate().is_err());

        config.webrtc_audio_source = Some("ozma-vm1.monitor".into());
        config.webrtc_audio_backend = WebRtcAudioBackend::PipeWire;
        assert!(config.validate().is_ok());
        assert_eq!(
            <WebRtcAudioBackend as ValueEnum>::from_str("pipewire", false).unwrap(),
            WebRtcAudioBackend::PipeWire
        );
        assert!(<WebRtcAudioBackend as ValueEnum>::from_str("alsa", false).is_err());
    }

    #[test]
    fn audio_router_names_and_track_root_validate() {
        assert_eq!(AudioRouterKind::default(), AudioRouterKind::None);
        assert_eq!(AudioRouterKind::None.as_str(), "none");
        assert_eq!(AudioRouterKind::PipeWire.as_str(), "pipewire");
        assert_eq!(AudioRouterKind::PulseAudio.as_str(), "pulseaudio");
        assert!(!AudioRouterKind::None.is_configured());
        assert!(AudioRouterKind::PipeWire.is_configured());
        assert_eq!(
            <AudioRouterKind as ValueEnum>::from_str("pipewire", false).unwrap(),
            AudioRouterKind::PipeWire
        );
        assert_eq!(
            <AudioRouterKind as ValueEnum>::from_str("pulseaudio", false).unwrap(),
            AudioRouterKind::PulseAudio
        );
        assert!(<AudioRouterKind as ValueEnum>::from_str("pipe-wire", false).is_err());

        let mut config = test_config(NodeProfile::Vm);
        config.audio_track_root = Some(PathBuf::new());
        assert!(config.validate().is_err());
    }
}
