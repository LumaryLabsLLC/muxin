//! Grok Build as a chat's brain, on the user's own Grok plan.
//!
//! xAI has no sign-in for apps it hasn't listed, so Muxin doesn't sign in to
//! Grok itself: it runs the `grok` program the user installed and signed in
//! to, as it does Claude Code and Codex.
//!
//! Like Codex, each turn is one process that ends with the turn; the next
//! one continues the same session (`--resume <id>`). What it does comes as
//! lines of JSON in the shape Claude Code prints (`--output-format
//! streaming-messages-json`), so they're read by the same code (messages.rs).
//! Started so that:
//!
//! - `.grok/config.toml` in the chat's folder names Muxin's bridge as an MCP
//!   server. Grok has no flag for that, and a folder's servers only load once
//!   the user has trusted the folder, so `GROK_FOLDER_TRUST=0` for this one
//!   process: the folder is the chat's own, and Muxin wrote what's in it.
//! - `--tools search_tool,use_tool`: none of its own tools (no shell, no
//!   files, no web) but the two it reaches MCP tools through.
//! - `--permission-mode dontAsk --allow "MCPTool(muxin__*)"`: Muxin's tools
//!   run without Grok asking, and anything else is refused. The chat's gate
//!   does the asking, inside the tool call.
//! - `GROK_CLAUDE_…_ENABLED=0`, `GROK_CURSOR_…_ENABLED=0`: Grok reads Claude
//!   Code's and Cursor's hooks, MCP servers, skills and rules too; not here.
//! - `--system-prompt-override`: Muxin's words instead of a coding agent's.
//!   `GROK_MEMORY=0`: nothing it remembers from the user's other sessions.
//! - The message goes in a file (`--prompt-file --verbatim`): not on the
//!   command line, where other users of the computer could read it, and not
//!   taken for one of Grok's own commands when it starts with a slash.
//!
//! What still comes along is what the user set up for Grok itself: its hooks,
//! its skills, and its plugins (Claude Code's plugins among them, which Grok
//! loads as its own). Their hooks run as in any Grok session, and a plugin's
//! MCP server is listed beside Muxin's; its tools can't run here.
//!
//! Grok doesn't send MCP tools to the model: the model looks one up
//! (`search_tool`) and calls it by name (`use_tool`). So it's given every
//! tool in full to look through, not the chat's short list (`finds_tools` in
//! mod.rs); measured, the short list cost it half as much again and took
//! twice as long. Its lookups are left out of the chat.
//!
//! As Grok Build 1.0.77 does all this. xAI's servers turn away versions
//! before 1.0.13, in words that say to run `grok update`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use super::{Brain, Event, Setup, tail};
use crate::chat::attach::{Attached, Kind};

/// Grok names an MCP server's tools `<server>__<tool>`; the server is "muxin".
const SERVER: &str = "muxin";
/// How long a tool call may take, waiting for the user included.
const TOOL_TIMEOUT_SECS: u64 = 6 * 3600;
/// How much of a tool's result Grok gives the model before it cuts it short
/// (20,000 bytes unless told): as much as Muxin's own brain gives.
const MOST_OUTPUT: usize = 60_000;
/// Grok's own tools for MCP: finding one, and calling one.
const FIND: &str = "search_tool";
const CALL: &str = "use_tool";
/// What a lookup is called here, so it's never taken for a tool of Muxin's.
const LOOKUP: &str = "grok:search_tool";

/// Pictures with one message, together. Grok takes them inside its prompt,
/// which is an argument on its command line, and a command line has an end
/// (about a megabyte on a Mac, of which base64 uses a third more).
const MOST_PICTURES: u64 = 600 * 1024;

/// Said after Muxin's own words: how its tools are reached from Grok.
const HOW: &str = "The muxin tools are the tools of the MCP server named \"muxin\". When one isn't among your tools by name, find it with search_tool and call it with use_tool by its full name (muxin__status).";

pub struct Grok {
    program: PathBuf,
    setup: Setup,
    /// Grok's id for the conversation, once it said it.
    session: Option<String>,
    /// The turn's process, kept here so a turn that was dropped halfway
    /// (the user stopped it) can still be ended by `stop`.
    child: Option<Child>,
    /// The pictures that go with the next message.
    attached: Vec<Attached>,
}

/// How a turn's message reaches Grok.
enum Prompt {
    /// In a file only the user can read: the usual way.
    File(PathBuf),
    /// As content blocks (its words, then pictures) on the command line:
    /// the only way Grok takes a picture.
    Blocks(String),
}

