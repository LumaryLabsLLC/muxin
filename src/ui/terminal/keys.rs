//! Keystrokes to the bytes a terminal program expects (xterm conventions).
//! Option/Alt acts as Meta, which is what agent CLIs expect for word motions
//! and Option+Enter newlines, except for what a Mac layout types with Option
//! (`option_types_text`).

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Mac layouts that don't need Option to type: all of ASCII is there without
/// it, and their language needs nothing from it, which leaves symbols like ∫
/// for Option+B. Option is Meta on these.
const OPTION_IS_META: &[&str] = &[
    "com.apple.keylayout.ABC",
    "com.apple.keylayout.Australian",
    "com.apple.keylayout.British-PC",
    "com.apple.keylayout.Canadian",
    "com.apple.keylayout.Colemak",
    "com.apple.keylayout.DVORAK-QWERTYCMD",
    "com.apple.keylayout.Dvorak",
    "com.apple.keylayout.Dvorak-Left",
    "com.apple.keylayout.Dvorak-Right",
    "com.apple.keylayout.PinyinKeyboard",
    "com.apple.keylayout.US",
    "com.apple.keylayout.USInternational-PC",
];

/// One of those but for `#`, which only Option+3 types there.
const BRITISH: &str = "com.apple.keylayout.British";

/// Whether a key pressed with Option can be a character on this Mac layout.
pub fn option_may_type(layout: &str) -> bool {
    !OPTION_IS_META.contains(&layout)
}

/// On a Mac, Option is how most layouts type: `@` is Option+L on a German
/// keyboard, `ą` Option+A on Polish Pro. Such a key is text for the platform
/// input method, dead keys included (German Option+N, then Space, for `~`).
/// Off a Mac, Alt is Meta and AltGr doesn't come as Alt, so this is only for Macs.
pub fn option_types_text(ks: &Keystroke, layout: &str) -> bool {
    let m = &ks.modifiers;
    // Enter, Tab and Space have a key_char as well, and keep their sequences.
    if !m.alt || m.platform || m.control || m.function || matches!(ks.key.as_str(), "enter" | "tab" | "space") {
        return false;
    }
    let Some(text) = ks.key_char.as_deref().filter(|text| !text.is_empty() && !text.chars().any(char::is_control)) else {
        return false;
    };
    match layout {
        BRITISH => text == "#",
        _ => !OPTION_IS_META.contains(&layout),
    }
}

/// Printable keys belong to the platform input method. Terminal escape
/// sequences and Control/Meta shortcuts continue through `to_bytes`.
pub fn is_text_input(ks: &Keystroke) -> bool {
    let m = &ks.modifiers;
    !m.platform && !m.control && !m.alt && !m.function
        && (ks.key == "space" || ks.key.chars().count() == 1
            || ks.key_char.as_ref().is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control)))
}

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

