//! `openrunner mcp`: a Model Context Protocol server over stdio, so agents
//! (Claude Code, Codex, any MCP client) can inspect and run the fleet.
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
OpenRunner manages a fleet of self-hosted GitHub Actions runners: the machines that host them \
(reached with the system ssh, or directly for host \"local\") and the runners on each.

Start with fleet_status and read its `attention` list first; it names the problems worth fixing \
and the machine/runner they're about. Ids: machines are slugs like \"mac-studio\"; runners are \
\"<machine-id>/<runner-name>\", or \"github/<name>\" for a runner GitHub knows but no machine lists.

Read-only: fleet_status, get_machine, list_runners, list_jobs, check_machines (runs a read-only \
check over ssh), get_operation, list_operations, get_settings, list_ssh_hosts.
Changes things: add_machine / remove_machine edit fleet.toml only. install_runners and \
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
anything there you wouldn't do in their own terminal without asking.";

/// Per-connection state: who the client is, so workspaces and commands record their owner.
#[derive(Default)]
pub struct Session {
    client: Mutex<Option<String>>,
}

impl Session {
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
                "serverInfo": { "name": "openrunner", "title": "OpenRunner", "version": env!("CARGO_PKG_VERSION") },
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
        "fleet_status" => ok(api::status(engine, args(a)?, Surface::Mcp).await?),
        "get_machine" => ok(api::machine(engine, args(a)?).await?),
        "list_runners" => ok(json!({ "runners": api::runners(engine, args(a)?).await? })),
        "list_jobs" => ok(api::jobs(engine, Surface::Mcp).await?),
        "check_machines" => ok(json!({ "machines": api::check(engine, args(a)?).await? })),
        "add_machine" => ok(api::add_machine(engine, args(a)?, Surface::Mcp).await?),
        "remove_machine" => ok(json!({ "removed": api::remove_machine(engine, args(a)?).await? })),
        "install_runners" => ok(api::install_runners(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "runner_action" => ok(api::runner_action(engine, args(a)?, Surface::Mcp, |_| {}).await?),
        "get_operation" => ok(api::operation(engine, args(a)?)?),
        "list_operations" => ok(json!({ "operations": api::operations(engine) })),
        "github_scopes" => ok(api::github_scopes(engine, args(a)?).await?),
        "get_settings" => ok(api::settings(engine)),
        "update_settings" => ok(api::update_settings(engine, args(a)?)?),
        "list_ssh_hosts" => ok(api::ssh_hosts()),
        "workspace_create" => ok(api::workspace_create(engine, args(a)?, &session.owner(), Surface::Mcp).await?),
        "workspace_list" => ok(api::workspaces_list(engine)),
        "workspace_get" => ok(api::workspace_get(engine, args(a)?, Surface::Mcp)?),
        "workspace_exec" => ok(api::workspace_exec(engine, args(a)?, &session.owner(), Surface::Mcp).await?),
        "workspace_read_file" => ok(api::workspace_read_file(engine, args(a)?, Surface::Mcp).await?),
        "workspace_write_file" => ok(api::workspace_write_file(engine, args(a)?, Surface::Mcp).await?),
        "workspace_list_files" => ok(api::workspace_list_files(engine, args(a)?, Surface::Mcp).await?),
        "workspace_changes" => ok(api::workspace_changes(engine, args(a)?, Surface::Mcp).await?),
        "workspace_sync" => ok(api::workspace_sync(engine, args(a)?, Surface::Mcp).await?),
        "workspace_destroy" => ok(api::workspace_destroy(engine, args(a)?, Surface::Mcp).await?),
        "workspace_allow" => ok(api::workspace_allow(engine, args(a)?, Surface::Mcp)?),
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
const WORKSPACE_ID: &str = "Workspace id from workspace_create or workspace_list, e.g. \"ws-7f3a2c\".";
const WS_PATH: &str = "Path relative to the workspace folder, e.g. \"src/main.rs\". Absolute paths and \"..\" escapes are rejected.";
const RUNNER_ID: &str = "Runner id: \"<machine-id>/<runner-name>\", or \"github/<name>\" for a runner no machine lists. A bare runner name works when unique.";

/// The tool catalog. Descriptions are written for a model deciding what to call.
pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "fleet_status",
            "description": "Overview of the whole fleet: counts of working/idle/offline runners, queued jobs, GitHub connection, every machine with CPU/memory/disk/uptime and a runner summary, and an `attention` list of problems (unreachable machines, stopped runners, jobs no runner can take, low disk). Call this first. Uses the last machine check unless refresh=true, which re-checks every machine over ssh first.",
            "inputSchema": schema(json!({
                "refresh": { "type": "boolean", "description": "Re-check all machines over ssh and sync GitHub before answering. Slower; default false." }
            }), &[]),
            "annotations": annotations("Fleet status", true, false, true, true),
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
            "description": "Add a machine to the fleet (written to fleet.toml, comments preserved) and check it. `host` is anything the system ssh accepts (user@host, an ~/.ssh/config alias; see list_ssh_hosts) or \"local\" for this computer. Key-based ssh must work without a password prompt. Doesn't install anything.",
            "inputSchema": schema(json!({
                "name": { "type": "string", "description": "Display name, e.g. \"Mac Studio\"." },
                "host": { "type": "string", "description": "ssh destination or \"local\"." },
                "check": { "type": "boolean", "description": "Check it right away and return what was found. Default true." }
            }), &["name", "host"]),
            "annotations": annotations("Add machine", false, false, false, true),
        }),
        json!({
            "name": "remove_machine",
            "description": "Remove a machine from fleet.toml. Nothing on the machine changes and its runners stay registered on GitHub; use runner_action remove first to take runners down.",
            "inputSchema": schema(json!({ "machine_id": { "type": "string", "description": MACHINE_ID } }), &["machine_id"]),
            "annotations": annotations("Remove machine", false, true, true, false),
        }),
        json!({
            "name": "install_runners",
            "description": "Install GitHub Actions runners on a machine and register each with the GitHub org/repo (`scope`) using a registration token from the user's gh login, then add them to fleet.toml. How they run (`mode`): \"auto\" (default) picks a boot-time service when the machine allows it (macOS, Windows, Linux with passwordless sudo), else Docker containers when Docker is available, else a background run.sh; \"docker\" runs one container per runner from GitHub's actions-runner image (no sudo, restarted by Docker, jobs get a clean Linux userland). Modifies the real machine and GitHub. Needs admin on the org (admin:org scope) or repo. Names default to \"<machine-id>-01\", \"-02\", ...",
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
            "description": "Control one runner on its machine: start, stop or restart its service; logs (read-only tail of its newest _diag log); or remove (stop, uninstall the service, deregister from GitHub, drop it from fleet.toml, optionally delete its folder). For a runner no machine lists (\"github/<name>\"), remove deregisters it on GitHub. stop and remove take the runner offline; confirm with the user first.",
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
            "name": "get_operation",
            "description": "Status and output of an install or runner action started by this server (use after wait=false or a timeout). Lines starting with \"▸ \" are OpenRunner's steps, \"✓ \" successes, \"error: \" problems.",
            "inputSchema": schema(json!({
                "operation_id": { "type": "integer" },
                "tail": { "type": "integer", "minimum": 0, "description": "Log lines from the end. Default 80." }
            }), &["operation_id"]),
            "annotations": annotations("Get operation", true, false, true, false),
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
            "description": "Host aliases from the user's ~/.ssh/config, to suggest as `host` for add_machine.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations("List ssh hosts", true, false, true, false),
        }),
        json!({
            "name": "workspace_create",
            "description": "Check out one of the user's own machines as your work environment, like a remote dev box: creates a persistent folder there (optionally cloning a git repo at a ref, or creating a new branch) and returns a workspace_id for the other workspace_* tools. Omit machine_id to get the least-loaded online machine that allows workspaces, filtered by `platform` (e.g. \"linux\" to run tests on Linux). Use one workspace per task and reuse it across steps. Nothing is deleted automatically: call workspace_destroy when done. Private GitHub repos are cloned with the user's gh login.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": "Machine to use; omit to let OpenRunner pick." },
                "platform": { "type": "string", "description": "When picking: platform prefix such as \"linux\", \"linux-x64\", \"darwin-arm64\", \"windows\"." },
                "name": { "type": "string", "description": "Short label, e.g. \"api-tests\". Defaults to the repo name." },
                "repo": { "type": "string", "description": "Git URL, or \"owner/repo\" for GitHub, to clone into the workspace." },
                "ref": { "type": "string", "description": "Branch, tag or commit to check out." },
                "branch": { "type": "string", "description": "New branch to create and switch to after checkout." },
                "isolation": { "type": "string", "enum": ["host", "docker"], "description": "\"host\" (default): commands run directly on the machine with its tools and login environment. \"docker\": a fresh, isolated container per workspace (folder mounted at /workspace); pick the toolchain with `image`. Prefer docker for untrusted code or when you need a clean environment." },
                "image": { "type": "string", "description": "Docker image for isolation \"docker\", e.g. \"node:22\", \"python:3.13\", \"rust:1\", \"golang:1.25\". Defaults to buildpack-deps:bookworm. Choose an image that already has your tools: the container runs as the ssh user, so apt-get install won't work." },
                "docker_socket": { "type": "boolean", "description": "Share the machine's Docker socket with the container (docker-in-docker). Root-equivalent on that machine; only when the task needs Docker." }
            }), &[]),
            "annotations": annotations("Create workspace", false, false, false, true),
        }),
        json!({
            "name": "workspace_list",
            "description": "All workspaces (yours and other agents'): machine, folder, repo/ref, owner, status, last command, and which machines can take new workspaces. Check here before creating a new one; reuse a ready workspace for the same task.",
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
            "description": "Copy code between a folder on this computer (usually the user's local checkout) and a workspace with rsync over ssh. direction=push sends the local folder to the workspace (e.g. to test uncommitted local changes on another OS); direction=pull brings the workspace's files back. delete=true also removes files on the receiving side that the sender doesn't have; pulling with delete into the user's checkout can remove their files, so confirm first.",
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
            "description": "Turn agent workspaces on or off for a machine (edits fleet.toml, comments preserved). Machines allow them by default; only change this when the user asks.",
            "inputSchema": schema(json!({
                "machine_id": { "type": "string", "description": MACHINE_ID },
                "allowed": { "type": "boolean" }
            }), &["machine_id", "allowed"]),
            "annotations": annotations("Allow workspaces", false, false, true, false),
        }),
    ]
}

