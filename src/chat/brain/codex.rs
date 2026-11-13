//! Codex as a chat's brain, on the user's own ChatGPT plan.
//!
//! Codex has no way to keep one process for a whole conversation without its
//! terminal, so each turn is a `codex exec --json` that ends with the turn;
//! the next one is `codex exec resume <thread id>`, which continues the same
//! thread. What it does comes as lines of JSON on stdout. Started so that:
//!
//! - `--ignore-user-config`: Muxin's bridge is its only MCP server, whatever
//!   the user has set up for themselves (it stays signed in as they are).
//! - `mcp_servers.muxin.default_tools_approval_mode = "approve"`: Codex
//!   itself refuses a tool marked as changing things when nobody can approve
//!   it; this leaves the asking to the chat's gate, inside the tool call.
//! - `tool_timeout_sec`: a call may wait a long while for the user's answer.
//! - `sandbox_mode = "read-only"`: Codex keeps its own shell (it has no
//!   switch for that), but it can only look, and only on this computer.
//! - `developer_instructions`: Muxin's words on who it is here.
//!
//! Its messages arrive whole, not word by word.

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

use super::{Brain, Event, Setup, result_text, tail};

const SERVER: &str = "muxin";
/// How long a tool call may take, waiting for the user included.
const TOOL_TIMEOUT_SECS: u64 = 6 * 3600;

pub struct Codex {
    program: PathBuf,
    setup: Setup,
    /// Codex's id for the conversation, once it said it.
    thread: Option<String>,
    /// The turn's process, kept here so a turn that was dropped halfway
    /// (the user stopped it) can still be ended by `stop`.
    child: Option<Child>,
}

impl Codex {
    pub fn new(program: PathBuf, setup: Setup) -> Codex {
        let thread = setup.resume.clone();
        Codex { program, setup, thread, child: None }
    }

    /// What Codex is started with for a turn. The message itself goes in on
    /// stdin (`-`), not here, where other users of the computer could read it.
    fn args(&self) -> Vec<String> {
        // Each value is TOML; a JSON string or list of strings is one.
        let set = |key: &str, value: Value| ["-c".to_string(), format!("mcp_servers.{SERVER}.{key}={value}")];
        let mut args = vec!["exec".to_string()];
        if let Some(id) = &self.thread {
            args.extend(["resume".to_string(), id.clone()]);
        }
        args.extend(["--json", "--skip-git-repo-check", "--ignore-user-config"].map(String::from));
        args.extend(set("command", json!(self.setup.exe.to_string_lossy())));
        args.extend(set("args", json!(["mcp", "--chat", self.setup.bridge.to_string_lossy()])));
        args.extend(set("default_tools_approval_mode", json!("approve")));
        args.extend(set("tool_timeout_sec", json!(TOOL_TIMEOUT_SECS)));
        args.extend(["-c".to_string(), "sandbox_mode=\"read-only\"".to_string()]);
        args.extend(["-c".to_string(), format!("developer_instructions={}", json!(self.setup.prompt))]);
        args.push("-".to_string());
        args
    }
}

/// What one line of Codex's output means: what to report, and whether the
/// turn is over (and how).
pub(crate) fn read(line: &Value) -> (Vec<Event>, Option<Result<(), String>>) {
    let mut events = Vec::new();
    let item = &line["item"];
    let id = || item["id"].as_str().unwrap_or_default().to_string();
    match line["type"].as_str().unwrap_or("") {
        "thread.started" => {
            if let Some(id) = line["thread_id"].as_str() {
                events.push(Event::Session(id.to_string()));
            }
        }
        "item.started" => match item["type"].as_str() {
            Some("mcp_tool_call") => events.push(Event::Tool { id: id(), name: item["tool"].as_str().unwrap_or_default().to_string(), args: item["arguments"].clone() }),
            // Its own shell, which can only look.
            Some("command_execution") => events.push(Event::Tool { id: id(), name: "codex_shell".into(), args: json!({ "command": item["command"] }) }),
            _ => {}
        },
        "item.completed" => match item["type"].as_str() {
            Some("agent_message") => events.push(Event::Text(item["text"].as_str().unwrap_or_default().trim_end().to_string())),
            Some("mcp_tool_call") => {
                let failed = item["status"] == "failed" || !item["error"].is_null() || item["result"]["is_error"] == true || item["result"]["isError"] == true;
                let text = match item["error"]["message"].as_str() {
                    Some(why) => why.to_string(),
                    None => result_text(&item["result"]["content"]),
                };
                events.push(Event::Result { id: id(), text, is_error: failed });
            }
            Some("command_execution") => {
                let failed = item["exit_code"].as_i64().is_some_and(|c| c != 0) || item["status"] == "failed";
                events.push(Event::Result { id: id(), text: item["aggregated_output"].as_str().unwrap_or_default().to_string(), is_error: failed });
            }
            _ => {}
        },
        "turn.completed" => {
            let u = &line["usage"];
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            events.push(Event::Usage { input: n("input_tokens"), output: n("output_tokens"), cached: n("cached_input_tokens"), cost: None });
            return (events, Some(Ok(())));
        }
        "turn.failed" => return (events, Some(Err(line["error"]["message"].as_str().unwrap_or("Codex couldn't finish the turn.").to_string()))),
        "error" => return (events, Some(Err(line["message"].as_str().unwrap_or("Codex reported an error.").to_string()))),
        _ => {}
    }
    (events, None)
}

