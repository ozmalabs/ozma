// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Keyboard-layout aware text → USB HID mapping for `POST /input/type`.
//!
//! The node injects USB HID usage ids; the guest's own keymap decides which
//! character each physical key produces. A US usage table therefore types the
//! wrong characters on a UK, German, or French guest. This module maps a
//! character to the physical key *and* the Shift/AltGr state that produces it
//! for a selected Linux console layout.
//!
//! # Supported layouts
//!
//! | Layout | Console keymap | Notes |
//! |---|---|---|
//! | `us` | `us` / `defkeymap` | Every printable US-ASCII character. |
//! | `uk` | `uk` (Alpine `gb`) | ISO UK: `"` on Shift+2, `£`, `#~` on HID `0x32`, `\` on HID `0x64`. |
//! | `de` | `de-latin1` / `de` | QWERTZ, umlauts, `ß`, AltGr for `@ € {[]} \`. |
//! | `fr` | `fr-latin9_nodeadkeys` (Alpine `kbd-bkeymaps`) | AZERTY, shifted digits, AltGr for `# {[]} \| @ ^ ~ \``, French accents. |
//!
//! The tables follow the Linux **console** keymaps shipped by `kbd` and
//! Alpine's `kbd-bkeymaps`, not the XKB desktop layouts. They intentionally do
//! not guess: a character that a layout only produces through a dead-key or
//! compose sequence is reported as [`CharOutcome::DeadKey`] (or
//! [`CharOutcome::Unsupported`] when it needs an IME), so the caller can return
//! a typed 400 before typing anything.
//!
//! The French table follows the **nodeadkeys** `fr-latin9` keymap shipped by
//! Alpine's `kbd-bkeymaps` and XKB, where `` ` `` is a direct AltGr+7 and
//! `^`/`~` are direct AltGr+9/AltGr+2. Plain `fr-latin9` defines AltGr+7 as
//! `dead_grave`, which would consume the following character; install
//! `fr-latin9_nodeadkeys` on the guest, or send raw `/input/key` events for
//! `` ` `` when using the dead-key variant. Layouts are not auto-detected.
//!
//! # Dead keys
//!
//! Dead-key composition is not implemented. On `de`, `^`, `~`, `` ` ``, and `´`
//! are dead keys in the console keymap, so they are rejected rather than
//! emitted (emitting the dead key alone would swallow the next character). On
//! `fr`, the printable ASCII forms `^`, `~`, and `` ` `` are available directly
//! through AltGr in `fr-latin9` and are therefore supported.

use std::fmt;
use std::str::FromStr;

use clap::ValueEnum;

use ozma_protocol::input::{InputEvent, KeyModifiers};

/// HID usage for the left Shift modifier.
pub const USAGE_LEFT_SHIFT: u8 = 0xE1;
/// HID usage for the right Alt (AltGr) modifier.
pub const USAGE_RIGHT_ALT: u8 = 0xE6;

/// Build one key event with no client modifier bits.
pub(crate) fn key_event(code: u32, down: bool) -> InputEvent {
    InputEvent::Key {
        code,
        down,
        modifiers: KeyModifiers(0),
    }
}

/// A keyboard layout whose physical key positions and dead keys are known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, ValueEnum)]
pub enum KeyboardLayout {
    /// US ANSI QWERTY.
    #[default]
    #[value(alias = "en-us")]
    Us,
    /// UK ISO QWERTY.
    #[value(alias = "gb", alias = "en-gb")]
    Uk,
    /// German QWERTZ (`de-latin1`/`de` console keymap).
    #[value(alias = "de-de")]
    De,
    /// French AZERTY (`fr-latin9` console keymap).
    #[value(alias = "fr-fr", alias = "azerty")]
    Fr,
}

