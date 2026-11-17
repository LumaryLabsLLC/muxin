//! The window's language. The English text in the code is the key: `tr`
//! gives it in the language chosen (Settings › General › Language, else the
//! system's), from `locales/<lang>/*.toml` (`"English" = "Tradução"`), and
//! the English itself when a line has no translation yet.
//!
//! Only the window is translated: the CLI, the MCP server and the API stay in
//! English (agents and scripts read them).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// The languages there are, as saved in `settings.language`, with their names.
pub(crate) const LANGUAGES: &[(&str, &str)] = &[("", "System"), ("en", "English"), ("pt-BR", "Português (Brasil)")];

/// pt-BR's files, one per part of the window.
const PT_BR: &[&str] = &[
    include_str!("../../locales/pt-BR/ui.toml"),
    include_str!("../../locales/pt-BR/a.toml"),
    include_str!("../../locales/pt-BR/b.toml"),
    include_str!("../../locales/pt-BR/c.toml"),
    include_str!("../../locales/pt-BR/d.toml"),
    include_str!("../../locales/pt-BR/e.toml"),
];

const EN: u8 = 0;
const PT: u8 = 1;
static CURRENT: AtomicU8 = AtomicU8::new(EN);

fn pt_br() -> &'static HashMap<String, &'static str> {
    static TABLE: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| parse(PT_BR).expect("locales/pt-BR"))
}

fn parse(files: &[&str]) -> Result<HashMap<String, &'static str>, String> {
    let mut out = HashMap::new();
    for (i, file) in files.iter().enumerate() {
        let table: toml::Table = toml::from_str(file).map_err(|e| format!("file {i}: {e}"))?;
        for (k, v) in table {
            let v = v.as_str().ok_or_else(|| format!("{k:?} isn't text"))?;
            // ponytail: leaked once per line at start, the table lives as long as the app.
            out.insert(k, &*Box::leak(v.to_string().into_boxed_str()));
        }
    }
    Ok(out)
}

/// Chooses the language: a `settings.language` ("" follows the system).
/// Takes effect at the next frame drawn.
pub(crate) fn set_language(setting: &str) {
    let lang = if setting.is_empty() { system_language() } else { setting.to_string() };
    CURRENT.store(if lang.to_ascii_lowercase().starts_with("pt") { PT } else { EN }, Ordering::Relaxed);
}

fn system_language() -> String {
    if let Some(l) = ["MUXIN_LANG", "LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.is_empty() && v != "C" && v != "POSIX") {
        return l;
    }
    // An app opened from the Finder has no LANG: ask macOS.
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(first) = s.split('"').nth(1) {
            return first.to_string();
        }
    }
    String::new()
}

/// `english` in the window's language.
pub(crate) fn tr(english: &'static str) -> &'static str {
    if CURRENT.load(Ordering::Relaxed) == PT {
        if let Some(t) = pt_br().get(english) {
            return t;
        }
    }
    english
}

/// `english|context` in the window's language: the same English that means
/// something else here ("Open|state" isn't the button "Open"). English shows
/// the part before `|`.
pub(crate) fn trc(keyed: &'static str) -> &'static str {
    match tr(keyed) {
        t if std::ptr::eq(t, keyed) => keyed.rsplit_once('|').map_or(keyed, |(english, _)| english),
        t => t,
    }
}

/// `english` in the window's language, with each `{name}` replaced.
pub(crate) fn trf(english: &'static str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut s = tr(english).to_string();
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), &value.to_string());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|(n, _)| n)).collect();
        v.sort();
        v
    }

    /// Every `tr("…")`/`trf("…")` in src/ui.
    fn keys_in_code() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"))];
        while let Some(dir) = dirs.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n.rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    for call in ["tr(\"", "trf(\"", "trc(\""] {
                        let mut pieces = src.split(call);
                        let mut before = pieces.next().unwrap_or_default();
                        for piece in pieces {
                            // `tr(` itself, not the end of `nsstr(` or `self.tr(`'s cousins.
                            let word = before.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_');
                            before = piece;
                            if word {
                                continue;
                            }
                            // The literal up to its closing quote, escapes kept as written.
                            let mut lit = String::new();
                            let mut chars = piece.chars();
                            while let Some(c) = chars.next() {
                                match c {
                                    '\\' => match chars.next() {
                                        Some('n') => lit.push('\n'),
                                        Some(o) => lit.push(o),
                                        None => {}
                                    },
                                    '"' => break,
                                    c => lit.push(c),
                                }
                            }
                            out.push((p.display().to_string(), lit));
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn pt_br_parses() {
        assert!(!pt_br().is_empty());
    }

    #[test]
    fn every_line_in_the_window_has_a_pt_br_translation() {
        let missing: Vec<_> = keys_in_code().into_iter().filter(|(_, k)| !pt_br().contains_key(k)).collect();
        assert!(missing.is_empty(), "{} lines without pt-BR, first ones: {:#?}", missing.len(), &missing[..missing.len().min(20)]);
    }

    #[test]
    fn translations_keep_their_placeholders() {
        let bad: Vec<_> = pt_br().iter().filter(|(k, v)| placeholders(k) != placeholders(v)).collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn no_unused_translations() {
        let used: std::collections::HashSet<String> = keys_in_code().into_iter().map(|(_, k)| k).collect();
        let unused: Vec<_> = pt_br().keys().filter(|k| !used.contains(*k)).collect();
        assert!(unused.is_empty(), "{unused:#?}");
    }

    #[test]
    fn tr_and_trf() {
        // ponytail: never switches CURRENT, other tests read English in parallel.
        assert_eq!(pt_br().get("Language"), Some(&"Idioma"));
        assert_eq!(tr("not a line anywhere"), "not a line anywhere");
        assert_eq!(trc("Open|state"), "Open");
        assert_eq!(pt_br().get("Open|state"), Some(&"Aberto"));
        assert_eq!(trf("{a} and {b}", &[("a", &1), ("b", &"x")]), "1 and x");
    }
}
