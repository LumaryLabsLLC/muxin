//! Claude Code as a chat's brain, on the user's own Claude plan.
//!
//! One `claude` process per chat, kept for as long as the chat is open. It
//! runs without its terminal (`-p`), taking each message as a line of JSON on
//! stdin and reporting what it does as lines of JSON on stdout. Started so
//! that it is only a model with Muxin's tools:
//!
//! - `--tools ""`: none of its own tools (no shell, no file edits).
//! - `--strict-mcp-config --mcp-config <file>`: Muxin's bridge is its only
//!   MCP server, whatever the user has set up for themselves.
//! - `--allowedTools mcp__muxin`: it never asks about a tool itself. The
//!   chat's gate does the asking, inside the tool call.
//! - `--setting-sources project`, in the chat's own empty folder: none of the
//!   user's hooks, plugins or CLAUDE.md come along.
//! - `--system-prompt`: Muxin's words instead of a coding agent's.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;

use super::{Brain, Event, Setup, tail};
use crate::chat::attach::{Attached, Kind};

/// Tools arrive as `mcp__<server>__<tool>`; the server is "muxin".
const SERVER: &str = "muxin";

struct Running {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
}

pub struct Claude {
    program: PathBuf,
    setup: Setup,
    /// Claude Code's id for the conversation, once it said it.
    session: Option<String>,
    running: Option<Running>,
    /// The pictures and PDFs that go with the next message.
    attached: Vec<Attached>,
}

impl Claude {
    pub fn new(program: PathBuf, setup: Setup) -> Claude {
        let session = setup.resume.clone();
        Claude { program, setup, session, running: None, attached: Vec::new() }
    }

    /// What Claude Code is started with.
    fn args(&self, mcp_config: &str) -> Vec<String> {
        let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"].map(String::from).to_vec();
        args.extend(["--strict-mcp-config".into(), "--mcp-config".into(), mcp_config.to_string()]);
        args.extend(["--tools".into(), String::new(), "--allowedTools".into(), format!("mcp__{SERVER}")]);
        args.extend(["--setting-sources".into(), "project".into()]);
        args.extend(["--system-prompt".into(), self.setup.prompt.clone()]);
        if let Some(model) = &self.setup.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &self.setup.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(id) = &self.session {
            args.extend(["--resume".into(), id.clone()]);
        }
        args
    }

    fn spawn(&mut self) -> Result<(), String> {
        // Beside the chat's work folder, not in it: nothing for Claude Code to pick up.
        let config = self.setup.work.parent().unwrap_or(&self.setup.work).join("mcp.json");
        let servers = json!({ "mcpServers": { SERVER: { "command": self.setup.exe.to_string_lossy(), "args": ["mcp", "--chat", self.setup.bridge.to_string_lossy()] } } });
        std::fs::write(&config, servers.to_string()).map_err(|e| format!("Couldn't write {}: {e}", config.display()))?;
        let mut child = tokio::process::Command::new(&self.program)
            .args(self.args(&config.to_string_lossy()))
            .current_dir(&self.setup.work)
            .env("PATH", super::path_for_child())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Couldn't start Claude Code ({}): {e}", self.program.display()))?;
        let stdin = child.stdin.take().ok_or("Claude Code started without a way to talk to it.")?;
        let stdout = child.stdout.take().ok_or("Claude Code started without a way to hear it.")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let kept = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut kept = kept.lock().unwrap();
                    kept.push_str(&line);
                    kept.push('\n');
                    // The end is what explains a failure.
                    if kept.len() > 16_000 {
                        let cut = kept.len() - 8_000;
                        let cut = (cut..kept.len()).find(|i| kept.is_char_boundary(*i)).unwrap_or(0);
                        kept.drain(..cut);
                    }
                }
            });
        }
        self.running = Some(Running { child, stdin, lines: BufReader::new(stdout).lines(), stderr });
        Ok(())
    }

    /// Why it ended, from what it printed on its way out.
    fn ended(&mut self) -> String {
        let said = self.running.take().map(|r| tail(&r.stderr.lock().unwrap(), 6)).unwrap_or_default();
        if said.is_empty() { "Claude Code ended before it answered.".into() } else { format!("Claude Code ended before it answered:\n{said}") }
    }
}

