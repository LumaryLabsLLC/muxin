//! What answers in a chat. A brain takes what the user said and says what
//! happens next, piece by piece: text, the tools it calls and what they
//! answered, what the turn used.
//!
//! Each is an agent CLI already on the user's computer, signed in to the
//! user's own plan: Claude Code (claude.rs), Codex (codex.rs) and Grok Build
//! (grok.rs). Muxin runs them without their terminal, with their own tools
//! off, and with Muxin's tools reached through the chat's bridge
//! (../bridge.rs), so every call passes the chat's gate.

pub mod catalog;
pub mod claude;
pub mod codex;
pub mod grok;
mod messages;

use std::path::PathBuf;

use serde_json::Value;
use tokio::sync::mpsc;

use super::attach::Attached;

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
    /// The model that's answering, by the name it's shown under.
    Model(String),
}

/// What a brain needs to start.
#[derive(Clone, Debug, Default)]
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
    /// The model to answer with, by the brain's own id for it (catalog.rs).
    /// None: whichever the brain itself would use.
    pub model: Option<String>,
    /// How hard it should think, as that model names it ("high"). None: the model's own.
    pub effort: Option<String>,
}

#[async_trait::async_trait]
pub trait Brain: Send {
    /// Runs one turn: sends `text`, and reports what happens on `events`
    /// until the brain is done. `Err` says, for the user, why it couldn't.
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String>;

    /// Ends what it's doing now. The conversation can go on afterwards.
    async fn stop(&mut self);

    /// Answers with this model, at this effort, from the next turn on, in
    /// the same conversation. None: the brain's own choice.
    fn choose(&mut self, _model: Option<String>, _effort: Option<String>) {}

    /// Takes the pictures and PDFs that go with the next message (text
    /// files are already in its words: ../attach.rs). `Err` says, for the
    /// user, that this brain can't be given one; nothing is taken then.
    fn attach(&mut self, files: Vec<Attached>) -> Result<(), String> {
        match files.first() {
            None => Ok(()),
            Some(file) => Err(format!("What answers here can't be given {}.", file.name)),
        }
    }
}

/// A brain Muxin can use: its id in settings, its name, its program.
pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    /// The program it is.
    pub program: &'static str,
    /// How to get it, for someone who doesn't have it.
    pub get: &'static str,
    /// It keeps MCP tools out of what it sends the model and looks them up
    /// as it needs them. So it's given every tool in full, to find them by,
    /// where the others get the chat's short list (../tools.rs).
    pub finds_tools: bool,
}

/// In the order Muxin prefers them when the user hasn't chosen.
pub const KINDS: [Kind; 3] = [
    Kind { id: "claude", name: "Claude Code", program: "claude", get: "https://claude.com/claude-code", finds_tools: false },
    Kind { id: "codex", name: "Codex", program: "codex", get: "https://developers.openai.com/codex/cli", finds_tools: false },
    Kind { id: "grok", name: "Grok Build", program: "grok", get: "https://docs.x.ai/build/overview", finds_tools: true },
];

/// The brains' ids, for saying which there are: "claude, codex or grok".
pub fn ids() -> String {
    let ids = KINDS.map(|k| k.id);
    format!("{} or {}", ids[..ids.len() - 1].join(", "), ids[ids.len() - 1])
}

/// What to say when there's nothing to answer with.
pub fn none_here() -> String {
    let programs: Vec<String> = KINDS.iter().map(|k| format!("{} ({})", k.name, k.get)).collect();
    let programs = format!("{} or {}", programs[..programs.len() - 1].join(", "), programs[programs.len() - 1]);
    format!("The chat needs something to answer with: {programs} on this computer, signed in.")
}

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
        // Where Grok Build's installer puts it.
        home.join(".grok/bin"),
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

/// The brains there are to use here, in the order Muxin prefers them: the
/// ones whose program this computer has.
pub fn available() -> Vec<&'static Kind> {
    KINDS.iter().filter(|k| find(k.program).is_some()).collect()
}

/// Starts a brain ("claude", "codex", "grok"). Nothing runs until its first turn.
pub fn start(id: &str, setup: Setup) -> Result<Box<dyn Brain>, String> {
    let kind = kind(id).ok_or_else(|| format!("\"{id}\" isn't a brain Muxin knows. Use {}.", ids()))?;
    let program = find(kind.program).ok_or_else(|| format!("{} isn't on this computer. Get it at {}.", kind.name, kind.get))?;
    Ok(match kind.id {
        "claude" => Box::new(claude::Claude::new(program, setup)),
        "grok" => Box::new(grok::Grok::new(program, setup)),
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
        let setup = Setup { exe: "muxin".into(), bridge: "/tmp/x.sock".into(), work: "/tmp".into(), prompt: String::new(), ..Default::default() };
        let err = start("gpt", setup).err().unwrap();
        assert!(err.contains("claude, codex or grok"), "{err}");
        let none = none_here();
        assert!(none.contains("Claude Code (https://claude.com/claude-code), Codex (") && none.contains(") or Grok Build (https://docs.x.ai/build/overview) on this computer"), "{none}");
    }

    #[test]
    fn every_program_has_a_driver_and_a_place_to_get_it() {
        for kind in KINDS.iter() {
            assert!(kind.get.starts_with("https://"), "{}", kind.id);
        }
        // In the order they're chosen when the user hasn't.
        assert_eq!(KINDS.map(|k| k.id), ["claude", "codex", "grok"]);
        assert_eq!(KINDS.iter().filter(|k| k.finds_tools).map(|k| k.id).collect::<Vec<_>>(), ["grok"]);
    }

    #[test]
    fn the_end_of_stderr_is_kept() {
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 3), "");
    }
}
