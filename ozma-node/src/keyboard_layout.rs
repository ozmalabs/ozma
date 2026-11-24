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
//! | `de` | `de-deadtilde` / `de-latin1` | QWERTZ, umlauts, `ß`, AltGr for `@ € {[]} \`, dead accents. |
//! | `fr` | `fr-latin9` (Alpine `kbd-bkeymaps`) | AZERTY, shifted digits, AltGr punctuation, French accents, dead accents. |
//!
//! The tables follow the Linux **console** keymaps shipped by `kbd` and
//! Alpine's `kbd-bkeymaps`, not the XKB desktop layouts. They intentionally do
//! not guess: a character that a layout only produces through a dead-key or
//! compose sequence is reported as [`CharOutcome::DeadKey`] (or
//! [`CharOutcome::Unsupported`] when it needs an IME), so the caller can return
//! a typed 400 before typing anything. Known dead-key and compose sequences are
//! resolved by [`resolve_dead_key_sequence`] and [`resolve_compose_sequence`].
//!
//! The French table targets Alpine's `fr-latin9` map (the dead-key variant).
//! Its direct accents and AltGr punctuation are the same as
//! `fr-latin9_nodeadkeys`; `` ` `` is mapped to the direct Shift+AltGr+`p`
//! position that both variants provide. With `fr-latin9`, the standard French
//! dead accents (`^`, `¨`, `´`, `` ` ``, `~`) are also available for
//! compositions. Layouts are not auto-detected.
//!
//! The German table targets Alpine's `de-deadtilde` map (the `de-latin1`
//! layout), where `~` is AltGr+`+` and is a dead key. Alpine's plain `de.bmap`
//! has a direct `~` at that position instead, so it cannot compose `~n`; load
//! `de-deadtilde.bmap` (or the matching `kbd` `de-latin1` map) for the German
//! dead-tilde sequences below.
//!
//! # Dead keys and compose
//!
//! A dead-key sequence is written with the dead accent mark followed by the
//! base character: `´e` → é, `` `e `` → è, `^e` → ê, `~n` → ñ, `¨u` → ü, and
//! `¸c` → ç. The node emits the dead-key press/release and then the base key,
//! so the guest's keymap performs the composition. A small compose set is also
//! accepted using ASCII prefixes: `'e` → é, `` `e `` → è, `^e` → ê, `~n` → ñ,
//! `"u` → ü, and `,c` → ç. Compose is available only on layouts that have dead
//! keys (`de` and `fr`); on `us`/`uk` those prefixes are ordinary characters.
//!
//! A prefix is consumed only when the pair is a documented composition.
//! Otherwise the prefix is treated as a normal character, so `^x` types `^x`
//! on `fr` (where `^` has a direct key) and is a typed 400 on `de` (where `^`
//! is dead-only). A dead key with no following composable character is never
//! silently dropped: it is a typed 400 before any event is submitted. IME,
//! CJK, emoji, and combining marks are still [`CharOutcome::Unsupported`]. A
//! real IME/clipboard path (for example pasting a composed string into the
//! guest) remains future work.

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
            Self::De => "de-deadtilde/de-latin1",
            Self::Fr => "fr-latin9",
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
        if let Some(dead) = DeadKey::from_mark(character) {
            if self.dead_key_chord(dead).is_some() {
                return CharOutcome::DeadKey;
            }
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

    /// The dead-key accents this layout's console keymap provides.
    pub fn dead_keys(self) -> &'static [(DeadKey, KeyChord)] {
        match self {
            Self::Us | Self::Uk => &[],
            Self::De => DE_DEAD_KEYS,
            Self::Fr => FR_DEAD_KEYS,
        }
    }

    /// The chord that presses one dead key on this layout.
    pub fn dead_key_chord(self, dead: DeadKey) -> Option<KeyChord> {
        self.dead_keys()
            .iter()
            .find(|(candidate, _)| *candidate == dead)
            .map(|(_, chord)| *chord)
    }

    /// Resolve `dead_char` + `base_char` on this layout.
    ///
    /// This is the two-argument form of [`resolve_dead_key_sequence`].
    pub fn resolve_dead_key_sequence(
        self,
        dead_char: char,
        base_char: char,
    ) -> Option<KeyChordSequence> {
        resolve_dead_key_sequence(self, dead_char, base_char)
    }

    /// Resolve an ASCII compose prefix + `base_char` on this layout.
    pub fn resolve_compose_sequence(
        self,
        prefix: char,
        base_char: char,
    ) -> Option<KeyChordSequence> {
        resolve_compose_sequence(self, prefix, base_char)
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

/// Two or more chords that together produce one character.
///
/// A dead-key composition is the dead-key chord followed by the base-key
/// chord. A compose sequence whose result has a direct key on the layout is a
/// single chord. The caller processes each chord as its own timed keystroke so
/// every press is held long enough for the guest to see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChordSequence {
    /// Chords in the order they must be sent.
    pub chords: Vec<KeyChord>,
}

