//! `muxin mcp`: a Model Context Protocol server over stdio, so agents
//! (Claude Code, Codex, any MCP client) can see and work with the user's machines.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout. Stdout carries
//! protocol messages only; logs go to stderr. Requests are handled
//! concurrently, so a long install doesn't block a `ping` or a status call.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::api::{self, ApiError, Surface};
use crate::fleet::Engine;

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
Muxin is one app for all of the user's machines (reached with the system ssh, or directly for \
host \"local\"): agent sessions, desktops, files, GitHub, workspaces, cloud machines, and the \
self-hosted GitHub Actions runners on them.

Start with status and read its `attention` list first; it names the problems worth fixing \
and the machine/runner they're about. Ids: machines are slugs like \"mac-studio\"; runners are \
\"<machine-id>/<runner-name>\", or \"github/<name>\" for a runner GitHub knows but no machine lists.

Read-only: status, get_machine, list_runners, list_jobs, check_machines (runs a read-only \
check over ssh), get_operation, list_operations, get_settings, list_ssh_hosts, list_files.
Changes things: add_machine / remove_machine edit config.toml (remove_machine also unpairs a \
machine paired for direct connections from this app). install_runners and \
runner_action run commands on real machines and register or deregister runners on GitHub; \
confirm with the user before stopping, removing or installing unless they asked for exactly that.

install_runners and runner_action wait for the operation by default and return a log tail; pass \
wait=false to get an operation_id and poll get_operation. Errors have a `code` (not_found, \
invalid, busy, github, unreachable, operation_failed, internal) and often a `hint` with the exact \
command that fixes the problem; show hints to the user rather than guessing.

Workspaces: you can also check out one of the user's machines as your own work environment. \
workspace_create makes a persistent folder there (optionally cloning a repo at a ref) and returns \
a workspace_id; workspace_exec runs shell commands in it with the machine user's login environment \
(use background=true for servers, then read the log file it returns); workspace_read_file, \
workspace_write_file and workspace_list_files work with its files; workspace_changes shows git \
status and diff; workspace_sync moves code between the user's local checkout and the workspace. \
Prefer one workspace per task, reuse it across steps, and call workspace_destroy when the task is \
done: nothing is deleted automatically. Commands run as the user on a real machine, so don't do \
anything there you wouldn't do in their own terminal without asking.

Agent sessions: long-running terminals on the user's machines, each running an AI agent CLI (Claude \
Code, Codex, OpenCode, Gemini CLI, Amp, Pi) or a plain shell, kept alive by Muxin's agent on \
the machine so they survive disconnects. The user watches and types into them in the Muxin app. session_list shows them \
(and which agents each machine has); session_start starts one, optionally with a first prompt; \
session_read returns its screen as text; session_send types into it; session_stop ends it. \
Sessions belong to the user: only start, type into or stop one when they asked you to.

Boxes: a task handed to an agent CLI that works on it by itself, in a container of its own on one \
of the user's machines (Docker), on a new branch of a fresh checkout, with its permission prompts \
off since the container is its sandbox. box_create makes one (repo, prompt, agent); box_list \
shows them (running, paused, git); box_send asks one more, starting it again when it's paused \
(a box pauses by itself when its agent has been quiet for a while); session_read and \
session_question work on a running box's session_id; box_destroy deletes one with its unpushed \
work. Only make or destroy boxes when the user asked.

GitHub: github_inbox (review requests, assigned issues, the user's PRs), github_repos (favorites first), github_repo \
(open issues and PRs, Actions runs, and where it's checked out on the machines), github_item (an \
issue or PR in full, by URL or owner/name#123). local_checkouts lists git checkouts on the machines; \
git_sync fetches, pulls or pushes one. To fix an issue, start a session in a checkout of its repo \
with the issue as the prompt.";

/// Per-connection state: who the client is, so workspaces and commands record their owner.
#[derive(Default)]
pub struct Session {
    client: Mutex<Option<String>>,
}

impl Session {
    /// A session for an app calling the tools in-process (Muxin for iPad):
    /// what it makes is the user's, "app", as the Mac app's is.
    #[cfg_attr(not(target_os = "ios"), allow(dead_code))]
    pub fn named(client: &str) -> Session {
        Session { client: Mutex::new(Some(client.to_string())) }
    }

    /// The MCP client's name from `initialize` (e.g. "claude-code"), else "mcp".
    pub fn owner(&self) -> String {
        self.client.lock().unwrap().clone().unwrap_or_else(|| "mcp".into())
    }
}

/// Serves MCP on this process's stdin/stdout until stdin closes.
pub async fn serve_stdio(engine: Arc<Engine>) -> anyhow::Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    serve(engine, stdin, tokio::io::stdout()).await
}

pub async fn serve<R, W>(engine: Arc<Engine>, mut reader: R, mut writer: W) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session = Arc::new(Session::default());
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap_or_default();
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(error_response(Value::Null, -32700, &format!("Parse error: {e}")));
                continue;
            }
        };
        let Some(obj) = msg.as_object() else {
            let _ = tx.send(error_response(Value::Null, -32600, "Expected a JSON-RPC object"));
            continue;
        };
        let method = obj.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = obj.get("id").cloned() else {
            // Notifications (initialized, cancelled, ...) need no reply.
            continue;
        };
        if method.is_empty() {
            // A response to something we never send; ignore.
            continue;
        }
        let engine = engine.clone();
        let session = session.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let reply = match handle(&engine, &session, &method, params).await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => error_response(id, code, &message),
            };
            let _ = tx.send(reply);
        });
    }
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn handle(engine: &Arc<Engine>, session: &Session, method: &str, params: Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let client = params.pointer("/clientInfo/name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty());
            if let Some(name) = client {
                *session.client.lock().unwrap() = Some(name.chars().take(64).collect());
            }
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
            let version = PROTOCOL_VERSIONS.iter().find(|v| **v == requested).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "muxin", "title": "Muxin", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "tools/call needs a tool name".to_string()))?;
            if !tools().iter().any(|t| t["name"] == name) {
                return Err((-32602, format!("Unknown tool: {name}")));
            }
            let args = params.get("arguments").cloned().filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
            Ok(call_tool(engine, session, name, args).await)
        }
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

/// Runs a tool and wraps the outcome as an MCP tool result. Failures are
/// results with `isError: true`, not protocol errors, so the model sees them.
pub async fn call_tool(engine: &Arc<Engine>, session: &Session, name: &str, args: Value) -> Value {
    // A screenshot comes back as an image the model can look at.
    if name == "desktop_screenshot" {
        let machine = args["machine_id"].as_str().unwrap_or("").to_string();
        let display = args["display"].as_str().map(str::to_string);
        return match api::desktop_screenshot(engine, &machine, display.as_deref()).await {
            Ok(shot) => {
                use base64::Engine as _;
                let about = format!(
                    "{}×{} screenshot of {machine} (screen {}×{}). desktop_input coordinates are in these pixels.",
                    shot.width, shot.height, shot.screen_width, shot.screen_height
                );
                json!({
                    "content": [
                        { "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&shot.jpeg), "mimeType": "image/jpeg" },
                        { "type": "text", "text": about },
                    ],
                    "structuredContent": serde_json::to_value(&shot).unwrap_or_default(),
                    "isError": false,
                })
            }
            Err(err) => tool_result(json!({ "error": err }), None, true),
        };
    }
    let outcome = dispatch(engine, session, name, args).await;
    match outcome {
        Ok(value) => {
            let text = readable_text(name, &value);
            tool_result(value, text, false)
        }
        Err(err) => tool_result(json!({ "error": err }), None, true),
    }
}

/// Plain text for results a model reads better unwrapped than as JSON: command
/// output, file contents, diffs. `structuredContent` always has the full object.
fn readable_text(tool: &str, v: &Value) -> Option<String> {
    let str_of = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "workspace_exec" => {
            let mut out = if let Some(pid) = v.get("background_pid").and_then(Value::as_u64) {
                format!("started in the background (pid {pid}), log: {}\n", str_of("log_path"))
            } else if v.get("timed_out").and_then(Value::as_bool) == Some(true) {
                format!("timed out after {} ms\n", v["duration_ms"])
            } else {
                format!("exit code: {} ({} ms)\n", v.get("exit_code").map(|c| c.to_string()).unwrap_or_else(|| "null".into()), v["duration_ms"])
            };
            if v.get("truncated").and_then(Value::as_bool) == Some(true) {
                out.push_str("(output truncated: the middle was dropped)\n");
            }
            for stream in ["stdout", "stderr"] {
                let text = str_of(stream);
                if !text.is_empty() {
                    out.push_str(&format!("\n{stream}:\n```\n{}\n```\n", text.trim_end_matches('\n')));
                }
            }
            if let Some(next) = v.get("next").and_then(Value::as_str) {
                out.push_str(&format!("\n{next}\n"));
            }
            Some(out)
        }
        "workspace_read_file" => {
            let mut out = format!("{} ({} bytes{})\n", str_of("path"), v["size"], if v["truncated"] == true { ", truncated" } else { "" });
            if v["binary"] == true {
                out.push_str("(binary file; content not shown)\n");
            } else {
                out.push_str(&format!("```\n{}\n```\n", str_of("content").trim_end_matches('\n')));
            }
            Some(out)
        }
        "workspace_changes" => Some(str_of("changes").to_string()),
        "session_read" => {
            let state = match (str_of("status"), v.get("busy").and_then(Value::as_bool)) {
                ("exited", _) => format!("exited (code {})", v.get("exit_code").map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
                (_, Some(true)) => "running, working".into(),
                _ => "running, idle".into(),
            };
            Some(format!("{} — {state}\n```\n{}\n```\n", str_of("session_id"), str_of("text")))
        }
        _ => None,
    }
}

fn tool_result(value: Value, text: Option<String>, is_error: bool) -> Value {
    let text = text.unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap_or_default());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": is_error,
    })
}

fn args<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|e| ApiError::invalid(format!("Bad arguments: {e}")))
}

fn ok<T: Serialize>(value: T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::invalid(e.to_string()))
}

