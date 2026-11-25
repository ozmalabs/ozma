// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Node configuration derived from the CLI. The node deliberately keeps
//! configuration small: capability detection and profiles do the rest.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::ValueEnum;
use ozma_auth::MachineClass;
use ozma_capture::{DBUS_DEFAULT_ID, QMP_DEFAULT_ID, VNC_DEFAULT_ID};
use ozma_display::vm::{VmConsoleBackend, MAX_VM_CONSOLES};
use ozma_input::SinkKind;
use ozma_protocol::NodeId;
use ozma_qmp::QmpConfig;
use ozma_usb::MediaRestartPolicy;
use serde::{Deserialize, Serialize};

use crate::display_outputs::{self, DisplayOutput, DisplayOutputSelection};
use crate::webrtc::WebRtcAudioBackend;

/// Default QMP connect/greeting timeout, matching `ozma-qmp`.
pub const DEFAULT_QMP_CONNECT_TIMEOUT_MS: u64 = 3_000;
/// Default QMP control-command timeout, matching `ozma-qmp`.
pub const DEFAULT_QMP_COMMAND_TIMEOUT_MS: u64 = 5_000;

/// Default dev/test synthetic capture width.
pub const DEFAULT_FAKE_WIDTH: u32 = 1280;
/// Default dev/test synthetic capture height.
pub const DEFAULT_FAKE_HEIGHT: u32 = 720;
/// Default dev/test synthetic capture frame rate.
pub const DEFAULT_FAKE_FPS: u32 = 30;
/// Largest dev/test synthetic capture dimension accepted.
///
/// 4K (3840x2160) is the validation target; this bound leaves room for 8K
/// without letting a typo allocate an unbounded RGB24 frame.
pub const MAX_FAKE_DIMENSION: u32 = 8_192;
/// Highest dev/test synthetic capture frame rate accepted.
pub const MAX_FAKE_FPS: u32 = 240;

/// ConfigFS composite-gadget root used by `--usb-configfs-root`.
pub const DEFAULT_USB_CONFIGFS_ROOT: &str = ozma_usb::DEFAULT_CONFIGFS_ROOT;
/// Gadget directory name presented by the node.
pub const DEFAULT_USB_GADGET_NAME: &str = "ozma";
/// Mass-storage function instance used for `--usb-media`.
pub const DEFAULT_USB_MASS_STORAGE_INSTANCE: &str = "usb0";

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

/// The PipeWire video node a desktop node captures.
///
/// `Default` lets the graph choose. A screen-cast session returns a normal
/// `Video/Source` node, so there is no portal-specific variant here: the portal
/// handshake happens outside this process, and the resulting node id/name is
/// passed as configuration. Parsing accepts the same forms as the capture
/// capability ids (`pipewire:name:...`, `pipewire:node:...`) plus the short
/// `name:`/`object:`/`node:` forms.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum DesktopCaptureTarget {
    /// Use the graph's default video source.
    #[default]
    Default,
    /// A named PipeWire object (`node.name`).
    Object(String),
    /// A numeric PipeWire node id.
    NodeId(u32),
}

impl DesktopCaptureTarget {
    /// Human-readable description for logs and diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::Default => "default PipeWire video source".to_owned(),
            Self::Object(name) => format!("PipeWire object `{name}`"),
            Self::NodeId(id) => format!("PipeWire node {id}"),
        }
    }
}

impl fmt::Display for DesktopCaptureTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::Object(name) => write!(formatter, "object:{name}"),
            Self::NodeId(id) => write!(formatter, "node:{id}"),
        }
    }
}

impl FromStr for DesktopCaptureTarget {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(
                "--desktop-capture-target must not be empty; use `default` or a node/object name"
                    .to_owned(),
            );
        }
        if raw == "default" || raw == "pipewire:default" {
            return Ok(Self::Default);
        }
        for prefix in ["object:", "name:", "pipewire:name:"] {
            if let Some(name) = raw.strip_prefix(prefix) {
                validate_capture_name(name)?;
                return Ok(Self::Object(name.to_owned()));
            }
        }
        for prefix in ["node:", "pipewire:node:"] {
            if let Some(id) = raw.strip_prefix(prefix) {
                let id = id.parse::<u32>().map_err(|_| {
                    format!(
                        "--desktop-capture-target `{raw}` has a non-numeric node id; \
                         use `node:<u32>`"
                    )
                })?;
                return Ok(Self::NodeId(id));
            }
        }
        // A bare name is accepted as a convenience; it is still an exact
        // PipeWire object name, not a fuzzy match.
        validate_capture_name(raw)?;
        Ok(Self::Object(raw.to_owned()))
    }
}

fn validate_capture_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("--desktop-capture-target name must not be empty".to_owned());
    }
    if name.chars().any(char::is_control) {
        return Err("--desktop-capture-target name must not contain control characters".to_owned());
    }
    Ok(())
}

/// XDG Desktop Portal `SelectSources.types` choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum PortalSourceKind {
    /// Select a monitor (the whole desktop screen).
    #[default]
    Monitor,
    /// Select one window.
    Window,
    /// Select a virtual source (ScreenCast version >= 2).
    Virtual,
}

impl PortalSourceKind {
    /// Stable CLI/API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::Virtual => "virtual",
        }
    }
}

/// Portal cursor embedding choice (`SelectSources.cursor_mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum PortalCursorKind {
    /// The compositor omits the cursor from the stream.
    #[default]
    Hidden,
    /// The compositor draws the cursor into the frames.
    Embedded,
    /// Cursor metadata is exported separately.
    Metadata,
}

impl PortalCursorKind {
    /// Stable CLI/API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Embedded => "embedded",
            Self::Metadata => "metadata",
        }
    }
}

/// Portal ScreenCast handshake settings.
///
/// The handshake may show a consent dialog and therefore runs in `main` before
/// the capture registry is finalised, never during capability enumeration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalCaptureOptions {
    /// Source type offered by the selection dialog.
    pub source: PortalSourceKind,
    /// Cursor embedding policy.
    pub cursor: PortalCursorKind,
    /// Restore token from a previous portal session, when supplied.
    pub restore_token: Option<String>,
    /// Bound on the whole consent/selection handshake.
    pub timeout: Duration,
    /// Requested PipeWire mode for the selected node.
    pub width: u32,
    /// Requested PipeWire height for the selected node.
    pub height: u32,
    /// Requested PipeWire frame rate for the selected node.
    pub fps: u32,
}

/// The desktop profile's resolved capture transport.
///
/// Portal capture is deliberately represented before its handshake: the stream
/// is resolved separately in `main`, and a denial/timeout is an honest no-capture
/// state rather than a fallback to X11, a camera, or the fake source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopCaptureSelection {
    /// Open an existing PipeWire `Video/Source` node.
    PipeWire(DesktopCaptureTarget),
    /// ffmpeg `x11grab` against an explicitly named X display.
    X11(ozma_capture::X11CaptureConfig),
    /// Run the XDG Portal ScreenCast handshake, then open the selected node.
    Portal(PortalCaptureOptions),
    /// Dev/test synthetic RGB24 source.
    Fake,
    /// No capture source is configured.
    None,
}

impl DesktopCaptureSelection {
    /// Short provider name for logs and `/api/v1/connection`.
    pub const fn provider_name(&self) -> &'static str {
        match self {
            Self::PipeWire(_) => "pipewire",
            Self::X11(_) => "x11",
            Self::Portal(_) => "portal",
            Self::Fake => "fake",
            Self::None => "none",
        }
    }
}

/// The desktop profile's resolved capture/input/audio selection.
///
/// This is pure like [`VmProviderConfig`]: it never probes or opens a source.
/// The portal handshake is the one asynchronous step and happens in `main`
/// before the registry is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopProviderConfig {
    /// Resolved capture transport.
    pub capture: DesktopCaptureSelection,
    /// Remote PipeWire instance name, when configured.
    pub capture_remote: Option<String>,
    /// Requested capture width.
    pub capture_width: u32,
    /// Requested capture height.
    pub capture_height: u32,
    /// Requested capture frame rate.
    pub capture_fps: u32,
    /// Ordered input sinks to try. Never empty and never contains QMP unless a
    /// QMP socket was configured.
    pub input_chain: Vec<SinkKind>,
    /// PipeWire audio capture target (default source or a sink monitor).
    pub audio_capture: ozma_audio::AudioTarget,
}

impl DesktopProviderConfig {
    /// Resolve the desktop profile from configuration. Performs no I/O.
    pub fn resolve(config: &NodeConfig) -> Result<Self, String> {
        config.validate()?;
        let capture_provider = resolve_capture(config)?;
        let capture = match capture_provider.as_deref() {
            Some("fake") => DesktopCaptureSelection::Fake,
            Some("pipewire") => {
                DesktopCaptureSelection::PipeWire(config.desktop_capture_target.clone())
            }
            Some("x11") => DesktopCaptureSelection::X11(config.x11_capture_config()?),
            Some("portal") => DesktopCaptureSelection::Portal(config.portal_capture_options()),
            Some(other) => {
                return Err(format!(
                    "capture provider `{other}` is not a desktop capture path; expected \
                     pipewire, x11, portal, or fake (dev/test)"
                ));
            }
            None => DesktopCaptureSelection::None,
        };
        Ok(Self {
            capture,
            capture_remote: config.desktop_capture_remote.clone(),
            capture_width: config.desktop_capture_width,
            capture_height: config.desktop_capture_height,
            capture_fps: config.desktop_capture_fps,
            input_chain: resolve_input_chain(config)?,
            audio_capture: config.audio_capture_target.clone(),
        })
    }