impl KeyboardLayout {
    /// Every layout name accepted by `--keyboard-layout`.
    pub const SUPPORTED: &'static [&'static str] = &["us", "uk", "de", "fr"];

    /// Stable lowercase name used by the CLI and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Uk => "uk",
            Self::De => "de",
            Self::Fr => "fr",
        }
    }

    /// The Linux console keymap this layout's table follows.
    pub const fn console_keymap(self) -> &'static str {
        match self {
            Self::Us => "us/defkeymap",
            Self::Uk => "uk/gb",
            Self::De => "de-latin1/de",
            Self::Fr => "fr-latin9_nodeadkeys",
        }
    }

    /// Map one character to the chord that types it on this layout.
    ///
    /// Letters are resolved from a physical-position table; digits,
    /// punctuation, whitespace, and layout-specific accented characters come
    /// from the explicit tables below.
    pub fn chord(self, character: char) -> CharOutcome {
        if let Some(chord) = self.special_chord(character) {
            return CharOutcome::Direct(chord);
        }
        if character.is_ascii_alphabetic() {
            return CharOutcome::Direct(self.letter_chord(character));
        }
        if self.dead_chars().contains(&character) {
            return CharOutcome::DeadKey;
        }
        CharOutcome::Unsupported
    }

    fn letter_chord(self, character: char) -> KeyChord {
        let shift = character.is_ascii_uppercase();
        let lower = character.to_ascii_lowercase();
        let usage = match self {
            Self::Us | Self::Uk => 0x04 + (lower as u8 - b'a'),
            // German QWERTZ swaps Y and Z relative to US positions.
            Self::De => match lower {
                'z' => 0x1C,
                'y' => 0x1D,
                _ => 0x04 + (lower as u8 - b'a'),
            },
            // French AZERTY moves A/Q, Z/W, and M to different physical keys.
            Self::Fr => match lower {
                'a' => 0x14,
                'q' => 0x04,
                'z' => 0x1A,
                'w' => 0x1D,
                'm' => 0x33,
                _ => 0x04 + (lower as u8 - b'a'),
            },
        };
        KeyChord {
            usage,
            shift,
            altgr: false,
        }
    }

    fn special_chord(self, character: char) -> Option<KeyChord> {
        let table = match self {
            Self::Us => US_SPECIAL,
            Self::Uk => UK_SPECIAL,
            Self::De => DE_SPECIAL,
            Self::Fr => FR_SPECIAL,
        };
        table
            .iter()
            .find(|&&(candidate, _, _, _)| candidate == character)
            .map(|&(_, usage, shift, altgr)| KeyChord {
                usage,
                shift,
                altgr,
            })
    }

    fn dead_chars(self) -> &'static [char] {
        match self {
            Self::Us | Self::Uk => &[],
            Self::De => DE_DEAD,
            Self::Fr => FR_DEAD,
        }
    }
}

impl fmt::Display for KeyboardLayout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for KeyboardLayout {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "us" | "en-us" => Ok(Self::Us),
            "uk" | "gb" | "en-gb" => Ok(Self::Uk),
            "de" | "de-de" => Ok(Self::De),
            "fr" | "fr-fr" | "azerty" => Ok(Self::Fr),
            other => Err(format!(
                "unknown keyboard layout {other:?}; supported layouts: {}",
                Self::SUPPORTED.join(", ")
            )),
        }
    }
}

/// One physical key press plus the modifiers needed to produce a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    /// HID keyboard-page usage id of the character key.
    pub usage: u8,
    /// Whether left Shift must be held.
    pub shift: bool,
    /// Whether right Alt (AltGr) must be held.
    pub altgr: bool,
}

impl KeyChord {
    /// Expand the chord into a balanced key-down/key-up event sequence.
    ///
    /// Order is shift-down, AltGr-down, key-down, key-up, AltGr-up,
    /// shift-up. Modifiers are released in reverse order, so no modifier can
    /// remain held if a caller submits the whole sequence.
    pub fn events(self) -> Vec<InputEvent> {
        let modifiers = [
            (USAGE_LEFT_SHIFT, self.shift),
            (USAGE_RIGHT_ALT, self.altgr),
        ];
        let pressed = modifiers.iter().filter(|(_, down)| *down).count();
        let mut events = Vec::with_capacity(2 + pressed * 2);

        for &(usage, down) in &modifiers {
            if down {
                events.push(key_event(u32::from(usage), true));
            }
        }
        events.push(key_event(u32::from(self.usage), true));
        events.push(key_event(u32::from(self.usage), false));
        for &(usage, down) in modifiers.iter().rev() {
            if down {
                events.push(key_event(u32::from(usage), false));
            }
        }
        events
    }
}

/// Result of mapping a character on a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharOutcome {
    /// The layout types this character directly.
    Direct(KeyChord),
    /// The character is only available through a dead-key sequence.
    DeadKey,
    /// The layout has no direct key for the character (or it needs an IME).
    Unsupported,
}

