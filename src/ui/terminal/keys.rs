//! Keystrokes to the bytes a terminal program expects (xterm conventions).
//! Option/Alt acts as Meta, which is what agent CLIs expect for word motions
//! and Option+Enter newlines.

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

pub fn to_bytes(ks: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    if m.platform {
        return None; // ⌘ shortcuts belong to the app
    }
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let cursor = |letter: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 { format!("\x1b[{n};{param}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
    };
    let meta = |bytes: &[u8]| -> Vec<u8> {
        if m.alt { [b"\x1b".as_slice(), bytes].concat() } else { bytes.to_vec() }
    };

    let named = match ks.key.as_str() {
        // Shift/Option+Enter: a newline in Claude Code, Codex and friends.
        "enter" => Some(if m.shift || m.alt { b"\x1b\r".to_vec() } else { b"\r".to_vec() }),
        "backspace" => Some(if m.control { b"\x08".to_vec() } else { meta(b"\x7f") }),
        "tab" => Some(if m.shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() }),
        "escape" => Some(meta(b"\x1b")),
        "space" if m.control => Some(b"\x00".to_vec()),
        "space" => Some(meta(b" ")),
        "up" => Some(cursor('A')),
        "down" => Some(cursor('B')),
        "right" => Some(cursor('C')),
        "left" => Some(cursor('D')),
        "home" => Some(cursor('H')),
        "end" => Some(cursor('F')),
        "insert" => Some(tilde(2)),
        "delete" => Some(tilde(3)),
        "pageup" => Some(tilde(5)),
        "pagedown" => Some(tilde(6)),
        "f1" | "f2" | "f3" | "f4" => {
            let letter = (b'P' + ks.key.as_bytes()[1] - b'1') as char;
            Some(if param > 1 { format!("\x1b[1;{param}{letter}").into_bytes() } else { format!("\x1bO{letter}").into_bytes() })
        }
        "f5" => Some(tilde(15)),
        "f6" => Some(tilde(17)),
        "f7" => Some(tilde(18)),
        "f8" => Some(tilde(19)),
        "f9" => Some(tilde(20)),
        "f10" => Some(tilde(21)),
        "f11" => Some(tilde(23)),
        "f12" => Some(tilde(24)),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    let key = ks.key.as_str();
    if m.control {
        let code = match key {
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => Some(k.as_bytes()[0].to_ascii_lowercase() - b'a' + 1),
            "@" | "2" => Some(0),
            "[" | "3" => Some(27),
            "\\" | "4" => Some(28),
            "]" | "5" => Some(29),
            "^" | "6" => Some(30),
            "_" | "-" | "/" | "7" => Some(31),
            "8" => Some(127),
            _ => None,
        };
        if let Some(code) = code {
            return Some(meta(&[code]));
        }
    }
    if m.alt {
        // Option composes characters on macOS ("∫" for b); send Meta+key instead.
        let base = if m.shift && key.len() == 1 { key.to_uppercase() } else { key.to_string() };
        if base.chars().count() == 1 {
            return Some(meta(base.as_bytes()));
        }
    }
    match &ks.key_char {
        Some(text) if !text.is_empty() => Some(text.as_bytes().to_vec()),
        _ if key.chars().count() == 1 => Some(key.as_bytes().to_vec()),
        _ => None,
    }
}

/// Wraps pasted text for the program: bracketed when it asked, newlines as Enter.
pub fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // A pasted end marker would end the paste early.
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn maps_keys() {
        let none = TermMode::empty();
        assert_eq!(to_bytes(&ks("enter"), none).unwrap(), b"\r");
        assert_eq!(to_bytes(&ks("shift-enter"), none).unwrap(), b"\x1b\r");
        assert_eq!(to_bytes(&ks("ctrl-c"), none).unwrap(), b"\x03");
        assert_eq!(to_bytes(&ks("escape"), none).unwrap(), b"\x1b");
        assert_eq!(to_bytes(&ks("up"), none).unwrap(), b"\x1b[A");
        assert_eq!(to_bytes(&ks("up"), TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(to_bytes(&ks("alt-left"), none).unwrap(), b"\x1b[1;3D");
        assert_eq!(to_bytes(&ks("alt-b"), none).unwrap(), b"\x1bb");
        assert_eq!(to_bytes(&ks("shift-tab"), none).unwrap(), b"\x1b[Z");
        assert_eq!(to_bytes(&ks("f5"), none).unwrap(), b"\x1b[15~");
        assert!(to_bytes(&ks("cmd-c"), none).is_none());
        assert_eq!(paste("a\nb", TermMode::BRACKETED_PASTE), b"\x1b[200~a\rb\x1b[201~");
    }
}