impl Grok {
    pub fn new(program: PathBuf, setup: Setup) -> Grok {
        let session = setup.resume.clone();
        Grok { program, setup, session, child: None, attached: Vec::new() }
    }

    /// The chat's folder: the message is kept there, beside the folder Grok
    /// runs in.
    fn folder(&self) -> &Path {
        self.setup.work.parent().unwrap_or(&self.setup.work)
    }

    /// What Grok is started with for a turn, and its message.
    fn args(&self, message: &Prompt) -> Vec<String> {
        let mut args: Vec<String> = match message {
            Prompt::File(file) => vec!["--prompt-file".into(), file.to_string_lossy().into_owned(), "--verbatim".into()],
            Prompt::Blocks(blocks) => vec!["--prompt-json".into(), blocks.clone(), "--verbatim".into()],
        };
        args.extend(["--output-format", "streaming-messages-json", "--include-partial-messages"].map(String::from));
        args.extend(["--tools".into(), format!("{FIND},{CALL}")]);
        args.extend(["--permission-mode".into(), "dontAsk".into(), "--allow".into(), format!("MCPTool({SERVER}__*)")]);
        args.extend(["--disable-web-search", "--no-subagents", "--no-plan"].map(String::from));
        args.extend(["--system-prompt-override".into(), format!("{}\n\n{HOW}", self.setup.prompt)]);
        if let Some(model) = &self.setup.model {
            args.extend(["-m".into(), model.clone()]);
        }
        if let Some(effort) = &self.setup.effort {
            args.extend(["--reasoning-effort".into(), effort.clone()]);
        }
        if let Some(id) = &self.session {
            args.extend(["--resume".into(), id.clone()]);
        }
        args
    }

    /// The chat folder's `.grok/config.toml`: Muxin's bridge, as Grok's MCP
    /// server here. A JSON string, or a list of them, is the same in TOML.
    fn config(&self) -> String {
        format!(
            "# Written by Muxin for this chat: its tools, for Grok.\n[mcp_servers.{SERVER}]\ncommand = {}\nargs = {}\nstartup_timeout_sec = 30\ntool_timeout_sec = {TOOL_TIMEOUT_SECS}\n",
            json!(self.setup.exe.to_string_lossy()),
            json!(["mcp", "--chat", self.setup.bridge.to_string_lossy()]),
        )
    }

    /// Writes what a turn needs on disk: the config, and the message. How
    /// the message reaches Grok: its file, or its blocks when pictures go with it.
    fn prepare(&self, text: &str, pictures: &[Attached]) -> Result<Prompt, String> {
        let dir = self.setup.work.join(".grok");
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
        let config = dir.join("config.toml");
        std::fs::write(&config, self.config()).map_err(|e| format!("Couldn't write {}: {e}", config.display()))?;
        if !pictures.is_empty() {
            return Ok(Prompt::Blocks(blocks(text, pictures)?));
        }
        let message = self.folder().join("grok-message.txt");
        write_private(&message, text).map_err(|e| format!("Couldn't write {}: {e}", message.display()))?;
        Ok(Prompt::File(message))
    }
}

/// A message as the content blocks Grok's `--prompt-json` takes (the Agent
/// Client Protocol's): `{"type":"text","text"}`, `{"type":"image","data","mimeType"}`.
fn blocks(text: &str, pictures: &[Attached]) -> Result<String, String> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    for file in pictures {
        if let Kind::Image(media) = file.kind {
            blocks.push(json!({ "type": "image", "data": file.base64()?, "mimeType": media }));
        }
    }
    Ok(Value::Array(blocks).to_string())
}

/// What Grok's environment says for a turn, besides what Muxin's own says.
fn env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        // The chat's folder is Muxin's own: its config counts without the user trusting the folder first.
        ("GROK_FOLDER_TRUST".into(), "0".into()),
        // It doesn't replace itself in the middle of a chat.
        ("GROK_DISABLE_AUTOUPDATER".into(), "1".into()),
        ("GROK_MAX_MCP_OUTPUT_BYTES".into(), MOST_OUTPUT.to_string()),
        // What it remembers from the user's other sessions stays out of this one.
        ("GROK_MEMORY".into(), "0".into()),
    ];
    // What the user set up for other agents stays with those.
    for vendor in ["CLAUDE", "CURSOR"] {
        for what in ["MCPS", "HOOKS", "SKILLS", "RULES", "AGENTS"] {
            env.push((format!("GROK_{vendor}_{what}_ENABLED"), "0".into()));
        }
    }
    env
}