/// `(character, HID usage, shift, AltGr)`.
type LayoutEntry = (char, u8, bool, bool);

// Whitespace, digits, and punctuation for the US layout. Letters are resolved
// by `KeyboardLayout::letter_chord`, so they must not appear here.
const US_SPECIAL: &[LayoutEntry] = &[
    ('\n', 0x28, false, false), // Enter
    ('\r', 0x28, false, false), // Enter
    ('\t', 0x2B, false, false), // Tab
    (' ', 0x2C, false, false),
    ('1', 0x1E, false, false),
    ('2', 0x1F, false, false),
    ('3', 0x20, false, false),
    ('4', 0x21, false, false),
    ('5', 0x22, false, false),
    ('6', 0x23, false, false),
    ('7', 0x24, false, false),
    ('8', 0x25, false, false),
    ('9', 0x26, false, false),
    ('0', 0x27, false, false),
    ('!', 0x1E, true, false),
    ('@', 0x1F, true, false),
    ('#', 0x20, true, false),
    ('$', 0x21, true, false),
    ('%', 0x22, true, false),
    ('^', 0x23, true, false),
    ('&', 0x24, true, false),
    ('*', 0x25, true, false),
    ('(', 0x26, true, false),
    (')', 0x27, true, false),
    ('-', 0x2D, false, false),
    ('_', 0x2D, true, false),
    ('=', 0x2E, false, false),
    ('+', 0x2E, true, false),
    ('[', 0x2F, false, false),
    ('{', 0x2F, true, false),
    (']', 0x30, false, false),
    ('}', 0x30, true, false),
    ('\\', 0x31, false, false),
    ('|', 0x31, true, false),
    (';', 0x33, false, false),
    (':', 0x33, true, false),
    ('\'', 0x34, false, false),
    ('"', 0x34, true, false),
    ('`', 0x35, false, false),
    ('~', 0x35, true, false),
    (',', 0x36, false, false),
    ('<', 0x36, true, false),
    ('.', 0x37, false, false),
    ('>', 0x37, true, false),
    ('/', 0x38, false, false),
    ('?', 0x38, true, false),
];

// UK ISO layout (`uk.map`). The `#`/`~` key is HID 0x32 and `\`/`|` is HID
// 0x64; `"` is Shift+2 and `@` is Shift+' rather than the US positions.
const UK_SPECIAL: &[LayoutEntry] = &[
    ('\n', 0x28, false, false),
    ('\r', 0x28, false, false),
    ('\t', 0x2B, false, false),
    (' ', 0x2C, false, false),
    ('1', 0x1E, false, false),
    ('2', 0x1F, false, false),
    ('3', 0x20, false, false),
    ('4', 0x21, false, false),
    ('5', 0x22, false, false),
    ('6', 0x23, false, false),
    ('7', 0x24, false, false),
    ('8', 0x25, false, false),
    ('9', 0x26, false, false),
    ('0', 0x27, false, false),
    ('!', 0x1E, true, false),
    ('"', 0x1F, true, false),
    ('£', 0x20, true, false),
    ('$', 0x21, true, false),
    ('%', 0x22, true, false),
    ('^', 0x23, true, false),
    ('&', 0x24, true, false),
    ('*', 0x25, true, false),
    ('(', 0x26, true, false),
    (')', 0x27, true, false),
    ('-', 0x2D, false, false),
    ('_', 0x2D, true, false),
    ('=', 0x2E, false, false),
    ('+', 0x2E, true, false),
    ('[', 0x2F, false, false),
    ('{', 0x2F, true, false),
    (']', 0x30, false, false),
    ('}', 0x30, true, false),
    (';', 0x33, false, false),
    (':', 0x33, true, false),
    ('\'', 0x34, false, false),
    ('@', 0x34, true, false),
    ('`', 0x35, false, false),
    ('¬', 0x35, true, false),
    ('#', 0x32, false, false),
    ('~', 0x32, true, false),
    ('\\', 0x64, false, false),
    ('|', 0x64, true, false),
    (',', 0x36, false, false),
    ('<', 0x36, true, false),
    ('.', 0x37, false, false),
    ('>', 0x37, true, false),
    ('/', 0x38, false, false),
    ('?', 0x38, true, false),
];

