//! Types and enums for MIDI control surfaces

use serde::{Deserialize, Serialize};

/// MIDI message structure
#[derive(Debug, Clone)]
pub struct MidiMessage {
    pub msg_type: String,
    pub channel: u8,
    pub control: Option<u8>,
    pub note: Option<u8>,
    pub value: u8,
}

/// Control types supported by the MIDI surface
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ControlType {
    Fader,
    Button,
    Rotary,
    JogWheel,
}

/// Button interaction styles
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    Toggle,
    Momentary,
}

/// LED light behavior styles
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LightStyle {
    State,
    AlwaysOn,
    Momentary,
    #[serde(rename = "false")]
    False,
}

impl Default for ButtonStyle {
    fn default() -> Self {
        ButtonStyle::Toggle
    }
}

impl Default for LightStyle {
    fn default() -> Self {
        LightStyle::State
    }
}

/// LCD display colors
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
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
        if let Some(color) = hex_color {
            let color = color.to_lowercase();
            match color.as_str() {
                "#ff0000" => Self::Red,
                "#00ff00" => Self::Green,
                "#0000ff" => Self::Blue,
                "#ffff00" => Self::Yellow,
                "#ff00ff" => Self::Magenta,
                "#00ffff" => Self::Cyan,
                "#ffffff" => Self::White,
                "#000000" => Self::Black,
                _ => {
                    // Try name match
                    if color.contains("red") {
                        Self::Red
                    } else if color.contains("green") {
                        Self::Green
                    } else if color.contains("blue") {
                        Self::Blue
                    } else if color.contains("yellow") {
                        Self::Yellow
                    } else if color.contains("magenta") {
                        Self::Magenta
                    } else if color.contains("cyan") {
                        Self::Cyan
                    } else if color.contains("black") {
                        Self::Black
                    } else {
                        Self::White
                    }
                }
            }
        } else {
            Self::White
        }
    }
}

/// LCD display invert options
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// 7-segment display font mapping
pub const SEGMENT_FONT: [u8; 128] = {
    let mut font = [0u8; 128];
    // Initialize with default values
    let mut i = 0;
    while i < 128 {
        font[i] = 0;
        i += 1;
    }
    
    // Numbers
    font[b'0' as usize] = 0x3F;
    font[b'1' as usize] = 0x06;
    font[b'2' as usize] = 0x5B;
    font[b'3' as usize] = 0x4F;
    font[b'4' as usize] = 0x66;
    font[b'5' as usize] = 0x6D;
    font[b'6' as usize] = 0x7D;
    font[b'7' as usize] = 0x07;
    font[b'8' as usize] = 0x7F;
    font[b'9' as usize] = 0x6F;
    
    // Letters
    font[b'A' as usize] = 0x77;
    font[b'B' as usize] = 0x7F;
    font[b'C' as usize] = 0x39;
    font[b'D' as usize] = 0x3F;
    font[b'E' as usize] = 0x79;
    font[b'F' as usize] = 0x71;
    font[b'G' as usize] = 0x3D;
    font[b'H' as usize] = 0x76;
    font[b'I' as usize] = 0x06;
    font[b'J' as usize] = 0x0E;
    font[b'K' as usize] = 0x75;
    font[b'L' as usize] = 0x38;
    font[b'M' as usize] = 0x37;
    font[b'N' as usize] = 0x37;
    font[b'O' as usize] = 0x3F;
    font[b'P' as usize] = 0x73;
    font[b'Q' as usize] = 0x67;
    font[b'R' as usize] = 0x77;
    font[b'S' as usize] = 0x6D;
    font[b'T' as usize] = 0x78;
    font[b'U' as usize] = 0x3E;
    font[b'V' as usize] = 0x3E;
    font[b'W' as usize] = 0x3E;
    font[b'X' as usize] = 0x49;
    font[b'Y' as usize] = 0x6E;
    font[b'Z' as usize] = 0x5B;
    
    // Special characters
    font[b' ' as usize] = 0x00;
    font[b'-' as usize] = 0x40;
    font[b'.' as usize] = 0x08;
    font[b':' as usize] = 0x09;
    font[b'(' as usize] = 0x39;
    font[b')' as usize] = 0x0F;
    
    font
};

pub fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
        .take(12)
        .map(|c| {
            let idx = c as u8 as usize;
            if idx < SEGMENT_FONT.len() {
                SEGMENT_FONT[idx]
            } else {
                SEGMENT_FONT[b' ' as usize]
            }
        })
        .collect()
}

/// Convert Unicode to ASCII for LCD displays
pub fn unidecode(text: &str) -> String {
    // Simple ASCII conversion for now
    text.chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}
//! Types and enums for MIDI control surfaces

use serde::{Deserialize, Serialize};

/// Control types supported by the MIDI surface
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ControlType {
    Fader,
    Button,
    Rotary,
    JogWheel,
}

/// Button interaction styles
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    Toggle,
    Momentary,
}