impl KeyChordSequence {
    fn new(chords: Vec<KeyChord>) -> Self {
        Self { chords }
    }

    /// Flatten the sequence into balanced key events.
    pub fn events(&self) -> Vec<InputEvent> {
        self.chords
            .iter()
            .copied()
            .flat_map(KeyChord::events)
            .collect()
    }
}

/// A dead accent this module can compose with a following base character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeadKey {
    /// Grave accent, `` ` ``.
    Grave,
    /// Acute accent, `´`.
    Acute,
    /// Circumflex, `^`.
    Circumflex,
    /// Tilde, `~`.
    Tilde,
    /// Diaeresis, `¨`.
    Diaeresis,
    /// Cedilla, `¸`.
    Cedilla,
}

impl DeadKey {
    /// The spacing character used to write this dead key in a request.
    pub const fn mark(self) -> char {
        match self {
            Self::Grave => '`',
            Self::Acute => '´',
            Self::Circumflex => '^',
            Self::Tilde => '~',
            Self::Diaeresis => '¨',
            Self::Cedilla => '¸',
        }
    }

    /// Resolve a dead-key mark written in a request.
    pub fn from_mark(character: char) -> Option<Self> {
        match character {
            '`' => Some(Self::Grave),
            '´' => Some(Self::Acute),
            '^' => Some(Self::Circumflex),
            '~' => Some(Self::Tilde),
            '¨' => Some(Self::Diaeresis),
            '¸' => Some(Self::Cedilla),
            _ => None,
        }
    }

    /// Resolve one ASCII prefix of the documented compose set.
    pub fn from_compose_prefix(character: char) -> Option<Self> {
        match character {
            '`' => Some(Self::Grave),
            '\'' => Some(Self::Acute),
            '^' => Some(Self::Circumflex),
            '~' => Some(Self::Tilde),
            '"' => Some(Self::Diaeresis),
            ',' => Some(Self::Cedilla),
            _ => None,
        }
    }

    /// The composed character for `dead` + `base`, or `None` when this module
    /// does not document that pair.
    ///
    /// The table is deliberately small and Latin-1 only. It says whether the
    /// guest's console composition is expected to succeed; the node types the
    /// dead key and the base key, not this result character.
    pub fn compose(self, base: char) -> Option<char> {
        match self {
            Self::Grave => compose_from(&GRAVE_COMPOSITIONS, base),
            Self::Acute => compose_from(&ACUTE_COMPOSITIONS, base),
            Self::Circumflex => compose_from(&CIRCUMFLEX_COMPOSITIONS, base),
            Self::Tilde => compose_from(&TILDE_COMPOSITIONS, base),
            Self::Diaeresis => compose_from(&DIAERESIS_COMPOSITIONS, base),
            Self::Cedilla => compose_from(&CEDILLA_COMPOSITIONS, base),
        }
    }
}

fn compose_from(table: &[(char, char)], base: char) -> Option<char> {
    table
        .iter()
        .find(|(candidate, _)| *candidate == base)
        .map(|(_, composed)| *composed)
}