// German QWERTZ (`de-latin1`/`de.map`).
const DE_SPECIAL: &[LayoutEntry] = &[
    ('\n', 0x28, false, false),
    ('\r', 0x28, false, false),
    ('\t', 0x2B, false, false),
    (' ', 0x2C, false, false),
    ('1', 0x1E, false, false),
    ('2', 0x1F, false, false),
    ('3', 0x20, false, false),
    ('4', 0x21, false, false),
    ('5', 0x22, false, false),
    ('6', 0x23, false, false),
    ('7', 0x24, false, false),
    ('8', 0x25, false, false),
    ('9', 0x26, false, false),
    ('0', 0x27, false, false),
    ('!', 0x1E, true, false),
    ('"', 0x1F, true, false),
    ('§', 0x20, true, false),
    ('$', 0x21, true, false),
    ('%', 0x22, true, false),
    ('&', 0x23, true, false),
    ('/', 0x24, true, false),
    ('(', 0x25, true, false),
    (')', 0x26, true, false),
    ('=', 0x27, true, false),
    ('ß', 0x2D, false, false),
    ('?', 0x2D, true, false),
    ('+', 0x30, false, false),
    ('*', 0x30, true, false),
    ('#', 0x32, false, false),
    ('\'', 0x32, true, false),
    ('ü', 0x2F, false, false),
    ('Ü', 0x2F, true, false),
    ('ö', 0x33, false, false),
    ('Ö', 0x33, true, false),
    ('ä', 0x34, false, false),
    ('Ä', 0x34, true, false),
    ('°', 0x35, true, false),
    ('<', 0x64, false, false),
    ('>', 0x64, true, false),
    ('|', 0x64, false, true),
    (',', 0x36, false, false),
    (';', 0x36, true, false),
    ('.', 0x37, false, false),
    (':', 0x37, true, false),
    ('-', 0x38, false, false),
    ('_', 0x38, true, false),
    ('@', 0x14, false, true),
    ('€', 0x08, false, true),
    ('¢', 0x06, false, true),
    ('{', 0x24, false, true),
    ('[', 0x25, false, true),
    (']', 0x26, false, true),
    ('}', 0x27, false, true),
    ('\\', 0x2D, false, true),
    ('µ', 0x10, false, true),
];

// `de-latin1`/`de` define these as dead keys. `^` is dead at HID 0x35, `´`
// and `` ` `` at 0x2E, and `~` is AltGr+`+` (HID 0x30) dead_tilde.
const DE_DEAD: &[char] = &['^', '~', '`', '´'];

// French AZERTY (`fr-latin9_nodeadkeys`). Digits are shifted; AltGr supplies
// the missing ASCII punctuation directly (`# {[]} \| @ ^ ~ \``), so no dead key
// is needed.
const FR_SPECIAL: &[LayoutEntry] = &[
    ('\n', 0x28, false, false),
    ('\r', 0x28, false, false),
    ('\t', 0x2B, false, false),
    (' ', 0x2C, false, false),
    ('&', 0x1E, false, false),
    ('é', 0x1F, false, false),
    ('"', 0x20, false, false),
    ('\'', 0x21, false, false),
    ('(', 0x22, false, false),
    ('-', 0x23, false, false),
    ('è', 0x24, false, false),
    ('_', 0x25, false, false),
    ('ç', 0x26, false, false),
    ('à', 0x27, false, false),
    (')', 0x2D, false, false),
    ('=', 0x2E, false, false),
    ('²', 0x35, false, false),
    ('$', 0x30, false, false),
    ('£', 0x30, true, false),
    ('ù', 0x34, false, false),
    ('*', 0x32, false, false),
    ('µ', 0x32, true, false),
    ('1', 0x1E, true, false),
    ('2', 0x1F, true, false),
    ('3', 0x20, true, false),
    ('4', 0x21, true, false),
    ('5', 0x22, true, false),
    ('6', 0x23, true, false),
    ('7', 0x24, true, false),
    ('8', 0x25, true, false),
    ('9', 0x26, true, false),
    ('0', 0x27, true, false),
    ('°', 0x2D, true, false),
    ('+', 0x2E, true, false),
    ('%', 0x34, true, false),
    ('?', 0x10, true, false),
    ('.', 0x36, true, false),
    ('/', 0x37, true, false),
    ('§', 0x38, true, false),
    (',', 0x10, false, false),
    (';', 0x36, false, false),
    (':', 0x37, false, false),
    ('!', 0x38, false, false),
    ('<', 0x64, false, false),
    ('>', 0x64, true, false),
    ('~', 0x1F, false, true),
    ('#', 0x20, false, true),
    ('{', 0x21, false, true),
    ('[', 0x22, false, true),
    ('|', 0x23, false, true),
    ('\\', 0x25, false, true),
    ('^', 0x26, false, true),
    ('@', 0x27, false, true),
    (']', 0x2D, false, true),
    ('}', 0x2E, false, true),
    // Alpine's `fr-latin9`/XKB places grave on AltGr+7; a Shift+AltGr+`^`
    // sequence is dead_abovering/apostrophe there, not a backtick.
    ('`', 0x24, false, true),
    ('¤', 0x08, false, true),
    ('É', 0x1F, true, true),
    ('È', 0x24, true, true),
    ('À', 0x27, true, true),
    ('Ç', 0x26, true, true),
    ('Ù', 0x34, true, true),
    ('â', 0x14, false, true),
    ('ê', 0x30, true, true),
    ('î', 0x0C, false, true),
    ('ô', 0x12, false, true),
    ('û', 0x18, false, true),
];