/// A file only the user can read, made or written over.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// A tool as Grok names it, as Muxin knows it. Grok calls an MCP tool
/// through its own `use_tool`: `{"tool_name": "muxin__status", "tool_input":
/// {…}}`. (One called by its full name directly, or with its arguments under
/// another name, is taken too.) A tool of Grok's own keeps its name.
fn tool(name: &str, input: &Value) -> (String, Value) {
    let ours = |full: &str| full.strip_prefix(&format!("{SERVER}__")).map(str::to_string);
    if let Some(name) = ours(name) {
        return (name, input.clone());
    }
    if name == CALL
        && let Some(inner) = ["tool_name", "name", "tool"].iter().find_map(|key| input[*key].as_str()).and_then(ours)
    {
        let args = ["tool_input", "arguments", "args", "input"].iter().map(|key| &input[*key]).find(|value| !value.is_null());
        let args = match args {
            // Some models write the arguments as text.
            Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
            Some(other) => other.clone(),
            None => json!({}),
        };
        return (inner, args);
    }
    if name == FIND {
        return (LOOKUP.to_string(), input.clone());
    }
    (name.to_string(), input.clone())
}

/// What one line of Grok's output means: what to report, and whether the
/// turn is over (and how).
pub(crate) fn read(line: &Value) -> (Vec<Event>, Option<Result<(), String>>) {
    let (mut events, end) = super::messages::read(line, "Grok Build", &tool);
    for event in &mut events {
        if let Event::Result { text, .. } = event {
            *text = result(text);
        }
    }
    (events, end.map(|end| end.map_err(|why| hint(plain(&why)))))
}

/// What a tool answered, as the tool said it. Grok hands an MCP tool's
/// result on inside an envelope of its own:
/// `{"type":"MCP","tool_name":…,"output":{"OkayOutput":"…"}}`, with
/// `{"Error":"…"}` in its place for one that failed.
fn result(text: &str) -> String {
    let Ok(envelope) = serde_json::from_str::<Value>(text) else { return text.to_string() };
    if envelope["type"] != "MCP" {
        return text.to_string();
    }
    match envelope["output"].as_object().and_then(|output| output.values().next()) {
        Some(Value::String(said)) => said.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => text.to_string(),
    }
}

/// A failure in words. Grok wraps what its servers answered in "Internal
/// error: {...}", the words themselves under `message`.
fn plain(why: &str) -> String {
    let inner = why.find('{').and_then(|at| serde_json::from_str::<Value>(&why[at..]).ok());
    match inner.as_ref().and_then(|json| json["message"].as_str()).map(str::trim).filter(|m| !m.is_empty()) {
        Some(message) => message.to_string(),
        None => why.trim().to_string(),
    }
}

/// Looking a tool up is Grok's own business, not something done to the
/// user's machines: it's left out of the chat, with what it found.
fn shown(event: Event, lookups: &mut HashSet<String>) -> Option<Event> {
    match &event {
        Event::Tool { id, name, .. } if name == LOOKUP => {
            lookups.insert(id.clone());
            None
        }
        Event::Result { id, .. } if lookups.contains(id) => None,
        _ => Some(event),
    }
}

/// A failure's words, with what to do when it reads like Grok isn't signed in.
fn hint(why: String) -> String {
    let low = why.to_lowercase();
    if ["log in", "login", "logged in", "sign in", "signed in", "unauthorized", "401", "credential", "authenticat"].iter().any(|sign| low.contains(sign)) {
        format!("{why}\nIf Grok isn't signed in, run `grok login` in a terminal, then try again.")
    } else {
        why
    }
}

#[async_trait::async_trait]
impl Brain for Grok {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        let pictures = std::mem::take(&mut self.attached);
        let message = self.prepare(text, &pictures)?;
        let mut child = tokio::process::Command::new(&self.program)
            .args(self.args(&message))
            .current_dir(&self.setup.work)
            .envs(env())
            .env("PATH", super::path_for_child())
            // Nobody is there to answer it; whatever it would ask, it gets no for.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Couldn't start Grok Build ({}): {e}", self.program.display()))?;
        let stdout = child.stdout.take().ok_or("Grok Build started without a way to hear it.")?;
        let stderr = child.stderr.take();
        let errors = tokio::spawn(async move {
            let mut said = String::new();
            if let Some(mut err) = stderr {
                let _ = err.read_to_string(&mut said).await;
            }
            said
        });
        self.child = Some(child);