/// Ready-to-paste setup for common MCP clients.
pub fn print_config() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "openrunner".into());
    let json_block = serde_json::to_string_pretty(&json!({
        "mcpServers": { "openrunner": { "command": exe, "args": ["mcp"] } }
    }))
    .unwrap_or_default();
    format!(
        "# Claude Code\nclaude mcp add openrunner -- {exe} mcp\n\n\
         # Codex (~/.codex/config.toml)\n[mcp_servers.openrunner]\ncommand = \"{exe}\"\nargs = [\"mcp\"]\n\n\
         # Other MCP clients (Claude Desktop, Cursor, ...)\n{json_block}\n\n\
         # Try it safely first with a simulated fleet: add \"--demo\" after \"mcp\".\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn demo() -> Arc<Engine> {
        Engine::new(Paths::resolve(Some(std::env::temp_dir().join("openrunner-mcp-test")), None), true)
    }

    #[test]
    fn tool_schemas_are_well_formed() {
        let tools = tools();
        assert_eq!(tools.len(), 26);
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
        let bg = readable_text("workspace_exec", &json!({ "background_pid": 42, "log_path": ".openrunner/logs/1.log", "duration_ms": 5 })).unwrap();
        assert!(bg.contains("pid 42") && bg.contains(".openrunner/logs/1.log"));
        assert!(readable_text("fleet_status", &json!({})).is_none());
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
        assert_eq!(init["result"]["serverInfo"]["name"], "openrunner");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("fleet_status"));

        send(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let list: Value = recv().await;
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), tools().len());

        send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fleet_status","arguments":{}}})).await;
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
