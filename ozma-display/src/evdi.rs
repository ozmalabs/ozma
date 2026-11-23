// SPDX-License-Identifier: AGPL-3.0-only
//! A safe EVDI detection scaffold.
//!
//! This module does **not** talk to the kernel driver. Linking `libevdi`
//! requires `dlopen`/FFI (`unsafe`) and an event loop that owns DRM buffers,
//! which is deliberately outside this crate's constraints. What it does is
//! probe the system using only filesystem reads and `std::process::Command`,
//! report precisely what is missing, and refuse to claim support that the
//! build cannot deliver.
//!
//! The M2 spike that must pass before this becomes a real provider is recorded
//! in `docs/remote-display-evdi-spike.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ozma_protocol::display::{
    DisplayEvent, DisplayId, DisplayMode, DisplayTopology, DpmsState, VirtualDisplayBackend,
    VirtualDisplayCaps, VirtualDisplaySpec,
};
use ozma_protocol::{ApiError, Result};
use serde_json::json;
use tokio::sync::broadcast;

use crate::error;
use crate::provider::{DisplayEventStream, DisplayPrivacy, VirtualDisplay, VirtualDisplayProvider};

const EVENT_CAPACITY: usize = 32;

/// What was found on this machine. All fields are observations, not promises.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvdiProbe {
    /// `/dev/dri` exists and contains at least one `card*`/`renderD*` node.
    pub dev_dri: bool,
    /// DRM nodes discovered under `/dev/dri`.
    pub dri_nodes: Vec<PathBuf>,
    /// The `evdi` kernel module is loaded (`/sys/module/evdi` or `/proc/modules`).
    pub evdi_module: bool,
    /// A `libevdi.so` was found on the loader paths.
    pub libevdi: bool,
    /// Discovered `libevdi` paths.
    pub libevdi_paths: Vec<PathBuf>,
    /// Running kernel release, for the spike's version record.
    pub kernel_release: Option<String>,
}

impl EvdiProbe {
    /// Inspect the local system. This never panics and never writes anything.
    pub fn detect() -> Self {
        let dri_nodes = dri_nodes();
        let libevdi_paths = libevdi_paths();
        Self {
            dev_dri: !dri_nodes.is_empty(),
            dri_nodes,
            evdi_module: evdi_module_loaded(),
            libevdi: !libevdi_paths.is_empty(),
            libevdi_paths,
            kernel_release: kernel_release(),
        }
    }

    /// True only when every prerequisite for the spike is present. Even then,
    /// this crate's provider is a scaffold, not an implementation.
    pub fn ready(&self) -> bool {
        self.dev_dri && self.evdi_module && self.libevdi
    }

    /// Human-readable names of the missing prerequisites.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.dev_dri {
            missing.push("/dev/dri");
        }
        if !self.evdi_module {
            missing.push("evdi kernel module");
        }
        if !self.libevdi {
            missing.push("libevdi");
        }
        missing
    }

    fn as_json(&self) -> serde_json::Value {
        json!({
            "dev_dri": self.dev_dri,
            "dri_nodes": self.dri_nodes.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "evdi_module": self.evdi_module,
            "libevdi": self.libevdi,
            "libevdi_paths": self.libevdi_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "kernel_release": self.kernel_release,
            "ready": self.ready(),
        })
    }
}

/// EVDI provider scaffold.
///
/// `capabilities().supported` is always `false`: this build can detect EVDI
/// but cannot create a connector without the unsafe libevdi integration and
/// the M2 spike. The UI should therefore offer mirroring, while the probe
/// explains exactly what an operator would need to install to run the spike.
#[derive(Clone)]
pub struct EvdiProvider {
    probe: EvdiProbe,
    events: broadcast::Sender<DisplayEvent>,
}

impl Default for EvdiProvider {
    fn default() -> Self {
        Self::detect()
    }
}

impl EvdiProvider {
    /// Probe the local system and build the scaffold.
    pub fn detect() -> Self {
        Self::from_probe(EvdiProbe::detect())
    }

