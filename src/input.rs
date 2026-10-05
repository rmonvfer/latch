use gpui::{Keystroke, Modifiers};
use libghostty_vt::key::{self, Key};

/// A GPUI keystroke translated into the terms libghostty's key encoder uses.
pub struct TranslatedKey {
    pub key: Key,
    /// The character the physical key produces with no modifiers (US layout),
    /// or NUL for keys without one. The Kitty protocol identifies keys by it.
    pub unshifted: char,
    pub mods: key::Mods,
    pub consumed_mods: key::Mods,
    /// Printable text the platform produced for this keystroke, if any.
    pub text: Option<String>,
}

pub fn translate_keystroke(keystroke: &Keystroke) -> TranslatedKey {
    let (key, unshifted, implied_shift) = physical_key(&keystroke.key);

    let mut mods = to_mods(&keystroke.modifiers);
    if implied_shift {
        mods |= key::Mods::SHIFT;
    }

    let text = keystroke
        .key_char
        .as_ref()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .cloned();

    let mut consumed_mods = key::Mods::empty();
    if text.is_some() {
        if mods.contains(key::Mods::SHIFT) {
            consumed_mods |= key::Mods::SHIFT;
        }
        // Option produces composed characters on macOS (e.g. option-s → ß).
        if mods.contains(key::Mods::ALT) {
            consumed_mods |= key::Mods::ALT;
        }
    }

    TranslatedKey {
        key,
        unshifted,
        mods,
        consumed_mods,
        text,
    }
}

pub fn to_mods(modifiers: &Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    if modifiers.shift {
        mods |= key::Mods::SHIFT;
    }
    if modifiers.alt {
        mods |= key::Mods::ALT;
    }
    if modifiers.control {
        mods |= key::Mods::CTRL;
    }
    if modifiers.platform {
        mods |= key::Mods::SUPER;
    }
    mods
}

/// Map a GPUI key name to a physical key, its unshifted codepoint, and
/// whether the name itself implies shift (GPUI reports shift-1 as "!").
fn physical_key(name: &str) -> (Key, char, bool) {
    let named = match name {
        "enter" => Some(Key::Enter),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "delete" => Some(Key::Delete),
        "escape" => Some(Key::Escape),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "insert" => Some(Key::Insert),
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        "f6" => Some(Key::F6),
        "f7" => Some(Key::F7),
        "f8" => Some(Key::F8),
        "f9" => Some(Key::F9),
        "f10" => Some(Key::F10),
        "f11" => Some(Key::F11),
        "f12" => Some(Key::F12),
        _ => None,
    };
    if let Some(key) = named {
        return (key, '\0', false);
    }
    if name == "space" {
        return (Key::Space, ' ', false);
    }

    let mut chars = name.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return (Key::Unidentified, '\0', false);
    };

    let (base, implied_shift) = match ch {
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        '~' => ('`', true),
        ch if ch.is_ascii_uppercase() => (ch.to_ascii_lowercase(), true),
        ch => (ch, false),
    };

    let key = match base {
        'a' => Key::A,
        'b' => Key::B,
        'c' => Key::C,
        'd' => Key::D,
        'e' => Key::E,
        'f' => Key::F,
        'g' => Key::G,
        'h' => Key::H,
        'i' => Key::I,
        'j' => Key::J,
        'k' => Key::K,
        'l' => Key::L,
        'm' => Key::M,
        'n' => Key::N,
        'o' => Key::O,
        'p' => Key::P,
        'q' => Key::Q,
        'r' => Key::R,
        's' => Key::S,
        't' => Key::T,
        'u' => Key::U,
        'v' => Key::V,
        'w' => Key::W,
        'x' => Key::X,
        'y' => Key::Y,
        'z' => Key::Z,
        '0' => Key::Digit0,
        '1' => Key::Digit1,
        '2' => Key::Digit2,
        '3' => Key::Digit3,
        '4' => Key::Digit4,
        '5' => Key::Digit5,
        '6' => Key::Digit6,
        '7' => Key::Digit7,
        '8' => Key::Digit8,
        '9' => Key::Digit9,
        '-' => Key::Minus,
        '=' => Key::Equal,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        '\'' => Key::Quote,
        ',' => Key::Comma,
        '.' => Key::Period,
        '/' => Key::Slash,
        '`' => Key::Backquote,
        _ => return (Key::Unidentified, ch, false),
    };
    (key, base, implied_shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    #[test]
    fn shifted_symbol_maps_to_physical_digit() {
        let translated = translate_keystroke(&keystroke("!", Some("!"), Modifiers::none()));
        assert_eq!(translated.key, Key::Digit1);
        assert_eq!(translated.unshifted, '1');
        assert!(translated.mods.contains(key::Mods::SHIFT));
        assert!(translated.consumed_mods.contains(key::Mods::SHIFT));
        assert_eq!(translated.text.as_deref(), Some("!"));
    }

    #[test]
    fn control_letter_has_no_text() {
        let translated = translate_keystroke(&keystroke("c", None, Modifiers::control()));
        assert_eq!(translated.key, Key::C);
        assert!(translated.mods.contains(key::Mods::CTRL));
        assert!(translated.text.is_none());
        assert!(translated.consumed_mods.is_empty());
    }

    #[test]
    fn enter_drops_newline_text() {
        let translated = translate_keystroke(&keystroke("enter", Some("\n"), Modifiers::none()));
        assert_eq!(translated.key, Key::Enter);
        assert!(translated.text.is_none());
    }

    #[test]
    fn non_ascii_character_is_unidentified_with_text() {
        let translated = translate_keystroke(&keystroke("ö", Some("ö"), Modifiers::none()));
        assert_eq!(translated.key, Key::Unidentified);
        assert_eq!(translated.text.as_deref(), Some("ö"));
    }
}