async fn dispatch(engine: &Arc<Engine>, session: &Session, name: &str, a: Value) -> Result<Value, ApiError> {
    match name {
        "status" | "fleet_status" => ok(api::status(engine, args(a)?, Surface::Mcp).await?),
        "get_machine" => ok(api::machine(engine, args(a)?).await?),
        "list_runners" => ok(json!({ "runners": api::runners(engine, args(a)?).await? })),
        "list_jobs" => ok(api::jobs(engine, Surface::Mcp).await?),
        "check_machines" => ok(json!({ "machines": api::check(engine, args(a)?).await? })),
        "add_machine" => ok(api::add_machine(engine, args(a)?, Surface::Mcp).await?),
        "remove_machine" => ok(json!({ "removed": api::remove_machine(engine, args(a)?).await? })),
        "rename_machine" => ok(json!({ "renamed": api::rename_machine(engine, args(a)?).await? })),
        "connect_machine" => ok(json!({ "machine": api::connect_machine(engine, args(a)?).await? })),
        "install_runners" => ok(api::install_runners(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "runner_action" => ok(api::runner_action(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "cancel_run" => ok(api::cancel_run(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "cloud_options" => ok(api::cloud_options(engine).await),
        "cloud_usage" => ok(api::cloud_usage(engine).await),
        "cloud_create" => ok(api::cloud_create(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "cloud_destroy" => ok(api::cloud_destroy(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "cloud_power" => ok(api::cloud_power(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "machine_power" => ok(api::machine_power(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "vm_options" => ok(api::vm_options(engine).await),
        "schedule_list" => ok(api::schedule_list(engine, args(a)?).await?),
        "schedule_save" => ok(api::schedule_save(engine, args(a)?).await?),
        "schedule_delete" => ok(api::schedule_delete(engine, args(a)?).await?),
        "schedule_run" => ok(api::schedule_run(engine, args(a)?).await?),
        "schedule_log" => ok(api::schedule_log(engine, args(a)?).await?),
        "vm_create" => ok(api::vm_create(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "vm_destroy" => ok(api::vm_destroy(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "box_list" => ok(api::box_list(engine, args(a)?).await?),
        "box_create" => ok(api::box_create(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "box_send" => ok(api::box_send(engine, args(a)?).await?),
        "box_pause" => ok(api::box_pause(engine, args(a)?).await?),
        "box_destroy" => ok(api::box_destroy(engine, args(a)?).await?),
        "get_operation" => ok(api::operation(engine, args(a)?)?),
        "cancel_operation" => ok(api::cancel_operation(engine, args(a)?)?),
        "list_files" => ok(api::list_files(engine, args(a)?).await?),
        "copy_files" => ok(api::copy_files(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "list_operations" => ok(json!({ "operations": api::operations(engine) })),
        "github_scopes" => ok(api::github_scopes(engine, args(a)?).await?),
        "get_settings" => ok(api::settings(engine)),
        "update_settings" => ok(api::update_settings(engine, args(a)?)?),
        "list_ssh_hosts" => ok(api::ssh_hosts(engine).await),
        "workspace_create" => ok(api::workspace_create(engine, args(a)?, &session.owner(), Surface::Mcp).await?),
        "workspace_list" => ok(api::workspaces_list(engine).await),
        "workspace_get" => ok(api::workspace_get(engine, args(a)?, Surface::Mcp)?),
        "workspace_exec" => ok(api::workspace_exec(engine, args(a)?, &session.owner(), Surface::Mcp).await?),
        "workspace_read_file" => ok(api::workspace_read_file(engine, args(a)?, Surface::Mcp).await?),
        "workspace_write_file" => ok(api::workspace_write_file(engine, args(a)?, Surface::Mcp).await?),
        "workspace_list_files" => ok(api::workspace_list_files(engine, args(a)?, Surface::Mcp).await?),
        "workspace_changes" => ok(api::workspace_changes(engine, args(a)?, Surface::Mcp).await?),
        "workspace_sync" => ok(api::workspace_sync(engine, args(a)?, Surface::Mcp).await?),
        "workspace_destroy" => ok(api::workspace_destroy(engine, args(a)?, Surface::Mcp).await?),
        "workspace_allow" => ok(api::workspace_allow(engine, args(a)?, Surface::Mcp)?),
        "session_list" => ok(api::sessions_list(engine, args(a)?).await?),
        "session_start" => ok(api::session_start(engine, args(a)?).await?),
        "session_send" => ok(api::session_send(engine, args(a)?).await?),
        "session_action" => ok(api::session_action(engine, args(a)?).await?),
        "session_question" => ok(api::session_question(engine, args(a)?).await?),
        "session_answer" => ok(api::session_answer(engine, args(a)?).await?),
        "session_read" => ok(api::session_read(engine, args(a)?).await?),
        "session_stop" => ok(api::session_stop(engine, args(a)?).await?),
        "session_restart" => ok(api::session_restart(engine, args(a)?).await?),
        "session_rename" => ok(api::session_rename(engine, args(a)?).await?),
        "session_handoff" => ok(api::session_handoff(engine, args(a)?).await?),
        "github_inbox" => ok(api::github_inbox(engine).await?),
        "github_repos" => ok(json!({ "repos": api::github_repos(engine).await? })),
        "github_favorite" => ok(api::github_favorite(engine, args(a)?)?),
        "github_accounts" => ok(api::github_accounts(engine).await?),
        "github_use_account" => ok(api::github_use_account(engine, args(a)?).await?),
        "github_repo" => ok(api::github_repo(engine, args(a)?).await?),
        "github_item" => ok(api::github_item(engine, args(a)?).await?),
        "local_checkouts" => ok(json!({ "checkouts": api::checkouts(engine, args(a)?).await? })),
        "git_sync" => ok(api::checkout_git(engine, args(a)?).await?),
        "git_status" => ok(api::git_status(engine, &args::<api::GitStatusRequest>(a)?.machine_id).await?),
        "machine_connection" => ok(api::machine_connection(engine, &args::<api::GitStatusRequest>(a)?.machine_id).await?),
        "desktop_input" => ok(api::desktop_input(engine, args(a)?).await?),
        "desktop_displays" => ok(json!({ "displays": api::desktop_displays(engine, args(a)?).await? })),
        "desktop_windows" => ok(json!({ "windows": api::desktop_windows(engine, args(a)?).await? })),
        "desktop_add_display" => ok(api::desktop_add_display(engine, args(a)?).await?),
        "desktop_remove_display" => ok(api::desktop_remove_display(engine, args(a)?).await?),
        "desktop_set_display" => ok(api::desktop_set_display(engine, args(a)?).await?),
        "log_sources" => ok(api::log_sources(engine, args(a)?).await?),
        "logs_read" => ok(api::logs_read(engine, args(a)?).await?),
        "machine_services" => ok(api::machine_services(engine, args(a)?).await?),
        "service_action" => ok(api::service_action(engine, args(a)?).await?),
        "docker_run" => ok(api::docker_run(engine, args(a)?).await?),
        "docker_pull" => ok(api::docker_pull(engine, args(a)?).await?),
        "compose_action" => ok(api::compose_action(engine, args(a)?).await?),
        "compose_project" => ok(api::compose_project(engine, args(a)?).await?),
        "machine_ports" => ok(json!({ "ports": api::machine_ports(engine, args(a)?).await? })),
        "agent_usage" => ok(api::usage(engine, args(a)?).await?),
        "machine_processes" => ok(api::machine_processes(engine, args(a)?).await?),
        "process_signal" => ok(api::process_signal(engine, args(a)?).await?),
        "run_on_machines" => ok(api::run_on_machines(engine, args(a)?).await?),
        "wake_machine" => ok(api::wake_machine(engine, args(a)?).await?),
        "direct_connection" => ok(api::set_direct(engine, args(a)?).await?),
        "git_setup" => ok(api::git_setup(engine, args(a)?).await?),
        other => Err(ApiError::not_found(format!("Unknown tool: {other}"))),
    }
}

fn annotations(title: &str, read_only: bool, destructive: bool, idempotent: bool, open_world: bool) -> Value {
    json!({
        "title": title,
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": open_world,
    })
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

const MACHINE_ID: &str = "Machine id (a slug like \"mac-studio\"; the display name also works).";
const DISPLAY_ID: &str = "One of the machine's displays, by its id from desktop_displays, or one app's window as \"window:<id>\" (an id from desktop_windows). Default: the whole desktop (Linux) or the main display (a Mac).";
const WORKSPACE_ID: &str = "Workspace id from workspace_create or workspace_list, e.g. \"ws-7f3a2c\".";
const WS_PATH: &str = "Path relative to the workspace folder, e.g. \"src/main.rs\". Absolute paths and \"..\" escapes are rejected.";
const SESSION_ID: &str = "Session id from session_list or session_start: \"<machine-id>/<name>\", e.g. \"beelink/claude-3f9a\". A bare name works when unique.";
const RUNNER_ID: &str = "Runner id: \"<machine-id>/<runner-name>\", or \"github/<name>\" for a runner no machine lists. A bare runner name works when unique.";

/// The tool catalog. Descriptions are written for a model deciding what to call.
pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "status",
            "description": "Overview of all the user's machines: counts of working/idle/offline runners, queued jobs, GitHub connection, every machine with CPU/memory/disk/uptime and a runner summary, and an `attention` list of problems (unreachable machines, stopped runners, jobs no runner can take, low disk). Call this first. Uses the last machine check unless refresh=true, which re-checks every machine over ssh first.",
            "inputSchema": schema(json!({
                "refresh": { "type": "boolean", "description": "Re-check all machines over ssh and sync GitHub before answering. Slower; default false." }
            }), &[]),
            "annotations": annotations("Status", true, false, true, true),
        }),
        json!({
            "name": "get_machine",
            "description": "Everything about one machine: resources, OS, each runner on it (state, where the state came from, labels, version, service, current job) and diagnostics with hints. Use it to explain why a machine or runner has a problem.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Get machine", true, false, true, false),
        }),
        json!({
            "name": "list_runners",
            "description": "List runners, optionally only those on one machine or in one state. Runners GitHub knows but no machine lists have machine_id null.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "state": { "type": "string", "enum": ["working", "idle", "offline", "unknown"], "description": "Only runners in this state." }
            }), &[]),
            "annotations": annotations("List runners", true, false, true, false),
        }),
        json!({
            "name": "list_jobs",
            "description": "Queued and running self-hosted jobs from the watched GitHub orgs/repos, with labels, age, and for queued jobs how many online runners could take them (0 means the job is stuck until a matching runner comes online).",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("List jobs", true, false, true, true),
        }),
        json!({
            "name": "check_machines",
            "description": "Run the read-only machine check now (over ssh, or locally for host \"local\"): CPU, memory, disk, uptime, installed runners and whether they're running. One machine or all. Changes nothing on the machines.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": "Only this machine; omit for all." } }), &[]),
            "annotations": annotations("Check machines", true, false, true, true),
        }),
        json!({
            "name": "add_machine",
            "description": "Add a machine to Muxin (written to config.toml, comments preserved) and check it. `host` is anything the system ssh accepts (user@host, an ~/.ssh/config alias; see list_ssh_hosts) or \"local\" for this computer. Key-based ssh must work without a password prompt. Doesn't install anything.",
            "inputSchema": schema(json!({
                "name": { "type": "string", "description": "Display name, e.g. \"Mac Studio\"." },
                "host": { "type": "string", "description": "ssh destination or \"local\"." },
                "check": { "type": "boolean", "description": "Check it right away and return what was found. Default true." }
            }), &["name", "host"]),
            "annotations": annotations("Add machine", false, false, false, true),
        }),
        json!({
            "name": "remove_machine",
            "description": "Remove a machine from config.toml. Its runners stay registered on GitHub; use runner_action remove first to take runners down. On the machine, only a direct-connection pairing with this app is undone (best-effort).",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Remove machine", false, true, true, false),
        }),
        json!({
            "name": "rename_machine",
            "description": "Give a machine a new display name in config.toml (comments preserved). Its machine_id doesn't change, so its runners, sessions, workspaces and favorites stay with it. Nothing on the machine changes.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "name": { "type": "string", "description": "The new display name, e.g. \"Studio\"." }
            }), &["machine_id", "name"]),
            "annotations": annotations("Rename machine", false, false, true, false),
        }),
        json!({
            "name": "connect_machine",
            "description": "Disconnect from a machine (connected: false) or connect to it again. Disconnected, Muxin closes its connection and stops checking it and reading its sessions (status \"disconnected\"); its sessions keep running there, and anything that would reach it fails with \"<name> is disconnected\". Connecting checks it again. Saved in config.toml; nothing on the machine changes. Only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "connected": { "type": "boolean", "description": "false disconnects; true (the default) connects." }
            }), &["machine_id"]),
            "annotations": annotations("Connect or disconnect a machine", false, false, true, false),
        }),
        json!({
            "name": "install_runners",
            "description": "Install GitHub Actions runners on a machine and register each with the GitHub org/repo (`scope`) using a registration token from the user's gh login, then add them to config.toml. How they run (`mode`): \"auto\" (default) picks a boot-time service when the machine allows it (macOS, Windows, Linux with passwordless sudo), else Docker containers when Docker is available, else a background run.sh; \"docker\" runs one container per runner from GitHub's actions-runner image (no sudo, restarted by Docker, jobs get a clean Linux userland). Modifies the real machine and GitHub. Needs admin on the org (admin:org scope) or repo. Names default to \"<machine-id>-01\", \"-02\", ...",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "scope": { "type": "string", "description": "GitHub org (\"acme\") or repo (\"acme/api\"). Optional when exactly one is watched." },
                "names": { "type": "array", "items": { "type": "string" }, "description": "Runner names (letters, digits, '-', '_', '.')." },
                "count": { "type": "integer", "minimum": 1, "maximum": 32, "description": "How many to install when names is empty. Default 1." },
                "labels": { "type": "array", "items": { "type": "string" }, "description": "Extra labels; self-hosted, OS and architecture are added by GitHub." },
                "runner_group": { "type": "string", "description": "Org runner group (org scopes only)." },
                "mode": { "type": "string", "enum": ["auto", "service", "background", "docker"], "description": "How the runners run. Default \"auto\" (service, else docker, else background)." },
                "image": { "type": "string", "description": "mode docker only: runner image. Default ghcr.io/actions/actions-runner:<latest runner version>." },
                "docker_socket": { "type": "boolean", "description": "mode docker only: mount the host's Docker socket so jobs can use Docker. Root-equivalent access to the machine; only for trusted workflows. Default false." },
                "as_service": { "type": "boolean", "description": "Legacy: false means mode \"background\". Prefer mode." },
                "wait": { "type": "boolean", "description": "Wait for the install to finish. Default true." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "How long to wait. Default 900." }
            }), &["machine_id"]),
            "annotations": annotations("Install runners", false, false, false, true),
        }),
        json!({
            "name": "runner_action",
            "description": "Control one runner on its machine: start, stop or restart its service; logs (read-only tail of its newest _diag log); or remove (stop, uninstall the service, deregister from GitHub, drop it from config.toml, optionally delete its folder). For a runner no machine lists (\"github/<name>\"), remove deregisters it on GitHub. stop and remove take the runner offline; confirm with the user first.",
            "inputSchema": schema(json!({
                "runner_id": { "type": "string", "description": RUNNER_ID },
                "action": { "type": "string", "enum": ["start", "stop", "restart", "logs", "remove"] },
                "delete_files": { "type": "boolean", "description": "With remove: also delete the runner's folder. Default false." },
                "wait": { "type": "boolean", "description": "Wait for it to finish. Default true." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "How long to wait. Default 180." }
            }), &["runner_id", "action"]),
            "annotations": annotations("Runner action", false, true, false, true),
        }),
        json!({
            "name": "cloud_options",
            "description": "The user's DigitalOcean sign-in, regions, sizes (with hourly and monthly prices) and the defaults for a new cloud machine. Read-only.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("Cloud options", true, false, true, true),
        }),
        json!({
            "name": "cloud_usage",
            "description": "What the user's cloud machines cost: each droplet's hourly rate, monthly cap, cost this month and since it was created (destroyed ones this month too), the totals now / this month / by the month's end, and the DigitalOcean account's month-to-date bill when the sign-in may read it. Droplets are billed while they exist, powered off too; only destroying one stops the charges. Read-only.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("Cloud usage", true, false, true, true),
        }),
        json!({
            "name": "cloud_create",
            "description": "Create a DigitalOcean droplet in the user's account, set it up (their user and SSH key, git, and the extras chosen: Docker, GitHub CLI, languages like Go, Rust, Python or PHP, agent CLIs, Tailscale), and add it to the user's machines. It COSTS MONEY every hour it exists: tell the user the size and price (cloud_options) and get their OK first. Takes a few minutes.",
            "inputSchema": schema(json!({
                "name": { "type": "string" },
                "region": { "type": "string", "description": "Region slug, e.g. nyc3. Default: the user's setting, else the nearest." },
                "size": { "type": "string", "description": "Size slug, e.g. s-2vcpu-4gb. Default: the user's setting." },
                "install": { "type": "array", "items": { "type": "string", "enum": crate::cloud::setup::EXTRAS.iter().map(|(id, _)| *id).collect::<Vec<_>>() }, "description": "What it comes with besides git: tools, languages (node, python, go, rust, php, ruby, java, bun, deno, dotnet), agent CLIs, tailscale. Default: the user's setting." },
                "wait": { "type": "boolean", "description": "Wait until it's ready. Default true." },
                "timeout_secs": { "type": "integer", "minimum": 1 }
            }), &["name"]),
            "annotations": annotations("Create cloud machine", false, false, false, true),
        }),
        json!({
            "name": "cloud_destroy",
            "description": "Destroy a cloud machine Muxin created: deletes the droplet (everything on it is lost) and removes it from Muxin. Confirm with the user first; make sure work there is pushed.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string" }, "wait": { "type": "boolean" } }), &["machine_id"]),
            "annotations": annotations("Destroy cloud machine", false, true, true, true),
        }),
        json!({
            "name": "cloud_power",
            "description": "Power a cloud machine on or off. DigitalOcean still bills a droplet while it's off; only cloud_destroy stops the cost.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string" }, "power": { "type": "string", "enum": ["on", "off"] }, "wait": { "type": "boolean" } }), &["machine_id", "power"]),
            "annotations": annotations("Cloud power", false, false, true, true),
        }),
        json!({
            "name": "machine_power",
            "description": "Power a machine on or off. A cloud machine: through its provider (as cloud_power). A VM or container that runs in Docker on another of the user's machines (get_machine's `parent` names the machine and its `container`): starts or stops that container there; stopping gives it two minutes to shut down cleanly, and starting waits until it answers ssh. It changes the user's machines: only when asked.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID }, "power": { "type": "string", "enum": ["on", "off"] }, "wait": { "type": "boolean" } }), &["machine_id", "power"]),
            "annotations": annotations("Power a machine on or off", false, false, true, true),
        }),
        json!({
            "name": "schedule_list",
            "description": "Schedules on the user's machines (or one): commands or agent sessions that run there at set times, kept and run by Muxin's agent through the machine's own scheduler (cron, launchd, Task Scheduler), so they run whether or not Muxin is open. Each with `when` (and `when_text`, in the machine's own time), `what`, `enabled`, `running` and `last_run` (ok, exit_code, or the agent session it started). `problem` says when a machine can't run them (no cron). Read-only.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &[]),
            "annotations": annotations("List schedules", true, false, true, false),
        }),
        json!({
            "name": "schedule_save",
            "description": "Create a schedule on a machine, or change one (pass its id). It runs `command` in the user's login shell in `folder`, or starts an agent session (`agent`: claude, codex, ...) with `prompt` in `folder`, which shows up in the app like any session. For an unattended agent run that ends by itself, use a command such as `claude -p \"...\"`. `when`: {every: \"minutes\"|\"hours\", n} (hours at `at`'s minute), or {every: \"day\"|\"weekdays\", at: \"HH:MM\"}, or {every: \"week\", days: [\"mon\", \"thu\"], at}; in the machine's own time. It runs on the user's machine unattended: only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "id": { "type": "string", "description": "An existing schedule's id, to change it." },
                "name": { "type": "string" },
                "when": { "type": "object", "properties": {
                    "every": { "type": "string", "enum": ["minutes", "hours", "day", "weekdays", "week"] },
                    "n": { "type": "integer", "minimum": 1 },
                    "at": { "type": "string", "description": "HH:MM" },
                    "days": { "type": "array", "items": { "type": "string" } }
                }, "required": ["every"] },
                "command": { "type": "string" },
                "agent": { "type": "string", "description": "claude, codex, opencode, gemini, amp, pi or shell." },
                "prompt": { "type": "string" },
                "folder": { "type": "string", "description": "Default ~." },
                "enabled": { "type": "boolean", "description": "Default true; false keeps it without running it." }
            }), &["machine_id", "name", "when"]),
            "annotations": annotations("Save schedule", false, false, true, true),
        }),
        json!({
            "name": "schedule_delete",
            "description": "Delete a schedule from a machine, with its run history. Confirm with the user first unless they asked.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID }, "schedule_id": { "type": "string" } }), &["machine_id", "schedule_id"]),
            "annotations": annotations("Delete schedule", false, true, true, true),
        }),
        json!({
            "name": "schedule_run",
            "description": "Run a schedule now, in the background on its machine (its next times don't change). Read the result with schedule_log.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID }, "schedule_id": { "type": "string" } }), &["machine_id", "schedule_id"]),
            "annotations": annotations("Run schedule now", false, false, false, true),
        }),
        json!({
            "name": "schedule_log",
            "description": "A schedule's last runs (newest first: started, ok, exit_code, seconds, or the agent session) and one run's output: the newest, or the one whose `started` you pass. Read-only.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID }, "schedule_id": { "type": "string" }, "started": { "type": "integer" } }), &["machine_id", "schedule_id"]),
            "annotations": annotations("Schedule runs and output", true, false, true, false),
        }),
        json!({
            "name": "vm_options",
            "description": "Where VMs can run and what: each of the user's machines with Docker (online, kvm, cores, free memory and disk), the systems it can run (Windows needs a Linux x86-64 machine with KVM; Linux runs anywhere with Docker), why one can't, the system catalog (ids like windows-11, windows-10, windows-server-2025, ubuntu-24.04, debian-13, fedora, alpine, arch) and the defaults. Read-only.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("VM options", true, false, true, false),
        }),
        json!({
            "name": "vm_create",
            "description": "Create a VM on one of the user's machines and add it to them (shown under that machine). Windows (dockur/windows with KVM) downloads and installs itself, 15-30 minutes the first time: call with wait: false and poll get_operation. Linux is a container with ssh, ready in under a minute (it shares the host's kernel, no systemd). Muxin sets up ssh with this computer's key and its agent, so sessions, files, workspace_exec and the desktop work like on any machine. `host` defaults to the machine with the most free memory that can run it. The result names the new machine_id. It uses the user's machine and disk: only when asked, or when the task needs a fresh machine; destroy it when done unless they want to keep it.",
            "inputSchema": schema(json!({
                "name": { "type": "string", "description": "Its name in Muxin, e.g. \"win-test\"." },
                "system": { "type": "string", "description": "A system id from vm_options, e.g. windows-11 or ubuntu-24.04." },
                "host": { "type": "string", "description": MACHINE_ID },
                "cpus": { "type": "integer", "minimum": 1, "description": "Default 4." },
                "memory_gb": { "type": "integer", "minimum": 1, "description": "Default 8." },
                "disk_gb": { "type": "integer", "minimum": 32, "description": "Windows' disk, grown as needed. Default 64." },
                "install": { "type": "array", "items": { "type": "string" }, "description": "Agent CLIs to install on a Linux VM (with Node.js): claude, codex, gemini, opencode, amp, pi. They sign in on first use." },
                "wait": { "type": "boolean", "description": "Wait for it to be ready. Default true (up to timeout_secs, default 600)." },
                "timeout_secs": { "type": "integer", "minimum": 1 }
            }), &["name", "system"]),
            "annotations": annotations("Create VM", false, false, false, true),
        }),
        json!({
            "name": "vm_destroy",
            "description": "Destroy a VM Muxin made (vm_create): removes its container and disk on its host (everything in it is lost) and the machine from Muxin. Confirm with the user first unless they asked for it, or you made it for the task; copy out anything worth keeping. machine_power stops one without deleting it.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID }, "wait": { "type": "boolean" } }), &["machine_id"]),
            "annotations": annotations("Destroy VM", false, true, true, true),
        }),
        json!({
            "name": "box_list",
            "description": "The user's boxes: tasks an agent CLI works on in a container of its own on one of their machines. Each has its id (box-ab12cd), machine, repo, branch, agent, state (running, starting or paused), busy (working right now), attention (waiting for an answer), session_id while it runs (for session_read and session_question), its task, and git (what it changed, its pull request). Read-only.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": "Only this machine's boxes." } }), &[]),
            "annotations": annotations("List boxes", true, false, true, false),
        }),
        json!({
            "name": "box_create",
            "description": "Hand a task to an agent in a new box: Muxin clones the repo onto a new branch in a container of its own on one of the user's machines (one with Docker; the first box there builds Muxin's image, a few minutes), runs the repo's .agents/setup, and starts the agent CLI there on the prompt with its permission prompts off (the container is its sandbox). It can push and open pull requests with the user's GitHub login. The box pauses by itself when its agent has been quiet for a while; box_send picks it up again. Waits until the agent has started (wait: false returns an operation_id to poll). Only when the user asked for it.",
            "inputSchema": schema(json!({
                "repo": { "type": "string", "description": "owner/name on GitHub, a git URL, or a checkout's folder on machine_id (see local_checkouts): the box starts from that checkout with its uncommitted changes." },
                "prompt": { "type": "string", "description": "The task: what the agent is asked to do (an issue's URL and text works well)." },
                "agent": { "type": "string", "enum": ["claude", "codex", "amp", "opencode", "gemini", "pi"], "description": "Which agent CLI. Default claude. It's signed in as it is on that machine (its ~/.claude, ~/.codex, ... are shared with the box); if it isn't signed in there, the user signs it in once from the box's terminal." },
                "machine_id": { "type": "string", "description": "Where it runs. Default: the online machine with Docker and the most free memory." },
                "name": { "type": "string", "description": "Its name. Default: the task's first words." },
                "branch": { "type": "string", "description": "The branch it works on. Default box/<name>." },
                "ref": { "type": "string", "description": "What it starts from: a branch, tag, commit, or pull/<n>/head for a pull request's code. Default: the default branch." },
                "cpus": { "type": "integer", "minimum": 1 },
                "memory_gb": { "type": "integer", "minimum": 1 },
                "idle_minutes": { "type": "integer", "minimum": 0, "description": "Pause after this many minutes without output from its agent. Default 15; 0 never pauses." },
                "wait": { "type": "boolean", "description": "Wait until its agent has started. Default true (up to timeout_secs, default 900)." },
                "timeout_secs": { "type": "integer", "minimum": 1 }
            }), &["repo", "prompt"]),
            "annotations": annotations("Create box", false, false, false, true),
        }),
        json!({
            "name": "box_send",
            "description": "Ask a box's agent something more: typed into it when it's running, else the box starts again and its agent continues its conversation with this as the next message. Without text it just starts the box again. Returns the box.",
            "inputSchema": schema(json!({
                "box_id": { "type": "string", "description": "A box id from box_list, e.g. box-ab12cd." },
                "text": { "type": "string", "description": "What to ask its agent." }
            }), &["box_id"]),
            "annotations": annotations("Ask a box", false, false, false, true),
        }),
        json!({
            "name": "box_pause",
            "description": "Pause a box now: its agent ends and its container stops (dev servers in it too); its files, branch and the agent's conversation stay. box_send starts it again.",
            "inputSchema": schema(json!({ "box_id": { "type": "string" } }), &["box_id"]),
            "annotations": annotations("Pause box", false, false, true, false),
        }),
        json!({
            "name": "box_destroy",
            "description": "Delete a box: its container and its checkout, with any work in it that wasn't pushed. Its branch on GitHub stays. Check its git (box_list) and confirm with the user first unless they asked for it.",
            "inputSchema": schema(json!({ "box_id": { "type": "string" } }), &["box_id"]),
            "annotations": annotations("Destroy box", false, true, true, false),
        }),
        json!({
            "name": "cancel_run",
            "description": "Cancel the GitHub Actions workflow run that a queued or running job belongs to (job_id from list_jobs). GitHub cancels whole runs, so every unfinished job in it stops. Confirm with the user first.",
            "inputSchema": schema(json!({
                "job_id": { "type": "integer", "description": "A queued or running job's id, from list_jobs." },
                "wait": { "type": "boolean", "description": "Wait for GitHub to accept it. Default true." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "How long to wait. Default 60." }
            }), &["job_id"]),
            "annotations": annotations("Cancel run", false, true, false, true),
        }),
        json!({
            "name": "get_operation",
            "description": "Status and output of an install or runner action started by this server (use after wait=false or a timeout). Lines starting with \"▸ \" are Muxin's steps, \"✓ \" successes, \"error: \" problems.",
            "inputSchema": schema(json!({
                "operation_id": { "type": "integer" },
                "tail": { "type": "integer", "minimum": 0, "description": "Log lines from the end. Default 80." }
            }), &["operation_id"]),
            "annotations": annotations("Get operation", true, false, true, false),
        }),
        json!({
            "name": "cancel_operation",
            "description": "Stops a running operation that can be stopped (a copy_files copy).",
            "inputSchema": schema(json!({ "operation_id": { "type": "integer" } }), &["operation_id"]),
            "annotations": annotations("Cancel operation", false, false, true, false),
        }),
        json!({
            "name": "list_files",
            "description": "What's in a folder on one of the user's machines (this computer included): names, file sizes, modified times; folders first. Linux and macOS machines.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string" },
                "path": { "type": "string", "description": "A folder, e.g. \"~\", \"~/build\", \"/srv/data\". Default: the home folder." }
            }), &["machine_id"]),
            "annotations": annotations("List files", true, false, true, false),
        }),
        json!({
            "name": "copy_files",
            "description": "Copies files and folders from one of the user's machines to another (this computer included), through Muxin: the machines don't need to reach each other. Folders come along whole, with permissions. Files with the same names in the destination folder are replaced, so check it with list_files first and confirm with the user before overwriting. Waits by default and returns the operation(s) with a log; with wait=false poll get_operation, and cancel_operation stops a copy.",
            "inputSchema": schema(json!({
                "from": { "type": "string", "description": "Machine with the files." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files or folders on it, e.g. [\"~/build/app.tar.gz\", \"~/reports\"]." },
                "to": { "type": "string", "description": "Machine to copy them to." },
                "to_dir": { "type": "string", "description": "Folder there (made if missing), e.g. \"~/Downloads\"." },
                "wait": { "type": "boolean", "description": "Wait for it to finish (default true)." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "How long to wait (default 3600)." }
            }), &["from", "paths", "to", "to_dir"]),
            "annotations": annotations("Copy files", false, true, false, false),
        }),
        json!({
            "name": "list_operations",
            "description": "Operations this server has run (installs, starts, stops, logs, removals), newest first, without their output.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("List operations", true, false, true, false),
        }),
        json!({
            "name": "github_scopes",
            "description": "Watch or stop watching GitHub orgs/repos. Watching gives live runner status and job data. Each added scope is checked first; reading org runners needs an org admin token (admin:org), and failures come back with the command that fixes them.",
            "inputSchema": schema(json!({
                "add": { "type": "array", "items": { "type": "string" }, "description": "Orgs (\"acme\") or repos (\"acme/api\") to watch." },
                "remove": { "type": "array", "items": { "type": "string" }, "description": "Orgs or repos to stop watching." }
            }), &[]),
            "annotations": annotations("GitHub scopes", false, false, true, true),
        }),
        json!({
            "name": "get_settings",
            "description": "Current settings: watched orgs/repos, polling and check intervals, file locations.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("Get settings", true, false, true, false),
        }),
        json!({
            "name": "update_settings",
            "description": "Change polling and check intervals or the GitHub API URL (for GitHub Enterprise Server). Only the fields given change.",
            "inputSchema": schema(json!({
                "github_poll_seconds": { "type": "integer", "minimum": 10, "description": "How often the window polls GitHub." },
                "scan_repos": { "type": "integer", "minimum": 0, "maximum": 50, "description": "Recently pushed repos per org scanned for jobs." },
                "check_interval_minutes": { "type": "integer", "minimum": 0, "description": "Automatic machine checks; 0 = only on request." },
                "api_url": { "type": "string", "description": "GitHub REST API base, e.g. https://ghe.example.com/api/v3." }
            }), &[]),
            "annotations": annotations("Update settings", false, false, true, false),
        }),
        json!({
            "name": "list_ssh_hosts",
            "description": "Hosts to suggest for add_machine: aliases from the user's ~/.ssh/config; computers on their Tailscale network (`tailnet`, when Tailscale is on here: each with the `host` to add it by, whether Tailscale sees it online, and `machine_id` when it's already in Muxin; a machine added by its tailnet name is reachable from anywhere); and computers on this local network that aren't in Muxin yet (`network`: `ssh: false` means a Mac with Remote Login off, which the user turns on in System Settings › General › Sharing). Listens on the network for a few seconds.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("List ssh hosts", true, false, true, false),
        }),
        json!({
            "name": "workspace_create",
            "description": "Check out one of the user's own machines as your work environment, like a remote dev box: creates a persistent folder there (optionally cloning a git repo at a ref, or creating a new branch) and returns a workspace_id for the other workspace_* tools. Omit machine_id to get the least-loaded online machine that allows workspaces, filtered by `platform` (e.g. \"linux\" to run tests on Linux). Use one workspace per task and reuse it across steps. Nothing is deleted automatically: call workspace_destroy when done. Private GitHub repos are cloned with the user's gh login. To work on a repository that's already checked out on a machine (local_checkouts), pass its folder as `worktree_of`: the workspace is then a git worktree of it on its own branch, quicker than a clone, and several agents can work on that repository side by side; workspace_destroy removes the worktree and keeps the branch.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": "Machine to use; omit to let Muxin pick." },
                "platform": { "type": "string", "description": "When picking: platform prefix such as \"linux\", \"linux-x64\", \"darwin-arm64\", \"windows\"." },
                "name": { "type": "string", "description": "Short label, e.g. \"api-tests\". Defaults to the repo name." },
                "repo": { "type": "string", "description": "Git URL, or \"owner/repo\" for GitHub, to clone into the workspace." },
                "ref": { "type": "string", "description": "Branch, tag or commit to check out; \"pull/123/head\" checks out pull request #123 (forks too)." },
                "branch": { "type": "string", "description": "New branch to create and switch to after checkout." },
                "isolation": { "type": "string", "enum": ["host", "docker"], "description": "\"host\" (default): commands run directly on the machine with its tools and login environment. \"docker\": a fresh, isolated container per workspace (folder mounted at /workspace); pick the toolchain with `image`. Prefer docker for untrusted code or when you need a clean environment." },
                "image": { "type": "string", "description": "Docker image for isolation \"docker\", e.g. \"node:22\", \"python:3.13\", \"rust:1\", \"golang:1.25\". Defaults to buildpack-deps:bookworm. Choose an image that already has your tools: the container runs as the ssh user, so apt-get install won't work." },
                "docker_socket": { "type": "boolean", "description": "Share the machine's Docker socket with the container (docker-in-docker). Root-equivalent on that machine; only when the task needs Docker." },
                "worktree_of": { "type": "string", "description": "A git checkout's folder on machine_id (from local_checkouts): make the workspace a git worktree of it instead of cloning `repo`. It starts from `ref` (else the default branch) on `branch` (a new one; else named after the workspace). Not with isolation \"docker\"." }
            }), &[]),
            "annotations": annotations("Create workspace", false, false, false, true),
        }),
        json!({
            "name": "workspace_list",
            "description": "All workspaces (yours and other agents'): machine, folder, repo/ref, owner, status, last command, git (branch, changes, pull request: a merged one means a worktree's work is done), and which machines can take new workspaces. Check here before creating a new one; reuse a ready workspace for the same task.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("List workspaces", true, false, true, false),
        }),
        json!({
            "name": "workspace_get",
            "description": "One workspace with its recent command history (command, exit code, duration, output tail) and the shell command a human can use to open a terminal in it.",
            "inputSchema": schema(json!({ "workspace_id": { "type": "string", "description": WORKSPACE_ID } }), &["workspace_id"]),
            "annotations": annotations("Get workspace", true, false, true, false),
        }),
        json!({
            "name": "workspace_exec",
            "description": "Run a shell command in a workspace on the user's machine, with that user's login environment (their PATH, Homebrew, nvm, ...). Returns exit_code, stdout and stderr (long output is capped, keeping the start and end). A non-zero exit_code is a normal result, not a tool error. Commands default to a 600s timeout (max 3600). For servers and watchers pass background=true: it returns right away with a pid and a log_path to read later. Commands run as the user on a real machine; ask before anything destructive outside the workspace.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "command": { "type": "string", "description": "Shell command, e.g. \"npm test\" or \"cargo build --release 2>&1 | tail -50\"." },
                "cwd": { "type": "string", "description": "Folder relative to the workspace to run in." },
                "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra environment variables." },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "Kill it after this many seconds. Default 600. Ignored with background." },
                "background": { "type": "boolean", "description": "Start detached and return its pid and log file. Default false." }
            }), &["workspace_id", "command"]),
            "annotations": annotations("Run command in workspace", false, false, false, true),
        }),
        json!({
            "name": "workspace_read_file",
            "description": "Read a text file from a workspace (default up to 256 KB; binary files are reported, not returned). Also use it to read the log of a background command.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "path": { "type": "string", "description": WS_PATH },
                "max_bytes": { "type": "integer", "minimum": 1, "maximum": 2097152, "description": "Read at most this many bytes. Default 262144." }
            }), &["workspace_id", "path"]),
            "annotations": annotations("Read workspace file", true, false, true, false),
        }),
        json!({
            "name": "workspace_write_file",
            "description": "Write (or append to) a text file in a workspace, creating parent folders. Replaces the whole file unless append=true.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "path": { "type": "string", "description": WS_PATH },
                "content": { "type": "string", "description": "UTF-8 text." },
                "append": { "type": "boolean", "description": "Append instead of replacing. Default false." }
            }), &["workspace_id", "path", "content"]),
            "annotations": annotations("Write workspace file", false, false, false, false),
        }),
        json!({
            "name": "workspace_list_files",
            "description": "List files and folders in a workspace (skips .git internals), with sizes.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "path": { "type": "string", "description": "Folder relative to the workspace. Default: the workspace itself." },
                "depth": { "type": "integer", "minimum": 1, "maximum": 10, "description": "How deep to list. Default 2." }
            }), &["workspace_id"]),
            "annotations": annotations("List workspace files", true, false, true, false),
        }),
        json!({
            "name": "workspace_changes",
            "description": "Review what changed in a workspace: `git status` plus the uncommitted diff (capped). Use it before reporting back or syncing changes to the user's checkout.",
            "inputSchema": schema(json!({ "workspace_id": { "type": "string", "description": WORKSPACE_ID } }), &["workspace_id"]),
            "annotations": annotations("Workspace changes", true, false, true, false),
        }),
        json!({
            "name": "workspace_sync",
            "description": "Copy code between a folder on this computer (usually the user's local checkout) and a workspace with rsync over ssh. direction=push sends the local folder to the workspace (e.g. to test uncommitted local changes on another OS); direction=pull brings the workspace's files back, but not its .git (commits made there stay there; commit here instead). delete=true also removes files on the receiving side that the sender doesn't have; pulling with delete into the user's checkout can remove their files, so confirm first.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "local_path": { "type": "string", "description": "Folder on this computer; absolute is safest (relative paths resolve against the MCP server's working directory)." },
                "direction": { "type": "string", "enum": ["push", "pull"] },
                "delete": { "type": "boolean", "description": "Mirror deletions to the receiving side. Default false." }
            }), &["workspace_id", "local_path", "direction"]),
            "annotations": annotations("Sync workspace", false, true, false, true),
        }),
        json!({
            "name": "workspace_destroy",
            "description": "Done with a workspace: stops background commands it started and deletes its folder on the machine (keep_files=true keeps the folder and only forgets the workspace). Uncommitted work in it is lost; pull or push it first if it matters.",
            "inputSchema": schema(json!({
                "workspace_id": { "type": "string", "description": WORKSPACE_ID },
                "keep_files": { "type": "boolean", "description": "Keep the folder on the machine. Default false." }
            }), &["workspace_id"]),
            "annotations": annotations("Destroy workspace", false, true, true, true),
        }),
        json!({
            "name": "workspace_allow",
            "description": "Turn agent workspaces on or off for a machine (edits config.toml, comments preserved). Machines allow them by default; only change this when the user asks.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "allowed": { "type": "boolean" }
            }), &["machine_id", "allowed"]),
            "annotations": annotations("Allow workspaces", false, false, true, false),
        }),
        json!({
            "name": "session_list",
            "description": "Agent sessions on the user's machines: terminals running Claude Code, Codex, OpenCode, Gemini CLI, Amp, Pi or a shell, kept alive by Muxin's agent on the machine. Each has an id (<machine-id>/<name>), agent, title, folder, status (running/exited), busy (printed output in the last few seconds, i.e. the agent is working) and the command a human can paste to attach. Also lists, per machine, the agent CLIs installed. Reads the machines over ssh, so it's always current (and sets up Muxin's agent on a machine the first time).",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &[]),
            "annotations": annotations("List agent sessions", true, false, true, true),
        }),
        json!({
            "name": "session_start",
            "description": "Start an agent CLI (or a shell) in a new session on a machine, in a folder there, optionally with a first prompt. It keeps running until stopped; the user can watch and type into it from the Muxin app. Returns the session. Only start sessions the user asked for.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "agent": { "type": "string", "enum": ["claude", "codex", "opencode", "gemini", "amp", "pi", "shell"], "description": "Which CLI to run. session_list shows which are installed on each machine." },
                "cwd": { "type": "string", "description": "Folder on the machine, e.g. \"~/code/api\". Default: the home folder." },
                "title": { "type": "string", "description": "Shown in the app's sidebar. Default: agent and folder name." },
                "prompt": { "type": "string", "description": "First message for the agent." }
            }), &["machine_id", "agent"]),
            "annotations": annotations("Start agent session", false, false, false, true),
        }),
        json!({
            "name": "session_read",
            "description": "The session's screen plus recent scrollback as plain text, with its status and whether it's busy. Use it to see what an agent is doing or waiting for.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": SESSION_ID },
                "lines": { "type": "integer", "minimum": 1, "maximum": 50000, "description": "Scrollback lines above the screen to include. Default 200." }
            }), &["session_id"]),
            "annotations": annotations("Read session screen", true, false, true, false),
        }),
        json!({
            "name": "session_send",
            "description": "Type text into a session, then press Enter (unless enter=false). Answers an agent's question or gives it a new instruction. Only send what the user asked you to.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": SESSION_ID },
                "text": { "type": "string", "description": "Text to type." },
                "enter": { "type": "boolean", "description": "Press Enter afterwards. Default true." }
            }), &["session_id", "text"]),
            "annotations": annotations("Type into session", false, false, false, false),
        }),
        json!({
            "name": "session_action",
            "description": "Ask a session's agent to do one of Muxin's session actions, by typing that action's prompt (the user may have changed it in Settings) into it: commit (commit its changes, don't push), commit_push, pull_request (push and open or update the branch's pull request), review (review the branch against the default branch, no code changes), fix_comments (address the pull request's review comments), conflicts (bring the branch up to date and resolve conflicts) or recap (summarize the session). session_list's `git` says which fit: a branch, a pull request. Only when the user asked for it.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": SESSION_ID },
                "action": { "type": "string", "enum": ["commit", "commit_push", "pull_request", "review", "fix_comments", "conflicts", "recap"], "description": "Which action." }
            }), &["session_id", "action"]),
            "annotations": annotations("Ask a session's agent", false, false, false, false),
        }),
        json!({
            "name": "session_question",
            "description": "What a session's agent is waiting for, read off its screen: a choice (\"Do you want to proceed?\" with its options, each with a key, and the one selected), or, when it's simply done, the end of what it said (options empty). Read-only.",
            "inputSchema": schema(json!({ "session_id": { "type": "string", "description": SESSION_ID } }), &["session_id"]),
            "annotations": annotations("Read what a session asks", true, false, true, false),
        }),
        json!({
            "name": "session_answer",
            "description": "Pick one of a waiting session's choices by its key from session_question (\"1\", \"2\", or \"#1\" for a menu without numbers). It approves or refuses what the agent asked: only when the user asked you to answer, and say which option. To reply with text, use session_send.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": SESSION_ID },
                "choice": { "type": "string", "description": "The option's key." }
            }), &["session_id", "choice"]),
            "annotations": annotations("Answer a session's question", false, false, false, false),
        }),
        json!({
            "name": "session_stop",
            "description": "End a session: closes it and whatever runs in it. Unsaved agent state is lost. Ask the user first unless they asked for exactly this.",
            "inputSchema": schema(json!({ "session_id": { "type": "string", "description": SESSION_ID } }), &["session_id"]),
            "annotations": annotations("Stop session", false, true, true, false),
        }),
        json!({
            "name": "session_restart",
            "description": "Run a session's agent again in the same session (for one that exited, or is stuck). Kills what's running in it first.",
            "inputSchema": schema(json!({ "session_id": { "type": "string", "description": SESSION_ID } }), &["session_id"]),
            "annotations": annotations("Restart session", false, true, false, false),
        }),
        json!({
            "name": "session_rename",
            "description": "Give a session a new title (what the app's sidebar and session_list show). The agent on its machine keeps it, so it sticks for every Muxin. An empty title goes back to the one it started with. Doesn't touch what runs in it.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": SESSION_ID },
                "title": { "type": "string", "description": "The new title, e.g. \"API server\"." }
            }), &["session_id", "title"]),
            "annotations": annotations("Rename session", false, false, true, false),
        }),
        json!({
            "name": "session_handoff",
            "description": "Hand a session's work to another agent (Claude Code to Codex, say), or a box's to another agent in the box: Muxin writes .muxin/handoff.md in its folder (where the git work stands, the agent's to-do list, and the conversation so far from the agent's own transcript: Claude Code, Codex, Gemini CLI and Pi; else what its terminal showed), starts the new agent there asked to read it first, and ends the old session unless keep. With machine_id the work moves to that machine first (git checkouts only; the folder it left stays as it was). Returns the new session, the note's path and what it was made from (`source`). Only when the user asked.",
            "inputSchema": schema(json!({
                "session_id": { "type": "string", "description": "A session id, or a box id (box-7k2m9q)." },
                "agent": { "type": "string", "description": "The agent to hand it to: claude, codex, opencode, gemini, amp or pi." },
                "keep": { "type": "boolean", "description": "Keep the old session running too. Default false." },
                "machine_id": { "type": "string", "description": "Move the work to this machine and continue there: its commits that aren't on origin, uncommitted changes and new files go into a workspace there (a worktree of a checkout of the repository, else a clone). Default: the session's own machine. Not for boxes." }
            }), &["session_id", "agent"]),
            "annotations": annotations("Hand off a session", false, true, false, false),
        }),
        json!({
            "name": "github_inbox",
            "description": "What needs the user on GitHub: pull requests waiting for their review, open issues and PRs assigned to them, and their own open pull requests (repo, number, title, labels, updated time, URL). Uses their GitHub login.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("GitHub inbox", true, false, true, true),
        }),
        json!({
            "name": "github_repos",
            "description": "The user's GitHub repositories (owned, collaborator, and through their orgs): their favorites first (`favorite: true`, the ones they work on most), then most recently pushed, with visibility, default branch, language and open issue count.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("GitHub repositories", true, false, true, true),
        }),
        json!({
            "name": "github_accounts",
            "description": "The GitHub accounts Muxin can use: the ones the GitHub CLI (gh) here is signed in to, plus each other machine's gh login (`machines`, e.g. a work laptop's work account). `using` (and `machine_id` when it's a machine's login) says which one Muxin uses for GitHub data, managing runners, and cloning into workspaces.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("GitHub accounts", true, false, true, false),
        }),
        json!({
            "name": "github_use_account",
            "description": "Switch which GitHub account Muxin uses: one the GitHub CLI here is signed in to (login), or another machine's gh login (machine_id). Changes whose repositories, inbox and runners Muxin sees and manages; doesn't change the account gh uses in the user's terminal. Only when asked. To add an account the user runs `gh auth login`.",
            "inputSchema": schema(json!({
                "login": { "type": "string", "description": "A login from github_accounts (optional with machine_id)." },
                "machine_id": { "type": "string", "description": "Use this machine's gh login (from github_accounts.machines)." }
            }), &[]),
            "annotations": annotations("Switch GitHub account", false, false, true, false),
        }),
        json!({
            "name": "github_favorite",
            "description": "Add a repository to the user's favorites in Muxin (listed first in the app and in github_repos), or remove it with favorite: false. Only when asked.",
            "inputSchema": schema(json!({
                "repo": { "type": "string", "description": "\"owner/name\" or a GitHub URL." },
                "favorite": { "type": "boolean", "description": "false removes it. Default true." }
            }), &["repo"]),
            "annotations": annotations("Favorite repository", false, false, true, false),
        }),
        json!({
            "name": "github_repo",
            "description": "One repository: its open issues and pull requests, recent GitHub Actions runs, and where it's checked out on the user's machines (machine, folder, branch, ahead/behind, uncommitted changes). Use the checkouts to pick where to work on it.",
            "inputSchema": schema(json!({ "repo": { "type": "string", "description": "\"owner/name\" or a GitHub URL." } }), &["repo"]),
            "annotations": annotations("GitHub repository", true, false, true, true),
        }),
        json!({
            "name": "github_item",
            "description": "An issue or pull request with its full description and comments; for pull requests also branches, size, check results (with failing check names) and reviews. Pass its URL or owner/name#123.",
            "inputSchema": schema(json!({ "reference": { "type": "string", "description": "Issue or PR URL, or \"owner/name#123\"." } }), &["reference"]),
            "annotations": annotations("GitHub issue or PR", true, false, true, true),
        }),
        json!({
            "name": "local_checkouts",
            "description": "Git checkouts on the user's machines (found in their code folders), each with the GitHub repo it tracks, branch, upstream, commits ahead/behind, and count of uncommitted files. Filter by machine or repo.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "repo": { "type": "string", "description": "Only checkouts of this repository (owner/name)." }
            }), &[]),
            "annotations": annotations("Local checkouts", true, false, true, true),
        }),
        json!({
            "name": "machine_connection",
            "description": "How Muxin reaches a machine right now: directly over QUIC (once paired), over a live ssh link, or not at all; with a measured round trip to its agent.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Machine connection", true, false, true, false),
        }),
        json!({
            "name": "direct_connection",
            "description": "Turn direct (QUIC) connections to a machine on or off. On: pairs with its agent over ssh (each side trusts only the other's certificate) and runs the agent's listener as a user service there (systemd or launchd), on UDP port 47821. Off: removes that service. Changes the machine; only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "enabled": { "type": "boolean" }
            }), &["machine_id", "enabled"]),
            "annotations": annotations("Direct connection", false, false, true, false),
        }),
        json!({
            "name": "desktop_screenshot",
            "description": "A picture of a machine's screen (Linux with an X11 desktop, a Mac, or Windows), or of one of its displays. The account Muxin connects as must be signed in there; when nobody is, it says so (the user can open the machine's own Remote Desktop or Screen Sharing from its menu in the app). Coordinates for desktop_input are in its pixels.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "display": { "type": "string", "description": DISPLAY_ID }
            }), &["machine_id"]),
            "annotations": annotations("Desktop screenshot", true, false, false, false),
        }),
        json!({
            "name": "desktop_input",
            "description": "Use a machine's desktop like a person: events run in order. {type: move|click|scroll|key|text|wait}: click {x, y, button?: left|right|middle, double?}; move {x, y}; scroll {x, y, dy} (positive scrolls down); key {key: \"enter\" | \"ctrl+c\" | \"alt+tab\" ...}; text {text}; wait {ms}. Coordinates in desktop_screenshot pixels; take a screenshot after to see what happened. It acts on the user's real screen: only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "display": { "type": "string", "description": DISPLAY_ID },
                "events": { "type": "array", "items": { "type": "object" } }
            }), &["machine_id", "events"]),
            "annotations": annotations("Desktop input", false, true, false, false),
        }),
        json!({
            "name": "desktop_displays",
            "description": "A machine's monitors: real ones and virtual ones Muxin added (virtual: true), each with an id to pass as `display`, its name, size and place on the desktop. On Linux, \"all\" is the whole desktop.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List displays", true, false, true, false),
        }),
        json!({
            "name": "desktop_windows",
            "description": "The apps' windows on a machine's desktop (Windows, a Mac, or Linux on X11), front first: each with its id, title, program (`app`), size, and whether it's minimized or in front. Pass \"window:<id>\" as `display` to desktop_screenshot and desktop_input to see and use just that window (smaller pictures, even while it's covered; a minimized one is restored, and clicks or keys bring it to the front).",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List apps' windows", true, false, true, false),
        }),
        json!({
            "name": "desktop_add_display",
            "description": "Add a virtual monitor to a machine's desktop, like plugging in another screen (no screen needed): windows can move there, and you can screenshot and use it on its own (pass its id as `display`). Linux (X11) and macOS. On Linux it takes root: when sudo there wants a password, this fails and the user adds the monitor from the Muxin app, which asks for it. Stays until removed or the machine restarts. Only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "width": { "type": "integer", "minimum": 640, "maximum": 7680, "description": "Default: the main monitor's width." },
                "height": { "type": "integer", "minimum": 480, "maximum": 4320, "description": "Default: the main monitor's height." },
                "name": { "type": "string", "description": "What to call it, e.g. \"Agent screen\"." }
            }), &["machine_id"]),
            "annotations": annotations("Add virtual monitor", false, false, false, false),
        }),
        json!({
            "name": "desktop_remove_display",
            "description": "Remove a virtual monitor Muxin added (desktop_displays shows them as virtual: true). Windows on it move to the other monitors.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "display": { "type": "string", "description": DISPLAY_ID }
            }), &["machine_id", "display"]),
            "annotations": annotations("Remove virtual monitor", false, true, true, false),
        }),
        json!({
            "name": "desktop_set_display",
            "description": "Change a monitor's resolution (one of its `modes` from desktop_displays) or how much the desktop scales things up on it (`scales`; 2: twice as big and as sharp, like a Retina screen; on Linux X11 one scale applies to every monitor). A 4K monitor at scale 2 reads like 1080p but stays sharp. Changes the user's real desktop: only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "display": { "type": "string", "description": DISPLAY_ID },
                "width": { "type": "integer", "description": "With height: the new resolution." },
                "height": { "type": "integer" },
                "scale": { "type": "number", "description": "1, 2, ... (Linux desktops that offer it)." }
            }), &["machine_id", "display"]),
            "annotations": annotations("Set monitor resolution or scale", false, false, true, false),
        }),
        json!({
            "name": "log_sources",
            "description": "What a machine has in the way of logs: the system log (journal on Linux, unified log on a Mac; kind system, and kernel on Linux), log files (with readable: false for root-only ones), Docker containers and running services (systemd units; user: true for the user's own). Pass an entry's kind and target to logs_read.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List log sources", true, false, true, false),
        }),
        json!({
            "name": "logs_read",
            "description": "A log's last lines on a machine: the system log, the kernel's, a service's, a file's or a container's (see log_sources). filter keeps the lines holding it (any case). Read-only.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "kind": { "type": "string", "enum": ["system", "kernel", "file", "docker", "service"] },
                "target": { "type": "string", "description": "The file's path, the container's name or id, or the unit (e.g. nginx.service). Empty for the system log; on a Mac a process name narrows it." },
                "user": { "type": "boolean", "description": "A service of the user's own (systemctl --user)." },
                "lines": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Default 200." },
                "filter": { "type": "string" }
            }), &["machine_id", "kind"]),
            "annotations": annotations("Read logs", true, false, true, false),
        }),
        json!({
            "name": "machine_services",
            "description": "A machine's services and Docker: systemd services on Linux (the system's and the user's own, user: true; at_boot says whether one starts at boot), Homebrew's on a Mac (brew services), containers (status, ports), each with cpu (percent of one core) and memory (bytes) when running; Docker images and Compose projects (with their files). Failed ones come first.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List services and containers", true, false, true, false),
        }),
        json!({
            "name": "service_action",
            "description": "Start, stop or restart a service or container on a machine; enable or disable whether a systemd service starts at boot. The system's services need root: when sudo there wants a password this fails, and the user does it from the Muxin app (which asks). Stopping ssh, tailscaled or the network can cut the machine off. It changes the user's machine: only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "name": { "type": "string", "description": "The service's or container's name, as machine_services lists it (e.g. nginx, postgres)." },
                "action": { "type": "string", "enum": ["start", "stop", "restart", "enable", "disable"] },
                "container": { "type": "boolean", "description": "Look among containers only (when a service has the same name)." }
            }), &["machine_id", "name", "action"]),
            "annotations": annotations("Start, stop or restart a service", false, true, false, false),
        }),
        json!({
            "name": "docker_run",
            "description": "Run a Docker container on a machine (docker run -d), pulling the image first if it isn't there. Waits for it (an operation; its log_tail has Docker's output) unless wait is false. It changes the user's machine: only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "image": { "type": "string", "description": "e.g. postgres:17" },
                "name": { "type": "string" },
                "ports": { "type": "array", "items": { "type": "string" }, "description": "host:container, e.g. 5432:5432" },
                "env": { "type": "array", "items": { "type": "string" }, "description": "KEY=value" },
                "volumes": { "type": "array", "items": { "type": "string" }, "description": "host path or volume:container path" },
                "restart": { "type": "string", "enum": ["no", "always", "unless-stopped", "on-failure"], "description": "Default unless-stopped." },
                "command": { "type": "string", "description": "Instead of the image's own command." },
                "wait": { "type": "boolean" }
            }), &["machine_id", "image"]),
            "annotations": annotations("Run a container", false, false, false, false),
        }),
        json!({
            "name": "docker_pull",
            "description": "Pull a Docker image onto a machine. Waits for it unless wait is false.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "image": { "type": "string" },
                "wait": { "type": "boolean" }
            }), &["machine_id", "image"]),
            "annotations": annotations("Pull an image", false, false, true, false),
        }),
        json!({
            "name": "machine_processes",
            "description": "A machine's processes, the busiest few hundred: pid, user, name, command line, cpu (percent of one core), memory (bytes), how long it has run, GPU memory when NVIDIA says, and yours (owned by the user Muxin runs as); and its GPUs (NVIDIA, AMD on Linux, Apple): name, busy percent, memory used and total, temperature and power when known. Read-only.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List a machine's processes and GPUs", true, false, true, false),
        }),
        json!({
            "name": "process_signal",
            "description": "Stop (SIGTERM, the default) or kill (SIGKILL) a process on a machine, by pid from machine_processes. Another user's process needs root: when sudo wants a password it fails with a hint to use the Muxin app. It ends something on the user's machine: only when asked, and say which process first.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "pid": { "type": "integer" },
                "signal": { "type": "string", "enum": ["stop", "kill"] }
            }), &["machine_id", "pid"]),
            "annotations": annotations("Stop or kill a process", false, true, false, false),
        }),
        json!({
            "name": "run_on_machines",
            "description": "Run one shell command on several machines at once: in each one's login shell (sh-family; PowerShell on Windows), from the home folder, without a terminal (nothing interactive). One operation per machine; waits for all unless wait is false, and returns each one's exit_code and log_tail. It runs on the user's real machines as the user: only when asked, and show the command first.",
            "inputSchema": schema(json!({
                "machine_ids": { "type": "array", "items": { "type": "string" }, "description": "Machine ids (or names)." },
                "command": { "type": "string" },
                "timeout_secs": { "type": "integer", "description": "How long each may run, default 600, at most 3600." },
                "wait": { "type": "boolean" }
            }), &["machine_ids", "command"]),
            "annotations": annotations("Run a command on machines", false, true, false, false),
        }),
        json!({
            "name": "wake_machine",
            "description": "Wake a machine that's asleep or shut down (Wake on LAN), when its network card is known (Muxin learns it while the machine is on) and it's set up to wake that way. The signal goes from this computer and from online machines on the same network. Muxin checks it again after 20 and 40 seconds.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Wake a machine", false, false, true, true),
        }),
        json!({
            "name": "agent_usage",
            "description": "What the agent CLIs (Claude Code, Codex) used over the last N days (default 7), from their own logs on each machine: tokens (input, output, cache reads and writes) and requests by model and machine, each day's cost (daily) and the busiest folders (folders), and the cost at Anthropic's and OpenAI's list prices (null for a model whose price isn't known). With a Claude or ChatGPT plan the user doesn't pay per token. Read-only.",
            "inputSchema": schema(json!({
                "days": { "type": "integer", "description": "The last N days, 1 to 400. Default 7." },
                "machine_id": { "type": "string", "description": "One machine; default: all that are online." }
            }), &[]),
            "annotations": annotations("Agent usage and cost", true, false, true, false),
        }),
        json!({
            "name": "machine_ports",
            "description": "What's listening on a machine: each TCP port (port, address it listens on, host to connect to) with its program or Docker container, and for web servers (web: true) the status and title of their front page. Read-only. The user opens them on their computer from the Muxin app (machine page › Servers), or with `muxin forward <machine> <port>`.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("List a machine's servers", true, false, true, false),
        }),
        json!({
            "name": "compose_project",
            "description": "A Docker Compose project's parts on a machine (machine_services lists the projects): each service's image (or build folder), the ports it opens on the machine, what it waits for (depends_on with its condition), its networks, volumes and the machine's files it mounts, whether it has a health check, and its containers' state and health; the project's networks and volumes. Environment values aren't included, only how many. Read-only.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "project": { "type": "string" }
            }), &["machine_id", "project"]),
            "annotations": annotations("Describe a Compose project", true, false, true, false),
        }),
        json!({
            "name": "compose_action",
            "description": "Bring a Docker Compose project up (detached), down, restart it or pull its images, on a machine (machine_services lists the projects). down stops and removes its containers. With service: start (with what it waits for), stop or restart just that service. Waits unless wait is false. Only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "project": { "type": "string" },
                "action": { "type": "string", "enum": ["up", "down", "restart", "pull", "start", "stop"] },
                "service": { "type": "string", "description": "One of the project's services (compose_project lists them): start, stop or restart only it." },
                "wait": { "type": "boolean" }
            }), &["machine_id", "project", "action"]),
            "annotations": annotations("Compose up, down, restart or pull", false, true, false, false),
        }),
        json!({
            "name": "git_status",
            "description": "Whether git on one of the user's machines can commit and push to GitHub: its name and email (can_commit), how a push gets a GitHub login (push: gh, other, none), the machine's GitHub CLI account, and the identity git_setup would use.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Git setup status", true, false, true, false),
        }),
        json!({
            "name": "git_setup",
            "description": "Set up git on a machine so commits and pushes work (for agents working there too). identity: true sets git's global name and email to the user's GitHub name and private noreply address (or pass name and email). credentials: true runs `gh auth setup-git`, so git uses the machine's GitHub CLI login for github.com. Changes the machine's global git config; only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "identity": { "type": "boolean" },
                "name": { "type": "string" },
                "email": { "type": "string" },
                "credentials": { "type": "boolean" }
            }), &["machine_id"]),
            "annotations": annotations("Set up git", false, false, true, false),
        }),
        json!({
            "name": "git_sync",
            "description": "Run git fetch, pull (fast-forward only) or push in a checkout on one of the user's machines. Changes their checkout or the remote; only when asked.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "path": { "type": "string", "description": "The checkout's folder on that machine (from local_checkouts)." },
                "action": { "type": "string", "enum": ["fetch", "pull", "push"] }
            }), &["machine_id", "path", "action"]),
            "annotations": annotations("Git fetch/pull/push", false, false, false, true),
        }),
    ]
}

