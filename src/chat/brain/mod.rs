//! What answers in a chat. A brain takes what the user said and says what
//! happens next, piece by piece: text, the tools it calls and what they
//! answered, what the turn used.
//!
//! Three are agent CLIs already on the user's computer, signed in to the
//! user's own plan: Claude Code (claude.rs), Codex (codex.rs) and Grok Build
//! (grok.rs). Muxin runs them without their terminal, with their own tools
//! off, and with Muxin's tools reached through the chat's bridge
//! (../bridge.rs), so every call passes the chat's gate.
//!
//! The rest are providers the user connected with an API key (../keys.rs):
//! Muxin asks their API itself and runs the tools the model calls right
//! here, each one past the chat's gate first. Anthropic's speaks its own
//! Messages API (anthropic.rs); the others, chat completions (openai.rs).

pub mod anthropic;
pub mod api;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod grok;
mod messages;
pub mod openai;

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
    /// "fast" for the faster, dearer way to run a model that has one. None: as usual.
    pub speed: Option<String>,
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

    /// Runs the model the faster way ("fast") from the next turn on, or as
    /// usual (None). A brain without such a choice takes no notice.
    fn speed(&mut self, _speed: Option<String>) {}

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

/// A brain Muxin can use: its id in settings, its name, and how it's reached.
pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    pub via: Via,
    /// Where to get it: the program, or a key for it.
    pub get: &'static str,
    /// It keeps MCP tools out of what it sends the model and looks them up
    /// as it needs them. So it's given every tool in full, to find them by,
    /// where the others get the chat's short list (../tools.rs).
    pub finds_tools: bool,
}

/// How a brain is reached.
pub enum Via {
    /// A program on this computer, signed in to the user's own plan.
    Program(&'static str),
    /// A provider's API, with a key the user connected (../keys.rs). Muxin
    /// asks it itself, and pays by the token.
    Api(Api),
}

/// A provider's API: how it's spoken to, and where it is unless the user
/// said another address when they connected it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Api {
    pub dialect: Dialect,
    pub base: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dialect {
    /// Anthropic's Messages API (anthropic.rs).
    Anthropic,
    /// Chat completions as OpenAI defined them, which most providers speak (openai.rs).
    OpenAi,
}

impl Kind {
    /// The program it is, when it's one.
    pub fn program(&self) -> Option<&'static str> {
        match self.via {
            Via::Program(program) => Some(program),
            Via::Api(_) => None,
        }
    }

    pub fn api(&self) -> Option<Api> {
        match self.via {
            Via::Api(api) => Some(api),
            Via::Program(_) => None,
        }
    }
}

/// The programs first, in the order Muxin prefers them when the user hasn't
/// chosen; then the providers reached with a key, as a list shows them.
pub const KINDS: [Kind; 10] = [
    Kind { id: "claude", name: "Claude Code", via: Via::Program("claude"), get: "https://claude.com/claude-code", finds_tools: false },
    Kind { id: "codex", name: "Codex", via: Via::Program("codex"), get: "https://developers.openai.com/codex/cli", finds_tools: false },
    Kind { id: "grok", name: "Grok Build", via: Via::Program("grok"), get: "https://docs.x.ai/build/overview", finds_tools: true },
    Kind { id: "anthropic", name: "Anthropic", via: Via::Api(Api { dialect: Dialect::Anthropic, base: "https://api.anthropic.com" }), get: "https://platform.claude.com/settings/keys", finds_tools: false },
    Kind { id: "openai", name: "OpenAI", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "https://api.openai.com/v1" }), get: "https://platform.openai.com/api-keys", finds_tools: false },
    Kind { id: "xai", name: "xAI", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "https://api.x.ai/v1" }), get: "https://console.x.ai", finds_tools: false },
    Kind { id: "google", name: "Google", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "https://generativelanguage.googleapis.com/v1beta/openai" }), get: "https://aistudio.google.com/apikey", finds_tools: false },
    Kind { id: "openrouter", name: "OpenRouter", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "https://openrouter.ai/api/v1" }), get: "https://openrouter.ai/keys", finds_tools: false },
    Kind { id: "deepseek", name: "DeepSeek", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "https://api.deepseek.com/v1" }), get: "https://platform.deepseek.com/api_keys", finds_tools: false },
    // Anything else that speaks chat completions, at an address the user gives: a gateway, a model run on this computer.
    Kind { id: CUSTOM, name: "OpenAI-compatible", via: Via::Api(Api { dialect: Dialect::OpenAi, base: "" }), get: "", finds_tools: false },
];