/// A message's blocks: its text, then each picture and PDF that goes with it.
fn content(text: &str, files: &[Attached]) -> Result<Value, String> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    for file in files {
        let (block, media) = match &file.kind {
            Kind::Image(media) => ("image", *media),
            Kind::Pdf => ("document", "application/pdf"),
            Kind::Text => continue,
        };
        blocks.push(json!({ "type": block, "source": { "type": "base64", "media_type": media, "data": file.base64()? } }));
    }
    Ok(Value::Array(blocks))
}

/// What one line of Claude Code's output means: what to report, and whether
/// the turn is over (and how). Its lines are read in messages.rs; what's
/// Claude Code's alone is here: how it names Muxin's tools, and what it says
/// about the user's plan.
pub(crate) fn read(line: &Value) -> (Vec<Event>, Option<Result<(), String>>) {
    if line["type"] == "rate_limit_event" {
        return (limit_text(&line["rate_limit_info"]).map(Event::Limit).into_iter().collect(), None);
    }
    super::messages::read(line, "Claude Code", &|name, input| (name.strip_prefix(&format!("mcp__{SERVER}__")).unwrap_or(name).to_string(), input.clone()))
}

/// A plan limit that's near or reached, in words; nothing while there's room.
fn limit_text(info: &Value) -> Option<String> {
    let status = info["status"].as_str()?;
    if status == "allowed" {
        return None;
    }
    let which = match info["rateLimitType"].as_str().unwrap_or("") {
        "seven_day" | "seven_day_opus" | "seven_day_sonnet" => "weekly",
        "five_hour" => "five-hour",
        _ => "usage",
    };
    let resets = info["resetsAt"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|t| t.with_timezone(&chrono::Local).format("%a %-d %b, %H:%M").to_string());
    let resets = resets.map(|r| format!(" It resets {r}.")).unwrap_or_default();
    Some(match info["utilization"].as_f64() {
        _ if status == "rejected" => format!("Your Claude plan's {which} limit is reached.{resets}"),
        Some(used) => format!("{:.0}% of your Claude plan's {which} limit is used.{resets}", used * 100.0),
        None => format!("Your Claude plan's {which} limit is close.{resets}"),
    })
}