// Latin-1 compositions used by the Linux console dead-key tables. The node
// only uses these to decide whether a pair is a documented composition.
const GRAVE_COMPOSITIONS: &[(char, char)] = &[
    ('a', 'à'),
    ('A', 'À'),
    ('e', 'è'),
    ('E', 'È'),
    ('i', 'ì'),
    ('I', 'Ì'),
    ('o', 'ò'),
    ('O', 'Ò'),
    ('u', 'ù'),
    ('U', 'Ù'),
];
const ACUTE_COMPOSITIONS: &[(char, char)] = &[
    ('a', 'á'),
    ('A', 'Á'),
    ('e', 'é'),
    ('E', 'É'),
    ('i', 'í'),
    ('I', 'Í'),
    ('o', 'ó'),
    ('O', 'Ó'),
    ('u', 'ú'),
    ('U', 'Ú'),
    ('y', 'ý'),
    ('Y', 'Ý'),
];
const CIRCUMFLEX_COMPOSITIONS: &[(char, char)] = &[
    ('a', 'â'),
    ('A', 'Â'),
    ('e', 'ê'),
    ('E', 'Ê'),
    ('i', 'î'),
    ('I', 'Î'),
    ('o', 'ô'),
    ('O', 'Ô'),
    ('u', 'û'),
    ('U', 'Û'),
];
const TILDE_COMPOSITIONS: &[(char, char)] = &[
    ('a', 'ã'),
    ('A', 'Ã'),
    ('n', 'ñ'),
    ('N', 'Ñ'),
    ('o', 'õ'),
    ('O', 'Õ'),
];
const DIAERESIS_COMPOSITIONS: &[(char, char)] = &[
    ('a', 'ä'),
    ('A', 'Ä'),
    ('e', 'ë'),
    ('E', 'Ë'),
    ('i', 'ï'),
    ('I', 'Ï'),
    ('o', 'ö'),
    ('O', 'Ö'),
    ('u', 'ü'),
    ('U', 'Ü'),
    ('y', 'ÿ'),
    ('Y', 'Ÿ'),
];
const CEDILLA_COMPOSITIONS: &[(char, char)] = &[('c', 'ç'), ('C', 'Ç')];

/// Resolve `dead_char` followed by `base_char` into the two chords that make
/// the guest's console compose the accented character.
///
/// `dead_char` must be the layout's dead accent mark (for example `´`, `` ` ``,
/// `^`, `~`, `¨`, or `¸`), not an ASCII compose prefix. Returns `None` when the
/// layout has no such dead key, when this module does not document the pair, or
/// when the base character has no direct key on the layout.
pub fn resolve_dead_key_sequence(
    layout: KeyboardLayout,
    dead_char: char,
    base_char: char,
) -> Option<KeyChordSequence> {
    let dead = DeadKey::from_mark(dead_char)?;
    let dead_chord = layout.dead_key_chord(dead)?;
    dead.compose(base_char)?;
    let base_chord = match layout.chord(base_char) {
        CharOutcome::Direct(chord) => chord,
        _ => return None,
    };
    Some(KeyChordSequence::new(vec![dead_chord, base_chord]))
}