    /// Build from an explicit probe. Useful for tests and for an operator
    /// running the probe on a different host.
    pub fn from_probe(probe: EvdiProbe) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self { probe, events }
    }

    pub fn probe(&self) -> &EvdiProbe {
        &self.probe
    }

    /// Actionable text for a UI failure card or a log line.
    pub fn remediation(&self) -> String {
        if self.probe.ready() {
            "EVDI prerequisites are present, but this build only ships the safe detection \
             scaffold. Complete the M2 spike in docs/remote-display-evdi-spike.md and link \
             libevdi through the privileged display helper before advertising support."
                .to_string()
        } else {
            "Install the EVDI kernel module and libevdi (on Debian-family systems usually \
             `evdi-dkms` plus `libevdi0`/`libevdi1`), load the module, and make sure /dev/dri \
             is accessible. Run `scripts/remote-display-spike.sh --probe` for a read-only \
             diagnosis, then see docs/remote-display-evdi-spike.md."
                .to_string()
        }
    }

    fn unsupported(&self, message: &str) -> ApiError {
        error::unsupported(message)
            .with_details(json!({
                "provider": "evdi",
                "missing": self.probe.missing(),
                "probe": self.probe.as_json(),
            }))
            .with_remediation(self.remediation())
    }
}

impl VirtualDisplayProvider for EvdiProvider {
    fn capabilities(&self) -> VirtualDisplayCaps {
        VirtualDisplayCaps {
            supported: false,
            backends: if self.probe.ready() {
                vec![VirtualDisplayBackend::Evdi]
            } else {
                Vec::new()
            },
            max_outputs: 0,
            hotplug: self.probe.ready(),
            programmable_edid: self.probe.ready(),
            stable_identity: true,
            hdr: Default::default(),
            rotation: true,
            scale: true,
        }
    }

    fn create(&self, _spec: VirtualDisplaySpec) -> Result<VirtualDisplay> {
        Err(self.unsupported(
            "The EVDI provider is a detection scaffold and cannot create a connector in this build",
        ))
    }

    fn destroy(&self, _id: DisplayId) -> Result<()> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn set_mode(&self, _id: DisplayId, _mode: DisplayMode) -> Result<()> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn set_dpms(&self, _id: DisplayId, _state: DpmsState) -> Result<()> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn set_privacy(&self, _id: DisplayId, _policy: &DisplayPrivacy) -> Result<()> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn set_topology(&self, _topology: DisplayTopology) -> Result<()> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn topology(&self) -> Result<DisplayTopology> {
        Err(self.unsupported("The EVDI provider is not implemented"))
    }

    fn events(&self) -> DisplayEventStream {
        self.events.subscribe()
    }
}

fn dri_nodes() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("card") || name.starts_with("renderD"))
                .unwrap_or(false)
        })
        .collect();
    nodes.sort();
    nodes
}

fn evdi_module_loaded() -> bool {
    if Path::new("/sys/module/evdi").exists() {
        return true;
    }
    fs::read_to_string("/proc/modules")
        .map(|modules| {
            modules
                .lines()
                .any(|line| line.split_whitespace().next() == Some("evdi"))
        })
        .unwrap_or(false)
}

fn kernel_release() -> Option<String> {
    if let Ok(release) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        let release = release.trim();
        if !release.is_empty() {
            return Some(release.to_string());
        }
    }
    // `/proc` is always present on Linux, but keep a portable fallback for
    // tests on unusual sandboxes. This is a read-only command.
    Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            } else {
                None
            }
        })
}

