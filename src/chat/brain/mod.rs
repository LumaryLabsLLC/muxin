//! What answers in a chat. A brain takes what the user said and says what
//! happens next, piece by piece: text, the tools it calls and what they
//! answered, what the turn used.
//!
//! The first two are agent CLIs already on the user's computer, signed in to
//! the user's own plan: Claude Code (claude.rs) and Codex (codex.rs). Muxin
//! runs them without their terminal, with their own tools off, and with
//! Muxin's tools reached through the chat's bridge (../bridge.rs), so every
//! call passes the chat's gate.

pub mod claude;
pub mod codex;

use std::path::PathBuf;

use serde_json::Value;
use tokio::sync::mpsc;

/// Something a brain says during a turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A piece of what it's saying, as it comes.
    Delta(String),
    /// A whole stretch of text it said. After its pieces, when the brain
    /// sends pieces; by itself when it doesn't.
    Text(String),
    /// A tool it called.
    Tool { id: String, name: String, args: Value },
    /// What that tool answered.
    Result { id: String, text: String, is_error: bool },
    /// What the turn used. `cost` when the brain says what it would cost.
    Usage { input: u64, output: u64, cached: u64, cost: Option<f64> },
    /// Something about the user's plan and its limits, in words.
    Limit(String),
    /// The brain's own id for this conversation, to continue it later.
    Session(String),
}

/// What a brain needs to start.
#[derive(Clone, Debug)]
pub struct Setup {
    /// This Muxin, which the brain runs as its MCP server (`mcp --chat`).
    pub exe: PathBuf,
    /// The bridge's socket.
    pub bridge: PathBuf,
    /// The folder it runs in: the chat's own, and empty.
    pub work: PathBuf,
    /// Who it is and how it should work here (prompt.rs).
    pub prompt: String,
    /// Its own id for the conversation, when there's one to continue.
    pub resume: Option<String>,
}

#[async_trait::async_trait]
pub trait Brain: Send {
    /// Runs one turn: sends `text`, and reports what happens on `events`
    /// until the brain is done. `Err` says, for the user, why it couldn't.
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String>;

    /// Ends what it's doing now. The conversation can go on afterwards.
    async fn stop(&mut self);
}

/// A brain Muxin can use: its id in settings, its name, its program.
pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    pub program: &'static str,
    /// How to get it, for someone who doesn't have it.
    pub get: &'static str,
}

pub const KINDS: [Kind; 2] = [
    Kind { id: "claude", name: "Claude Code", program: "claude", get: "https://claude.com/claude-code" },
    Kind { id: "codex", name: "Codex", program: "codex", get: "https://developers.openai.com/codex/cli" },
];

pub fn kind(id: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.id == id)
}

/// Where a program is: on PATH, or where its installer usually puts it. Apps
/// started from Finder, the Dock or a desktop launcher get a short PATH
/// without the user's own folders.
pub fn find(program: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{program}.exe") } else { program.to_string() };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(usual_dirs());
    dirs.into_iter().map(|d| d.join(&file)).find(|f| f.is_file())
}

fn usual_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        home.join(".local/bin"),
        home.join("bin"),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        "/home/linuxbrew/.linuxbrew/bin".into(),
        home.join(".npm-global/bin"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".cargo/bin"),
    ]
}

/// PATH for a brain's program: this process's, then the usual places, so a
/// program found there finds what it needs there too (node, git).
pub fn path_for_child() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for dir in usual_dirs() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The brains this computer has, in the order Muxin prefers them.
pub fn installed() -> Vec<&'static Kind> {
    KINDS.iter().filter(|k| find(k.program).is_some()).collect()
}

/// Starts the brain `id` ("claude", "codex"). Nothing runs until its first turn.
pub fn start(id: &str, setup: Setup) -> Result<Box<dyn Brain>, String> {
    let kind = kind(id).ok_or_else(|| format!("\"{id}\" isn't a brain Muxin knows. Use {}.", KINDS.map(|k| k.id).join(" or ")))?;
    let program = find(kind.program).ok_or_else(|| format!("{} isn't on this computer. Get it at {}.", kind.name, kind.get))?;
    Ok(match kind.id {
        "claude" => Box::new(claude::Claude::new(program, setup)),
        _ => Box::new(codex::Codex::new(program, setup)),
    })
}

/// The end of what a program printed on stderr, for an error's words.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A tool's result as text: its text blocks, with a word for what isn't text.
pub(crate) fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                Some(other) => format!("[{other}]"),
                None => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_result_reads_as_its_text() {
        assert_eq!(result_text(&json!([{"type": "text", "text": "a"}, {"type": "image", "data": "…"}, {"type": "text", "text": "b"}])), "a\n[image]\nb");
        assert_eq!(result_text(&json!("plain")), "plain");
        assert_eq!(result_text(&Value::Null), "");
    }

    #[test]
    fn an_unknown_brain_says_which_there_are() {
        let setup = Setup { exe: "muxin".into(), bridge: "/tmp/x.sock".into(), work: "/tmp".into(), prompt: String::new(), resume: None };
        let err = start("gpt", setup).err().unwrap();
        assert!(err.contains("claude or codex"), "{err}");
    }

    #[test]
    fn the_end_of_stderr_is_kept() {
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 3), "");
    }
}
