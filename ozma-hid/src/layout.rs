//! Keyboard layout maps: character → (enigo Key, shift required).
//!
//! Mirrors the layout dicts in `controller/paste_typing.py`.

use enigo::Key;

/// Supported keyboard layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Us,
    Uk,
    De,
}

impl Layout {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "uk" => Self::Uk,
            "de" => Self::De,
            _    => Self::Us,
        }
    }
}

/// A single keystroke: the enigo `Key` plus whether Shift must be held.
#[derive(Debug, Clone)]
pub struct KeyStroke {
    pub key: Key,
    pub shift: bool,
    /// AltGr (Right Alt) required — used in DE and other layouts.
    pub altgr: bool,
}

impl KeyStroke {
    fn plain(key: Key) -> Self { Self { key, shift: false, altgr: false } }
    fn shifted(key: Key) -> Self { Self { key, shift: true, altgr: false } }
    fn altgr(key: Key) -> Self { Self { key, shift: false, altgr: true } }
}

/// Return the keystroke needed to produce `ch` on the given layout.
/// Returns `None` for characters that have no mapping.
pub fn keystroke_for(ch: char, layout: Layout) -> Option<KeyStroke> {
    match layout {
        Layout::Us => us_keystroke(ch),
        Layout::Uk => uk_keystroke(ch),
        Layout::De => de_keystroke(ch),
    }
}

// ── US QWERTY ────────────────────────────────────────────────────────────────

fn us_keystroke(ch: char) -> Option<KeyStroke> {
    use Key::*;
    Some(match ch {
        // Lowercase letters
        'a'..='z' => KeyStroke::plain(Unicode(ch)),
        // Uppercase letters
        'A'..='Z' => KeyStroke::shifted(Unicode(ch.to_lowercase().next().unwrap())),
        // Digits
        '0'..='9' => KeyStroke::plain(Unicode(ch)),
        // Shift+digit symbols
        '!' => KeyStroke::shifted(Unicode('1')),
        '@' => KeyStroke::shifted(Unicode('2')),
        '#' => KeyStroke::shifted(Unicode('3')),
        '$' => KeyStroke::shifted(Unicode('4')),
        '%' => KeyStroke::shifted(Unicode('5')),
        '^' => KeyStroke::shifted(Unicode('6')),
        '&' => KeyStroke::shifted(Unicode('7')),
        '*' => KeyStroke::shifted(Unicode('8')),
        '(' => KeyStroke::shifted(Unicode('9')),
        ')' => KeyStroke::shifted(Unicode('0')),
        // Unshifted punctuation
        ' '  => KeyStroke::plain(Space),
        '\n' => KeyStroke::plain(Return),
        '\t' => KeyStroke::plain(Tab),
        '-'  => KeyStroke::plain(Unicode('-')),
        '='  => KeyStroke::plain(Unicode('=')),
        '['  => KeyStroke::plain(Unicode('[')),
        ']'  => KeyStroke::plain(Unicode(']')),
        '\\' => KeyStroke::plain(Unicode('\\')),
        ';'  => KeyStroke::plain(Unicode(';')),
        '\'' => KeyStroke::plain(Unicode('\'')),
        '`'  => KeyStroke::plain(Unicode('`')),
        ','  => KeyStroke::plain(Unicode(',')),
        '.'  => KeyStroke::plain(Unicode('.')),
        '/'  => KeyStroke::plain(Unicode('/')),
        // Shifted punctuation
        '_' => KeyStroke::shifted(Unicode('-')),
        '+' => KeyStroke::shifted(Unicode('=')),
        '{' => KeyStroke::shifted(Unicode('[')),
        '}' => KeyStroke::shifted(Unicode(']')),
        '|' => KeyStroke::shifted(Unicode('\\')),
        ':' => KeyStroke::shifted(Unicode(';')),
        '"' => KeyStroke::shifted(Unicode('\'')),
        '~' => KeyStroke::shifted(Unicode('`')),
        '<' => KeyStroke::shifted(Unicode(',')),
        '>' => KeyStroke::shifted(Unicode('.')),
        '?' => KeyStroke::shifted(Unicode('/')),
        _   => return None,
    })
}

// ── UK QWERTY ────────────────────────────────────────────────────────────────

fn uk_keystroke(ch: char) -> Option<KeyStroke> {
    // Start from US, override UK differences
    match ch {
        '"' => Some(KeyStroke::shifted(enigo::Key::Unicode('2'))),
        '@' => Some(KeyStroke::shifted(enigo::Key::Unicode('\''))),
        '£' => Some(KeyStroke::shifted(enigo::Key::Unicode('3'))),
        // '#' and '~' use the non-US hash key (0x32); enigo handles via Unicode
        '#' => Some(KeyStroke::plain(enigo::Key::Unicode('#'))),
        '~' => Some(KeyStroke::shifted(enigo::Key::Unicode('#'))),
        '\\' => Some(KeyStroke::plain(enigo::Key::Unicode('\\'))),
        '|'  => Some(KeyStroke::shifted(enigo::Key::Unicode('\\'))),
        _    => us_keystroke(ch),
    }
}

// ── German QWERTZ ────────────────────────────────────────────────────────────

fn de_keystroke(ch: char) -> Option<KeyStroke> {
    use enigo::Key::Unicode;
    match ch {
        // Z/Y swapped
        'z' => Some(KeyStroke::plain(Unicode('y'))),
        'y' => Some(KeyStroke::plain(Unicode('z'))),
        'Z' => Some(KeyStroke::shifted(Unicode('y'))),
        'Y' => Some(KeyStroke::shifted(Unicode('z'))),
        // AltGr combinations
        '@' => Some(KeyStroke::altgr(Unicode('q'))),
        '€' => Some(KeyStroke::altgr(Unicode('e'))),
        '{' => Some(KeyStroke::altgr(Unicode('7'))),
        '}' => Some(KeyStroke::altgr(Unicode('0'))),
        '[' => Some(KeyStroke::altgr(Unicode('8'))),
        ']' => Some(KeyStroke::altgr(Unicode('9'))),
        '\\' => Some(KeyStroke::altgr(Unicode('-'))),
        '|'  => Some(KeyStroke::altgr(Unicode('<'))),
        '~'  => Some(KeyStroke::altgr(Unicode('+'))),
        _    => us_keystroke(ch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_uppercase() {
        let ks = keystroke_for('A', Layout::Us).unwrap();
        assert!(ks.shift);
        assert!(!ks.altgr);
    }

    #[test]
    fn us_at_sign() {
        let ks = keystroke_for('@', Layout::Us).unwrap();
        assert!(ks.shift);
    }

    #[test]
    fn de_z_y_swap() {
        let z = keystroke_for('z', Layout::De).unwrap();
        assert_eq!(z.key, enigo::Key::Unicode('y'));
        let y = keystroke_for('y', Layout::De).unwrap();
        assert_eq!(y.key, enigo::Key::Unicode('z'));
    }

    #[test]
    fn de_euro_altgr() {
        let ks = keystroke_for('€', Layout::De).unwrap();
        assert!(ks.altgr);
    }

    #[test]
    fn unknown_char_returns_none() {
        assert!(keystroke_for('\x00', Layout::Us).is_none());
    }
}