/// Resolve one ASCII compose prefix and base character.
///
/// On layouts with dead keys the prefix maps to its dead accent and the result
/// is the same as [`resolve_dead_key_sequence`]. When the layout has no dead
/// key for that accent but types the composed character directly (for example
/// `,c` → ç on `fr`), the sequence is that single direct chord.
pub fn resolve_compose_sequence(
    layout: KeyboardLayout,
    prefix: char,
    base_char: char,
) -> Option<KeyChordSequence> {
    if layout.dead_keys().is_empty() {
        return None;
    }
    let dead = DeadKey::from_compose_prefix(prefix)?;
    let composed = dead.compose(base_char)?;
    if let Some(dead_chord) = layout.dead_key_chord(dead) {
        let base_chord = match layout.chord(base_char) {
            CharOutcome::Direct(chord) => chord,
            _ => return None,
        };
        return Some(KeyChordSequence::new(vec![dead_chord, base_chord]));
    }
    match layout.chord(composed) {
        CharOutcome::Direct(chord) => Some(KeyChordSequence::new(vec![chord])),
        _ => None,
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

// Alpine `de-deadtilde` / `kbd` `de-latin1` dead-key positions. `^` is dead at
// HID 0x35; `´` (unshifted), `` ` `` (shifted), and `¸` (AltGr) share HID
// 0x2E; `¨` is AltGr at HID 0x2F; `~` is AltGr+`+` at HID 0x30.
const DE_DEAD_KEYS: &[(DeadKey, KeyChord)] = &[
    (
        DeadKey::Acute,
        KeyChord {
            usage: 0x2E,
            shift: false,
            altgr: false,
        },
    ),
    (
        DeadKey::Grave,
        KeyChord {
            usage: 0x2E,
            shift: true,
            altgr: false,
        },
    ),
    (
        DeadKey::Circumflex,
        KeyChord {
            usage: 0x35,
            shift: false,
            altgr: false,
        },
    ),
    (
        DeadKey::Tilde,
        KeyChord {
            usage: 0x30,
            shift: false,
            altgr: true,
        },
    ),
    (
        DeadKey::Diaeresis,
        KeyChord {
            usage: 0x2F,
            shift: false,
            altgr: true,
        },
    ),
    (
        DeadKey::Cedilla,
        KeyChord {
            usage: 0x2E,
            shift: false,
            altgr: true,
        },
    ),
];

// French AZERTY (`fr-latin9`). Digits are shifted; AltGr supplies the missing
// ASCII punctuation directly (`# {[]} \| @ ^ ~`), so those characters do not
// need a dead key even though the same map also defines French dead accents.
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
    // Alpine's `fr-latin9` and `fr-latin9_nodeadkeys` both place the direct
    // grave/backtick character at Shift+AltGr+`p` (HID 0x13). AltGr+`è` (HID
    // 0x24) is `dead_grave` on plain `fr-latin9`, so it must not be used for a
    // literal backtick.
    ('`', 0x13, true, true),
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

// Alpine `fr-latin9` dead-key positions. `^` is unshifted and `¨` shifted at
// HID 0x2F; `´`, `` ` ``, and `~` are AltGr at HID 0x1E, 0x24, and 0x2F.
// `fr-latin9` has no dead cedilla; `,c` composes to the direct `ç` key instead.
const FR_DEAD_KEYS: &[(DeadKey, KeyChord)] = &[
    (
        DeadKey::Acute,
        KeyChord {
            usage: 0x1E,
            shift: false,
            altgr: true,
        },
    ),
    (
        DeadKey::Grave,
        KeyChord {
            usage: 0x24,
            shift: false,
            altgr: true,
        },
    ),
    (
        DeadKey::Circumflex,
        KeyChord {
            usage: 0x2F,
            shift: false,
            altgr: false,
        },
    ),
    (
        DeadKey::Tilde,
        KeyChord {
            usage: 0x2F,
            shift: false,
            altgr: true,
        },
    ),
    (
        DeadKey::Diaeresis,
        KeyChord {
            usage: 0x2F,
            shift: true,
            altgr: false,
        },
    ),
];

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
        assert_direct(layout, '`', 0x13, true, true);
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

    fn chord(usage: u8, shift: bool, altgr: bool) -> KeyChord {
        KeyChord {
            usage,
            shift,
            altgr,
        }
    }

    fn assert_sequence(layout: KeyboardLayout, dead: char, base: char, expected: &[KeyChord]) {
        let sequence = resolve_dead_key_sequence(layout, dead, base)
            .unwrap_or_else(|| panic!("{layout} {dead:?}+{base:?} must resolve"));
        assert_eq!(sequence.chords, expected, "{layout} {dead:?}+{base:?}");
        assert_eq!(
            sequence.events(),
            expected
                .iter()
                .copied()
                .flat_map(KeyChord::events)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dead_key_chords_match_the_alpine_keymaps() {
        // de-deadtilde / de-latin1.
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Acute),
            Some(chord(0x2E, false, false))
        );
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Grave),
            Some(chord(0x2E, true, false))
        );
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Circumflex),
            Some(chord(0x35, false, false))
        );
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Tilde),
            Some(chord(0x30, false, true))
        );
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Diaeresis),
            Some(chord(0x2F, false, true))
        );
        assert_eq!(
            KeyboardLayout::De.dead_key_chord(DeadKey::Cedilla),
            Some(chord(0x2E, false, true))
        );

        // fr-latin9.
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Acute),
            Some(chord(0x1E, false, true))
        );
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Grave),
            Some(chord(0x24, false, true))
        );
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Circumflex),
            Some(chord(0x2F, false, false))
        );
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Tilde),
            Some(chord(0x2F, false, true))
        );
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Diaeresis),
            Some(chord(0x2F, true, false))
        );
        assert_eq!(
            KeyboardLayout::Fr.dead_key_chord(DeadKey::Cedilla),
            None,
            "fr-latin9 has no dead cedilla; ç is a direct key"
        );

        assert!(KeyboardLayout::Us.dead_keys().is_empty());
        assert!(KeyboardLayout::Uk.dead_keys().is_empty());
    }

    #[test]
    fn german_dead_key_sequences_use_dead_then_base_chords() {
        let layout = KeyboardLayout::De;
        assert_sequence(
            layout,
            '´',
            'e',
            &[chord(0x2E, false, false), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '´',
            'E',
            &[chord(0x2E, false, false), chord(0x08, true, false)],
        );
        assert_sequence(
            layout,
            '`',
            'e',
            &[chord(0x2E, true, false), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '^',
            'e',
            &[chord(0x35, false, false), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '~',
            'n',
            &[chord(0x30, false, true), chord(0x11, false, false)],
        );
        assert_sequence(
            layout,
            '¨',
            'u',
            &[chord(0x2F, false, true), chord(0x18, false, false)],
        );
        assert_sequence(
            layout,
            '¸',
            'c',
            &[chord(0x2E, false, true), chord(0x06, false, false)],
        );
    }

    #[test]
    fn french_dead_key_sequences_use_dead_then_base_chords() {
        let layout = KeyboardLayout::Fr;
        assert_sequence(
            layout,
            '´',
            'e',
            &[chord(0x1E, false, true), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '`',
            'e',
            &[chord(0x24, false, true), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '^',
            'e',
            &[chord(0x2F, false, false), chord(0x08, false, false)],
        );
        assert_sequence(
            layout,
            '~',
            'n',
            &[chord(0x2F, false, true), chord(0x11, false, false)],
        );
        assert_sequence(
            layout,
            '¨',
            'u',
            &[chord(0x2F, true, false), chord(0x18, false, false)],
        );
        assert_sequence(
            layout,
            '¨',
            'U',
            &[chord(0x2F, true, false), chord(0x18, true, false)],
        );
    }

    #[test]
    fn unknown_or_unavailable_dead_key_pairs_do_not_resolve() {
        for (layout, dead, base) in [
            (KeyboardLayout::De, '^', 'x'),
            (KeyboardLayout::De, '´', 'b'),
            (KeyboardLayout::De, '~', 'q'),
            (KeyboardLayout::Fr, '^', 'x'),
            (KeyboardLayout::Fr, '~', 'q'),
            (KeyboardLayout::Fr, '¸', 'c'),
            (KeyboardLayout::Us, '´', 'e'),
            (KeyboardLayout::Uk, '`', 'e'),
        ] {
            assert_eq!(
                resolve_dead_key_sequence(layout, dead, base),
                None,
                "{layout} {dead:?}+{base:?}"
            );
        }
        // A base that is not directly typeable or is itself a dead key.
        assert_eq!(
            resolve_dead_key_sequence(KeyboardLayout::De, '´', '日'),
            None
        );
        assert_eq!(
            resolve_dead_key_sequence(KeyboardLayout::De, '´', '^'),
            None
        );
    }

    #[test]
    fn compose_set_maps_documented_pairs_only() {
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::De, '\'', 'e')
                .unwrap()
                .chords,
            vec![chord(0x2E, false, false), chord(0x08, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::De, '"', 'u')
                .unwrap()
                .chords,
            vec![chord(0x2F, false, true), chord(0x18, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::De, ',', 'c')
                .unwrap()
                .chords,
            vec![chord(0x2E, false, true), chord(0x06, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Fr, '\'', 'e')
                .unwrap()
                .chords,
            vec![chord(0x1E, false, true), chord(0x08, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Fr, '"', 'u')
                .unwrap()
                .chords,
            vec![chord(0x2F, true, false), chord(0x18, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Fr, '~', 'n')
                .unwrap()
                .chords,
            vec![chord(0x2F, false, true), chord(0x11, false, false)]
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Fr, ',', 'c')
                .unwrap()
                .chords,
            vec![chord(0x26, false, false)],
            "fr-latin9 has no dead cedilla, so ,c uses the direct ç key"
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Fr, ',', 'C')
                .unwrap()
                .chords,
            vec![chord(0x26, true, true)]
        );

        // Unknown pairs and layouts without dead keys are not invented.
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::De, '\'', 'b'),
            None
        );
        assert_eq!(resolve_compose_sequence(KeyboardLayout::Fr, '~', 'q'), None);
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Us, '\'', 'e'),
            None
        );
        assert_eq!(
            resolve_compose_sequence(KeyboardLayout::Uk, '\'', 'e'),
            None
        );
    }

    #[test]
    fn dead_key_sequence_modifiers_are_balanced_and_released() {
        let sequence = resolve_dead_key_sequence(KeyboardLayout::De, '¨', 'U').unwrap();
        let events = sequence.events();
        // Dead diaeresis: AltGr down, ü down/up, AltGr up. Base U: Shift down,
        // u down/up, Shift up. Nothing is left held.
        assert_eq!(
            events,
            vec![
                key_event(0xE6, true),
                key_event(0x2F, true),
                key_event(0x2F, false),
                key_event(0xE6, false),
                key_event(0xE1, true),
                key_event(0x18, true),
                key_event(0x18, false),
                key_event(0xE1, false),
            ]
        );
        for event in events {
            if let InputEvent::Key { code, down, .. } = event {
                assert!(
                    !down || !(0xE0..=0xE7).contains(&code) || code == 0xE1 || code == 0xE6,
                    "unexpected modifier state"
                );
            }
        }
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
