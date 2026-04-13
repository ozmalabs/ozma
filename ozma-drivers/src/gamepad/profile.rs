// SPDX-License-Identifier: AGPL-3.0-only
//! Controller profile detection (Xbox / PlayStation / Nintendo / generic).

use serde::{Deserialize, Serialize};

/// High-level controller family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Xbox,
    PlayStation,
    Nintendo,
    Generic,
}

/// Detailed profile for a specific controller model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerProfile {
    pub family:      Family,
    /// e.g. "360", "one", "series", "ds4", "dualsense", "switch_pro", ""
    pub variant:     String,
    /// Face-button labels in South / East / North / West order.
    pub south_label: String,
    pub east_label:  String,
    pub north_label: String,
    pub west_label:  String,
}

impl ControllerProfile {
    /// Unique surface-id prefix derived from family + variant.
    pub fn surface_id(&self) -> String {
        let family = match self.family {
            Family::Xbox        => "xbox",
            Family::PlayStation => "playstation",
            Family::Nintendo    => "nintendo",
            Family::Generic     => "generic",
        };
        if self.variant.is_empty() {
            format!("gamepad-{family}")
        } else {
            format!("gamepad-{family}-{}", self.variant)
        }
    }
}

/// Detect a controller profile from the gilrs `Gamepad::name()` string.
pub fn detect_profile(name: &str) -> ControllerProfile {
    let lower = name.to_lowercase();

    // PlayStation — most specific first
    if lower.contains("dualsense") {
        return ps("dualsense");
    }
    if lower.contains("dualshock 4") || lower.contains("dualshock4") {
        return ps("ds4");
    }
    if lower.contains("dualshock 3") || lower.contains("dualshock3") {
        return ps("ds3");
    }
    if lower.contains("sony") || lower.contains("playstation") {
        return ps("");
    }

    // Nintendo
    if lower.contains("switch pro") || lower.contains("pro controller") {
        return nintendo("switch_pro");
    }
    if lower.contains("joycon") || lower.contains("joy-con") {
        return nintendo("joycon");
    }

    // Xbox — most specific first
    if lower.contains("xbox series") {
        return xbox("series");
    }
    if lower.contains("xbox one") {
        return xbox("one");
    }
    if lower.contains("xbox 360") {
        return xbox("360");
    }
    if lower.contains("xbox") || lower.contains("microsoft") || lower.contains("xinput") {
        return xbox("");
    }

    // 8BitDo and other known generics
    if lower.contains("8bitdo") || lower.contains("logitech") || lower.contains("steelseries") {
        return generic();
    }

    generic()
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn xbox(variant: &str) -> ControllerProfile {
    ControllerProfile {
        family:      Family::Xbox,
        variant:     variant.to_owned(),
        south_label: "A".into(),
        east_label:  "B".into(),
        north_label: "X".into(),
        west_label:  "Y".into(),
    }
}

fn ps(variant: &str) -> ControllerProfile {
    ControllerProfile {
        family:      Family::PlayStation,
        variant:     variant.to_owned(),
        south_label: "Cross".into(),
        east_label:  "Circle".into(),
        north_label: "Triangle".into(),
        west_label:  "Square".into(),
    }
}

fn nintendo(variant: &str) -> ControllerProfile {
    ControllerProfile {
        family:      Family::Nintendo,
        variant:     variant.to_owned(),
        south_label: "B".into(),
        east_label:  "A".into(),
        north_label: "X".into(),
        west_label:  "Y".into(),
    }
}

fn generic() -> ControllerProfile {
    ControllerProfile {
        family:      Family::Generic,
        variant:     String::new(),
        south_label: "South".into(),
        east_label:  "East".into(),
        north_label: "North".into(),
        west_label:  "West".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dualsense() {
        let p = detect_profile("Sony DualSense Wireless Controller");
        assert_eq!(p.family, Family::PlayStation);
        assert_eq!(p.variant, "dualsense");
        assert_eq!(p.south_label, "Cross");
    }

    #[test]
    fn detects_xbox_series() {
        let p = detect_profile("Xbox Series X Controller");
        assert_eq!(p.family, Family::Xbox);
        assert_eq!(p.variant, "series");
        assert_eq!(p.south_label, "A");
    }

    #[test]
    fn detects_switch_pro() {
        let p = detect_profile("Nintendo Switch Pro Controller");
        assert_eq!(p.family, Family::Nintendo);
        assert_eq!(p.variant, "switch_pro");
    }

    #[test]
    fn falls_back_to_generic() {
        let p = detect_profile("Unknown HID Gamepad");
        assert_eq!(p.family, Family::Generic);
    }

    #[test]
    fn surface_id_no_variant() {
        let p = detect_profile("Xbox Controller");
        assert_eq!(p.surface_id(), "gamepad-xbox");
    }

    #[test]
    fn surface_id_with_variant() {
        let p = detect_profile("Xbox 360 Controller");
        assert_eq!(p.surface_id(), "gamepad-xbox-360");
    }
}