/// The provider whose address is the user's to give.
pub const CUSTOM: &str = "custom";

/// The programs' ids, for saying which there are: "claude, codex or grok".
/// (A provider with a key is named in Settings › AI, once it's connected.)
pub fn ids() -> String {
    let ids: Vec<&str> = KINDS.iter().filter(|k| k.program().is_some()).map(|k| k.id).collect();
    format!("{} or {}", ids[..ids.len() - 1].join(", "), ids[ids.len() - 1])
}

/// What to say when there's nothing to answer with.
pub fn none_here() -> String {
    let programs: Vec<String> = KINDS.iter().filter(|k| k.program().is_some()).map(|k| format!("{} ({})", k.name, k.get)).collect();
    let programs = format!("{} or {}", programs[..programs.len() - 1].join(", "), programs[programs.len() - 1]);
    format!("The chat needs something to answer with: {programs} on this computer, signed in, or a provider connected with its API key in Settings › AI.")
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
/// programs this computer has, then the providers the user connected.
pub fn available(chat: &crate::config::ChatSettings) -> Vec<&'static Kind> {
    KINDS
        .iter()
        .filter(|k| match k.via {
            Via::Program(program) => find(program).is_some(),
            Via::Api(_) => chat.providers.contains_key(k.id),
        })
        .collect()
}

/// Starts a brain that's a program ("claude", "codex", "grok"). Nothing runs
/// until its first turn. (One reached with a key is made where the chat is
/// opened: it calls Muxin's tools itself, so it needs the chat's gate.)
pub fn start(id: &str, setup: Setup) -> Result<Box<dyn Brain>, String> {
    let kind = kind(id).ok_or_else(|| format!("\"{id}\" isn't a brain Muxin knows. Use {}.", ids()))?;
    let program = kind.program().ok_or_else(|| format!("{} isn't a program to start.", kind.name))?;
    let program = find(program).ok_or_else(|| format!("{} isn't on this computer. Get it at {}.", kind.name, kind.get))?;
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
        for kind in KINDS.iter().filter(|k| k.id != CUSTOM) {
            assert!(kind.get.starts_with("https://"), "{}", kind.id);
        }
        // The programs come first, in the order they're chosen when the user hasn't; then the providers reached with a key.
        assert_eq!(KINDS.iter().filter(|k| k.program().is_some()).map(|k| k.id).collect::<Vec<_>>(), ["claude", "codex", "grok"]);
        assert_eq!(KINDS.iter().position(|k| k.api().is_some()), Some(3));
        assert_eq!(KINDS.iter().filter(|k| k.api().is_some()).map(|k| k.id).collect::<Vec<_>>(), ["anthropic", "openai", "xai", "google", "openrouter", "deepseek", "custom"]);
        // Each provider has an address of its own, but the one whose address is the user's to give.
        assert!(KINDS.iter().filter_map(|k| k.api().map(|api| (k.id, api.base))).all(|(id, base)| (id == CUSTOM) == base.is_empty()));
        assert_eq!(kind("anthropic").and_then(|k| k.api()).map(|api| api.dialect), Some(Dialect::Anthropic));
        // Only the programs on this computer answer until a provider is connected.
        let mut chat = crate::config::ChatSettings::default();
        assert!(available(&chat).iter().all(|k| k.program().is_some()));
        chat.providers.insert("openrouter".into(), Default::default());
        assert_eq!(available(&chat).last().map(|k| k.id), Some("openrouter"));
        assert_eq!(KINDS.iter().filter(|k| k.finds_tools).map(|k| k.id).collect::<Vec<_>>(), ["grok"]);
    }

    #[test]
    fn the_end_of_stderr_is_kept() {
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 3), "");
    }
}
