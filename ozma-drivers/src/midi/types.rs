//! MIDI types and enums

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// LCD color enum for Behringer displays
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Black = 0,
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
    White = 7,
}

impl Color {
    pub fn from_hex(hex_color: Option<&str>) -> Self {
        let hex_color = match hex_color {
            Some(color) => color.to_lowercase(),
            None => return Color::White,
        };

        match hex_color.as_str() {
            "#ff0000" => Color::Red,
            "#00ff00" => Color::Green,
            "#0000ff" => Color::Blue,
            "#ffff00" => Color::Yellow,
            "#ff00ff" => Color::Magenta,
            "#00ffff" => Color::Cyan,
            "#ffffff" => Color::White,
            "#000000" => Color::Black,
            _ => {
                // Try name match
                match hex_color.as_str() {
                    s if s.contains("red") => Color::Red,
                    s if s.contains("green") => Color::Green,
                    s if s.contains("blue") => Color::Blue,
                    s if s.contains("yellow") => Color::Yellow,
                    s if s.contains("magenta") => Color::Magenta,
                    s if s.contains("cyan") => Color::Cyan,
                    s if s.contains("black") => Color::Black,
                    _ => Color::White,
                }
            }
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Invert mode for LCD displays
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

impl Invert {
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// 7-segment display font mapping
pub const SEGMENT_FONT: &[u8; 91] = &[
    0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F, // 0-9
    0x77, 0x7C, 0x39, 0x5E, 0x79, 0x71, 0x3D, 0x76, 0x06, 0x1E, // A-J
    0x75, 0x38, 0x37, 0x37, 0x3F, 0x73, 0x67, 0x50, 0x6D, 0x78, // K-T
    0x3E, 0x1C, 0x3E, 0x49, 0x6E, 0x5B, 0x00, 0x00, 0x00, 0x00, // U-Z, [, \, ], ^, _
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // `a-j
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // k-z
    0x00, 0x40, 0x08, 0x09, 0x39, 0x0F, // {, |, }, ~, (, )
];

pub fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| {
            if c.is_ascii() && (c as u8) < 128 {
                SEGMENT_FONT[c as usize]
            } else {
                0
            }
        })
        .collect()
}

/// Control types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlType {
    #[serde(rename = "fader")]
    Fader,
    #[serde(rename = "button")]
    Button,
    #[serde(rename = "rotary")]
    Rotary,
    #[serde(rename = "jogwheel")]
    JogWheel,
}

/// Button styles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ButtonStyle {
    #[serde(rename = "toggle")]
    Toggle,
    #[serde(rename = "momentary")]
    Momentary,
}

/// Light styles for buttons
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LightStyle {
    #[serde(rename = "state")]
    State,
    #[serde(rename = "always_on")]
    AlwaysOn,
    #[serde(rename = "momentary")]
    Momentary,
    #[serde(rename = "off")]
    Off,
}

/// Control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    #[serde(rename = "type")]
    pub control_type: ControlType,
    #[serde(default)]
    pub control: Option<u8>,
    #[serde(default)]
    pub note: Option<u8>,
    #[serde(default)]
    pub style: Option<ButtonStyle>,
    #[serde(default)]
    pub light: Option<LightStyle>,
    #[serde(default)]
    pub binding: Option<ControlBinding>,
}

/// Display types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DisplayType {
    #[serde(rename = "scribble_top")]
    ScribbleTop,
    #[serde(rename = "scribble_bottom")]
    ScribbleBottom,
    #[serde(rename = "scribble")]
    Scribble,
}

/// Display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(rename = "type")]
    pub display_type: DisplayType,
    #[serde(default)]
    pub binding: Option<String>,
}

/// Control binding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBinding {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(rename = "to_target", default)]
    pub to_target_transform: Option<TransformSpec>,
    #[serde(rename = "from_target", default)]
    pub from_target_transform: Option<TransformSpec>,
}

/// Transform specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransformSpec {
    #[serde(rename = "midi_to_float")]
    MidiToFloat,
    #[serde(rename = "float_to_midi")]
    FloatToMidi,
    #[serde(rename = "map")]
    Map { from: Vec<f64>, to: Vec<f64> },
}

/// MIDI surface configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSurfaceConfig {
    pub device: String,
    #[serde(default)]
    pub controls: HashMap<String, ControlConfig>,
    #[serde(default)]
    pub displays: HashMap<String, DisplayConfig>,
}