/// LED light behavior styles
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LightStyle {
    State,
    AlwaysOn,
    Momentary,
    #[serde(rename = "false")]
    False,
}

impl Default for ButtonStyle {
    fn default() -> Self {
        ButtonStyle::Toggle
    }
}

impl Default for LightStyle {
    fn default() -> Self {
        LightStyle::State
    }
}

/// LCD display colors
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
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
        if let Some(color) = hex_color {
            let color = color.to_lowercase();
            match color.as_str() {
                "#ff0000" => Self::Red,
                "#00ff00" => Self::Green,
                "#0000ff" => Self::Blue,
                "#ffff00" => Self::Yellow,
                "#ff00ff" => Self::Magenta,
                "#00ffff" => Self::Cyan,
                "#ffffff" => Self::White,
                "#000000" => Self::Black,
                _ => {
                    // Try name match
                    if color.contains("red") {
                        Self::Red
                    } else if color.contains("green") {
                        Self::Green
                    } else if color.contains("blue") {
                        Self::Blue
                    } else if color.contains("yellow") {
                        Self::Yellow
                    } else if color.contains("magenta") {
                        Self::Magenta
                    } else if color.contains("cyan") {
                        Self::Cyan
                    } else if color.contains("black") {
                        Self::Black
                    } else {
                        Self::White
                    }
                }
            }
        } else {
            Self::White
        }
    }
}

/// LCD display invert options
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// 7-segment display font mapping
pub const SEGMENT_FONT: [u8; 128] = {
    let mut font = [0u8; 128];
    // Initialize with default values
    let mut i = 0;
    while i < 128 {
        font[i] = 0;
        i += 1;
    }
    
    // Numbers
    font[b'0' as usize] = 0x3F;
    font[b'1' as usize] = 0x06;
    font[b'2' as usize] = 0x5B;
    font[b'3' as usize] = 0x4F;
    font[b'4' as usize] = 0x66;
    font[b'5' as usize] = 0x6D;
    font[b'6' as usize] = 0x7D;
    font[b'7' as usize] = 0x07;
    font[b'8' as usize] = 0x7F;
    font[b'9' as usize] = 0x6F;
    
    // Letters
    font[b'A' as usize] = 0x77;
    font[b'B' as usize] = 0x7F;
    font[b'C' as usize] = 0x39;
    font[b'D' as usize] = 0x3F;
    font[b'E' as usize] = 0x79;
    font[b'F' as usize] = 0x71;
    font[b'G' as usize] = 0x3D;
    font[b'H' as usize] = 0x76;
    font[b'I' as usize] = 0x06;
    font[b'J' as usize] = 0x0E;
    font[b'K' as usize] = 0x75;
    font[b'L' as usize] = 0x38;
    font[b'M' as usize] = 0x37;
    font[b'N' as usize] = 0x37;
    font[b'O' as usize] = 0x3F;
    font[b'P' as usize] = 0x73;
    font[b'Q' as usize] = 0x67;
    font[b'R' as usize] = 0x77;
    font[b'S' as usize] = 0x6D;
    font[b'T' as usize] = 0x78;
    font[b'U' as usize] = 0x3E;
    font[b'V' as usize] = 0x3E;
    font[b'W' as usize] = 0x3E;
    font[b'X' as usize] = 0x49;
    font[b'Y' as usize] = 0x6E;
    font[b'Z' as usize] = 0x5B;
    
    // Special characters
    font[b' ' as usize] = 0x00;
    font[b'-' as usize] = 0x40;
    font[b'.' as usize] = 0x08;
    font[b':' as usize] = 0x09;
    font[b'(' as usize] = 0x39;
    font[b')' as usize] = 0x0F;
    
    font
};

pub fn render_7seg(text: &str) -> Vec<u8> {
    text.chars()
        .take(12)
        .map(|c| {
            let idx = c as u8 as usize;
            if idx < SEGMENT_FONT.len() {
                SEGMENT_FONT[idx]
            } else {
                SEGMENT_FONT[b' ' as usize]
            }
        })
        .collect()
}

/// Convert Unicode to ASCII for LCD displays
pub fn unidecode(text: &str) -> String {
    // Simple ASCII conversion for now
    text.chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}
//! MIDI control surface types and enums

use serde::{Deserialize, Serialize};

/// LCD colors for Behringer X-Touch displays
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
    /// Convert hex color string to LCD color
    pub fn from_hex(hex_color: Option<&str>) -> Self {
        match hex_color {
            Some(color) => {
                let color = color.to_lowercase();
                match color.as_str() {
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
                        if color.contains("red") {
                            Color::Red
                        } else if color.contains("green") {
                            Color::Green
                        } else if color.contains("blue") {
                            Color::Blue
                        } else if color.contains("yellow") {
                            Color::Yellow
                        } else if color.contains("magenta") {
                            Color::Magenta
                        } else if color.contains("cyan") {
                            Color::Cyan
                        } else if color.contains("black") {
                            Color::Black
                        } else {
                            Color::White
                        }
                    }
                }
            }
            None => Color::White,
        }
    }
}