        let mut lines = BufReader::new(stdout).lines();
        let mut lookups = HashSet::new();
        let mut end = None;
        while let Ok(Some(text)) = lines.next_line().await {
            let Ok(line) = serde_json::from_str::<Value>(&text) else { continue };
            let (said, done) = read(&line);
            for event in said.into_iter().filter_map(|event| shown(event, &mut lookups)) {
                if let Event::Session(id) = &event {
                    self.session = Some(id.clone());
                }
                let _ = events.send(event);
            }
            if done.is_some() {
                end = done;
            }
        }
        let status = match self.child.take() {
            Some(mut child) => child.wait().await.ok(),
            None => None,
        };
        match end {
            Some(end) => end,
            None => {
                let said = tail(&errors.await.unwrap_or_default(), 6);
                let code = status.and_then(|s| s.code()).map(|c| format!(" (exit code {c})")).unwrap_or_default();
                Err(hint(if said.is_empty() { format!("Grok Build ended before it answered{code}.") } else { format!("Grok Build ended before it answered{code}:\n{said}") }))
            }
        }
    }

    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    // Each turn is its own process, started with what's chosen now.
    fn choose(&mut self, model: Option<String>, effort: Option<String>) {
        self.setup.model = model;
        self.setup.effort = effort;
    }

    fn attach(&mut self, files: Vec<Attached>) -> Result<(), String> {
        if let Some(file) = files.iter().find(|f| !matches!(f.kind, Kind::Image(_))) {
            return Err(format!("Grok Build can't be given {} here: it takes pictures and text. Claude Code and your ChatGPT plan read PDFs.", file.name));
        }
        let size: u64 = files.iter().filter_map(|f| std::fs::metadata(&f.path).ok()).map(|m| m.len()).sum();
        if size > MOST_PICTURES {
            return Err(format!("Grok Build takes pictures up to {} KB together here, and these are {} KB. Attach a smaller one, or ask Claude Code, Codex or your ChatGPT plan.", MOST_PICTURES / 1024, size.div_ceil(1024)));
        }
        self.attached = files;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup { exe: "/opt/Muxin \"beta\"/muxin".into(), bridge: "/tmp/c.sock".into(), work: "/tmp/chats/c-1/work".into(), prompt: "You are Muxin's assistant.".into(), ..Default::default() }
    }

    #[test]
    fn it_starts_as_a_model_with_only_muxins_tools() {
        let mut grok = Grok::new("grok".into(), setup());
        let args = grok.args(&Prompt::File("/tmp/chats/c-1/grok-message.txt".into()));
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        // The message is a file's, taken as it is.
        assert_eq!(after("--prompt-file"), Some("/tmp/chats/c-1/grok-message.txt"));
        assert!(args.contains(&"--verbatim".to_string()));
        assert_eq!(after("--output-format"), Some("streaming-messages-json"));
        // No tools of its own but its way to MCP tools; only Muxin's run, and it asks about none.
        assert_eq!(after("--tools"), Some("search_tool,use_tool"));
        assert_eq!(after("--permission-mode"), Some("dontAsk"));
        assert_eq!(after("--allow"), Some("MCPTool(muxin__*)"));
        for off in ["--disable-web-search", "--no-subagents", "--no-plan"] {
            assert!(args.contains(&off.to_string()), "{off}");
        }
        assert!(!args.iter().any(|a| a == "--always-approve" || a == "--yolo" || a == "bypassPermissions"));
        let prompt = after("--system-prompt-override").unwrap();
        assert!(prompt.starts_with("You are Muxin's assistant.\n\n") && prompt.contains("muxin__status"));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"-m".to_string()) && !args.contains(&"--reasoning-effort".to_string()));
        // A conversation it had before is continued, with the model and effort chosen since.
        grok.session = Some("0198c5a1-77aa".into());
        grok.choose(Some("grok-4.6".into()), Some("low".into()));
        let args = grok.args(&Prompt::File("m.txt".into()));
        assert_eq!(args[args.len() - 2..], ["--resume", "0198c5a1-77aa"]);
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!((after("-m"), after("--reasoning-effort")), (Some("grok-4.6"), Some("low")));
    }


    /// A picture and a PDF in a folder of their own, as a chat keeps them.
    fn attached(name: &str) -> Vec<Attached> {
        let dir = std::env::temp_dir().join(format!("muxin-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("shot.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join("paper.pdf"), b"%PDF").unwrap();
        vec![Attached { name: "shot.png".into(), path: dir.join("shot.png"), kind: Kind::Image("image/png") }, Attached { name: "paper.pdf".into(), path: dir.join("paper.pdf"), kind: Kind::Pdf }]
    }

    #[test]
    fn pictures_go_in_the_prompt_as_blocks_and_what_it_cant_take_is_said() {
        let mut grok = Grok::new("grok".into(), setup());
        let files = attached("grok-attach");
        let why = grok.attach(files.clone()).unwrap_err();
        assert!(why.starts_with("Grok Build can't be given paper.pdf here"), "{why}");
        // As it took them from a real run: the Agent Client Protocol's blocks.
        let prompt = blocks("What color?", &files[..1]).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&prompt).unwrap(), json!([{"type": "text", "text": "What color?"}, {"type": "image", "data": "AQID", "mimeType": "image/png"}]));
        let args = grok.args(&Prompt::Blocks(prompt.clone()));
        assert_eq!(args[..3], ["--prompt-json".to_string(), prompt, "--verbatim".to_string()]);
        // A command line has an end: pictures too big for it are refused, with what else could take them.
        let big = files[0].path.with_file_name("big.png");
        std::fs::write(&big, vec![0u8; 700 * 1024]).unwrap();
        let why = grok.attach(vec![Attached { name: "big.png".into(), path: big, kind: Kind::Image("image/png") }]).unwrap_err();
        assert!(why.starts_with("Grok Build takes pictures up to 600 KB together here, and these are 700 KB."), "{why}");
        assert!(grok.attached.is_empty());
        grok.attach(vec![files[0].clone()]).unwrap();
        assert_eq!(grok.attached.len(), 1);
    }

    #[test]
    fn the_chats_folder_tells_grok_where_muxins_tools_are() {
        let config = Grok::new("grok".into(), setup()).config();
        // Paths with quotes or spaces stay one TOML value each.
        let read: toml::Table = toml::from_str(&config).expect(&config);
        let server = &read["mcp_servers"]["muxin"];
        assert_eq!(server["command"].as_str(), Some("/opt/Muxin \"beta\"/muxin"));
        assert_eq!(server["args"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect::<Vec<_>>(), ["mcp", "--chat", "/tmp/c.sock"]);
        // A call may wait hours for the user's answer.
        assert_eq!(server["tool_timeout_sec"].as_integer(), Some(21600));

        let env = env();
        let set = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(set("GROK_FOLDER_TRUST"), Some("0"));
        assert_eq!(set("GROK_MEMORY"), Some("0"));
        for key in ["GROK_CLAUDE_HOOKS_ENABLED", "GROK_CLAUDE_MCPS_ENABLED", "GROK_CURSOR_MCPS_ENABLED", "GROK_CURSOR_RULES_ENABLED", "GROK_CLAUDE_SKILLS_ENABLED"] {
            assert_eq!(set(key), Some("0"), "{key}");
        }
        assert_eq!(env.len(), 14);
    }

    #[test]
    fn a_tool_is_known_by_muxins_name_for_it_however_grok_calls_it() {
        let restart = json!({"runner_id": "beelink/beelink-01", "action": "restart"});
        // By its full name.
        assert_eq!(tool("muxin__runner_action", &restart), ("runner_action".to_string(), restart.clone()));
        // Through Grok's own use_tool, as it does; and with the arguments under another name, or as text.
        assert_eq!(tool("use_tool", &json!({"tool_name": "muxin__runner_action", "tool_input": restart})), ("runner_action".to_string(), restart.clone()));
        assert_eq!(tool("use_tool", &json!({"tool_name": "muxin__runner_action", "arguments": restart})), ("runner_action".to_string(), restart.clone()));
        assert_eq!(tool("use_tool", &json!({"name": "muxin__runner_action", "input": restart.to_string()})), ("runner_action".to_string(), restart.clone()));
        assert_eq!(tool("use_tool", &json!({"tool_name": "muxin__status", "tool_input": {}})), ("status".to_string(), json!({})));
        assert_eq!(tool("use_tool", &json!({"tool_name": "muxin__status"})), ("status".to_string(), json!({})));
        // Muxin's own use_tool, reached either way, is still to be unwrapped (chat/tools.rs does).
        let list = json!({"name": "snippet_list"});
        assert_eq!(tool("muxin__use_tool", &list), ("use_tool".to_string(), list.clone()));
        assert_eq!(tool("use_tool", &json!({"tool_name": "muxin__use_tool", "tool_input": list})), ("use_tool".to_string(), list));
        // Another server's tool, which Grok refuses here, is shown as it was called.
        let other = json!({"tool_name": "stripe__refund", "tool_input": {}});
        assert_eq!(tool("use_tool", &other), ("use_tool".to_string(), other.clone()));
        assert_eq!(tool("search_tool", &json!({"query": "runners"})).0, LOOKUP);
    }

    /// Lines as Grok Build 1.0.77 printed them, shortened: one lookup, two
    /// calls at once (one of them failing), and the answer.
    #[test]
    fn a_turn_reads_as_text_tools_and_usage_without_its_lookups() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"01a1d692-7de5","apiKeySource":"oauth","model":"grok-4.7","cwd":"/tmp/chats/c-1/work","permissionMode":"dontAsk","tools":["search_tool","use_tool"],"slash_commands":["compact"],"mcp_servers":[{"name":"stripe","status":"pending"},{"name":"muxin","status":"pending"}],"skills":["animate"],"uuid":"u1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_0","type":"message","role":"assistant","model":"grok-4.7","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I'll check your saved snippets"}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_0","type":"message","role":"assistant","model":"grok-4.7","content":[{"type":"thinking","thinking":"The user wants to know which snippets they have saved"},{"type":"text","text":"I'll check your saved snippets and one machine."},{"type":"tool_use","id":"call-272a18cc-0","name":"search_tool","input":{"query":"muxin snippets list get machine","limit":10}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":16270,"output_tokens":85,"cache_read_input_tokens":1152,"cache_creation_input_tokens":0}},"parent_tool_use_id":null,"session_id":"01a1d692-7de5","uuid":"u2"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-272a18cc-0","content":"{\"type\":\"SearchTool\",\"result_count\":10,\"content\":\"{\\n  \\\"results\\\": [\\n    {\\n      \\\"server\\\": \\\"muxin\\\",\\n      \\\"tools\\\": [\\n        {\\n          \\\"tool_name\\\": \\\"muxin__snippet_list\\\"\\n        }\\n      ]\\n    }\\n  ]\\n}\"}"}]},"parent_tool_use_id":null,"session_id":"01a1d692-7de5","uuid":"u3"}"#,
            r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"grok-4.7","content":[{"type":"thinking","thinking":"I will call snippet_list and get_machine."},{"type":"tool_use","id":"call-af78f931-1","name":"use_tool","input":{"tool_name":"muxin__snippet_list","tool_input":{}}},{"type":"tool_use","id":"call-af78f931-2","name":"use_tool","input":{"tool_name":"muxin__get_machine","tool_input":{"machine_id":"nope"}}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":2018,"output_tokens":75,"cache_read_input_tokens":17408,"cache_creation_input_tokens":0}},"parent_tool_use_id":null,"session_id":"01a1d692-7de5","uuid":"u4"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-af78f931-1","content":"{\"type\":\"MCP\",\"tool_name\":\"snippet_list\",\"server_name\":\"muxin\",\"output\":{\"OkayOutput\":\"{\\\"snippets\\\":[],\\\"folders\\\":[]}\"}}","is_error":false},{"type":"tool_result","tool_use_id":"call-af78f931-2","content":"{\"type\":\"MCP\",\"tool_name\":\"get_machine\",\"server_name\":\"muxin\",\"output\":{\"Error\":\"{\\\"error\\\":{\\\"code\\\":\\\"not_found\\\",\\\"message\\\":\\\"No machine \\\\\\\"nope\\\\\\\".\\\"}}\"},\"is_error\":true}","is_error":true}]},"parent_tool_use_id":null,"session_id":"01a1d692-7de5","uuid":"u5"}"#,
            r#"{"type":"assistant","message":{"id":"msg_3","type":"message","role":"assistant","model":"grok-4.7","content":[{"type":"thinking","thinking":"The user requested two short lines."},{"type":"text","text":"You have no saved snippets.\nThere's no machine \"nope\"."}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":2746,"output_tokens":196,"cache_read_input_tokens":21120,"cache_creation_input_tokens":0}},"parent_tool_use_id":null,"session_id":"01a1d692-7de5","uuid":"u6"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":13401,"duration_api_ms":11021,"num_turns":4,"result":"You have no saved snippets.\nThere's no machine \"nope\".","stop_reason":"end_turn","total_cost_usd":0.078566,"usage":{"input_tokens":22950,"output_tokens":527,"cache_read_input_tokens":59008,"cache_creation_input_tokens":0,"server_tool_use":{"web_search_requests":0}},"modelUsage":{"grok-4.7-build":{"inputTokens":22950,"outputTokens":527,"cacheReadInputTokens":59008,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.078566}},"session_id":"01a1d692-7de5","uuid":"u7"}"#,
        ];
        let mut all = Vec::new();
        let mut lookups = HashSet::new();
        let mut end = None;
        for line in lines {
            let (events, done) = read(&serde_json::from_str(line).unwrap());
            all.extend(events.into_iter().filter_map(|event| shown(event, &mut lookups)));
            end = end.or(done);
        }
        assert_eq!(
            all,
            [
                Event::Session("01a1d692-7de5".into()),
                Event::Model("grok-4.7".into()),
                Event::Delta("I'll check your saved snippets".into()),
                Event::Text("I'll check your saved snippets and one machine.".into()),
                // The lookup and what it found aren't here. The calls are, by Muxin's names, with what the tools themselves said.
                Event::Tool { id: "call-af78f931-1".into(), name: "snippet_list".into(), args: json!({}) },
                Event::Tool { id: "call-af78f931-2".into(), name: "get_machine".into(), args: json!({"machine_id": "nope"}) },
                Event::Result { id: "call-af78f931-1".into(), text: r#"{"snippets":[],"folders":[]}"#.into(), is_error: false },
                Event::Result { id: "call-af78f931-2".into(), text: r#"{"error":{"code":"not_found","message":"No machine \"nope\"."}}"#.into(), is_error: true },
                Event::Text("You have no saved snippets.\nThere's no machine \"nope\".".into()),
                Event::Usage { input: 81958, output: 527, cached: 59008, cost: Some(0.078566) },
            ]
        );
        assert_eq!(end, Some(Ok(())));
    }

    #[test]
    fn a_result_is_what_the_tool_said_not_groks_envelope_for_it() {
        assert_eq!(result(r#"{"type":"MCP","tool_name":"status","server_name":"muxin","output":{"OkayOutput":"9 machines"}}"#), "9 machines");
        assert_eq!(result(r#"{"type":"MCP","tool_name":"status","server_name":"muxin","output":{"Error":"no"},"is_error":true}"#), "no");
        // What isn't that envelope stays as it is: a tool's own JSON, plain words, a lookup's findings.
        for other in [r#"{"type":"SearchTool","result_count":1,"content":"{}"}"#, r#"{"summary":"9 machines"}"#, "9 machines", r#"{"type":"MCP","output":{}}"#] {
            assert_eq!(result(other), other);
        }
    }

    /// Both lines as Grok Build 0.2.118 printed them, when xAI's servers
    /// wouldn't take that version any more (ids shortened).
    #[test]
    fn a_turn_its_servers_refuse_says_what_they_said() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"01a1d686-c796","apiKeySource":"oauth","model":"grok-4.7","cwd":"/tmp/chats/c-1/work","permissionMode":"dontAsk","tools":["search_tool","use_tool"],"slash_commands":[],"mcp_servers":[{"name":"muxin","status":"connected"}],"skills":[],"uuid":"u1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":826,"duration_api_ms":0,"num_turns":0,"stop_reason":null,"total_cost_usd":0.0,"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"server_tool_use":{"web_search_requests":0}},"modelUsage":{},"errors":["Internal error: {\n  \"message\": \"API error (status 426 Upgrade Required): Your Grok CLI version (0.2.118) is outdated. Please update to version 1.0.13 or later via `grok update` or the installation documentation.\",\n  \"http_status\": 426\n}"],"session_id":"01a1d686-c796","uuid":"u2"}"#,
        ];
        let (events, end) = read(&serde_json::from_str(lines[0]).unwrap());
        assert_eq!(events, [Event::Session("01a1d686-c796".into()), Event::Model("grok-4.7".into())]);
        assert_eq!(end, None);
        // Nothing was used, so nothing is said about use; and the reason is the words, not the JSON around them.
        let (events, end) = read(&serde_json::from_str(lines[1]).unwrap());
        assert_eq!(events, []);
        assert_eq!(end, Some(Err("API error (status 426 Upgrade Required): Your Grok CLI version (0.2.118) is outdated. Please update to version 1.0.13 or later via `grok update` or the installation documentation.".into())));
        assert_eq!(plain("  stream closed {not json  "), "stream closed {not json");
    }

    #[test]
    fn a_failure_that_reads_like_no_sign_in_says_how_to_sign_in() {
        let (_, end) = read(&json!({"type": "error", "message": "Couldn't start session: not logged in"}));
        assert_eq!(end, Some(Err("Couldn't start session: not logged in\nIf Grok isn't signed in, run `grok login` in a terminal, then try again.".into())));
        let (_, end) = read(&json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["The model is overloaded."]}));
        assert_eq!(end, Some(Err("The model is overloaded.".into())));
        assert_eq!(hint("Grok Build ended before it answered (exit code 1):\n401 Unauthorized".into()).lines().count(), 3);
    }

    /// A stand-in for the program: it checks how it was started, and answers
    /// in the real one's shapes.
    #[cfg(unix)]
    const STAND_IN: &str = r#"#!/bin/sh