#[async_trait::async_trait]
impl Brain for Codex {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        let mut child = tokio::process::Command::new(&self.program)
            .args(self.args())
            .current_dir(&self.setup.work)
            .env("PATH", super::path_for_child())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Couldn't start Codex ({}): {e}", self.program.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // Closing it is what tells Codex the message is complete.
            let _ = stdin.write_all(text.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        let stdout = child.stdout.take().ok_or("Codex started without a way to hear it.")?;
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
        let mut end = None;
        while let Ok(Some(text)) = lines.next_line().await {
            let Ok(line) = serde_json::from_str::<Value>(&text) else { continue };
            let (said, done) = read(&line);
            for event in said {
                if let Event::Session(id) = &event {
                    self.thread = Some(id.clone());
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
                Err(if said.is_empty() { format!("Codex ended before it answered{code}.") } else { format!("Codex ended before it answered{code}:\n{said}") })
            }
        }
    }

    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup { exe: "/opt/Muxin \"beta\"/muxin".into(), bridge: "/tmp/c.sock".into(), work: "/tmp/chats/c-1/work".into(), prompt: "You are Muxin's assistant.\nBe brief.".into(), resume: None }
    }

    #[test]
    fn a_first_turn_starts_a_thread_and_later_ones_resume_it() {
        let mut codex = Codex::new("codex".into(), setup());
        let args = codex.args();
        assert_eq!(args[..2], ["exec", "--json"]);
        assert_eq!(args.last().map(String::as_str), Some("-"));
        // Paths and words with quotes or new lines stay one TOML value each.
        assert!(args.contains(&r#"mcp_servers.muxin.command="/opt/Muxin \"beta\"/muxin""#.to_string()), "{args:?}");
        assert!(args.contains(&r#"mcp_servers.muxin.args=["mcp","--chat","/tmp/c.sock"]"#.to_string()));
        assert!(args.contains(&r#"mcp_servers.muxin.default_tools_approval_mode="approve""#.to_string()));
        assert!(args.contains(&r#"developer_instructions="You are Muxin's assistant.\nBe brief.""#.to_string()));
        assert!(args.contains(&r#"sandbox_mode="read-only""#.to_string()));
        assert!(args.contains(&"--ignore-user-config".to_string()));

        codex.thread = Some("01a1d587-1799".into());
        assert_eq!(codex.args()[..4], ["exec", "resume", "01a1d587-1799", "--json"]);
    }

    /// Lines as Codex 0.166 printed them, shortened.
    #[test]
    fn a_turn_reads_as_text_tools_and_usage() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"01a1d587-1799"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I'll restart it.\n"}}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"mcp_tool_call","server":"muxin","tool":"runner_action","arguments":{"runner_id":"beelink/beelink-01","action":"restart"},"result":null,"error":null,"status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"muxin","tool":"runner_action","arguments":{"runner_id":"beelink/beelink-01","action":"restart"},"result":{"content":[{"type":"text","text":"restarted"}],"structured_content":null},"error":null,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"beelink-01 is back."}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":74067,"cached_input_tokens":62464,"output_tokens":114,"reasoning_output_tokens":16}}"#,
        ];
        let mut all = Vec::new();
        let mut end = None;
        for line in lines {
            let (events, done) = read(&serde_json::from_str(line).unwrap());
            all.extend(events);
            end = end.or(done);
        }
        assert_eq!(
            all,
            [
                Event::Session("01a1d587-1799".into()),
                Event::Text("I'll restart it.".into()),
                Event::Tool { id: "item_1".into(), name: "runner_action".into(), args: json!({"runner_id": "beelink/beelink-01", "action": "restart"}) },
                Event::Result { id: "item_1".into(), text: "restarted".into(), is_error: false },
                Event::Text("beelink-01 is back.".into()),
                Event::Usage { input: 74067, output: 114, cached: 62464, cost: None },
            ]
        );
        assert_eq!(end, Some(Ok(())));
    }

    #[test]
    fn a_refused_tool_and_a_failed_turn_say_why() {
        // What Codex says when it won't run a tool itself; with Muxin's settings it doesn't come to this.
        let line = json!({"type": "item.completed", "item": {"id": "item_2", "type": "mcp_tool_call", "tool": "runner_action", "result": null, "error": {"message": "MCP tool call requires approval, but approval policy is never"}, "status": "failed"}});
        assert_eq!(read(&line).0, [Event::Result { id: "item_2".into(), text: "MCP tool call requires approval, but approval policy is never".into(), is_error: true }]);
        // The chat's own no arrives as a result marked as an error.
        let line = json!({"type": "item.completed", "item": {"id": "item_3", "type": "mcp_tool_call", "tool": "runner_action", "result": {"content": [{"type": "text", "text": "The user said no"}], "is_error": true}, "error": null, "status": "completed"}});
        assert_eq!(read(&line).0, [Event::Result { id: "item_3".into(), text: "The user said no".into(), is_error: true }]);
        assert_eq!(read(&json!({"type": "turn.failed", "error": {"message": "You've hit your usage limit."}})).1, Some(Err("You've hit your usage limit.".into())));
        assert_eq!(read(&json!({"type": "error", "message": "stream disconnected"})).1, Some(Err("stream disconnected".into())));
    }
}