/// Invert options for LCD displays
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// Control types supported by MIDI surfaces
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonStyle {
    #[serde(rename = "toggle")]
    Toggle,
    #[serde(rename = "momentary")]
    Momentary,
}

/// Light styles for buttons
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightStyle {
    #[serde(rename = "state")]
    State,
    #[serde(rename = "always_on")]
    AlwaysOn,
    #[serde(rename = "momentary")]
    Momentary,
}

/// MIDI control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiControlConfig {
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
}

/// Display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(rename = "type")]
    pub display_type: String,
    pub binding: Option<String>,
}

/// MIDI surface configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidiSurfaceConfig {
    pub device: String,
    #[serde(default)]
    pub controls: std::collections::HashMap<String, MidiControlConfig>,
    #[serde(default)]
    pub displays: std::collections::HashMap<String, DisplayConfig>,
}
//! MIDI control surface types and enums

use std::collections::HashMap;

/// LCD colors for Behringer X-Touch displays
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Convert hex color string to LCD color
    pub fn from_hex(hex_color: Option<&str>) -> Self {
        if let Some(color_str) = hex_color {
            let color_str = color_str.to_lowercase();
            let color_map: HashMap<&str, Color> = [
                ("#ff0000", Color::Red),
                ("#00ff00", Color::Green),
                ("#0000ff", Color::Blue),
                ("#ffff00", Color::Yellow),
                ("#ff00ff", Color::Magenta),
                ("#00ffff", Color::Cyan),
                ("#ffffff", Color::White),
                ("#000000", Color::Black),
            ]
            .iter()
            .cloned()
            .collect();

            if let Some(color) = color_map.get(color_str.as_str()) {
                return *color;
            }

            // Try name match
            for (name, color) in [
                ("red", Color::Red),
                ("green", Color::Green),
                ("blue", Color::Blue),
                ("yellow", Color::Yellow),
                ("magenta", Color::Magenta),
                ("cyan", Color::Cyan),
                ("white", Color::White),
                ("black", Color::Black),
            ] {
                if color_str.contains(name) {
                    return color;
                }
            }
        }
        Color::White
    }
}

/// Invert settings for LCD display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invert {
    None = 0,
    Top = 1,
    Bottom = 2,
    Both = 3,
}

/// 7-segment display font for Behringer displays
pub struct SegmentFont;

impl SegmentFont {
    const FONT_MAP: [(char, u8); 50] = [
        ('0', 0x3F), ('1', 0x06), ('2', 0x5B), ('3', 0x4F), ('4', 0x66),
        ('5', 0x6D), ('6', 0x7D), ('7', 0x07), ('8', 0x7F), ('9', 0x6F),
        ('A', 0x77), ('B', 0x7F), ('C', 0x39), ('D', 0x3F), ('E', 0x79),
        ('F', 0x71), ('G', 0x3D), ('H', 0x76), ('I', 0x06), ('J', 0x0E),
        ('K', 0x75), ('L', 0x38), ('M', 0x37), ('N', 0x37), ('O', 0x3F),
        ('P', 0x73), ('Q', 0x67), ('R', 0x77), ('S', 0x6D), ('T', 0x78),
        ('U', 0x3E), ('V', 0x3E), ('W', 0x3E), ('X', 0x49), ('Y', 0x6E),
        ('Z', 0x5B), (' ', 0x00), ('-', 0x40), ('.', 0x08), (':', 0x09),
        ('(', 0x39), (')', 0x0F),
    ];

    pub fn render_char(c: char) -> u8 {
        for &(ch, value) in &Self::FONT_MAP {
            if ch == c.to_ascii_uppercase() {
                return value;
            }
        }
        0
    }

    pub fn render_text(text: &str) -> Vec<u8> {
        text.chars()
            .take(12)
            .map(Self::render_char)
            .collect()
    }
}

/// Control types for MIDI surfaces
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlType {
    Fader,
    Button,
    Rotary,
    JogWheel,
}

impl ControlType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "fader" => Some(ControlType::Fader),
            "button" => Some(ControlType::Button),
            "rotary" => Some(ControlType::Rotary),
            "jogwheel" => Some(ControlType::JogWheel),
            _ => None,
        }
    }
}

/// Button styles
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonStyle {
    Toggle,
    Momentary,
}

impl ButtonStyle {
    pub fn from_str(s: &str) -> Self {
        match s {
            "momentary" => ButtonStyle::Momentary,
            _ => ButtonStyle::Toggle,
        }
    }
}

/// Light styles for buttons
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightStyle {
    State,
    AlwaysOn,
    Momentary,
    Off,
}

impl LightStyle {
    pub fn from_str(s: &str) -> Self {
        match s {
            "always_on" => LightStyle::AlwaysOn,
            "momentary" => LightStyle::Momentary,
            "false" => LightStyle::Off,
            _ => LightStyle::State,
        }
    }
}