/// Ready-to-paste setup for common MCP clients.
pub fn print_config() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "muxin".into());
    let json_block = serde_json::to_string_pretty(&json!({
        "mcpServers": { "muxin": { "command": exe, "args": ["mcp"] } }
    }))
    .unwrap_or_default();
    format!(
        "# Claude Code\nclaude mcp add muxin -- {exe} mcp\n\n\
         # Codex (~/.codex/config.toml)\n[mcp_servers.muxin]\ncommand = \"{exe}\"\nargs = [\"mcp\"]\n\n\
         # Other MCP clients (Claude Desktop, Cursor, ...)\n{json_block}\n\n\
         # Try it safely first with simulated machines: add \"--demo\" after \"mcp\".\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn demo() -> Arc<Engine> {
        Engine::new(Paths::resolve(Some(std::env::temp_dir().join("muxen-mcp-test")), None), true)
    }

    #[test]
    fn tool_schemas_are_well_formed() {
        let tools = tools();
        assert_eq!(tools.len(), 96);
        let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), tools.len(), "duplicate tool names");
        for t in &tools {
            let name = t["name"].as_str().unwrap();
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{name}");
            assert!(t["description"].as_str().unwrap().len() > 40, "{name}");
            assert_eq!(t["inputSchema"]["type"], "object", "{name}");
            for req in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(t["inputSchema"]["properties"].get(req.as_str().unwrap()).is_some(), "{name}: {req}");
            }
            assert!(t["annotations"]["readOnlyHint"].is_boolean());
            assert_eq!(t["inputSchema"]["additionalProperties"], false, "{name}");
        }
    }

    #[test]
    fn workspace_tools_are_annotated_for_what_they_do() {
        let tools = tools();
        let get = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap_or_else(|| panic!("missing {n}")).clone();
        for read_only in ["workspace_list", "workspace_get", "workspace_read_file", "workspace_list_files", "workspace_changes"] {
            assert_eq!(get(read_only)["annotations"]["readOnlyHint"], true, "{read_only}");
        }
        for changes in ["workspace_create", "workspace_exec", "workspace_write_file", "workspace_sync", "workspace_destroy", "workspace_allow"] {
            assert_eq!(get(changes)["annotations"]["readOnlyHint"], false, "{changes}");
        }
        assert_eq!(get("workspace_destroy")["annotations"]["destructiveHint"], true);
        let exec = get("workspace_exec");
        assert_eq!(exec["inputSchema"]["required"], json!(["workspace_id", "command"]));
        assert_eq!(exec["inputSchema"]["properties"]["env"]["type"], "object");
        assert_eq!(get("workspace_sync")["inputSchema"]["properties"]["direction"]["enum"], json!(["push", "pull"]));
        assert!(INSTRUCTIONS.contains("workspace_create"));
    }

    #[test]
    fn exec_results_read_as_text() {
        let text = readable_text(
            "workspace_exec",
            &json!({ "exit_code": 1, "duration_ms": 1200, "stdout": "ok\n", "stderr": "boom", "timed_out": false, "truncated": false, "background_pid": null }),
        )
        .unwrap();
        assert!(text.starts_with("exit code: 1 (1200 ms)"), "{text}");
        assert!(text.contains("stdout:\n```\nok\n```") && text.contains("stderr:\n```\nboom\n```"), "{text}");
        let bg = readable_text("workspace_exec", &json!({ "background_pid": 42, "log_path": ".muxen/logs/1.log", "duration_ms": 5 })).unwrap();
        assert!(bg.contains("pid 42") && bg.contains(".muxen/logs/1.log"));
        assert!(readable_text("status", &json!({})).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_records_the_client_as_owner() {
        let engine = demo();
        let session = Session::default();
        assert_eq!(session.owner(), "mcp");
        handle(&engine, &session, "initialize", json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code","version":"2"}})).await.unwrap();
        assert_eq!(session.owner(), "claude-code");
    }

    /// Needs the workspace backend (src/fleet/workspaces.rs); run with `cargo test -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs the workspace backend"]
    async fn workspace_tools_round_trip_in_demo() {
        let engine = demo();
        engine.run_check(None).await;
        let session = Session::default();
        handle(&engine, &session, "initialize", json!({"clientInfo":{"name":"codex"}})).await.unwrap();
        let created = call_tool(&engine, &session, "workspace_create", json!({"platform":"linux","repo":"acme/api"})).await;
        assert_eq!(created["isError"], false, "{created}");
        let id = created["structuredContent"]["workspace"]["id"].as_str().unwrap().to_string();
        assert_eq!(created["structuredContent"]["workspace"]["owner"], "codex");
        let ran = call_tool(&engine, &session, "workspace_exec", json!({"workspace_id": id, "command": "pwd"})).await;
        assert_eq!(ran["isError"], false, "{ran}");
        assert!(ran["content"][0]["text"].as_str().unwrap().starts_with("exit code: 0"));
        let destroyed = call_tool(&engine, &session, "workspace_destroy", json!({"workspace_id": id})).await;
        assert_eq!(destroyed["isError"], false, "{destroyed}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speaks_mcp_over_a_stream() {
        let engine = demo();
        let (client, server) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = tokio::io::split(server);
        let task = tokio::spawn(serve(engine, BufReader::new(server_read), server_write));
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut lines = BufReader::new(client_read).lines();
        let mut recv = async || -> Value {
            let line = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line()).await.expect("reply in time");
            serde_json::from_str(&line.unwrap().unwrap()).unwrap()
        };

        let mut send = async |msg: Value| {
            let mut bytes = serde_json::to_vec(&msg).unwrap();
            bytes.push(b'\n');
            client_write.write_all(&bytes).await.unwrap();
        };
        send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
        let init: Value = recv().await;
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "muxin");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("Start with status"));

        send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let list: Value = recv().await;
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), tools().len());

        send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}})).await;
        let status: Value = recv().await;
        assert_eq!(status["result"]["isError"], false);
        assert_eq!(status["result"]["structuredContent"]["counts"]["machines"], 9);
        assert!(status["result"]["content"][0]["text"].as_str().unwrap().contains("\"summary\""));

        // Tool failures are results with isError, carrying a code an agent can branch on.
        send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_machine","arguments":{"machine_id":"nope"}}})).await;
        let missing: Value = recv().await;
        assert_eq!(missing["result"]["isError"], true);
        assert_eq!(missing["result"]["structuredContent"]["error"]["code"], "not_found");

        send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"runner_action","arguments":{"runner_id":"x","action":"explode"}}})).await;
        let bad: Value = recv().await;
        assert_eq!(bad["result"]["structuredContent"]["error"]["code"], "invalid");

        // Workspace arguments are validated before anything runs.
        send(json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"workspace_read_file","arguments":{"workspace_id":"ws-x","path":"../../etc/passwd"}}})).await;
        let escape: Value = recv().await;
        assert_eq!(escape["result"]["isError"], true);
        assert_eq!(escape["result"]["structuredContent"]["error"]["code"], "invalid");
        send(json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"workspace_exec","arguments":{"workspace_id":"ws-x"}}})).await;
        let missing_arg: Value = recv().await;
        assert_eq!(missing_arg["result"]["structuredContent"]["error"]["code"], "invalid");
        send(json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"workspace_get","arguments":{"workspace_id":"ws-nope"}}})).await;
        let unknown_ws: Value = recv().await;
        assert_eq!(unknown_ws["result"]["structuredContent"]["error"]["code"], "not_found");

        send(json!({"jsonrpc":"2.0","id":6,"method":"nope/nothing"})).await;
        let unknown: Value = recv().await;
        assert_eq!(unknown["error"]["code"], -32601);

        send(json!({"jsonrpc":"2.0","id":7,"method":"ping"})).await;
        let pong: Value = recv().await;
        assert_eq!(pong["id"], 7);

        // Closing the client ends the session.
        drop(send);
        drop(recv);
        drop(client_write);
        drop(lines);
        tokio::time::timeout(std::time::Duration::from_secs(10), task).await.expect("server exits on EOF").unwrap().unwrap();
    }
}
