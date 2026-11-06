//! Newer prompt rules (question.rs) from muxin.dev, so a CLI that draws its
//! prompts a new way is read right without a Muxin release. The site serves
//! a copy of assets/prompt-rules.json (Muxin's own); to change what released
//! copies read, raise its `version` and publish it there. At most once a
//! day, a plain request for a static file (nothing about the user goes with
//! it), kept in Muxin's folder. Anything wrong with it: the rules Muxin has.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::question::{self, Rules};

const URL: &str = "https://muxin.dev/prompt-rules.json";
/// How old the copy may get before it's fetched again.
pub const EVERY: Duration = Duration::from_secs(24 * 3600);

fn cache(dir: &Path) -> PathBuf {
    dir.join("prompt-rules.json")
}

/// Rules from their JSON, when they're ones to use.
fn parse(text: &str) -> Option<Rules> {
    serde_json::from_str::<Rules>(text).ok().filter(Rules::sane)
}

/// Uses the copy fetched before, if there's one (at start-up).
pub fn load(dir: &Path) {
    if let Some(r) = std::fs::read_to_string(cache(dir)).ok().as_deref().and_then(parse) {
        question::use_rules(r);
    }
}

/// Whether the copy is older than a day (or there's none).
pub fn due(dir: &Path) -> bool {
    std::fs::metadata(cache(dir)).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_none_or(|age| age >= EVERY)
}

/// Fetches the rules and uses them when they're newer and sane. True when it did.
pub async fn refresh(dir: &Path) -> Result<bool, String> {
    let url = std::env::var("MUXIN_PROMPT_RULES_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| URL.to_string());
    refresh_from(&url, dir).await
}

async fn refresh_from(url: &str, dir: &Path) -> Result<bool, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).user_agent(concat!("Muxin/", env!("CARGO_PKG_VERSION"))).build().map_err(|e| e.to_string())?;
    let reply = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !reply.status().is_success() {
        return Err(format!("{url} answered {}", reply.status()));
    }
    let text = reply.text().await.map_err(|e| e.to_string())?;
    if text.len() > 64 * 1024 {
        return Err("the rules are too big".into());
    }
    let Some(rules) = parse(&text) else { return Err("the rules aren't ones Muxin can use".into()) };
    // Keep it (its time says when it was fetched), even when it changes nothing.
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(cache(dir), &text);
    Ok(question::use_rules(rules))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rules_file_takes_what_it_says_and_the_rest_from_muxins() {
        // Only one list given: the others are Muxin's own.
        let r = parse(r#"{ "version": 2, "menu_marks": ["❯", "›", "▸"] }"#).unwrap();
        assert_eq!(r.version, 2);
        assert_eq!(r.menu_marks, ["❯", "›", "▸"]);
        assert_eq!(r.input_marks, Rules::default().input_marks);
        // A menu drawn with the new mark is read by them, not by the old ones.
        let screen = " Trust this folder?\n\n ▸ No, exit\n   Yes, I trust it\n\n Enter to confirm\n";
        assert_eq!(question::read_with(&r, screen).options.len(), 2);
        assert!(question::read_with(&Rules::default(), screen).options.is_empty());
        // Nonsense, an empty list that's needed, or a huge mark: not used.
        assert!(parse("not json").is_none());
        assert!(parse(r#"{ "input_marks": [] }"#).is_none());
        assert!(parse(&format!(r#"{{ "bullets": ["{}"] }}"#, "x".repeat(41))).is_none());
        // Its own copy round-trips.
        assert_eq!(parse(&serde_json::to_string(&Rules::default()).unwrap()), Some(Rules::default()));
    }

    /// The copy muxin.dev serves (public/prompt-rules.json in the website's
    /// repo) starts as this file: Muxin's own rules, as data.
    #[test]
    fn the_published_rules_are_muxins_own() {
        assert_eq!(parse(include_str!("../../../assets/prompt-rules.json")), Some(Rules::default()));
    }

    /// A copy from the site is kept, used, and read again at start-up.
    #[tokio::test]
    async fn fetches_keeps_and_loads_the_rules() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = include_str!("../../../assets/prompt-rules.json");
        let server = std::thread::spawn(move || {
            for status in ["200 OK", "404 Not Found"] {
                let (mut c, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let _ = c.read(&mut buf);
                let text = if status.starts_with("200") { body } else { "nope" };
                let _ = write!(c, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
            }
        });
        let dir = std::env::temp_dir().join(format!("muxin-rules-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = format!("http://127.0.0.1:{port}/prompt-rules.json");
        // Muxin's own rules again: taken (they're no older), and kept.
        assert_eq!(refresh_from(&url, &dir).await, Ok(true));
        assert!(!due(&dir));
        assert_eq!(std::fs::read_to_string(cache(&dir)).unwrap(), body);
        // The site answering badly changes nothing.
        assert!(refresh_from(&url, &dir).await.is_err());
        assert_eq!(std::fs::read_to_string(cache(&dir)).unwrap(), body);
        load(&dir);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_copy_is_due() {
        let dir = std::env::temp_dir().join(format!("muxin-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(due(&dir), "none yet");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(cache(&dir), "{}").unwrap();
        assert!(!due(&dir), "just fetched");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
