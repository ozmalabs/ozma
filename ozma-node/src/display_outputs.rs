// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Controller-facing display-output list for VM consoles.
//!
//! The controller's `DirectRegisterRequest` carries `display_outputs` as a
//! JSON-encoded list. For a VM node those entries are console heads, not host
//! connectors, so the same [`VmDisplayTopology`] model used by `ozma-display`
//! validates and normalizes them.
//!
//! CLI form (`--display-output`, repeatable):
//!
//! ```text
//! <label>:<width>x<height>[:primary][:<source>]
//! <index>:<label>:<width>x<height>[:primary][:<source>]
//! ```
//!
//! `source` is one of `qmp`, `dbus`, `vnc`, or `looking-glass`; when it is
//! absent the node's selected capture provider is used. `index` defaults to the
//! argument's position, which matches QEMU's `Console_N` numbering.
//!
//! Only the console selected for live capture gets a `capture_source_id`. The
//! other consoles are still advertised (the controller can show their geometry
//! and offer a picker), but they are not claimed to have a working capture
//! source. This module never fabricates a source id or a frame.

use std::fmt;
use std::str::FromStr;

use ozma_display::vm::{VmConsole, VmConsoleBackend, VmDisplayTopology};
use serde::{Deserialize, Serialize};

/// One controller-facing display output (a VM console head).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayOutput {
    /// Zero-based `Console_N` index.
    pub index: u32,
    /// Human-readable console label.
    pub name: String,
    /// Capture transport (`qmp`, `dbus`, `vnc`, `looking_glass`).
    pub source_type: String,
    /// Capture source id, or an empty string when this console is not the
    /// selected live-capture console.
    pub capture_source_id: String,
    /// Console width in physical pixels (`0` when it is not known yet).
    pub width: u32,
    /// Console height in physical pixels (`0` when it is not known yet).
    pub height: u32,
    /// Whether this console is the layout primary.
    #[serde(default)]
    pub primary: bool,
}

/// Parsed `--display-output` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayOutputSpec {
    /// Explicit `Console_N` index, or `None` to use the argument position.
    pub index: Option<u32>,
    /// Console label.
    pub label: String,
    /// Console width in physical pixels.
    pub width: u32,
    /// Console height in physical pixels.
    pub height: u32,
    /// Whether the guest/user designated this console primary.
    pub primary: bool,
    /// Optional per-console source override.
    pub backend: Option<VmConsoleBackend>,
}

impl fmt::Display for DisplayOutputSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}x{}", self.label, self.width, self.height)?;
        if self.primary {
            f.write_str(":primary")?;
        }
        if let Some(backend) = self.backend {
            write!(f, ":{backend}")?;
        }
        Ok(())
    }
}

impl FromStr for DisplayOutputSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err("--display-output must not be empty".to_owned());
        }
        let mut parts: Vec<&str> = value.split(':').collect();

        // An explicit leading integer is the console index. Labels may not
        // contain `:`; that keeps `label:WxH` unambiguous.
        let index = if parts.len() >= 2 {
            match parts[0].trim().parse::<u32>() {
                Ok(index) => {
                    parts.remove(0);
                    Some(index)
                }
                Err(_) => None,
            }
        } else {
            None
        };

        if parts.len() < 2 {
            return Err(format!(
                "invalid --display-output {value:?}; expected \
                 <label>:<width>x<height>[:primary][:<source>] or \
                 <index>:<label>:<width>x<height>[:primary][:<source>]"
            ));
        }

        let label = parts[0].trim();
        if label.is_empty() {
            return Err(format!("--display-output {value:?} has an empty label"));
        }
        let (width, height) = parse_size(parts[1], value)?;

        let mut primary = false;
        let mut backend = None;
        for flag in &parts[2..] {
            let flag = flag.trim();
            if flag.is_empty() {
                continue;
            }
            let normalized = flag.to_ascii_lowercase().replace('_', "-");
            match normalized.as_str() {
                "primary" | "primary=true" | "primary=1" => primary = true,
                "primary=false" | "primary=0" => primary = false,
                other => {
                    let backend_name = other
                        .strip_prefix("source=")
                        .or_else(|| other.strip_prefix("backend="))
                        .unwrap_or(other);
                    let parsed = VmConsoleBackend::from_str(backend_name).map_err(|_| {
                        format!(
                            "invalid --display-output {value:?}: unknown flag {flag:?}; \
                             expected primary or a source (qmp, dbus, vnc, looking-glass)"
                        )
                    })?;
                    if backend.is_some() {
                        return Err(format!(
                            "--display-output {value:?} names more than one source"
                        ));
                    }
                    backend = Some(parsed);
                }
            }
        }

        Ok(Self {
            index,
            label: label.to_owned(),
            width,
            height,
            primary,
            backend,
        })
    }
}

