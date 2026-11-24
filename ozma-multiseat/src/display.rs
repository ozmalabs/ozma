// SPDX-License-Identifier: AGPL-3.0-only
//! Display output enumeration and backend detection.
//!
//! The planner needs a stable list of outputs, a region for each output, and a
//! screen name that can be passed to `ozma-node --profile desktop`. This
//! module models that vocabulary and provides two honest Linux paths:
//!
//! * [`XrandrDisplayBackend`] parses `xrandr --listactivemonitors` for a
//!   specific X display. It never inherits `DISPLAY` implicitly.
//! * [`crate::drm::detect_connectors`] reads DRM connector status/modes from
//!   sysfs for availability detection. It is not a mode-setting backend.
//!
//! Test fakes live in [`crate::fake`] and are never selected by production
//! detection.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;

/// One display output (physical monitor, X11 RandR monitor, or virtual head).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayOutput {
    /// Backend-provided output/monitor index. Used for ordering and status.
    pub index: u32,
    /// Connector or RandR monitor name, for example `HDMI-1` or `DP-2`.
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// X offset of the output within the root screen.
    #[serde(default)]
    pub x: i32,
    /// Y offset of the output within the root screen.
    #[serde(default)]
    pub y: i32,
    /// X display string, for example `:0` or `:1`.
    ///
    /// This is a deliberate field rather than an ambient `DISPLAY` lookup so a
    /// plan can be reproduced on another host.
    #[serde(default)]
    pub x_screen: String,
    /// Whether the backend marked this output primary.
    #[serde(default)]
    pub primary: bool,
    /// Whether this output was created by a virtual-display path.
    #[serde(default)]
    pub virtual_display: bool,
}

impl DisplayOutput {
    /// Build a physical output.
    pub fn new(
        index: u32,
        name: impl Into<String>,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
        x_screen: impl Into<String>,
    ) -> Self {
        Self {
            index,
            name: name.into(),
            width,
            height,
            x,
            y,
            x_screen: x_screen.into(),
            primary: false,
            virtual_display: false,
        }
    }

    /// Mark this output primary.
    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    /// Mark this output as a virtual display.
    pub fn virtual_display(mut self) -> Self {
        self.virtual_display = true;
        self
    }

    /// Human-readable geometry, `WIDTHxHEIGHT+X+Y`.
    pub fn geometry(&self) -> String {
        format!("{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }
}

/// Side-effect-free detection result for a display backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendDetection {
    /// Backend name (`xrandr`, `drm`, `fake`).
    pub backend: String,
    /// Whether the backend can be used on this host.
    pub available: bool,
    /// Why it is unavailable, or what was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl BackendDetection {
    /// A usable backend.
    pub fn available(backend: impl Into<String>, reason: impl Into<Option<String>>) -> Self {
        Self {
            backend: backend.into(),
            available: true,
            reason: reason.into(),
        }
    }

    /// An unusable backend with an explanation.
    pub fn unavailable(backend: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            available: false,
            reason: Some(reason.into()),
        }
    }
}

/// Enumerates display outputs and optionally creates/destroys virtual heads.
///
/// Implementations must not mutate the hardware merely to enumerate. Virtual
/// display creation is explicit and may return `None` when the host cannot
/// provide one; the planner caps seats rather than pretending it succeeded.
pub trait DisplayBackend {
    /// Stable backend name for status and diagnostics.
    fn backend_name(&self) -> &'static str;

    /// Enumerate outputs that are currently usable for capture.
    fn enumerate(&self) -> Result<Vec<DisplayOutput>, MultiseatError>;

    /// Create a virtual output, or return `None` when unsupported.
    fn create_virtual(
        &self,
        _width: u32,
        _height: u32,
    ) -> Result<Option<DisplayOutput>, MultiseatError> {
        Ok(None)
    }

    /// Destroy a previously created virtual output. Returning `false` means the
    /// backend did nothing (including when the output is not virtual).
    fn destroy_virtual(&self, _display: &DisplayOutput) -> Result<bool, MultiseatError> {
        Ok(false)
    }
}