// Accents available only as dead keys in `fr-latin9_nodeadkeys`.
const FR_DEAD: &[char] = &['¨', '´', '¸'];

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_direct(layout: KeyboardLayout, character: char, usage: u8, shift: bool, altgr: bool) {
        assert_eq!(
            layout.chord(character),
            CharOutcome::Direct(KeyChord {
                usage,
                shift,
                altgr,
            }),
            "{layout} {character:?}"
        );
    }

    #[test]
    fn supported_layouts_parse_display_and_round_trip() {
        assert_eq!(KeyboardLayout::SUPPORTED, &["us", "uk", "de", "fr"]);
        for name in KeyboardLayout::SUPPORTED {
            let layout: KeyboardLayout = name.parse().unwrap();
            assert_eq!(layout.as_str(), *name);
            assert_eq!(layout.to_string(), *name);
        }
        assert_eq!("gb".parse::<KeyboardLayout>().unwrap(), KeyboardLayout::Uk);
        assert_eq!(
            "en-gb".parse::<KeyboardLayout>().unwrap(),
            KeyboardLayout::Uk
        );
        assert_eq!(
            "azerty".parse::<KeyboardLayout>().unwrap(),
            KeyboardLayout::Fr
        );
        assert!("dvorak".parse::<KeyboardLayout>().is_err());
        assert_eq!(KeyboardLayout::default(), KeyboardLayout::Us);
    }

    #[test]
    fn layout_tables_have_no_duplicate_characters() {
        for layout in [
            KeyboardLayout::Us,
            KeyboardLayout::Uk,
            KeyboardLayout::De,
            KeyboardLayout::Fr,
        ] {
            let table = match layout {
                KeyboardLayout::Us => US_SPECIAL,
                KeyboardLayout::Uk => UK_SPECIAL,
                KeyboardLayout::De => DE_SPECIAL,
                KeyboardLayout::Fr => FR_SPECIAL,
            };
            let mut seen = std::collections::BTreeSet::new();
            for &(character, usage, _, _) in table {
                assert!(
                    !character.is_ascii_alphabetic(),
                    "{layout}: letter {character:?} belongs in letter_chord"
                );
                assert!(seen.insert(character), "{layout}: duplicate {character:?}");
                assert!(
                    (0x04..=0x38).contains(&usage) || usage == 0x64,
                    "{layout}: {character:?} has unexpected HID usage {usage:#04x}"
                );
            }
        }
    }

    #[test]
    fn every_layout_maps_all_ascii_letters_including_shift() {
        for layout in [
            KeyboardLayout::Us,
            KeyboardLayout::Uk,
            KeyboardLayout::De,
            KeyboardLayout::Fr,
        ] {
            for lower in 'a'..='z' {
                let upper = lower.to_ascii_uppercase();
                match layout.chord(lower) {
                    CharOutcome::Direct(chord) => {
                        assert!(!chord.shift, "{layout} {lower:?} must not need Shift");
                        assert!(!chord.altgr, "{layout} {lower:?} must not need AltGr");
                        assert_direct(layout, upper, chord.usage, true, false);
                    }
                    other => panic!("{layout} {lower:?}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn us_layout_covers_every_printable_ascii_character() {
        for byte in 0x20u8..=0x7E {
            let character = char::from(byte);
            assert!(
                matches!(KeyboardLayout::Us.chord(character), CharOutcome::Direct(_)),
                "US missing {character:?}"
            );
        }
        assert_direct(KeyboardLayout::Us, 'a', 0x04, false, false);
        assert_direct(KeyboardLayout::Us, 'A', 0x04, true, false);
        assert_direct(KeyboardLayout::Us, '!', 0x1E, true, false);
        assert_direct(KeyboardLayout::Us, '?', 0x38, true, false);
        assert_direct(KeyboardLayout::Us, '\\', 0x31, false, false);
        assert_direct(KeyboardLayout::Us, '`', 0x35, false, false);
        assert_direct(KeyboardLayout::Us, '\n', 0x28, false, false);
        assert_direct(KeyboardLayout::Us, '\t', 0x2B, false, false);
    }

    #[test]
    fn uk_layout_matches_iso_samples() {
        let layout = KeyboardLayout::Uk;
        assert_direct(layout, 'a', 0x04, false, false);
        assert_direct(layout, 'A', 0x04, true, false);
        assert_direct(layout, '2', 0x1F, false, false);
        assert_direct(layout, '"', 0x1F, true, false);
        assert_direct(layout, '£', 0x20, true, false);
        assert_direct(layout, '@', 0x34, true, false);
        assert_direct(layout, '\'', 0x34, false, false);
        assert_direct(layout, '#', 0x32, false, false);
        assert_direct(layout, '~', 0x32, true, false);
        assert_direct(layout, '\\', 0x64, false, false);
        assert_direct(layout, '|', 0x64, true, false);
        assert_direct(layout, '`', 0x35, false, false);
        assert_direct(layout, '¬', 0x35, true, false);
    }

    #[test]
    fn de_layout_matches_qwertz_samples() {
        let layout = KeyboardLayout::De;
        assert_direct(layout, 'z', 0x1C, false, false);
        assert_direct(layout, 'Z', 0x1C, true, false);
        assert_direct(layout, 'y', 0x1D, false, false);
        assert_direct(layout, 'Y', 0x1D, true, false);
        assert_direct(layout, 'ü', 0x2F, false, false);
        assert_direct(layout, 'Ü', 0x2F, true, false);
        assert_direct(layout, 'ö', 0x33, false, false);
        assert_direct(layout, 'ä', 0x34, false, false);
        assert_direct(layout, 'ß', 0x2D, false, false);
        assert_direct(layout, '?', 0x2D, true, false);
        assert_direct(layout, '#', 0x32, false, false);
        assert_direct(layout, '\'', 0x32, true, false);
        assert_direct(layout, '@', 0x14, false, true);
        assert_direct(layout, '€', 0x08, false, true);
        assert_direct(layout, '{', 0x24, false, true);
        assert_direct(layout, '[', 0x25, false, true);
        assert_direct(layout, ']', 0x26, false, true);
        assert_direct(layout, '}', 0x27, false, true);
        assert_direct(layout, '\\', 0x2D, false, true);
        assert_direct(layout, '<', 0x64, false, false);
        assert_direct(layout, '>', 0x64, true, false);
        assert_direct(layout, '|', 0x64, false, true);
        assert_direct(layout, '°', 0x35, true, false);
    }

    #[test]
    fn fr_layout_matches_azerty_samples() {
        let layout = KeyboardLayout::Fr;
        assert_direct(layout, 'a', 0x14, false, false);
        assert_direct(layout, 'q', 0x04, false, false);
        assert_direct(layout, 'z', 0x1A, false, false);
        assert_direct(layout, 'w', 0x1D, false, false);
        assert_direct(layout, 'm', 0x33, false, false);
        assert_direct(layout, '1', 0x1E, true, false);
        assert_direct(layout, '&', 0x1E, false, false);
        assert_direct(layout, 'é', 0x1F, false, false);
        assert_direct(layout, 'è', 0x24, false, false);
        assert_direct(layout, 'ç', 0x26, false, false);
        assert_direct(layout, 'à', 0x27, false, false);
        assert_direct(layout, 'ù', 0x34, false, false);
        assert_direct(layout, ',', 0x10, false, false);
        assert_direct(layout, ';', 0x36, false, false);
        assert_direct(layout, ':', 0x37, false, false);
        assert_direct(layout, '!', 0x38, false, false);
        assert_direct(layout, '<', 0x64, false, false);
        assert_direct(layout, '>', 0x64, true, false);
        assert_direct(layout, '#', 0x20, false, true);
        assert_direct(layout, '{', 0x21, false, true);
        assert_direct(layout, '[', 0x22, false, true);
        assert_direct(layout, '|', 0x23, false, true);
        assert_direct(layout, '\\', 0x25, false, true);
        assert_direct(layout, '^', 0x26, false, true);
        assert_direct(layout, '@', 0x27, false, true);
        assert_direct(layout, ']', 0x2D, false, true);
        assert_direct(layout, '}', 0x2E, false, true);
        assert_direct(layout, '~', 0x1F, false, true);
        assert_direct(layout, '`', 0x24, false, true);
        assert_direct(layout, 'É', 0x1F, true, true);
    }

    #[test]
    fn dead_key_only_characters_are_reported_not_typed() {
        for character in ['^', '~', '`'] {
            assert_eq!(
                KeyboardLayout::De.chord(character),
                CharOutcome::DeadKey,
                "German {character:?} is a dead key"
            );
        }
        assert_eq!(KeyboardLayout::De.chord('´'), CharOutcome::DeadKey);
        assert_eq!(KeyboardLayout::Fr.chord('¨'), CharOutcome::DeadKey);
        assert_eq!(KeyboardLayout::Fr.chord('´'), CharOutcome::DeadKey);

        // The same ASCII characters are direct on layouts that provide them.
        assert!(matches!(
            KeyboardLayout::Us.chord('^'),
            CharOutcome::Direct(_)
        ));
        assert!(matches!(
            KeyboardLayout::Uk.chord('`'),
            CharOutcome::Direct(_)
        ));
        assert!(matches!(
            KeyboardLayout::Fr.chord('^'),
            CharOutcome::Direct(_)
        ));
    }

    #[test]
    fn unsupported_and_ime_characters_are_not_guessed() {
        for character in ['é', '日', '本', '🙂', '\u{0301}', '\u{200b}'] {
            assert_eq!(
                KeyboardLayout::Us.chord(character),
                CharOutcome::Unsupported,
                "US {character:?}"
            );
        }
        assert_eq!(KeyboardLayout::Uk.chord('é'), CharOutcome::Unsupported);
        assert_eq!(KeyboardLayout::De.chord('é'), CharOutcome::Unsupported);
        assert_eq!(KeyboardLayout::De.chord('日'), CharOutcome::Unsupported);
        assert_eq!(KeyboardLayout::Fr.chord('日'), CharOutcome::Unsupported);
    }

    #[test]
    fn chords_expand_to_balanced_events_and_release_modifiers_in_reverse() {
        let plain = KeyChord {
            usage: 0x04,
            shift: false,
            altgr: false,
        }
        .events();
        assert_eq!(plain, vec![key_event(0x04, true), key_event(0x04, false)]);

        let shifted = KeyChord {
            usage: 0x1E,
            shift: true,
            altgr: false,
        }
        .events();
        assert_eq!(
            shifted,
            vec![
                key_event(0xE1, true),
                key_event(0x1E, true),
                key_event(0x1E, false),
                key_event(0xE1, false),
            ]
        );

        let altgr = KeyChord {
            usage: 0x14,
            shift: false,
            altgr: true,
        }
        .events();
        assert_eq!(
            altgr,
            vec![
                key_event(0xE6, true),
                key_event(0x14, true),
                key_event(0x14, false),
                key_event(0xE6, false),
            ]
        );

        let both = KeyChord {
            usage: 0x2F,
            shift: true,
            altgr: true,
        }
        .events();
        assert_eq!(
            both,
            vec![
                key_event(0xE1, true),
                key_event(0xE6, true),
                key_event(0x2F, true),
                key_event(0x2F, false),
                key_event(0xE6, false),
                key_event(0xE1, false),
            ]
        );
    }
}