#[async_trait::async_trait]
impl Brain for Claude {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        if self.running.as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_some()) {
            self.running = None;
        }
        if self.running.is_none() {
            self.spawn()?;
        }
        let message = json!({ "type": "user", "message": { "role": "user", "content": content(text, &std::mem::take(&mut self.attached))? } });
        let mut bytes = message.to_string().into_bytes();
        bytes.push(b'\n');
        let sent = {
            let running = self.running.as_mut().expect("just started");
            match running.stdin.write_all(&bytes).await {
                Ok(()) => running.stdin.flush().await,
                Err(e) => Err(e),
            }
        };
        if sent.is_err() {
            return Err(self.ended());
        }
        loop {
            let next = self.running.as_mut().expect("still here").lines.next_line().await;
            let Ok(Some(text)) = next else { return Err(self.ended()) };
            let Ok(line) = serde_json::from_str::<Value>(&text) else { continue };
            let (said, end) = read(&line);
            for event in said {
                if let Event::Session(id) = &event {
                    self.session = Some(id.clone());
                }
                let _ = events.send(event);
            }
            if let Some(end) = end {
                return end;
            }
        }
    }

    async fn stop(&mut self) {
        // It has no "stop this turn" that keeps the process: end it, and the
        // next turn starts another that continues the conversation.
        if let Some(mut running) = self.running.take() {
            let _ = running.child.start_kill();
            let _ = running.child.wait().await;
        }
    }

    // A picture or a PDF is a block of the message, beside its text.
    fn attach(&mut self, files: Vec<Attached>) -> Result<(), String> {
        self.attached = files;
        Ok(())
    }

    fn choose(&mut self, model: Option<String>, effort: Option<String>) {
        if (&model, &effort) == (&self.setup.model, &self.setup.effort) {
            return;
        }
        self.setup.model = model;
        self.setup.effort = effort;
        // A process keeps the model it was started with: this one ends
        // (it's killed when dropped), and the next turn starts another
        // that continues the conversation.
        self.running = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup { exe: "/Applications/Muxin.app/Contents/MacOS/muxin".into(), bridge: "/tmp/c.sock".into(), work: "/tmp/chats/c-1/work".into(), prompt: "You are Muxin's assistant.".into(), ..Default::default() }
    }

    #[test]
    fn it_starts_as_a_model_with_only_muxins_tools() {
        let mut claude = Claude::new("claude".into(), setup());
        let args = claude.args("/tmp/chats/c-1/mcp.json");
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!(after("--tools"), Some(""));
        assert_eq!(after("--allowedTools"), Some("mcp__muxin"));
        assert_eq!(after("--mcp-config"), Some("/tmp/chats/c-1/mcp.json"));
        assert_eq!(after("--setting-sources"), Some("project"));
        assert_eq!(after("--system-prompt"), Some("You are Muxin's assistant."));
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--model".to_string()) && !args.contains(&"--effort".to_string()));
        // A conversation it had before is continued, with the model and effort chosen since.
        claude.session = Some("b59117ac".into());
        claude.choose(Some("sonnet".into()), Some("high".into()));
        let args = claude.args("x");
        assert_eq!(args[args.len() - 2..], ["--resume", "b59117ac"]);
        let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
        assert_eq!((after("--model"), after("--effort")), (Some("sonnet"), Some("high")));
    }

    /// Lines as Claude Code 2.1 printed them, shortened.
    #[test]
    fn a_turn_reads_as_text_tools_and_usage() {
        let lines = [
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"init","session_id":"b59117ac-3099","tools":["mcp__muxin__status"],"model":"claude-opus-5-5"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01","name":"mcp__muxin__runner_action","input":{"runner_id":"beelink/beelink-01","action":"restart"}}]}}"#,
            r#"{"type":"tool_progress","tool_name":"mcp__muxin__runner_action","elapsed_time_seconds":30,"heartbeat":true}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01","content":[{"type":"text","text":"restarted"}]}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done: "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done: beelink-01 is back."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done: beelink-01 is back.","total_cost_usd":0.0475,"usage":{"input_tokens":12,"cache_read_input_tokens":9000,"cache_creation_input_tokens":300,"output_tokens":41}}"#,
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
                Event::Session("b59117ac-3099".into()),
                Event::Model("claude-opus-5-5".into()),
                Event::Tool { id: "toolu_01".into(), name: "runner_action".into(), args: json!({"runner_id": "beelink/beelink-01", "action": "restart"}) },
                Event::Result { id: "toolu_01".into(), text: "restarted".into(), is_error: false },
                Event::Delta("Done: ".into()),
                Event::Text("Done: beelink-01 is back.".into()),
                Event::Usage { input: 9312, output: 41, cached: 9000, cost: Some(0.0475) },
            ]
        );
        assert_eq!(end, Some(Ok(())));
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
    fn a_picture_and_a_pdf_are_blocks_of_the_message() {
        let blocks = content("What's this?", &attached("claude-attach")).unwrap();
        assert_eq!(blocks[0], json!({"type": "text", "text": "What's this?"}));
        assert_eq!(blocks[1], json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQID"}}));
        assert_eq!(blocks[2], json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERg=="}}));
        // With nothing attached, the message is its text.
        assert_eq!(content("hi", &[]).unwrap(), json!([{"type": "text", "text": "hi"}]));
        // They go with one message: taken by `attach`, used by the next turn.
        let mut claude = Claude::new("claude".into(), setup());
        claude.attach(attached("claude-attach")).unwrap();
        assert_eq!(claude.attached.len(), 2);
    }

    #[test]
    fn a_failed_turn_says_why() {
        let (_, end) = read(&json!({"type": "result", "subtype": "success", "is_error": true, "result": "Invalid API key · Please run /login"}));
        assert_eq!(end, Some(Err("Invalid API key · Please run /login".into())));
        let (_, end) = read(&json!({"type": "result", "subtype": "error_max_turns", "is_error": false}));
        assert_eq!(end, Some(Err("Claude Code stopped: error max turns.".into())));
        // A declined tool is a result the model reads, not a failed turn.
        let (events, end) = read(&json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "is_error": true, "content": "The user said no to this"}]}}));
        assert_eq!(events, [Event::Result { id: "t".into(), text: "The user said no to this".into(), is_error: true }]);
        assert_eq!(end, None);
    }

    #[test]
    fn a_plan_limit_is_said_only_when_its_near() {
        assert_eq!(limit_text(&json!({"status": "allowed", "rateLimitType": "five_hour"})), None);
        let near = limit_text(&json!({"status": "allowed_warning", "rateLimitType": "seven_day", "utilization": 0.86})).unwrap();
        assert_eq!(near, "86% of your Claude plan's weekly limit is used.");
        let reached = limit_text(&json!({"status": "rejected", "rateLimitType": "five_hour", "utilization": 1.0, "resetsAt": 1794776400})).unwrap();
        assert!(reached.starts_with("Your Claude plan's five-hour limit is reached. It resets "), "{reached}");
    }
}