fn parse_size(token: &str, whole: &str) -> Result<(u32, u32), String> {
    let (width, height) = token
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("invalid --display-output {whole:?}: expected <width>x<height>"))?;
    let width = width
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid --display-output {whole:?}: bad width {width:?}"))?;
    let height = height
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid --display-output {whole:?}: bad height {height:?}"))?;
    if width == 0 || height == 0 {
        return Err(format!(
            "invalid --display-output {whole:?}: width and height must be greater than zero"
        ));
    }
    Ok((width, height))
}

/// Validated display outputs plus the console selected for live capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayOutputSelection {
    /// Controller-facing outputs in layout order.
    pub outputs: Vec<DisplayOutput>,
    /// Console index the node's one capture provider targets.
    pub active_console: u32,
    /// Honest description when more consoles exist than live capture sources.
    pub limitation: Option<String>,
}

impl DisplayOutputSelection {
    /// Empty selection, used when no VM display source is configured.
    pub fn empty() -> Self {
        Self {
            outputs: Vec::new(),
            active_console: 0,
            limitation: None,
        }
    }

    /// The JSON string stored in registration and shown on
    /// `/api/v1/connection`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.outputs).unwrap_or_else(|_| "[]".to_owned())
    }

    /// The JSON value form for the connection endpoint.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.outputs).unwrap_or_else(|_| serde_json::json!([]))
    }

    /// Primary console index, following the topology's normalization.
    pub fn primary_console(&self) -> Option<u32> {
        self.outputs
            .iter()
            .find(|output| output.primary)
            .map(|output| output.index)
    }
}