fn libevdi_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for directory in [
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/lib",
        "/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/arm-linux-gnueabihf",
    ] {
        for name in ["libevdi.so", "libevdi.so.1", "libevdi.so.0"] {
            let path = Path::new(directory).join(name);
            if path.exists() {
                paths.push(path);
            }
        }
    }

    // `ldconfig -p` catches multiarch/custom loader directories that the
    // static list above does not know about. Failure is normal in containers.
    if let Ok(output) = Command::new("ldconfig").arg("-p").output() {
        if output.status.success() {
            if let Ok(text) = String::from_utf8(output.stdout) {
                for line in text.lines() {
                    if line.contains("libevdi.so") {
                        let Some(index) = line.find("=>") else {
                            continue;
                        };
                        let path = PathBuf::from(line[index + 2..].trim());
                        if path.exists() {
                            paths.push(path);
                        }
                    }
                }
            }
        }
    }

    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_probe() -> EvdiProbe {
        EvdiProbe {
            dev_dri: false,
            dri_nodes: Vec::new(),
            evdi_module: false,
            libevdi: false,
            libevdi_paths: Vec::new(),
            kernel_release: Some("test-kernel".into()),
        }
    }

    fn ready_probe() -> EvdiProbe {
        EvdiProbe {
            dev_dri: true,
            dri_nodes: vec![PathBuf::from("/dev/dri/card0")],
            evdi_module: true,
            libevdi: true,
            libevdi_paths: vec![PathBuf::from("/usr/lib/libevdi.so.1")],
            kernel_release: Some("test-kernel".into()),
        }
    }

    fn spec() -> VirtualDisplaySpec {
        VirtualDisplaySpec::new(
            "remote-1",
            "Remote",
            (300, 190),
            DisplayMode::new(1920, 1080, 60),
        )
    }

    #[test]
    fn absence_is_reported_with_remediation() {
        let provider = EvdiProvider::from_probe(missing_probe());
        let capabilities = provider.capabilities();
        assert!(!capabilities.supported);
        assert!(capabilities.backends.is_empty());
        assert_eq!(capabilities.max_outputs, 0);

        let err = provider.create(spec()).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_UNSUPPORTED);
        assert!(err.remediation.is_some());
        let remediation = err.remediation.unwrap();
        assert!(remediation.contains("evdi-dkms"));
        assert!(remediation.contains("remote-display-spike"));
        assert_eq!(
            err.details["missing"],
            json!(["/dev/dri", "evdi kernel module", "libevdi"])
        );
    }

    #[test]
    fn prerequisites_do_not_imply_implementation() {
        let provider = EvdiProvider::from_probe(ready_probe());
        assert!(provider.probe().ready());
        assert!(provider.probe().missing().is_empty());
        let capabilities = provider.capabilities();
        assert!(!capabilities.supported);
        assert_eq!(capabilities.backends, vec![VirtualDisplayBackend::Evdi]);

        let err = provider.create(spec()).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::DISPLAY_UNSUPPORTED);
        assert!(err.remediation.unwrap().contains("scaffold"));
        assert_eq!(err.details["missing"], json!([]));
        assert_eq!(err.details["provider"], "evdi");
    }

    #[test]
    fn every_operation_returns_a_structured_unsupported_error() {
        let provider = EvdiProvider::from_probe(missing_probe());
        let id = DisplayId::new("remote-1");
        let errors = [
            provider.destroy(id.clone()).unwrap_err(),
            provider
                .set_mode(id.clone(), DisplayMode::new(1920, 1080, 60))
                .unwrap_err(),
            provider.set_dpms(id.clone(), DpmsState::Off).unwrap_err(),
            provider
                .set_privacy(id.clone(), &DisplayPrivacy::default())
                .unwrap_err(),
            provider
                .set_topology(DisplayTopology::default())
                .unwrap_err(),
            provider.topology().unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.code, ozma_protocol::codes::DISPLAY_UNSUPPORTED);
        }
    }

    #[test]
    fn events_returns_an_independent_receiver() {
        let provider = EvdiProvider::from_probe(missing_probe());
        let mut first = provider.events();
        let mut second = provider.events();
        assert!(first.try_recv().is_err());
        assert!(second.try_recv().is_err());
    }

    #[test]
    fn detection_is_read_only_and_does_not_panic() {
        let probe = EvdiProbe::detect();
        // Only assert invariants that hold on any host.
        assert_eq!(probe.dev_dri, !probe.dri_nodes.is_empty());
        assert_eq!(probe.libevdi, !probe.libevdi_paths.is_empty());
        assert_eq!(
            probe.ready(),
            probe.dev_dri && probe.evdi_module && probe.libevdi
        );
    }

    #[test]
    fn missing_lists_only_absent_prerequisites() {
        let mut probe = ready_probe();
        probe.libevdi = false;
        probe.libevdi_paths.clear();
        assert_eq!(probe.missing(), vec!["libevdi"]);
        assert!(!probe.ready());
    }
}