/// A command typed in and run (a snippet): pasted, so the program sees it as
/// typed text whatever is in it, then Enter.
pub fn typed_command(command: &str, mode: TermMode) -> Vec<u8> {
    let mut bytes = paste(command.trim_end(), mode);
    bytes.push(b'\r');
    bytes
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

    #[test]
    fn a_snippet_is_typed_and_run() {
        assert_eq!(typed_command("df -h\n", TermMode::empty()), b"df -h\r");
        // Several lines: each runs (or, bracketed, all of them once Enter comes).
        assert_eq!(typed_command("cd /tmp\nls", TermMode::empty()), b"cd /tmp\rls\r");
        assert_eq!(typed_command("cd /tmp\nls", TermMode::BRACKETED_PASTE), b"\x1b[200~cd /tmp\rls\x1b[201~\r");
    }

    #[test]
    fn text_goes_to_ime_but_terminal_controls_do_not() {
        for key in ["a", "shift-a", "1", "/", "space", "shift-space"] {
            assert!(is_text_input(&ks(key)), "{key}");
        }
        for key in ["enter", "shift-enter", "backspace", "escape", "tab", "up", "ctrl-c", "ctrl-space", "alt-b", "cmd-v"] {
            assert!(!is_text_input(&ks(key)), "{key}");
        }
        // macOS supplies key_char for these named keys as well. In
        // particular Shift+Enter must keep its terminal escape sequence.
        for (key, text) in [("enter", "\n"), ("shift-enter", "\n"), ("tab", "\t")] {
            let mut stroke = ks(key);
            stroke.key_char = Some(text.into());
            assert!(!is_text_input(&stroke), "{key}");
        }
    }

    #[test]
    fn function_modified_letters_keep_the_terminal_path() {
        for key in ["a", "j"] {
            let mut stroke = ks(&format!("fn-{key}"));
            for text in [None, Some(key.to_string())] {
                stroke.key_char = text;
                assert!(!is_text_input(&stroke), "fn-{key}");
                assert_eq!(to_bytes(&stroke, TermMode::empty()).unwrap(), key.as_bytes());
            }
        }
    }

    const GERMAN: &str = "com.apple.keylayout.German";
    const US: &str = "com.apple.keylayout.US";

    /// Option+key as GPUI reports it on a Mac: key_char is what the layout
    /// types there. A dead key's is its accent and a no-break space.
    fn option(key: &str, text: &str) -> Keystroke {
        let mut stroke = ks(&format!("alt-{key}"));
        stroke.key_char = Some(text.into());
        stroke
    }

    #[test]
    fn a_german_keyboard_types_with_option() {
        // From macOS's own German layout. Option+Shift+7 comes as "/" without
        // shift, Option+Shift+6 as "&", since Shift+7 and Shift+6 type those.
        let keys = [("l", "@"), ("5", "["), ("6", "]"), ("7", "|"), ("8", "{"), ("9", "}"), ("/", "\\"), ("e", "€"), ("q", "«"), ("b", "∫")];
        for (key, text) in keys {
            assert!(option_types_text(&option(key, text), GERMAN), "{key}");
        }
        for (key, text) in [("n", "~\u{a0}"), ("u", "¨\u{a0}"), ("&", "^\u{a0}")] {
            assert!(option_types_text(&option(key, text), GERMAN), "dead {key}");
        }
        let mut shifted = ks("alt-shift-l");
        shifted.key_char = Some("ﬂ".into());
        assert!(option_types_text(&shifted, GERMAN));
    }

    #[test]
    fn option_stays_meta_on_a_us_keyboard() {
        // From macOS's own tables, dead keys and all. Option+Shift+` comes as "~".
        let keys = [("b", "∫"), ("f", "ƒ"), ("d", "∂"), ("l", "¬"), ("5", "∞"), ("n", "˜\u{a0}"), ("e", "´\u{a0}"), ("`", "`\u{a0}"), ("~", "`")];
        let layouts = ["ABC", "Australian", "British", "British-PC", "Canadian", "Colemak", "Dvorak", "US", "USInternational-PC"];
        for layout in layouts.map(|l| format!("com.apple.keylayout.{l}")) {
            for (key, text) in keys {
                let stroke = option(key, text);
                assert!(!option_types_text(&stroke, &layout), "{layout} {key}");
                assert!(!is_text_input(&stroke), "{layout} {key}");
                assert_eq!(to_bytes(&stroke, TermMode::empty()).unwrap(), format!("\x1b{key}").into_bytes());
            }
        }
        let mut shifted = ks("alt-shift-b");
        shifted.key_char = Some("ı".into());
        assert!(!option_types_text(&shifted, US));
        assert_eq!(to_bytes(&shifted, TermMode::empty()).unwrap(), b"\x1bB");
    }

    #[test]
    fn a_british_keyboard_only_types_its_hash_with_option() {
        let british = "com.apple.keylayout.British";
        assert!(option_types_text(&option("3", "#"), british));
        assert!(!option_types_text(&option("b", "∫"), british));
        assert!(!option_types_text(&option("2", "€"), british));
    }

    #[test]
    fn layouts_not_known_to_be_meta_type_with_option() {
        for (layout, key, text) in [
            ("com.apple.keylayout.PolishPro", "a", "ą"),
            ("com.apple.keylayout.Swedish", "\\", "@"),
            ("com.apple.keylayout.French", "5", "{"),
            ("com.apple.keylayout.Brazilian-Pro", "c", "ç"),
            ("com.apple.keylayout.USExtended", "e", "´\u{a0}"),
            ("com.apple.keylayout.Russian", "2", "@"),
            ("org.sil.ukelele.keyboardlayout.custom", "l", "@"),
        ] {
            assert!(option_types_text(&option(key, text), layout), "{layout}");
        }
    }

    #[test]
    fn option_may_type_on_every_layout_that_can() {
        for layout in [GERMAN, "com.apple.keylayout.British", "com.apple.keylayout.PolishPro", "org.sil.ukelele.keyboardlayout.custom"] {
            assert!(option_may_type(layout), "{layout}");
        }
        for layout in [US, "com.apple.keylayout.ABC", "com.apple.keylayout.Dvorak"] {
            assert!(!option_may_type(layout), "{layout}");
        }
    }

    #[test]
    fn named_keys_keep_their_sequences_on_every_layout() {
        let none = TermMode::empty();
        for layout in [GERMAN, US] {
            for (key, text, bytes) in [("enter", "\n", b"\x1b\r".as_slice()), ("tab", "\t", b"\t"), ("space", " ", b"\x1b ")] {
                let stroke = option(key, text);
                assert!(!option_types_text(&stroke, layout), "{layout} {key}");
                assert_eq!(to_bytes(&stroke, none).unwrap(), bytes);
            }
            for (key, bytes) in [("backspace", b"\x1b\x7f".as_slice()), ("left", b"\x1b[1;3D"), ("escape", b"\x1b\x1b"), ("f5", b"\x1b[15;3~")] {
                let stroke = ks(&format!("alt-{key}"));
                assert!(!option_types_text(&stroke, layout), "{layout} {key}");
                assert_eq!(to_bytes(&stroke, none).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn only_option_alone_types() {
        // GPUI leaves key_char out with these; they stay shortcuts either way.
        for key in ["ctrl-alt-l", "cmd-alt-l", "fn-alt-l", "l", "shift-l"] {
            let mut stroke = ks(key);
            assert!(!option_types_text(&stroke, GERMAN), "{key}");
            stroke.key_char = Some("@".into());
            assert!(!option_types_text(&stroke, GERMAN), "{key} with key_char");
        }
        assert_eq!(to_bytes(&ks("ctrl-alt-l"), TermMode::empty()).unwrap(), b"\x1b\x0c");
        assert!(to_bytes(&ks("cmd-alt-l"), TermMode::empty()).is_none());
        // No character on Option, or a control character: nothing to type.
        assert!(!option_types_text(&ks("alt-l"), GERMAN));
        assert!(!option_types_text(&option("l", ""), GERMAN));
        assert!(!option_types_text(&option("l", "\u{1}"), GERMAN));
    }

    #[test]
    fn chinese_paste_preserves_utf8_and_bracketed_paste_mode() {
        assert_eq!(paste("你好😀", TermMode::empty()), "你好😀".as_bytes());
        assert_eq!(paste("你好\n世界", TermMode::BRACKETED_PASTE), "\x1b[200~你好\r世界\x1b[201~".as_bytes());
    }
}