/// Build a normalized topology from repeated `--display-output` arguments.
///
/// `default_backend` is applied to consoles that do not name a source.
/// `capture_source_id` is attached only to `active_console`; pass `None` when
/// no capture provider is configured. `active_console` defaults to the
/// topology primary.
pub fn outputs_from_specs(
    specs: &[String],
    default_backend: VmConsoleBackend,
    capture_source_id: Option<&str>,
    active_console: Option<u32>,
) -> Result<DisplayOutputSelection, String> {
    if specs.is_empty() {
        return Ok(DisplayOutputSelection::empty());
    }

    let parsed = specs
        .iter()
        .map(|spec| spec.parse::<DisplayOutputSpec>())
        .collect::<Result<Vec<_>, _>>()?;

    let mut consoles = Vec::with_capacity(parsed.len());
    for (position, spec) in parsed.iter().enumerate() {
        let index = spec.index.unwrap_or(position as u32);
        let label = if spec.label.trim().is_empty() {
            format!("Console_{index}")
        } else {
            spec.label.clone()
        };
        consoles.push(VmConsole::new(
            index,
            label,
            spec.width,
            spec.height,
            spec.backend.unwrap_or(default_backend),
            spec.primary,
        ));
    }

    let mut topology = VmDisplayTopology::from_consoles(consoles);
    topology
        .validate()
        .map_err(|error| format!("invalid --display-output topology: {error}"))?;

    // The model allows zero explicit primaries and resolves a fallback; make
    // that choice explicit so registration and the connection endpoint agree.
    // Multiple explicit primaries are already rejected by `validate`.
    if !topology.consoles.iter().any(|console| console.primary) {
        let fallback = if topology.get(0).is_some() {
            0
        } else {
            topology.consoles[0].index
        };
        if let Some(console) = topology
            .consoles
            .iter_mut()
            .find(|console| console.index == fallback)
        {
            console.primary = true;
        }
    }

    let active = active_console
        .or_else(|| topology.primary().map(VmConsole::index))
        .unwrap_or(0);

    let mut limitation = if topology.get(active).is_none() {
        Some(format!(
            "the capture provider targets Console_{active}, which is not among the \
             advertised display outputs; no output has a live capture source"
        ))
    } else if topology.consoles.len() > 1 {
        Some(format!(
            "only Console_{active} has a live capture source; the other consoles are \
             advertised for layout/selection but require restarting with that console selected"
        ))
    } else {
        None
    };
    if capture_source_id.is_none() {
        limitation = Some(match limitation {
            Some(existing) => format!("{existing}; no capture provider is configured"),
            None => "no capture provider is configured for this display output".to_owned(),
        });
    }

    let outputs = topology
        .consoles
        .iter()
        .map(|console| DisplayOutput {
            index: console.index,
            name: console.label.clone(),
            source_type: console.backend.as_str().to_owned(),
            capture_source_id: if console.index == active {
                capture_source_id.unwrap_or_default().to_owned()
            } else {
                String::new()
            },
            width: console.width,
            height: console.height,
            primary: console.primary,
        })
        .collect();

    Ok(DisplayOutputSelection {
        outputs,
        active_console: active,
        limitation,
    })
}