/// Linux X11 display enumeration through `xrandr --listactivemonitors`.
///
/// The display is explicit. Constructing this backend never reads `DISPLAY`;
/// [`Self::detect_ambient`] is the separate, explicit ambient check.
#[derive(Clone, Debug)]
pub struct XrandrDisplayBackend {
    display: String,
    program: PathBuf,
}

impl XrandrDisplayBackend {
    /// Use the `xrandr` found on `PATH` for `display`.
    pub fn new(display: impl Into<String>) -> Self {
        Self {
            display: display.into(),
            program: PathBuf::from("xrandr"),
        }
    }

    /// Use a specific `xrandr` binary.
    pub fn with_program(display: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            display: display.into(),
            program: program.into(),
        }
    }

    /// The configured X display string.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Check whether `xrandr` exists and `display` is non-empty.
    ///
    /// This does not run X queries and does not prove a server is reachable.
    pub fn detect(display: &str) -> BackendDetection {
        if display.trim().is_empty() {
            return BackendDetection::unavailable(
                "xrandr",
                "no explicit X display configured (ambient DISPLAY is not inherited)",
            );
        }
        match find_on_path("xrandr") {
            Some(path) => BackendDetection::available(
                "xrandr",
                Some(format!(
                    "xrandr at {} for display {display}",
                    path.display()
                )),
            ),
            None => BackendDetection::unavailable("xrandr", "xrandr was not found on PATH"),
        }
    }

    /// Detect using the ambient `DISPLAY` variable. This is an explicit helper
    /// for an operator-facing detection command, not a planner default.
    pub fn detect_ambient() -> BackendDetection {
        match std::env::var("DISPLAY") {
            Ok(display) if !display.trim().is_empty() => Self::detect(&display),
            _ => BackendDetection::unavailable("xrandr", "DISPLAY is not set"),
        }
    }

    /// Run `xrandr --listactivemonitors` and parse the active monitors.
    pub fn enumerate(&self) -> Result<Vec<DisplayOutput>, MultiseatError> {
        let output = Command::new(&self.program)
            .arg("--listactivemonitors")
            .env("DISPLAY", &self.display)
            .output()
            .map_err(|error| {
                MultiseatError::DisplayBackend(format!(
                    "failed to run {} for display {}: {error}",
                    self.program.display(),
                    self.display
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MultiseatError::DisplayBackend(format!(
                "xrandr --listactivemonitors failed for display {}: {}",
                self.display,
                truncate(stderr.trim(), 240)
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_listactivemonitors(&stdout, &self.display)
    }
}

impl DisplayBackend for XrandrDisplayBackend {
    fn backend_name(&self) -> &'static str {
        "xrandr"
    }

    fn enumerate(&self) -> Result<Vec<DisplayOutput>, MultiseatError> {
        XrandrDisplayBackend::enumerate(self)
    }
}

/// Parse `xrandr --listactivemonitors` output.
///
/// Example line:
///
/// ```text
///  0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
/// ```
///
/// Returns an ordered vector. An empty monitor list is valid (`Monitors: 0`);
/// malformed lines are ignored, but output that contains monitor-looking lines
/// none of which parse is an error instead of a silent empty result.
pub fn parse_listactivemonitors(
    output: &str,
    x_screen: &str,
) -> Result<Vec<DisplayOutput>, MultiseatError> {
    let mut displays = Vec::new();
    let mut candidate_lines = 0usize;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Monitors:") {
            continue;
        }
        if !trimmed.contains(':') {
            continue;
        }
        candidate_lines += 1;
        if let Some(display) = parse_monitor_line(trimmed, x_screen) {
            displays.push(display);
        }
    }

    if displays.is_empty() && candidate_lines > 0 {
        return Err(MultiseatError::DisplayBackend(
            "xrandr --listactivemonitors produced no parseable active monitors".to_owned(),
        ));
    }

    Ok(displays)
}

fn parse_monitor_line(line: &str, x_screen: &str) -> Option<DisplayOutput> {
    let (index_part, rest) = line.split_once(':')?;
    let index = index_part.trim().parse::<u32>().ok()?;

    let mut tokens = rest.split_whitespace();
    let flags_name = tokens.next()?;
    let geometry = tokens.next()?;
    let trailing_name = tokens.next();

    let flags_primary = flags_name.contains('*');
    let stripped_name = flags_name.trim_start_matches(['+', '*']);
    let name = trailing_name
        .filter(|name| !name.is_empty())
        .unwrap_or(stripped_name);
    if name.is_empty() {
        return None;
    }

    let (width, height, x, y) = parse_monitor_geometry(geometry)?;
    Some(DisplayOutput {
        index,
        name: name.to_owned(),
        width,
        height,
        x,
        y,
        x_screen: x_screen.to_owned(),
        primary: flags_primary,
        virtual_display: false,
    })
}

/// Parse `WIDTH/MMxHEIGHT/MM+X+Y` (for example `1920/530x1080/300+0+0`).
fn parse_monitor_geometry(geometry: &str) -> Option<(u32, u32, i32, i32)> {
    let (width_part, rest) = geometry.split_once('x')?;
    let width = width_part.split('/').next()?.parse::<u32>().ok()?;

    let (height_part, size_and_offsets) = rest.split_once('/')?;
    let height = height_part.parse::<u32>().ok()?;

    // `size_and_offsets` starts with the physical size in millimetres, for
    // example `300+0+0` or `0-10+5`. The geometry begins at the first sign.
    let offsets_start = size_and_offsets.find(['+', '-'])?;
    let (x, y) = parse_offsets(&size_and_offsets[offsets_start..])?;
    Some((width, height, x, y))
}

/// Parse the `+X+Y` / `-X+Y` suffix after the millimetre size.
fn parse_offsets(value: &str) -> Option<(i32, i32)> {
    let first = value.find(['+', '-'])?;
    let rest = &value[first + 1..];
    let second_rel = rest.find(['+', '-'])?;
    let second = first + 1 + second_rel;
    let x = value[..second].parse::<i32>().ok()?;
    let y = value[second..].parse::<i32>().ok()?;
    Some((x, y))
}

/// Find an executable on `PATH`.
pub(crate) fn find_on_path(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|path| path.is_file())
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let truncated: String = value.chars().take(limit).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_active_monitors() {
        let output = "\
Monitors: 2
 0: +*HDMI-1 1920/530x1080/300+0+0  HDMI-1
 1: +DP-2 2560/600x1440/340+1920+0  DP-2
";
        let displays = parse_listactivemonitors(output, ":0").expect("parse");
        assert_eq!(displays.len(), 2);
        assert_eq!(displays[0].name, "HDMI-1");
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].x, 0);
        assert!(displays[0].primary);
        assert_eq!(displays[1].name, "DP-2");
        assert_eq!(displays[1].x, 1920);
        assert_eq!(displays[1].x_screen, ":0");
    }

    #[test]
    fn parses_negative_and_virtual_offsets() {
        let output = " 0: +Virtual-1 1280/0x720/0-10+5  Virtual-1\n";
        let displays = parse_listactivemonitors(output, ":1").expect("parse");
        assert_eq!(displays[0].x, -10);
        assert_eq!(displays[0].y, 5);
        assert_eq!(displays[0].x_screen, ":1");
        assert!(!displays[0].primary);
    }

    #[test]
    fn empty_monitor_list_is_valid() {
        assert!(parse_listactivemonitors("Monitors: 0\n", ":0")
            .expect("empty")
            .is_empty());
    }

    #[test]
    fn malformed_monitor_lines_are_an_error() {
        let error = parse_listactivemonitors(" 0: bogus\n", ":0").expect_err("must fail");
        assert!(error.to_string().contains("no parseable"));
    }
}