message=""; resumed="new"
while [ $# -gt 0 ]; do
  case "$1" in
    --prompt-file) message=$(cat "$2"); shift ;;
    --resume) resumed="$2"; shift ;;
  esac
  shift
done
[ "$GROK_FOLDER_TRUST" = "0" ] || { echo "the folder's config would not count" >&2; exit 1; }
[ "$GROK_CLAUDE_HOOKS_ENABLED" = "0" ] || { echo "Claude Code's hooks would run" >&2; exit 1; }
grep -q '^\[mcp_servers.muxin\]' .grok/config.toml || { echo "no server in $(pwd)/.grok/config.toml" >&2; exit 1; }
if [ "$message" = "fail" ]; then echo "Error: not logged in. Run grok login." >&2; exit 1; fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-1","model":"grok-build"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_0","name":"search_tool","input":{"query":"status"}}]}}'
printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_0","content":"muxin__status"}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_1","name":"use_tool","input":{"tool_name":"muxin__status","tool_input":{"refresh":true}}}]}}'
printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","content":"{\"type\":\"MCP\",\"tool_name\":\"status\",\"server_name\":\"muxin\",\"output\":{\"OkayOutput\":\"9 machines\"}}","is_error":false}]}}'
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"%s (%s)"}]}}\n' "$message" "$resumed"
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0,"usage":{"input_tokens":800,"cache_read_input_tokens":4000,"output_tokens":20}}'
"#;

    #[cfg(unix)]
    async fn turn(grok: &mut Grok, text: &str) -> (Vec<Event>, Result<(), String>) {
        // A script just written can be "busy" for a moment while another test starts a program.
        for _ in 0..40 {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let outcome = grok.turn(text, &tx).await;
            drop(tx);
            if outcome.as_ref().is_err_and(|why| why.contains("busy")) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            let mut seen = Vec::new();
            while let Some(event) = rx.recv().await {
                seen.push(event);
            }
            return (seen, outcome);
        }
        panic!("the stand-in never started");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_runs_the_program_in_the_chats_folder_and_the_next_resumes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muxin-grok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("c-1").join("work");
        std::fs::create_dir_all(&work).unwrap();
        let program = dir.join("grok");
        std::fs::write(&program, STAND_IN).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let setup = Setup { exe: "/opt/muxin".into(), bridge: dir.join("c.sock"), work: work.clone(), prompt: "You are Muxin's assistant.".into(), ..Default::default() };
        let mut grok = Grok::new(program, setup);

        let (seen, outcome) = turn(&mut grok, "how many machines?").await;
        assert_eq!(outcome, Ok(()), "{seen:?}");
        assert_eq!(
            seen,
            [
                Event::Session("s-1".into()),
                Event::Model("grok-build".into()),
                Event::Tool { id: "call_1".into(), name: "status".into(), args: json!({"refresh": true}) },
                Event::Result { id: "call_1".into(), text: "9 machines".into(), is_error: false },
                Event::Text("how many machines? (new)".into()),
                Event::Usage { input: 4800, output: 20, cached: 4000, cost: None },
            ]
        );
        // The message was a file only the user can read, beside the folder Grok ran in.
        let message = dir.join("c-1").join("grok-message.txt");
        assert_eq!(std::fs::read_to_string(&message).unwrap(), "how many machines?");
        assert_eq!(std::fs::metadata(&message).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(work.join(".grok/config.toml").is_file());

        // The next turn goes on with the session the first one named.
        let (seen, outcome) = turn(&mut grok, "and runners?").await;
        assert_eq!(outcome, Ok(()));
        assert!(seen.contains(&Event::Text("and runners? (s-1)".into())), "{seen:?}");

        // One that ends without an answer says what it printed, and how to sign in when that's what it reads like.
        let (seen, outcome) = turn(&mut grok, "fail").await;
        assert!(seen.is_empty());
        assert_eq!(outcome, Err("Grok Build ended before it answered (exit code 1):\nError: not logged in. Run grok login.\nIf Grok isn't signed in, run `grok login` in a terminal, then try again.".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