/// The single implicit console for a VM capture provider.
///
/// `width`/`height` must be what the provider actually produces; pass `0` for
/// providers whose geometry is only known after the first frame (QMP, D-Bus,
/// Looking Glass) instead of guessing a mode.
pub fn single_output(
    backend: VmConsoleBackend,
    index: u32,
    capture_source_id: Option<&str>,
    width: u32,
    height: u32,
    label: Option<&str>,
) -> DisplayOutputSelection {
    let label = label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Console_{index}"));
    DisplayOutputSelection {
        outputs: vec![DisplayOutput {
            index,
            name: label,
            source_type: backend.as_str().to_owned(),
            capture_source_id: capture_source_id.unwrap_or_default().to_owned(),
            width,
            height,
            primary: true,
        }],
        active_console: index,
        limitation: capture_source_id
            .is_none()
            .then(|| "no capture provider is configured for this display output".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_label_size_and_primary_without_an_index() {
        let spec: DisplayOutputSpec = "HDMI-1:1920x1080:primary".parse().unwrap();
        assert_eq!(spec.index, None);
        assert_eq!(spec.label, "HDMI-1");
        assert_eq!((spec.width, spec.height), (1920, 1080));
        assert!(spec.primary);
        assert_eq!(spec.backend, None);
    }

    #[test]
    fn parses_explicit_index_and_source() {
        let spec: DisplayOutputSpec = "2:DP-1:2560x1440:dbus".parse().unwrap();
        assert_eq!(spec.index, Some(2));
        assert_eq!(spec.label, "DP-1");
        assert_eq!((spec.width, spec.height), (2560, 1440));
        assert!(!spec.primary);
        assert_eq!(spec.backend, Some(VmConsoleBackend::Dbus));

        let spec: DisplayOutputSpec = "0:Console_0:1280x800:primary:looking-glass"
            .parse()
            .unwrap();
        assert_eq!(spec.backend, Some(VmConsoleBackend::LookingGlass));
        assert!(spec.primary);
    }

    #[test]
    fn rejects_malformed_specs() {
        for value in [
            "",
            "HDMI-1",
            "HDMI-1:1920",
            "HDMI-1:0x1080",
            "HDMI-1:1920x0",
            "HDMI-1:1920x1080:blue",
            ":1920x1080",
            "HDMI-1:1920x1080:qmp:vnc",
        ] {
            assert!(
                value.parse::<DisplayOutputSpec>().is_err(),
                "{value:?} should be rejected"
            );
        }
    }

    #[test]
    fn builds_a_normalized_topology_and_marks_only_the_active_source() {
        let specs = vec![
            "0:Console_0:1920x1080:primary:dbus".to_owned(),
            "1:Console_1:1280x1024:dbus".to_owned(),
        ];
        let selection =
            outputs_from_specs(&specs, VmConsoleBackend::Qmp, Some("dbus0"), Some(0)).unwrap();

        assert_eq!(selection.outputs.len(), 2);
        assert_eq!(selection.outputs[0].index, 0);
        assert_eq!(selection.outputs[0].source_type, "dbus");
        assert_eq!(selection.outputs[0].capture_source_id, "dbus0");
        assert!(selection.outputs[0].primary);
        assert_eq!(selection.outputs[1].index, 1);
        assert_eq!(selection.outputs[1].capture_source_id, "");
        assert!(!selection.outputs[1].primary);
        assert!(selection.limitation.is_some());
    }

    #[test]
    fn defaults_the_source_and_primary_when_omitted() {
        let specs = vec![
            "0:Console_0:1920x1080".to_owned(),
            "1:Console_1:1920x1080".to_owned(),
        ];
        let selection =
            outputs_from_specs(&specs, VmConsoleBackend::Vnc, Some("vnc0"), Some(1)).unwrap();
        assert!(selection.outputs.iter().all(|out| out.source_type == "vnc"));
        // No explicit primary: Console_0 is the normalized fallback even when
        // another console is the active capture head.
        assert!(selection.outputs[0].primary);
        assert!(!selection.outputs[1].primary);
        assert_eq!(selection.primary_console(), Some(0));
        assert_eq!(selection.outputs[1].capture_source_id, "vnc0");
    }

    #[test]
    fn rejects_multiple_primaries_and_duplicate_indices() {
        let two_primaries = vec![
            "0:Console_0:1920x1080:primary".to_owned(),
            "1:Console_1:1920x1080:primary".to_owned(),
        ];
        let error = outputs_from_specs(&two_primaries, VmConsoleBackend::Dbus, Some("dbus0"), None)
            .unwrap_err();
        assert!(error.contains("primary"), "{error}");

        let duplicate = vec![
            "0:Console_0:1920x1080".to_owned(),
            "0:Console_1:1920x1080".to_owned(),
        ];
        let error = outputs_from_specs(&duplicate, VmConsoleBackend::Dbus, Some("dbus0"), None)
            .unwrap_err();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn reports_an_active_console_that_is_not_advertised_without_fabricating_a_source() {
        let specs = vec!["2:Console_2:1920x1080".to_owned()];
        let selection =
            outputs_from_specs(&specs, VmConsoleBackend::Dbus, Some("dbus0"), Some(0)).unwrap();
        assert_eq!(selection.outputs.len(), 1);
        assert_eq!(selection.outputs[0].index, 2);
        assert_eq!(selection.outputs[0].capture_source_id, "");
        let limitation = selection.limitation.unwrap();
        assert!(limitation.contains("Console_0"), "{limitation}");
        assert!(
            limitation.contains("no output has a live capture source"),
            "{limitation}"
        );
    }

    #[test]
    fn single_output_reports_unknown_geometry_as_zero() {
        let selection = single_output(VmConsoleBackend::LookingGlass, 0, Some("lg0"), 0, 0, None);
        assert_eq!(selection.outputs.len(), 1);
        assert_eq!(selection.outputs[0].name, "Console_0");
        assert_eq!(selection.outputs[0].source_type, "looking_glass");
        assert_eq!(selection.outputs[0].capture_source_id, "lg0");
        assert_eq!(selection.outputs[0].width, 0);
        assert!(selection.outputs[0].primary);
    }
}