    /// Human-readable input chain for logs.
    pub fn input_chain_names(&self) -> Vec<&'static str> {
        self.input_chain.iter().map(|kind| kind.as_str()).collect()
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
    let looking_glass_requested = config.looking_glass_shm.is_some();

    if let Some(value) = &config.provider_overrides.capture {
        let name = value.trim().to_ascii_lowercase();
        return match name.as_str() {
            "qmp" => {
                if config.qmp_socket.is_none() {
                    return Err("--capture-provider qmp requires --qmp-socket".to_owned());
                }
                if config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider qmp conflicts with --looking-glass-shm; \
                         select one VM display capture provider"
                            .to_owned(),
                    );
                }
                Ok(Some("qmp".to_owned()))
            }
            "vnc" => {
                if config.profile != NodeProfile::Vm {
                    return Err("--capture-provider vnc requires --profile vm".to_owned());
                }
                if config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider vnc conflicts with --looking-glass-shm; \
                         select one VM display capture provider"
                            .to_owned(),
                    );
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
                if config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider dbus conflicts with --looking-glass-shm; \
                         select one VM display capture provider"
                            .to_owned(),
                    );
                }
                Ok(Some("dbus".to_owned()))
            }
            "looking-glass" | "looking_glass" | "lookingglass" => {
                if config.profile != NodeProfile::Vm {
                    return Err("--capture-provider looking-glass requires --profile vm".to_owned());
                }
                if config.looking_glass_shm.is_none() {
                    return Err(
                        "--capture-provider looking-glass requires --looking-glass-shm".to_owned(),
                    );
                }
                if vnc_requested {
                    return Err(
                        "--capture-provider looking-glass conflicts with --vnc-host/--vnc-port; \
                         select one VM display capture provider"
                            .to_owned(),
                    );
                }
                if dbus_requested {
                    return Err(
                        "--capture-provider looking-glass conflicts with the D-Bus display \
                         flags; select one VM display capture provider"
                            .to_owned(),
                    );
                }
                Ok(Some("looking-glass".to_owned()))
            }
            // Dev/test only: a synthetic RGB24 source for validating the
            // encode/HLS path on a host with no real capture device. It is
            // deliberately unavailable on the hardware profile so it can never
            // become a production default or mask a missing V4L2 source.
            "fake" => {
                if config.profile == NodeProfile::Hardware {
                    return Err(
                        "--capture-provider fake is a dev/test synthetic source and is not \
                         available in the hardware profile; use --profile vm or desktop"
                            .to_owned(),
                    );
                }
                if vnc_requested {
                    return Err(
                        "--capture-provider fake conflicts with --vnc-host/--vnc-port; \
                         remove the VNC flags"
                            .to_owned(),
                    );
                }
                if dbus_requested {
                    return Err(
                        "--capture-provider fake conflicts with the D-Bus display flags; \
                         remove --dbus-console/--dbus-width/--dbus-height/--dbus-fps"
                            .to_owned(),
                    );
                }
                if config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider fake conflicts with --looking-glass-shm; \
                         remove the Looking Glass flags"
                            .to_owned(),
                    );
                }
                if config.x11_requested {
                    return Err(
                        "--capture-provider fake conflicts with the X11 capture flags; \
                         remove --x11-display/--x11-offset/--x11-width/--x11-height/--x11-fps"
                            .to_owned(),
                    );
                }
                if config.portal_requested {
                    return Err(
                        "--capture-provider fake conflicts with the portal capture flags; \
                         remove --portal-source-type/--portal-cursor/--portal-restore-token/\
                         --portal-timeout-ms"
                            .to_owned(),
                    );
                }
                Ok(Some("fake".to_owned()))
            }
            "pipewire" => {
                if config.profile != NodeProfile::Desktop {
                    // The hardware registry already includes PipeWire; keep the
                    // historical permissive override (it is not used to build
                    // the hardware registry) outside the desktop profile.
                    return Ok(Some("pipewire".to_owned()));
                }
                if vnc_requested || dbus_requested {
                    return Err(
                        "--capture-provider pipewire conflicts with the VM display flags; \
                         remove --vnc-host/--vnc-port and the D-Bus display flags"
                            .to_owned(),
                    );
                }
                if config.x11_requested {
                    return Err(
                        "--capture-provider pipewire conflicts with the X11 capture flags; \
                         remove --x11-display/--x11-offset/--x11-width/--x11-height/--x11-fps"
                            .to_owned(),
                    );
                }
                if config.portal_requested {
                    return Err(
                        "--capture-provider pipewire conflicts with the portal capture flags; \
                         remove the --portal-* flags"
                            .to_owned(),
                    );
                }
                Ok(Some("pipewire".to_owned()))
            }
            "x11" => {
                if config.profile != NodeProfile::Desktop {
                    return Err("--capture-provider x11 requires --profile desktop".to_owned());
                }
                if vnc_requested || dbus_requested || config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider x11 conflicts with the VM display flags; \
                         select one capture provider"
                            .to_owned(),
                    );
                }
                if config.x11_display.is_none() {
                    return Err(
                        "--capture-provider x11 requires --x11-display (:0, :0.0, or host:10.0)"
                            .to_owned(),
                    );
                }
                Ok(Some("x11".to_owned()))
            }
            "portal" => {
                if config.profile != NodeProfile::Desktop {
                    return Err("--capture-provider portal requires --profile desktop".to_owned());
                }
                if vnc_requested || dbus_requested || config.looking_glass_shm.is_some() {
                    return Err(
                        "--capture-provider portal conflicts with the VM display flags; \
                         select one capture provider"
                            .to_owned(),
                    );
                }
                // The portal handshake may be denied or unavailable; that is a
                // warned no-capture state, not a reason to reject the profile.
                Ok(Some("portal".to_owned()))
            }
            _ if config.profile == NodeProfile::Vm => Err(format!(
                "unknown capture provider {value:?} for the vm profile; expected qmp, dbus, \
                 vnc, looking-glass, or fake (dev/test)"
            )),
            _ if config.profile == NodeProfile::Desktop => Err(format!(
                "unknown capture provider {value:?} for the desktop profile; expected pipewire, \
                 x11, portal, or fake (dev/test)"
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
        if config.x11_requested {
            return Err("the X11 capture flags require --profile desktop".to_owned());
        }
        if config.portal_requested {
            return Err("the portal capture flags require --profile desktop".to_owned());
        }
        let selected_endpoints = [vnc_requested, dbus_requested, looking_glass_requested]
            .into_iter()
            .filter(|selected| *selected)
            .count();
        if selected_endpoints > 1 {
            return Err(
                "--vnc-host/--vnc-port, the D-Bus display flags, and --looking-glass-shm are \
                 mutually exclusive; select exactly one VM display capture provider"
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
        if looking_glass_requested {
            return Ok(Some("looking-glass".to_owned()));
        }
        if vnc_requested {
            validate_vnc_flags(config, false)?;
            return Ok(Some("vnc".to_owned()));
        }
        // QMP keeps precedence when no VNC/D-Bus/Looking Glass endpoint and no
        // provider was asked for. A VM with none configured gets no capture.
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
    if looking_glass_requested {
        return Err(
            "--looking-glass-shm selects Looking Glass capture, which requires --profile vm"
                .to_owned(),
        );
    }
    if config.x11_requested {
        if config.profile == NodeProfile::Desktop {
            return Ok(Some("x11".to_owned()));
        }
        return Err(
            "--x11-display/--x11-offset/--x11-width/--x11-height/--x11-fps select X11 capture, \
             which requires --profile desktop"
                .to_owned(),
        );
    }
    if config.portal_requested {
        if config.profile == NodeProfile::Desktop {
            return Ok(Some("portal".to_owned()));
        }
        return Err(
            "the --portal-* flags select XDG Portal ScreenCast capture, which requires \
             --profile desktop"
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

/// The VM console a configured capture provider targets.
///
/// This is the bridge between provider selection and the display-output list:
/// D-Bus can select `Console_N`, while QMP/VNC/Looking Glass capture a single
/// default head. Geometry is whatever the provider actually produces; `0` means
/// it is not known until the first frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureDisplaySource {
    /// Capture transport.
    pub backend: VmConsoleBackend,
    /// Advertised capture source id.
    pub source_id: String,
    /// Console index the provider targets.
    pub console_index: u32,
    /// Produced width, or `0` when unknown before the first frame.
    pub width: u32,
    /// Produced height, or `0` when unknown before the first frame.
    pub height: u32,
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
    /// Serve the local capture source as an RFB/VNC endpoint.
    pub vnc_server: bool,
    /// Explicit RFB bind address. `None` keeps the loopback default; supplying
    /// any value is the operator opt-in that permits a widened bind.
    pub vnc_bind: Option<String>,
    /// RFB listener port; `0` asks the OS for a free port.
    pub vnc_server_port: u16,
    /// File containing the RFB VNC authentication password. Read once at
    /// startup; its presence enables RFB security type 2.
    pub vnc_password_file: Option<PathBuf>,
    /// Explicit opt-in for a non-loopback RFB bind **without** a password.
    pub vnc_allow_unauthenticated: bool,
    /// Maximum concurrent RFB viewers.
    pub vnc_max_clients: usize,
    /// Close an RFB viewer that sends no client message for this many seconds.
    /// `0` disables the idle timeout.
    pub vnc_idle_timeout_seconds: u64,
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
    /// Console selected for the one live capture source (`--capture-console`).
    /// D-Bus honours any index; QMP/VNC/Looking Glass capture the default head.
    pub capture_console: Option<u32>,
    /// Repeated `--display-output <label:WxH[:primary][:source]>` arguments.
    pub display_output_specs: Vec<String>,
    /// Validated/normalized display outputs; empty when none were configured
    /// (or when the raw `--display-outputs-json` escape hatch is used).
    pub display_outputs: Vec<DisplayOutput>,
    /// Looking Glass KVMFR shared-memory path (`--looking-glass-shm`).
    pub looking_glass_shm: Option<PathBuf>,
    /// Looking Glass polling rate (`--looking-glass-fps`).
    pub looking_glass_fps: u32,
    /// Looking Glass capture source id (`--looking-glass-id`).
    pub looking_glass_id: String,
    /// Looking Glass human-readable source name (`--looking-glass-name`).
    pub looking_glass_name: String,
    /// Dev/test synthetic capture width (`--fake-width`).
    ///
    /// Only used when the capture provider is `fake`, which is rejected for the
    /// hardware profile. Never a production default.
    pub fake_width: u32,
    /// Dev/test synthetic capture height (`--fake-height`).
    pub fake_height: u32,
    /// Dev/test synthetic capture frame rate (`--fake-fps`).
    pub fake_fps: u32,
    /// Requested desktop PipeWire capture width.
    pub desktop_capture_width: u32,
    /// Requested desktop PipeWire capture height.
    pub desktop_capture_height: u32,
    /// Requested desktop PipeWire capture frame rate.
    pub desktop_capture_fps: u32,
    /// Desktop PipeWire video node (default, object name, or node id).
    pub desktop_capture_target: DesktopCaptureTarget,
    /// Optional PipeWire remote instance for desktop capture.
    pub desktop_capture_remote: Option<String>,
    /// Explicit X11 display for `--capture-provider x11` (e.g. `:0`).
    ///
    /// Never inherited from ambient `DISPLAY`; an empty value is invalid.
    pub x11_display: Option<String>,
    /// X11 capture region X offset.
    pub x11_x: u32,
    /// X11 capture region Y offset.
    pub x11_y: u32,
    /// X11 capture output width.
    pub x11_width: u32,
    /// X11 capture output height.
    pub x11_height: u32,
    /// X11 capture frame rate.
    pub x11_fps: u32,
    /// True when any X11 flag was supplied explicitly. An X11 flag on a
    /// non-desktop profile is a typed configuration error.
    pub x11_requested: bool,
    /// Portal ScreenCast source type.
    pub portal_source_type: PortalSourceKind,
    /// Portal cursor mode.
    pub portal_cursor: PortalCursorKind,
    /// Restore token from an earlier portal session.
    pub portal_restore_token: Option<String>,
    /// Bound on the portal consent/selection handshake.
    pub portal_timeout: Duration,
    /// True when any portal flag was supplied explicitly.
    pub portal_requested: bool,
    /// PipeWire audio capture target (`default`, `object:...`, `node:N`, or
    /// `monitor:<sink>`). A monitor target captures an existing sink's audio.
    pub audio_capture_target: ozma_audio::AudioTarget,
    /// Audio routing backend. `none` leaves `/audio/*` at typed 501.
    pub audio_router: AudioRouterKind,
    /// Directory containing reference tracks for `POST /audio/play`.
    pub audio_track_root: Option<PathBuf>,
    /// ffmpeg executable used by VNC capture and HLS.
    pub ffmpeg_bin: PathBuf,
    /// VAAPI render node used by encoder detection and the HLS VAAPI argv.
    pub vaapi_device: PathBuf,
    /// CUDA ordinal used by the HLS NVENC argv (`-gpu <n>`).
    ///
    /// `None` leaves NVIDIA device selection to ffmpeg/the driver; it is never
    /// replaced by an invented ordinal.
    pub nvenc_gpu: Option<u32>,
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
    /// Opt in to creating and binding a composite USB gadget (`--usb-gadget`).
    ///
    /// Never enabled by a profile default: a node without the flag advertises
    /// no USB capability and performs no ConfigFS writes.
    pub usb_gadget: bool,
    /// ConfigFS composite-gadget root (`--usb-configfs-root`).
    pub usb_configfs_root: PathBuf,
    /// UDC the gadget binds to (`--usb-udc`). `None` leaves the gadget
    /// unavailable rather than guessing a controller.
    pub usb_udc: Option<String>,
    /// Initial mass-storage backing image (`--usb-media`).
    pub usb_media: Option<PathBuf>,
    /// Expose the mass-storage LUN read-only (`--usb-media-ro`).
    pub usb_media_ro: bool,
    /// Present the image as a read-only CD-ROM (`--usb-media-cdrom`).
    pub usb_media_cdrom: bool,
    /// Auto-start the userspace SCSI export from this image
    /// (`--usb-media-export`). Distinct from the ConfigFS kernel LUN above.
    pub usb_media_export: Option<PathBuf>,
    /// Auto-start export format (`raw`, `fat32`, `iso9660`).
    pub usb_media_export_format: String,
    /// Serve the auto-start raw export read-write through the bounded overlay.
    pub usb_media_export_read_write: bool,
    /// Optional absolute overlay persistence file for the auto-start export.
    pub usb_media_export_persist: Option<PathBuf>,
    /// Commit the auto-start overlay on SCSI flush.
    pub usb_media_export_persist_on_flush: bool,
    /// Commit the auto-start overlay on stop/shutdown.
    pub usb_media_export_persist_on_stop: bool,
    /// Load an existing auto-start overlay before serving.
    pub usb_media_export_reload: bool,
    /// Optional per-transfer deadline for the auto-start export.
    pub usb_media_export_timeout_ms: Option<u64>,
    /// Optional data-phase bound for the auto-start export.
    pub usb_media_export_max_transfer_bytes: Option<usize>,
    /// Maximum bounded restarts after a transport/rebind failure.
    pub usb_media_restart_max_attempts: u32,
    /// First restart delay in milliseconds.
    pub usb_media_restart_backoff_ms: u64,
    /// Restart backoff ceiling in milliseconds.
    pub usb_media_restart_max_backoff_ms: u64,
    /// Reload the persisted overlay when a supervised restart rebuilds.
    pub usb_media_restart_reload_overlay: bool,
    /// Add a UAC2 audio function (`--usb-uac2`).
    pub usb_uac2: bool,
    /// Add a FunctionFS vendor function (`--usb-vendor`).
    pub usb_vendor: bool,
    /// Optional absolute FunctionFS daemon path (`--usb-vendor-daemon`).
    pub usb_vendor_daemon: Option<PathBuf>,
    /// Start the UAC2 isochronous ↔ host-audio bridge (`--usb-uac2-bridge`).
    ///
    /// Requires `--usb-uac2`, a bound UAC2 gadget with its ALSA card present,
    /// and usable host audio devices. Without this flag the UAC2 function is
    /// descriptor-only and `/usb` reports a typed `unsupported` bridge.
    pub usb_uac2_bridge: bool,
    /// Host playback device receiving host→device UAC2 PCM.
    pub usb_uac2_sink_device: String,
    /// Host capture device feeding device→host UAC2 PCM.
    pub usb_uac2_source_device: String,
}

impl NodeConfig {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|value| value.trim() == capability)
    }

    /// True when the dev/test synthetic capture provider was requested.
    ///
    /// This is only ever true for an explicit `--capture-provider fake`; no
    /// profile default selects it.
    pub fn fake_capture_requested(&self) -> bool {
        self.provider_overrides
            .capture
            .as_deref()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("fake"))
    }

    /// The display head the configured capture provider targets.
    ///
    /// Returns `None` when no VM display provider is configured (including the
    /// dev/test `fake` source, which has no console). D-Bus honours
    /// `--capture-console` (falling back to `--dbus-console`); the other
    /// providers expose one default head.
    pub fn capture_display_source(&self) -> Result<Option<CaptureDisplaySource>, String> {
        let Some(capture) = resolve_capture(self)? else {
            return Ok(None);
        };
        let normalized = capture.trim().to_ascii_lowercase().replace('_', "-");
        let backend = match normalized.as_str() {
            "qmp" => VmConsoleBackend::Qmp,
            "dbus" => VmConsoleBackend::Dbus,
            "vnc" => VmConsoleBackend::Vnc,
            "looking-glass" | "lookingglass" => VmConsoleBackend::LookingGlass,
            // `fake` (and any hardware/desktop provider) is not a VM console.
            _ => return Ok(None),
        };
        let (source_id, console_index, width, height) = match backend {
            VmConsoleBackend::Qmp => (QMP_DEFAULT_ID.to_owned(), 0, 0, 0),
            VmConsoleBackend::Dbus => (
                DBUS_DEFAULT_ID.to_owned(),
                self.capture_console.unwrap_or(self.dbus_console),
                0,
                0,
            ),
            VmConsoleBackend::Vnc => (
                VNC_DEFAULT_ID.to_owned(),
                0,
                self.vnc_width,
                self.vnc_height,
            ),
            VmConsoleBackend::LookingGlass => (self.looking_glass_id.clone(), 0, 0, 0),
        };
        if console_index >= MAX_VM_CONSOLES {
            return Err(format!(
                "capture console {console_index} is out of range (maximum {})",
                MAX_VM_CONSOLES - 1
            ));
        }
        Ok(Some(CaptureDisplaySource {
            backend,
            source_id,
            console_index,
            width,
            height,
        }))
    }

    /// Resolve the controller-facing display-output list.
    ///
    /// * repeated `--display-output` specs are parsed into a
    ///   [`VmDisplayTopology`](ozma_display::vm::VmDisplayTopology) and
    ///   validated;
    /// * otherwise a non-empty raw `--display-outputs-json` is left untouched;
    /// * otherwise a VM display provider contributes a single implicit console.
    pub fn resolve_display_outputs(&self) -> Result<DisplayOutputSelection, String> {
        let source = self.capture_display_source()?;

        let mut selection = if !self.display_output_specs.is_empty() {
            if self.profile != NodeProfile::Vm {
                return Err(
                    "--display-output describes VM console heads and requires --profile vm; \
                     use --display-outputs-json for host display outputs"
                        .to_owned(),
                );
            }
            let (backend, source_id, active) = match source.as_ref() {
                Some(source) => (
                    source.backend,
                    Some(source.source_id.as_str()),
                    source.console_index,
                ),
                None => (
                    VmConsoleBackend::Qmp,
                    None,
                    self.capture_console.unwrap_or(0),
                ),
            };
            display_outputs::outputs_from_specs(
                &self.display_output_specs,
                backend,
                source_id,
                Some(active),
            )?
        } else if !self.display_outputs_json.trim().is_empty()
            && self.display_outputs_json.trim() != "[]"
        {
            // Hardware/legacy outputs are already validated as a JSON array and
            // are not VM consoles; do not reinterpret them through the model.
            DisplayOutputSelection::empty()
        } else if let Some(source) = source.as_ref() {
            display_outputs::single_output(
                source.backend,
                source.console_index,
                Some(&source.source_id),
                source.width,
                source.height,
                None,
            )
        } else {
            DisplayOutputSelection::empty()
        };

        if let (Some(requested), Some(source)) = (self.capture_console, source.as_ref()) {
            if source.backend != VmConsoleBackend::Dbus && requested != source.console_index {
                let note = format!(
                    "--capture-console {requested} is not supported by the {} capture \
                     provider; it captures Console_{} only",
                    source.backend, source.console_index
                );
                selection.limitation = Some(match selection.limitation.take() {
                    Some(existing) => format!("{existing}; {note}"),
                    None => note,
                });
            }
        }

        // A per-output `source` is layout/selection metadata, not a promise
        // that this node runs that transport. Never attach the live provider's
        // source id to a console advertised as a different transport.
        if let Some(source) = source.as_ref() {
            if let Some(active) = selection
                .outputs
                .iter_mut()
                .find(|output| output.index == selection.active_console)
            {
                if active.source_type != source.backend.as_str() {
                    active.capture_source_id.clear();
                    let note = format!(
                        "the {} capture provider targets Console_{}, which is advertised as \
                         {}; no capture_source_id is claimed for it",
                        source.backend, selection.active_console, active.source_type
                    );
                    selection.limitation = Some(match selection.limitation.take() {
                        Some(existing) => format!("{existing}; {note}"),
                        None => note,
                    });
                }
            }
        }

        Ok(selection)
    }

    /// Number of displayed outputs, including the raw JSON escape hatch.
    pub fn display_output_count(&self) -> usize {
        if !self.display_outputs.is_empty() {
            return self.display_outputs.len();
        }
        serde_json::from_str::<Vec<serde_json::Value>>(&self.display_outputs_json)
            .map(|outputs| outputs.len())
            .unwrap_or(0)
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
        if let Some(index) = self.capture_console {
            if index >= MAX_VM_CONSOLES {
                return Err(format!(
                    "--capture-console must be between 0 and {}, got {index}",
                    MAX_VM_CONSOLES - 1
                ));
            }
        }
        let raw_display_outputs = self.display_outputs_json.trim();
        // `main` mirrors the normalized `--display-output` topology into
        // `display_outputs_json` for registration. That generated value is not
        // a conflicting raw escape hatch, so validate against the serialized
        // topology instead of rejecting every configured VM console.
        let generated_display_outputs =
            serde_json::to_string(&self.display_outputs).unwrap_or_default();
        if !self.display_output_specs.is_empty()
            && !raw_display_outputs.is_empty()
            && raw_display_outputs != "[]"
            && raw_display_outputs != generated_display_outputs
        {
            return Err(
                "--display-output and a non-empty --display-outputs-json are mutually exclusive; \
                 use --display-output for VM consoles or the JSON escape hatch for host outputs"
                    .to_owned(),
            );
        }
        // This validates the topology (duplicate indices, zero dimensions,
        // multiple primaries) as well as the profile restriction on
        // `--display-output`.
        self.resolve_display_outputs()?;
        if self.looking_glass_fps == 0 || self.looking_glass_fps > MAX_FAKE_FPS {
            return Err(format!(
                "--looking-glass-fps must be between 1 and {MAX_FAKE_FPS}, got {}",
                self.looking_glass_fps
            ));
        }
        validate_path("--looking-glass-shm", self.looking_glass_shm.as_deref())?;
        if self.looking_glass_id.trim().is_empty() {
            return Err("--looking-glass-id must not be empty or whitespace".to_owned());
        }
        if self.looking_glass_name.trim().is_empty() {
            return Err("--looking-glass-name must not be empty or whitespace".to_owned());
        }
        if self.looking_glass_shm.is_some() && self.profile != NodeProfile::Vm {
            return Err(
                "--looking-glass-shm captures a VM's KVMFR region and requires --profile vm"
                    .to_owned(),
            );
        }
        if let Some(gpu) = self.nvenc_gpu {
            if gpu > i32::MAX as u32 {
                return Err(format!(
                    "--nvenc-gpu must be a CUDA ordinal between 0 and {}, got {gpu}",
                    i32::MAX
                ));
            }
        }
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
        let vnc_server_configured = self.vnc_bind.is_some()
            || self.vnc_server_port != 0
            || self.vnc_password_file.is_some()
            || self.vnc_allow_unauthenticated
            || self.vnc_max_clients != crate::vnc_server::DEFAULT_MAX_CLIENTS
            || self.vnc_idle_timeout_seconds != 0;
        if !self.vnc_server && vnc_server_configured {
            return Err("--vnc-bind, --vnc-server-port, --vnc-password-file, \
                 --vnc-allow-unauthenticated, --vnc-max-clients, and \
                 --vnc-idle-timeout-seconds configure the internal RFB server and require \
                 --vnc-server"
                .to_owned());
        }
        if self.vnc_server {
            let raw = self
                .vnc_bind
                .as_deref()
                .unwrap_or(crate::vnc_server::DEFAULT_VNC_BIND)
                .trim();
            let bind = raw.parse::<std::net::IpAddr>().map_err(|error| {
                format!("--vnc-bind `{raw}` must be an IP address for the RFB listener: {error}")
            })?;
            if !bind.is_loopback()
                && self.vnc_password_file.is_none()
                && !self.vnc_allow_unauthenticated
            {
                return Err(format!(
                    "refusing unauthenticated non-loopback VNC bind `{bind}`: configure \
                     --vnc-password-file, or set --vnc-allow-unauthenticated to accept the risk"
                ));
            }
            if self.vnc_max_clients == 0
                || self.vnc_max_clients > crate::vnc_server::MAX_CLIENTS_LIMIT
            {
                return Err(format!(
                    "--vnc-max-clients must be between 1 and {}, got {}",
                    crate::vnc_server::MAX_CLIENTS_LIMIT,
                    self.vnc_max_clients
                ));
            }
            if self.vnc_idle_timeout_seconds > crate::vnc_server::MAX_IDLE_TIMEOUT_SECONDS {
                return Err(format!(
                    "--vnc-idle-timeout-seconds must be between 0 and {}, got {}",
                    crate::vnc_server::MAX_IDLE_TIMEOUT_SECONDS,
                    self.vnc_idle_timeout_seconds
                ));
            }
            if let Some(path) = &self.vnc_password_file {
                if path.as_os_str().is_empty() {
                    return Err("--vnc-password-file must not be empty".to_owned());
                }
            }
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
        if self.fake_width == 0 || self.fake_height == 0 {
            return Err("--fake-width/--fake-height must be greater than zero".to_owned());
        }
        if self.fake_width > MAX_FAKE_DIMENSION || self.fake_height > MAX_FAKE_DIMENSION {
            return Err(format!(
                "--fake-width/--fake-height exceed the {MAX_FAKE_DIMENSION}-pixel dev/test bound"
            ));
        }
        if self.fake_fps == 0 || self.fake_fps > MAX_FAKE_FPS {
            return Err(format!(
                "--fake-fps must be between 1 and {MAX_FAKE_FPS}, got {}",
                self.fake_fps
            ));
        }
        if self.desktop_capture_width == 0 || self.desktop_capture_height == 0 {
            return Err(
                "--desktop-capture-width/--desktop-capture-height must be greater than zero"
                    .to_owned(),
            );
        }
        if self.desktop_capture_width > MAX_FAKE_DIMENSION
            || self.desktop_capture_height > MAX_FAKE_DIMENSION
        {
            return Err(format!(
                "--desktop-capture-width/--desktop-capture-height exceed the \
                 {MAX_FAKE_DIMENSION}-pixel bound"
            ));
        }
        if self.desktop_capture_fps == 0 || self.desktop_capture_fps > MAX_FAKE_FPS {
            return Err(format!(
                "--desktop-capture-fps must be between 1 and {MAX_FAKE_FPS}, got {}",
                self.desktop_capture_fps
            ));
        }
        if let Some(remote) = &self.desktop_capture_remote {
            if remote.trim().is_empty() {
                return Err("--desktop-capture-remote must not be empty or whitespace".to_owned());
            }
        }
        if self.x11_requested {
            if self.profile != NodeProfile::Desktop {
                return Err("the X11 capture flags require --profile desktop".to_owned());
            }
            self.x11_capture_config()?;
        }
        if self.portal_requested && self.profile != NodeProfile::Desktop {
            return Err("the portal capture flags require --profile desktop".to_owned());
        }
        if self.portal_timeout.is_zero() {
            return Err("--portal-timeout-ms must be greater than zero".to_owned());
        }
        if let Some(token) = &self.portal_restore_token {
            if token.is_empty() || token.chars().any(char::is_control) {
                return Err(
                    "--portal-restore-token must be non-empty and contain no control characters"
                        .to_owned(),
                );
            }
        }
        self.audio_capture_target
            .validate()
            .map_err(|error| format!("--audio-capture-target: {error}"))?;
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
        if self.usb_configfs_root.as_os_str().is_empty()
            || self.usb_configfs_root.to_string_lossy().trim().is_empty()
        {
            return Err("--usb-configfs-root must not be empty".to_owned());
        }
        if let Some(udc) = &self.usb_udc {
            validate_udc_name(udc)?;
            if !self.usb_gadget {
                return Err("--usb-udc requires --usb-gadget".to_owned());
            }
        }
        validate_path("--usb-media", self.usb_media.as_deref())?;
        if let Some(path) = self.usb_media.as_deref() {
            if !path.is_absolute() {
                return Err(format!(
                    "--usb-media {} must be an absolute path; ConfigFS stores the path verbatim",
                    path.display()
                ));
            }
        }
        if self.usb_media.is_some() && !self.usb_gadget {
            return Err("--usb-media requires --usb-gadget".to_owned());
        }
        if self.usb_media_ro && self.usb_media.is_none() {
            return Err("--usb-media-ro requires --usb-media".to_owned());
        }
        if self.usb_media_cdrom && self.usb_media.is_none() {
            return Err("--usb-media-cdrom requires --usb-media".to_owned());
        }
        validate_path("--usb-media-export", self.usb_media_export.as_deref())?;
        if let Some(path) = self.usb_media_export.as_deref() {
            if !path.is_absolute() {
                return Err(format!(
                    "--usb-media-export {} must be an absolute path",
                    path.display()
                ));
            }
        }
        let export_format = self.usb_media_export_format.trim().to_ascii_lowercase();
        if !matches!(
            export_format.as_str(),
            "raw" | "nbd" | "fat32" | "vfat" | "iso" | "iso9660"
        ) {
            return Err(format!(
                "--usb-media-export-format must be raw, fat32, or iso9660, got {:?}",
                self.usb_media_export_format
            ));
        }
        let format_is_read_only =
            matches!(export_format.as_str(), "fat32" | "vfat" | "iso" | "iso9660");
        if self.usb_media_export_read_write && format_is_read_only {
            return Err(
                "--usb-media-export-read-write is invalid for FAT32/ISO 9660 media".to_owned(),
            );
        }
        if self.usb_media_export.is_some() && !self.usb_gadget {
            return Err("--usb-media-export requires --usb-gadget".to_owned());
        }
        if self.usb_media_export.is_none() {
            if self.usb_media_export_persist.is_some() {
                return Err("--usb-media-export-persist requires --usb-media-export".to_owned());
            }
            if self.usb_media_export_reload {
                return Err("--usb-media-export-reload requires --usb-media-export".to_owned());
            }
            if self.usb_media_export_timeout_ms.is_some()
                || self.usb_media_export_max_transfer_bytes.is_some()
            {
                return Err(
                    "--usb-media-export timeout/transfer bounds require --usb-media-export"
                        .to_owned(),
                );
            }
        }
        if let Some(persist) = self.usb_media_export_persist.as_deref() {
            if !persist.is_absolute() {
                return Err(format!(
                    "--usb-media-export-persist {} must be an absolute path",
                    persist.display()
                ));
            }
            if !self.usb_media_export_read_write {
                return Err(
                    "--usb-media-export-persist requires --usb-media-export-read-write".to_owned(),
                );
            }
        }
        if self.usb_media_export_reload
            && (!self.usb_media_export_read_write || self.usb_media_export_persist.is_none())
        {
            return Err(
                "--usb-media-export-reload requires --usb-media-export-read-write and \
                 --usb-media-export-persist"
                    .to_owned(),
            );
        }
        let restart = MediaRestartPolicy {
            max_attempts: self.usb_media_restart_max_attempts,
            backoff_ms: self.usb_media_restart_backoff_ms,
            max_backoff_ms: self.usb_media_restart_max_backoff_ms,
            reload_overlay: self.usb_media_restart_reload_overlay,
        };
        restart
            .validate()
            .map_err(|error| format!("invalid --usb-media-restart policy: {error}"))?;
        if (self.usb_uac2 || self.usb_vendor) && !self.usb_gadget {
            return Err("--usb-uac2/--usb-vendor require --usb-gadget".to_owned());
        }
        if self.usb_uac2_bridge && !self.usb_uac2 {
            return Err("--usb-uac2-bridge requires --usb-uac2".to_owned());
        }
        if self.usb_uac2_bridge && self.usb_uac2_sink_device.trim().is_empty() {
            return Err("--usb-uac2-sink-device must not be empty".to_owned());
        }
        if self.usb_uac2_bridge && self.usb_uac2_source_device.trim().is_empty() {
            return Err("--usb-uac2-source-device must not be empty".to_owned());
        }
        if self.usb_vendor_daemon.is_some() && !self.usb_vendor {
            return Err("--usb-vendor-daemon requires --usb-vendor".to_owned());
        }
        if let Some(path) = self.usb_vendor_daemon.as_deref() {
            if !path.is_absolute() {
                return Err(format!(
                    "--usb-vendor-daemon {} must be an absolute path",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// Resolve the profile's capture/input/power providers from flags.
    pub fn vm_providers(&self) -> Result<VmProviderConfig, String> {
        VmProviderConfig::resolve(self)
    }

    /// Resolve the desktop profile's capture/input/audio selection.
    pub fn desktop_providers(&self) -> Result<DesktopProviderConfig, String> {
        DesktopProviderConfig::resolve(self)
    }

    /// The active capture provider name, or `None` when no source is configured.
    ///
    /// This is the pure provider resolver used by logs and
    /// `/api/v1/connection`; it never probes or opens a source.
    pub fn capture_provider_name(&self) -> Result<Option<String>, String> {
        resolve_capture(self)
    }

    /// Build the X11 provider configuration from the desktop flags.
    ///
    /// A missing display is a typed configuration error; it is never replaced
    /// by ambient `DISPLAY` or a scan of the local X servers.
    pub fn x11_capture_config(&self) -> Result<ozma_capture::X11CaptureConfig, String> {
        let display = self
            .x11_display
            .as_deref()
            .map(str::trim)
            .filter(|display| !display.is_empty())
            .ok_or_else(|| {
                "--capture-provider x11 requires --x11-display (:0, :0.0, or host:10.0)".to_owned()
            })?;
        let config = ozma_capture::X11CaptureConfig::new(display)
            .with_offset(self.x11_x, self.x11_y)
            .with_dimensions(self.x11_width, self.x11_height)
            .with_fps(self.x11_fps)
            .with_ffmpeg_bin(self.ffmpeg_bin.clone());
        config
            .validate()
            .map_err(|error| format!("--x11-display/geometry: {}", error.message))?;
        Ok(config)
    }

    /// Build the portal handshake settings from the desktop flags.
    pub fn portal_capture_options(&self) -> PortalCaptureOptions {
        PortalCaptureOptions {
            source: self.portal_source_type,
            cursor: self.portal_cursor,
            restore_token: self.portal_restore_token.clone(),
            timeout: self.portal_timeout,
            width: self.desktop_capture_width,
            height: self.desktop_capture_height,
            fps: self.desktop_capture_fps,
        }
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

/// Reject a UDC name ConfigFS cannot bind.
///
/// The name is written verbatim to the gadget's `UDC` attribute, so it must not
/// contain path separators, whitespace, or control characters.
fn validate_udc_name(udc: &str) -> Result<(), String> {
    let valid = !udc.is_empty()
        && udc.len() <= 63
        && !udc.starts_with('.')
        && udc.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "--usb-udc {udc:?} is not a valid UDC name (letters, digits, '-', '_', '.', ':')"
        ))
    }
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
            vnc_server: false,
            vnc_bind: None,
            vnc_server_port: 0,
            vnc_password_file: None,
            vnc_allow_unauthenticated: false,
            vnc_max_clients: crate::vnc_server::DEFAULT_MAX_CLIENTS,
            vnc_idle_timeout_seconds: 0,
            dbus_console: ozma_capture::DBUS_DEFAULT_CONSOLE_INDEX,
            dbus_width: ozma_capture::DBUS_DEFAULT_WIDTH,
            dbus_height: ozma_capture::DBUS_DEFAULT_HEIGHT,
            dbus_fps: ozma_capture::DBUS_DEFAULT_FPS,
            dbus_requested: false,
            capture_console: None,
            display_output_specs: Vec::new(),
            display_outputs: Vec::new(),
            looking_glass_shm: None,
            looking_glass_fps: ozma_capture::LOOKING_GLASS_DEFAULT_FPS,
            looking_glass_id: ozma_capture::LOOKING_GLASS_DEFAULT_ID.to_owned(),
            looking_glass_name: ozma_capture::LOOKING_GLASS_DEFAULT_NAME.to_owned(),
            fake_width: DEFAULT_FAKE_WIDTH,
            fake_height: DEFAULT_FAKE_HEIGHT,
            fake_fps: DEFAULT_FAKE_FPS,
            desktop_capture_width: DEFAULT_FAKE_WIDTH,
            desktop_capture_height: DEFAULT_FAKE_HEIGHT,
            desktop_capture_fps: DEFAULT_FAKE_FPS,
            desktop_capture_target: DesktopCaptureTarget::Default,
            desktop_capture_remote: None,
            x11_display: None,
            x11_x: 0,
            x11_y: 0,
            x11_width: ozma_capture::X11_DEFAULT_WIDTH,
            x11_height: ozma_capture::X11_DEFAULT_HEIGHT,
            x11_fps: ozma_capture::X11_DEFAULT_FPS,
            x11_requested: false,
            portal_source_type: PortalSourceKind::Monitor,
            portal_cursor: PortalCursorKind::Hidden,
            portal_restore_token: None,
            portal_timeout: ozma_capture::PORTAL_DEFAULT_RESPONSE_TIMEOUT,
            portal_requested: false,
            audio_capture_target: ozma_audio::AudioTarget::Default,
            audio_router: AudioRouterKind::None,
            audio_track_root: None,
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
            nvenc_gpu: None,
            audio_sink: None,
            webrtc_audio_source: None,
            webrtc_audio_backend: WebRtcAudioBackend::Auto,
            vm_guest_ip: None,
            display_outputs_json: "[]".into(),
            pci_devices_json: "[]".into(),
            usb_gadget: false,
            usb_configfs_root: PathBuf::from(DEFAULT_USB_CONFIGFS_ROOT),
            usb_udc: None,
            usb_media: None,
            usb_media_ro: false,
            usb_media_cdrom: false,
            usb_media_export: None,
            usb_media_export_format: "raw".to_owned(),
            usb_media_export_read_write: false,
            usb_media_export_persist: None,
            usb_media_export_persist_on_flush: true,
            usb_media_export_persist_on_stop: true,
            usb_media_export_reload: false,
            usb_media_export_timeout_ms: None,
            usb_media_export_max_transfer_bytes: None,
            usb_media_restart_max_attempts: ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS,
            usb_media_restart_backoff_ms: ozma_usb::USB_MEDIA_RESTART_DEFAULT_BACKOFF_MS,
            usb_media_restart_max_backoff_ms: ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_BACKOFF_MS,
            usb_media_restart_reload_overlay: true,
            usb_uac2: false,
            usb_vendor: false,
            usb_vendor_daemon: None,
            usb_uac2_bridge: false,
            usb_uac2_sink_device: "default".to_owned(),
            usb_uac2_source_device: "default".to_owned(),
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
    fn fake_capture_is_dev_only_and_keeps_requested_geometry() {
        // Explicit synthetic capture is accepted for the VM profile...
        let config = vm_config_with_overrides("fake", "", "");
        assert_eq!(
            config.vm_providers().unwrap().capture.as_deref(),
            Some("fake")
        );
        assert!(config.fake_capture_requested());

        // ...and for the dev desktop profile.
        let mut desktop = test_config(NodeProfile::Desktop);
        desktop.provider_overrides = ProviderOverrides::from_cli("fake", "", "", "", "");
        desktop.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&desktop.provider_overrides);
        assert_eq!(
            desktop.vm_providers().unwrap().capture.as_deref(),
            Some("fake")
        );

        // It must never be selectable on the hardware profile.
        let mut hardware = test_config(NodeProfile::Hardware);
        hardware.provider_overrides = ProviderOverrides::from_cli("fake", "", "", "", "");
        hardware.providers = ProviderSelection::for_profile(NodeProfile::Hardware)
            .with_overrides(&hardware.provider_overrides);
        let error = hardware.vm_providers().unwrap_err();
        assert!(error.contains("dev/test"), "{error}");
        assert!(error.contains("hardware profile"), "{error}");

        // 4K geometry and 30 fps are valid; zero/oversized values are not.
        let mut config = vm_config_with_overrides("fake", "", "");
        config.fake_width = 3_840;
        config.fake_height = 2_160;
        config.fake_fps = 30;
        assert!(config.validate().is_ok());

        for (width, height, fps) in [(0, 2_160, 30), (3_840, 0, 30), (3_840, 2_160, 0)] {
            let mut config = vm_config_with_overrides("fake", "", "");
            config.fake_width = width;
            config.fake_height = height;
            config.fake_fps = fps;
            assert!(config.validate().is_err(), "{width}x{height}@{fps}");
        }

        let mut config = vm_config_with_overrides("fake", "", "");
        config.fake_width = MAX_FAKE_DIMENSION + 1;
        assert!(config.validate().is_err());
        config.fake_width = MAX_FAKE_DIMENSION;
        config.fake_fps = MAX_FAKE_FPS + 1;
        assert!(config.validate().is_err());
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
        assert!(
            error.contains("expected qmp, dbus, vnc, looking-glass, or fake"),
            "{error}"
        );
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
    fn vnc_server_flags_require_enable_and_validate_the_bind_address() {
        let mut orphan = test_config(NodeProfile::Hardware);
        orphan.vnc_bind = Some("127.0.0.1".into());
        let error = orphan.validate().unwrap_err();
        assert!(error.contains("--vnc-server"), "{error}");

        let mut orphan = test_config(NodeProfile::Hardware);
        orphan.vnc_server_port = 5900;
        let error = orphan.validate().unwrap_err();
        assert!(error.contains("--vnc-server"), "{error}");

        let mut orphan = test_config(NodeProfile::Hardware);
        orphan.vnc_password_file = Some(PathBuf::from("/run/ozma/vnc.pass"));
        let error = orphan.validate().unwrap_err();
        assert!(error.contains("--vnc-server"), "{error}");

        let mut enabled = test_config(NodeProfile::Hardware);
        enabled.vnc_server = true;
        enabled.validate().unwrap();

        // A non-loopback bind without a password or the explicit override is
        // refused at configuration time, before any socket is bound.
        let mut widened = test_config(NodeProfile::Hardware);
        widened.vnc_server = true;
        widened.vnc_bind = Some("0.0.0.0".into());
        let error = widened.validate().unwrap_err();
        assert!(error.contains("--vnc-password-file"), "{error}");

        let mut overridden = widened.clone();
        overridden.vnc_allow_unauthenticated = true;
        overridden.validate().unwrap();

        let mut authenticated = widened;
        authenticated.vnc_password_file = Some(PathBuf::from("/run/ozma/vnc.pass"));
        authenticated.validate().unwrap();

        let mut invalid = test_config(NodeProfile::Hardware);
        invalid.vnc_server = true;
        invalid.vnc_bind = Some("not-an-ip".into());
        let error = invalid.validate().unwrap_err();
        assert!(error.contains("--vnc-bind"), "{error}");

        let mut too_many = test_config(NodeProfile::Hardware);
        too_many.vnc_server = true;
        too_many.vnc_max_clients = crate::vnc_server::MAX_CLIENTS_LIMIT + 1;
        let error = too_many.validate().unwrap_err();
        assert!(error.contains("--vnc-max-clients"), "{error}");

        let mut idle = test_config(NodeProfile::Hardware);
        idle.vnc_server = true;
        idle.vnc_idle_timeout_seconds = crate::vnc_server::MAX_IDLE_TIMEOUT_SECONDS + 1;
        let error = idle.validate().unwrap_err();
        assert!(error.contains("--vnc-idle-timeout-seconds"), "{error}");
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
    fn looking_glass_flags_select_the_provider_and_validate() {
        let mut config = test_config(NodeProfile::Vm);
        config.looking_glass_shm = Some(PathBuf::from("/dev/shm/looking-glass"));

        let providers = config.vm_providers().unwrap();
        assert_eq!(providers.capture.as_deref(), Some("looking-glass"));

        let source = config.capture_display_source().unwrap().unwrap();
        assert_eq!(source.backend, VmConsoleBackend::LookingGlass);
        assert_eq!(source.source_id, ozma_capture::LOOKING_GLASS_DEFAULT_ID);

        let selection = config.resolve_display_outputs().unwrap();
        assert_eq!(selection.outputs.len(), 1);
        assert_eq!(selection.outputs[0].source_type, "looking_glass");
        assert_eq!(
            selection.outputs[0].capture_source_id,
            ozma_capture::LOOKING_GLASS_DEFAULT_ID
        );
        assert!(selection.outputs[0].primary);

        let mut invalid_fps = config.clone();
        invalid_fps.looking_glass_fps = 0;
        assert!(invalid_fps.validate().is_err());

        let mut explicit_without_shm = test_config(NodeProfile::Vm);
        explicit_without_shm.provider_overrides =
            ProviderOverrides::from_cli("looking-glass", "", "", "", "");
        assert!(explicit_without_shm
            .vm_providers()
            .unwrap_err()
            .contains("--looking-glass-shm"));
    }

    #[test]
    fn looking_glass_conflicts_with_other_vm_capture_sources() {
        let mut vnc = test_config(NodeProfile::Vm);
        vnc.vnc_host = Some("127.0.0.1".into());
        vnc.vnc_port = Some(5901);
        vnc.looking_glass_shm = Some(PathBuf::from("/dev/shm/looking-glass"));
        assert!(vnc.vm_providers().is_err());

        let mut dbus = test_config(NodeProfile::Vm);
        dbus.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        dbus.dbus_requested = true;
        dbus.looking_glass_shm = Some(PathBuf::from("/dev/shm/looking-glass"));
        assert!(dbus.vm_providers().is_err());

        let mut hardware = test_config(NodeProfile::Hardware);
        hardware.looking_glass_shm = Some(PathBuf::from("/dev/shm/looking-glass"));
        assert!(hardware.validate().is_err());

        let mut explicit_qmp = test_config(NodeProfile::Vm);
        explicit_qmp.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        explicit_qmp.provider_overrides = ProviderOverrides::from_cli("qmp", "", "", "", "");
        explicit_qmp.looking_glass_shm = Some(PathBuf::from("/dev/shm/looking-glass"));
        assert!(explicit_qmp.vm_providers().is_err());
    }

    #[test]
    fn display_output_specs_validate_and_normalize_through_the_topology() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        config.display_output_specs = vec![
            "0:Console_0:1920x1080:primary:qmp".into(),
            "1:Console_1:1280x1024:qmp".into(),
        ];

        let selection = config.resolve_display_outputs().unwrap();
        assert_eq!(selection.outputs.len(), 2);
        assert!(selection.outputs[0].primary);
        assert_eq!(
            selection.outputs[0].capture_source_id,
            ozma_capture::QMP_DEFAULT_ID
        );
        assert_eq!(selection.outputs[1].capture_source_id, "");
        assert!(selection.limitation.is_some());

        let json: serde_json::Value = serde_json::from_str(&selection.to_json()).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[0]["source_type"], "qmp");
        assert_eq!(json[1]["capture_source_id"], "");

        // `main` mirrors the resolved topology into `display_outputs_json`
        // for registration. That generated value must not be mistaken for a
        // conflicting raw --display-outputs-json escape hatch, otherwise a VM
        // node with --display-output would build an empty capture registry.
        let mut mirrored = config.clone();
        mirrored.display_outputs = selection.outputs.clone();
        mirrored.display_outputs_json = selection.to_json();
        mirrored
            .validate()
            .expect("generated display JSON is valid");
        assert_eq!(
            mirrored.vm_providers().unwrap().capture.as_deref(),
            Some("qmp")
        );

        let mut two_primaries = config.clone();
        two_primaries.display_output_specs = vec![
            "0:Console_0:1920x1080:primary".into(),
            "1:Console_1:1280x1024:primary".into(),
        ];
        assert!(two_primaries.validate().is_err());

        let mut non_vm = test_config(NodeProfile::Hardware);
        non_vm.display_output_specs = vec!["HDMI-1:1920x1080".into()];
        assert!(non_vm.validate().is_err());

        let mut conflicting_raw = config.clone();
        conflicting_raw.display_outputs_json =
            r#"[{"name":"HDMI-1","width":1920,"height":1080}]"#.into();
        assert!(conflicting_raw.validate().is_err());
    }

    #[test]
    fn a_mismatched_per_output_source_does_not_claim_the_live_capture_id() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        config.display_output_specs = vec!["0:Console_0:1920x1080:vnc".into()];
        let selection = config.resolve_display_outputs().unwrap();
        assert_eq!(selection.outputs[0].source_type, "vnc");
        assert_eq!(selection.outputs[0].capture_source_id, "");
        let limitation = selection.limitation.unwrap();
        assert!(limitation.contains("advertised as vnc"), "{limitation}");
    }

    #[test]
    fn display_output_specs_without_an_explicit_primary_fall_back_to_console_zero() {
        let mut config = test_config(NodeProfile::Vm);
        config.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        config.display_output_specs = vec![
            "0:Console_0:1920x1080".into(),
            "1:Console_1:1280x1024".into(),
        ];
        let selection = config.resolve_display_outputs().unwrap();
        assert_eq!(selection.primary_console(), Some(0));
        assert_eq!(selection.outputs[1].capture_source_id, "");
    }

    #[test]
    fn capture_console_selects_the_dbus_head_and_reports_single_head_limits() {
        let mut dbus = test_config(NodeProfile::Vm);
        dbus.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        dbus.dbus_requested = true;
        dbus.capture_console = Some(2);
        let source = dbus.capture_display_source().unwrap().unwrap();
        assert_eq!(source.backend, VmConsoleBackend::Dbus);
        assert_eq!(source.console_index, 2);
        let selection = dbus.resolve_display_outputs().unwrap();
        assert_eq!(selection.outputs[0].index, 2);
        assert_eq!(
            selection.outputs[0].capture_source_id,
            ozma_capture::DBUS_DEFAULT_ID
        );
        assert!(selection.limitation.is_none());

        let mut qmp = test_config(NodeProfile::Vm);
        qmp.qmp_socket = Some(PathBuf::from("/run/ozma/qmp.sock"));
        qmp.capture_console = Some(2);
        let source = qmp.capture_display_source().unwrap().unwrap();
        assert_eq!(source.backend, VmConsoleBackend::Qmp);
        assert_eq!(source.console_index, 0);
        let limitation = qmp.resolve_display_outputs().unwrap().limitation.unwrap();
        assert!(limitation.contains("--capture-console 2"), "{limitation}");
        assert!(limitation.contains("Console_0"), "{limitation}");
    }

    #[test]
    fn nvenc_gpu_is_optional_and_bounded() {
        let mut config = test_config(NodeProfile::Vm);
        assert_eq!(config.nvenc_gpu, None, "no default GPU is invented");
        assert!(config.validate().is_ok());

        config.nvenc_gpu = Some(1);
        assert!(config.validate().is_ok());

        config.nvenc_gpu = Some(i32::MAX as u32 + 1);
        let error = config.validate().expect_err("out-of-range ordinal");
        assert!(error.contains("--nvenc-gpu"), "{error}");
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

    #[test]
    fn desktop_profile_resolves_pipewire_capture_input_and_audio() {
        let config = test_config(NodeProfile::Desktop);
        let resolved = config.desktop_providers().unwrap();

        assert_eq!(
            resolved.capture,
            DesktopCaptureSelection::PipeWire(DesktopCaptureTarget::Default)
        );
        assert_eq!(
            resolved.input_chain,
            vec![SinkKind::Uinput, SinkKind::HidGadget, SinkKind::Recording]
        );
        assert_eq!(resolved.audio_capture, ozma_audio::AudioTarget::Default);
        assert_eq!(resolved.capture_width, DEFAULT_FAKE_WIDTH);
        assert_eq!(resolved.capture_fps, DEFAULT_FAKE_FPS);

        // QMP is only added when an input socket was configured.
        let mut with_qmp = test_config(NodeProfile::Desktop);
        with_qmp.qmp_socket = Some(PathBuf::from("/run/ozma/ctrl.qmp"));
        assert_eq!(
            with_qmp.desktop_providers().unwrap().input_chain,
            vec![
                SinkKind::Uinput,
                SinkKind::Qmp,
                SinkKind::HidGadget,
                SinkKind::Recording
            ]
        );

        // The explicit dev/test source is not a PipeWire target.
        let mut fake = test_config(NodeProfile::Desktop);
        fake.provider_overrides = ProviderOverrides::from_cli("fake", "", "", "", "");
        fake.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&fake.provider_overrides);
        assert_eq!(
            fake.desktop_providers().unwrap().capture,
            DesktopCaptureSelection::Fake
        );
    }

    #[test]
    fn desktop_capture_target_parses_node_and_object_forms() {
        for (raw, expected) in [
            ("default", DesktopCaptureTarget::Default),
            ("pipewire:default", DesktopCaptureTarget::Default),
            (
                "object:screen-cast",
                DesktopCaptureTarget::Object("screen-cast".into()),
            ),
            (
                "name:screen-cast",
                DesktopCaptureTarget::Object("screen-cast".into()),
            ),
            (
                "pipewire:name:screen-cast",
                DesktopCaptureTarget::Object("screen-cast".into()),
            ),
            ("node:42", DesktopCaptureTarget::NodeId(42)),
            ("pipewire:node:42", DesktopCaptureTarget::NodeId(42)),
            (
                "bare-name",
                DesktopCaptureTarget::Object("bare-name".into()),
            ),
        ] {
            assert_eq!(
                raw.parse::<DesktopCaptureTarget>().unwrap(),
                expected,
                "{raw}"
            );
        }

        for raw in [
            "",
            "  ",
            "object:",
            "name:",
            "node:",
            "node:not-a-number",
            "pipewire:node:",
            "screen\u{7}",
        ] {
            assert!(
                raw.parse::<DesktopCaptureTarget>().is_err(),
                "{raw:?} should be rejected"
            );
        }

        assert_eq!(DesktopCaptureTarget::NodeId(7).to_string(), "node:7");
    }

    #[test]
    fn desktop_rejects_unknown_capture_and_requires_x11_display() {
        let mut config = test_config(NodeProfile::Desktop);
        config.provider_overrides = ProviderOverrides::from_cli("v4l2", "", "", "", "");
        config.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&config.provider_overrides);
        let error = config.desktop_providers().unwrap_err();
        assert!(error.contains("expected pipewire, x11, portal"), "{error}");

        // X11 is selected but has no explicit display: a typed configuration
        // error, never ambient DISPLAY.
        let mut x11 = test_config(NodeProfile::Desktop);
        x11.provider_overrides = ProviderOverrides::from_cli("x11", "", "", "", "");
        x11.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&x11.provider_overrides);
        let error = x11.desktop_providers().unwrap_err();
        assert!(error.contains("--x11-display"), "{error}");

        // X11/portal flags belong to the desktop profile only.
        let mut vm = test_config(NodeProfile::Vm);
        vm.x11_display = Some(":0".into());
        vm.x11_requested = true;
        let error = vm.vm_providers().unwrap_err();
        assert!(error.contains("--profile desktop"), "{error}");

        let mut hardware = test_config(NodeProfile::Hardware);
        hardware.portal_source_type = PortalSourceKind::Window;
        hardware.portal_requested = true;
        assert!(hardware.validate().is_err());

        let mut vm_portal = test_config(NodeProfile::Vm);
        vm_portal.provider_overrides = ProviderOverrides::from_cli("portal", "", "", "", "");
        vm_portal.providers = ProviderSelection::for_profile(NodeProfile::Vm)
            .with_overrides(&vm_portal.provider_overrides);
        assert!(vm_portal.desktop_providers().is_err());

        let mut pipewire = test_config(NodeProfile::Desktop);
        pipewire.provider_overrides = ProviderOverrides::from_cli("pipewire", "", "", "", "");
        pipewire.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&pipewire.provider_overrides);
        assert_eq!(
            pipewire.desktop_providers().unwrap().capture,
            DesktopCaptureSelection::PipeWire(DesktopCaptureTarget::Default)
        );
    }

    #[test]
    fn desktop_x11_capture_resolves_flags_to_provider_config() {
        let mut config = test_config(NodeProfile::Desktop);
        config.provider_overrides = ProviderOverrides::from_cli("x11", "", "", "", "");
        config.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&config.provider_overrides);
        config.x11_display = Some(":0.0".into());
        config.x11_x = 10;
        config.x11_y = 20;
        config.x11_width = 640;
        config.x11_height = 480;
        config.x11_fps = 15;
        config.x11_requested = true;
        config.ffmpeg_bin = PathBuf::from("/opt/ffmpeg");

        let resolved = config.desktop_providers().unwrap();
        let DesktopCaptureSelection::X11(x11) = resolved.capture else {
            panic!("expected an X11 capture selection");
        };
        assert_eq!(x11.display, ":0.0");
        assert_eq!((x11.x, x11.y), (10, 20));
        assert_eq!((x11.width, x11.height, x11.fps), (640, 480, 15));
        assert_eq!(x11.ffmpeg_bin, PathBuf::from("/opt/ffmpeg"));
        assert_eq!(
            ozma_capture::x11grab_argv(&x11),
            vec![
                "/opt/ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "x11grab",
                "-i",
                ":0.0+10,20",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-s",
                "640x480",
                "-r",
                "15",
                "-",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );

        // An X11 flag alone selects the X11 provider on the desktop profile.
        let mut implicit = test_config(NodeProfile::Desktop);
        implicit.x11_display = Some(":1".into());
        implicit.x11_requested = true;
        assert_eq!(
            implicit
                .desktop_providers()
                .unwrap()
                .capture
                .provider_name(),
            "x11"
        );

        // Invalid geometry/rate is rejected before any capture is attempted.
        let mut invalid = config.clone();
        invalid.x11_width = 0;
        assert!(invalid.validate().is_err());
        let mut invalid = config.clone();
        invalid.x11_fps = ozma_capture::MAX_X11_FPS + 1;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn desktop_portal_capture_validates_options() {
        let mut config = test_config(NodeProfile::Desktop);
        config.provider_overrides = ProviderOverrides::from_cli("portal", "", "", "", "");
        config.providers = ProviderSelection::for_profile(NodeProfile::Desktop)
            .with_overrides(&config.provider_overrides);
        config.portal_source_type = PortalSourceKind::Window;
        config.portal_cursor = PortalCursorKind::Embedded;
        config.portal_restore_token = Some("token-1".into());
        config.portal_timeout = Duration::from_millis(9_000);
        config.portal_requested = true;
        config.desktop_capture_width = 1280;
        config.desktop_capture_height = 720;
        config.desktop_capture_fps = 24;

        let resolved = config.desktop_providers().unwrap();
        let DesktopCaptureSelection::Portal(options) = resolved.capture else {
            panic!("expected a portal capture selection");
        };
        assert_eq!(options.source, PortalSourceKind::Window);
        assert_eq!(options.cursor, PortalCursorKind::Embedded);
        assert_eq!(options.restore_token.as_deref(), Some("token-1"));
        assert_eq!(options.timeout, Duration::from_millis(9_000));
        assert_eq!(
            (options.width, options.height, options.fps),
            (1280, 720, 24)
        );

        // A portal flag alone also selects portal capture on desktop.
        let mut implicit = test_config(NodeProfile::Desktop);
        implicit.portal_cursor = PortalCursorKind::Metadata;
        implicit.portal_requested = true;
        assert_eq!(
            implicit
                .desktop_providers()
                .unwrap()
                .capture
                .provider_name(),
            "portal"
        );

        let mut zero_timeout = config.clone();
        zero_timeout.portal_timeout = Duration::ZERO;
        assert!(zero_timeout.validate().is_err());
        let mut empty_token = config.clone();
        empty_token.portal_restore_token = Some(String::new());
        assert!(empty_token.validate().is_err());
        let mut control_token = config.clone();
        control_token.portal_restore_token = Some("bad\u{7}token".into());
        assert!(control_token.validate().is_err());
    }

    #[test]
    fn desktop_audio_capture_target_and_capture_bounds_validate() {
        let mut config = test_config(NodeProfile::Desktop);
        config.audio_capture_target = "monitor:ozma-desktop".parse().unwrap();
        config.desktop_capture_target = "node:12".parse().unwrap();
        config.desktop_capture_width = 2560;
        config.desktop_capture_height = 1440;
        config.desktop_capture_fps = 60;
        assert!(config.validate().is_ok());
        let resolved = config.desktop_providers().unwrap();
        assert_eq!(
            resolved.audio_capture,
            ozma_audio::AudioTarget::SinkMonitor("ozma-desktop".into())
        );
        assert_eq!(
            resolved.capture,
            DesktopCaptureSelection::PipeWire(DesktopCaptureTarget::NodeId(12))
        );

        config.desktop_capture_width = 0;
        assert!(config.validate().is_err());
        config.desktop_capture_width = 2560;
        config.desktop_capture_fps = 0;
        assert!(config.validate().is_err());
        config.desktop_capture_fps = 60;
        config.desktop_capture_remote = Some("   ".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn usb_flags_are_opt_in_and_validate_media_and_udc() {
        let mut config = test_config(NodeProfile::Hardware);
        assert!(config.validate().is_ok());
        assert!(!config.has_capability("usb"));

        // Every USB control flag requires the explicit opt-in.
        let mut udc_without_gadget = config.clone();
        udc_without_gadget.usb_udc = Some("dummy_udc.0".into());
        assert!(udc_without_gadget.validate().is_err());

        let mut media_without_gadget = config.clone();
        media_without_gadget.usb_media = Some(PathBuf::from("/srv/installer.iso"));
        assert!(media_without_gadget.validate().is_err());

        let mut uac2_without_gadget = config.clone();
        uac2_without_gadget.usb_uac2 = true;
        assert!(uac2_without_gadget.validate().is_err());

        let mut vendor_without_gadget = config.clone();
        vendor_without_gadget.usb_vendor = true;
        assert!(vendor_without_gadget.validate().is_err());

        config.usb_gadget = true;
        config.usb_udc = Some("dummy_udc.0".into());
        assert!(config.validate().is_ok());

        let mut extras = config.clone();
        extras.usb_uac2 = true;
        assert!(extras.validate().is_ok());
        extras.usb_vendor = true;
        assert!(extras.validate().is_ok());
        extras.usb_vendor_daemon = Some(PathBuf::from("/usr/local/bin/ozma-ffs"));
        assert!(extras.validate().is_ok());
        extras.usb_vendor_daemon = Some(PathBuf::from("relative-daemon"));
        assert!(extras.validate().is_err());
        extras.usb_vendor = false;
        extras.usb_vendor_daemon = Some(PathBuf::from("/usr/local/bin/ozma-ffs"));
        assert!(extras.validate().is_err());

        // UDC names are written verbatim to ConfigFS and must stay safe.
        for bad in ["", "a/b", "dummy udc.0", "../udc", ".hidden"] {
            let mut invalid = config.clone();
            invalid.usb_udc = Some(bad.into());
            assert!(invalid.validate().is_err(), "{bad:?} must be rejected");
        }

        // Read-only/CD-ROM flags only make sense with a backing image.
        config.usb_media_ro = true;
        assert!(config.validate().is_err());
        config.usb_media_ro = false;
        config.usb_media_cdrom = true;
        assert!(config.validate().is_err());
        config.usb_media_cdrom = false;

        config.usb_media = Some(PathBuf::from("/srv/installer.iso"));
        config.usb_media_ro = true;
        config.usb_media_cdrom = true;
        assert!(config.validate().is_ok());

        config.usb_media = Some(PathBuf::from("relative.img"));
        assert!(config.validate().is_err());

        config.usb_media = Some(PathBuf::new());
        assert!(config.validate().is_err());

        // The userspace auto-start export is separate and also opt-in.
        let mut export = test_config(NodeProfile::Hardware);
        export.usb_media_export = Some(PathBuf::from("/srv/export.img"));
        assert!(export.validate().is_err());

        export.usb_gadget = true;
        export.usb_udc = Some("dummy_udc.0".into());
        assert!(export.validate().is_ok());

        export.usb_media_export_read_write = true;
        export.usb_media_export_persist = Some(PathBuf::from("/srv/overlay.bin"));
        assert!(export.validate().is_ok());

        export.usb_media_export_reload = true;
        assert!(export.validate().is_ok());

        export.usb_media_export_format = "iso9660".into();
        assert!(export.validate().is_err());
        export.usb_media_export_format = "raw".into();
        export.usb_media_export_reload = false;
        export.usb_media_export_persist = Some(PathBuf::from("relative.bin"));
        assert!(export.validate().is_err());
        export.usb_media_export_persist = None;
        export.usb_media_restart_max_attempts = 17;
        assert!(export.validate().is_err());
        export.usb_media_restart_max_attempts = 3;
        export.usb_media_restart_backoff_ms = 500;
        export.usb_media_restart_max_backoff_ms = 100;
        assert!(export.validate().is_err());

        // The default root is always a usable, explicit path.
        assert_eq!(
            test_config(NodeProfile::Hardware).usb_configfs_root,
            PathBuf::from(DEFAULT_USB_CONFIGFS_ROOT)
        );
    }
}
