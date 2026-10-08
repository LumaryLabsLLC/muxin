//! Workspaces in demo mode: no ssh, no files on disk. Commands get believable
//! canned output and files live in memory, so the app and agents can try the
//! whole flow safely.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use super::registry::WorkspaceRecord;
use super::{ExecResult, FileContent, FileEntry, SyncDirection, file_content};
use crate::demo::World;
use crate::fleet::Engine;
use crate::fleet::model::{WorkspaceCommand, WorkspaceInfo};

/// A workspace's files in demo mode.
#[derive(Debug, Clone, Default)]
pub struct DemoTree {
    pub files: BTreeMap<String, String>,
    /// Files that came with the checkout; others written later show as untracked.
    pub original: BTreeSet<String>,
    pub changed: BTreeSet<String>,
    pub branch: String,
    pub git: bool,
}

async fn pause(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn demo_path(platform: &str, id: &str) -> String {
    if platform.starts_with("darwin") {
        format!("/Users/ci/muxen/workspaces/{id}")
    } else if platform.starts_with("windows") {
        format!("C:\\Users\\ci\\muxen\\workspaces\\{id}")
    } else {
        format!("/home/ci/muxen/workspaces/{id}")
    }
}

/// Files a fresh checkout of `repo` gets.
fn template(repo: Option<&str>) -> Vec<(&'static str, String)> {
    let name = repo.map(|r| r.rsplit('/').next().unwrap_or(r).trim_end_matches(".git").to_string());
    match name.as_deref() {
        None => Vec::new(),
        Some("api") => vec![
            ("README.md", "# acme/api\n\nThe Acme checkout and orders API.\n\n    npm ci\n    npm test\n".into()),
            ("package.json", "{\n  \"name\": \"@acme/api\",\n  \"private\": true,\n  \"scripts\": {\n    \"dev\": \"tsx watch src/server.ts\",\n    \"test\": \"vitest run\"\n  }\n}\n".into()),
            ("src/server.ts", "import { createServer } from './app';\n\ncreateServer().listen(3000, () => console.log('api listening on :3000'));\n".into()),
            ("src/routes/checkout.ts", "export function total(items: Item[], discount = 0) {\n  const subtotal = items.reduce((s, i) => s + i.price * i.qty, 0);\n  return subtotal * (1 - discount) * 1.08;\n}\n".into()),
            ("e2e/checkout.spec.ts", "import { test, expect } from '@playwright/test';\n\ntest('applies the discount before tax', async ({ page }) => {\n  await page.goto('/checkout');\n  await expect(page.getByTestId('total')).toHaveText('$97.20');\n});\n".into()),
            (".github/workflows/ci.yml", "name: CI\non: [push, pull_request]\njobs:\n  test:\n    runs-on: [self-hosted, Linux, X64]\n    steps:\n      - uses: actions/checkout@v4\n      - run: npm ci && npm test\n".into()),
        ],
        Some("desktop") => vec![
            ("README.md", "# acme/desktop\n\nThe Acme desktop app (SwiftUI).\n\n    swift build\n    swift test\n".into()),
            ("Package.swift", "// swift-tools-version: 6.0\nimport PackageDescription\n\nlet package = Package(name: \"Desktop\", platforms: [.macOS(.v14)], targets: [.executableTarget(name: \"Desktop\"), .testTarget(name: \"DesktopTests\", dependencies: [\"Desktop\"])])\n".into()),
            ("Sources/Desktop/App.swift", "import SwiftUI\n\n@main\nstruct DesktopApp: App {\n    var body: some Scene { WindowGroup { ContentView() } }\n}\n".into()),
            ("Tests/DesktopTests/AppTests.swift", "import Testing\n@testable import Desktop\n\n@Test func launches() { #expect(true) }\n".into()),
        ],
        Some(other) => vec![
            ("README.md", format!("# {other}\n")),
            ("package.json", format!("{{\n  \"name\": \"{other}\",\n  \"scripts\": {{ \"test\": \"vitest run\" }}\n}}\n")),
            ("src/index.ts", "export const hello = () => 'world';\n".into()),
        ],
    }
}

fn tree_for(repo: Option<&str>, branch: &str) -> DemoTree {
    let files: BTreeMap<String, String> = template(repo).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    DemoTree { original: files.keys().cloned().collect(), files, changed: BTreeSet::new(), branch: branch.to_string(), git: repo.is_some() }
}

fn command(cmd: &str, exit: i32, ago_secs: i64, ms: u64, tail: &[&str], by: &str) -> WorkspaceCommand {
    WorkspaceCommand {
        command: cmd.into(),
        exit_code: Some(exit),
        started_at: Utc::now() - chrono::Duration::seconds(ago_secs),
        duration_ms: ms,
        output_tail: tail.iter().map(|s| s.to_string()).collect(),
        background: false,
        timed_out: false,
        by: Some(by.into()),
    }
}

/// Workspaces agents have been using (one of them in a container), so the app has something to show.
pub(super) fn seed(world: &World) -> Vec<WorkspaceRecord> {
    let now = Utc::now();
    let api_id = "ws-k3p9xa";
    let desk_id = "ws-7d2m4q";
    let web_id = "ws-q8v2rn";
    let mut api_tree = tree_for(Some("acme/api"), "fix/flaky-e2e");
    api_tree.files.insert(
        "e2e/checkout.spec.ts".into(),
        "import { test, expect } from '@playwright/test';\n\ntest('applies the discount before tax', async ({ page }) => {\n  await page.goto('/checkout');\n  await page.getByTestId('total').waitFor();\n  await expect(page.getByTestId('total')).toHaveText('$97.20');\n});\n".into(),
    );
    api_tree.changed.insert("e2e/checkout.spec.ts".into());
    let desk_tree = tree_for(Some("acme/desktop"), "main");
    {
        let mut files = world.workspace_files.lock().unwrap();
        files.insert(api_id.into(), api_tree);
        files.insert(desk_id.into(), desk_tree);
        files.insert(web_id.into(), tree_for(Some("acme/web"), "chore/node-22"));
    }

    let api = WorkspaceInfo {
        id: api_id.into(),
        name: "api".into(),
        machine_id: "ubuntu-nuc".into(),
        machine_name: "Ubuntu NUC".into(),
        host: "nuc-ubuntu".into(),
        platform: "linux-x64".into(),
        path: demo_path("linux-x64", api_id),
        repo: Some("acme/api".into()),
        git_ref: Some("main".into()),
        branch: Some("fix/flaky-e2e".into()),
        owner: Some("claude-code".into()),
        status: "ready".into(),
        error: None,
        created_at: now - chrono::Duration::hours(2),
        last_used_at: now - chrono::Duration::minutes(4),
        commands: 4,
        history: vec![
            command("git status", 0, 6900, 41, &["On branch fix/flaky-e2e", "nothing to commit, working tree clean"], "claude-code"),
            command("npm ci", 0, 6840, 9120, &["added 412 packages, and audited 413 packages in 9s", "found 0 vulnerabilities"], "claude-code"),
            command(
                "npm test -- e2e/checkout.spec.ts",
                1,
                1500,
                14210,
                &[
                    " ✗ e2e/checkout.spec.ts > applies the discount before tax (5003 ms)",
                    "   → Timed out 5000ms waiting for expect(locator).toHaveText(expected)",
                    " Test Files  1 failed (1)",
                    "      Tests  1 failed | 11 passed (12)",
                ],
                "claude-code",
            ),
            command(
                "npm test -- e2e/checkout.spec.ts",
                0,
                250,
                11840,
                &[" ✓ e2e/checkout.spec.ts (12 tests) 9.42s", " Test Files  1 passed (1)", "      Tests  12 passed (12)"],
                "claude-code",
            ),
        ],
        shell: String::new(),
        isolation: "host".into(),
        image: None,
        container: None,
    };
    let desk = WorkspaceInfo {
        id: desk_id.into(),
        name: "desktop".into(),
        machine_id: "mac-studio".into(),
        machine_name: "Mac Studio".into(),
        host: "ci@mac-studio.local".into(),
        platform: "darwin-arm64".into(),
        path: demo_path("darwin-arm64", desk_id),
        repo: Some("acme/desktop".into()),
        git_ref: Some("main".into()),
        branch: None,
        owner: Some("codex".into()),
        status: "ready".into(),
        error: None,
        created_at: now - chrono::Duration::hours(26),
        last_used_at: now - chrono::Duration::hours(3),
        commands: 2,
        history: vec![
            command("swift build", 0, 11200, 48200, &["Building for debugging...", "Build complete! (48.20s)"], "codex"),
            command(
                "swift test",
                0,
                10900,
                31800,
                &["Test Suite 'All tests' passed at 2026-10-07 13:22:41.118.", "\t Executed 58 tests, with 0 failures (0 unexpected) in 4.211 seconds"],
                "codex",
            ),
        ],
        shell: String::new(),
        isolation: "host".into(),
        image: None,
        container: None,
    };
    let web = WorkspaceInfo {
        id: web_id.into(),
        name: "web".into(),
        machine_id: "ubuntu-nuc".into(),
        machine_name: "Ubuntu NUC".into(),
        host: "nuc-ubuntu".into(),
        platform: "linux-x64".into(),
        path: demo_path("linux-x64", web_id),
        repo: Some("acme/web".into()),
        git_ref: Some("main".into()),
        branch: Some("chore/node-22".into()),
        owner: Some("claude-code".into()),
        status: "ready".into(),
        error: None,
        created_at: now - chrono::Duration::minutes(50),
        last_used_at: now - chrono::Duration::minutes(12),
        commands: 3,
        history: vec![
            command("node -v", 0, 2900, 180, &["v22.11.0"], "claude-code"),
            command("npm ci", 0, 2840, 21400, &["added 655 packages, and audited 656 packages in 21s", "found 0 vulnerabilities"], "claude-code"),
            command("npm run build", 0, 720, 38900, &["vite v6.0.3 building for production...", "✓ 1342 modules transformed.", "✓ built in 37.62s"], "claude-code"),
        ],
        shell: String::new(),
        isolation: "docker".into(),
        image: Some("node:22".into()),
        container: Some(super::container_name(web_id)),
    };
    vec![
        WorkspaceRecord { info: api, background_pids: Vec::new(), windows: false, next_log: 1, docker_socket: false },
        WorkspaceRecord { info: desk, background_pids: Vec::new(), windows: false, next_log: 1, docker_socket: false },
        WorkspaceRecord { info: web, background_pids: Vec::new(), windows: false, next_log: 1, docker_socket: false },
    ]
}

pub(super) async fn create(engine: &Arc<Engine>, op: u64, rec: &WorkspaceRecord, repo: Option<&str>) -> Result<String, String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    let info = &rec.info;
    engine.op_log(op, format!("▸ Creating {} on {} ({})", info.id, info.machine_name, info.host));
    pause(300).await;
    w.reachable(&info.machine_id, &info.host).map_err(|e| format!("{} is unreachable: {e}", info.machine_name))?;
    let path = demo_path(&info.platform, &info.id);
    engine.op_log(op, format!("Folder: {path}"));
    let branch = info.branch.clone().or_else(|| info.git_ref.clone()).unwrap_or_else(|| "main".into());
    if let Some(r) = repo {
        engine.op_log(op, format!("Cloning {r}"));
        for line in [
            "Cloning into '.'...".to_string(),
            "remote: Enumerating objects: 1284, done.".into(),
            "remote: Counting objects: 100% (1284/1284), done.".into(),
            "remote: Compressing objects: 100% (731/731), done.".into(),
            "Receiving objects: 100% (1284/1284), 2.31 MiB | 18.40 MiB/s, done.".into(),
            "Resolving deltas: 100% (402/402), done.".into(),
        ] {
            engine.op_log(op, line);
            pause(120).await;
        }
        if let Some(b) = &info.branch {
            engine.op_log(op, format!("Switched to a new branch '{b}'"));
        }
        engine.op_log(op, format!("Checked out {branch} at 3f9c2e1"));
    }
    if let (Some(image), Some(container)) = (&info.image, &info.container) {
        engine.op_log(op, format!("Pulling {image}"));
        for line in [format!("Digest: sha256:4f6e1b2a9c0d…"), format!("Status: Downloaded newer image for {image}")] {
            pause(250).await;
            engine.op_log(op, line);
        }
        engine.op_log(op, format!("Starting container {container} from {image}"));
        pause(300).await;
        engine.op_log(op, format!("Container {container} is running"));
    }
    w.workspace_files.lock().unwrap().insert(info.id.clone(), tree_for(repo, &branch));
    Ok(path)
}

pub(super) async fn exec(engine: &Arc<Engine>, rec: &WorkspaceRecord, cmd: &str, cwd: &str, background: bool) -> Result<ExecResult, String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    w.reachable(&rec.info.machine_id, &rec.info.host).map_err(|e| format!("{} is unreachable: {e}", rec.info.machine_name))?;
    if background {
        let n = engine
            .update_workspaces(|list| {
                list.iter_mut().find(|r| r.info.id == rec.info.id).map(|r| {
                    let n = r.next_log.max(1);
                    r.next_log = n + 1;
                    n
                })
            })?
            .unwrap_or(1);
        let log = format!(".muxen/logs/{n}.log");
        let mut files = w.workspace_files.lock().unwrap();
        let tree = files.entry(rec.info.id.clone()).or_default();
        tree.files.insert(log.clone(), format!("$ {cmd}\n> ready on http://localhost:3000\n"));
        let pid = 40_000 + (cmd.len() as u32 * 37 + n * 101) % 20_000;
        return Ok(ExecResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
            timed_out: false,
            truncated: false,
            background_pid: Some(pid),
            log_path: Some(log),
        });
    }
    pause(150 + (cmd.len() as u64 * 13) % 400).await;
    let (stdout, stderr, code) = respond(&w, &rec.info, cmd.trim(), cwd).await;
    Ok(ExecResult {
        exit_code: Some(code),
        stdout,
        stderr,
        duration_ms: 0,
        timed_out: false,
        truncated: false,
        background_pid: None,
        log_path: None,
    })
}

/// Canned output for the commands agents run most.
async fn respond(w: &World, info: &WorkspaceInfo, cmd: &str, cwd: &str) -> (String, String, i32) {
    let ok = |s: &str| (s.to_string(), String::new(), 0);
    let tree = w.workspace_files.lock().unwrap().get(&info.id).cloned().unwrap_or_default();
    let root = if info.container.is_some() { "/workspace".to_string() } else { info.path.clone() };
    let here = if cwd.is_empty() { root } else { format!("{root}/{cwd}") };
    let linux = !info.platform.starts_with("darwin");
    let first = cmd.split_whitespace().next().unwrap_or("");
    match cmd {
        "pwd" => return ok(&here),
        "whoami" => return ok("ci"),
        "hostname" => return ok(&info.machine_id),
        "true" | ":" => return ok(""),
        "false" => return (String::new(), String::new(), 1),
        "node -v" | "node --version" => return ok("v22.11.0"),
        "npm -v" | "npm --version" => return ok("10.9.0"),
        "cargo --version" => return ok("cargo 1.91.0 (ea2d97820 2025-10-10)"),
        "rustc --version" => return ok("rustc 1.91.0 (f8297e351 2025-10-28)"),
        "go version" => return ok(if linux { "go version go1.25.3 linux/amd64" } else { "go version go1.25.3 darwin/arm64" }),
        "python3 --version" | "python --version" => return ok("Python 3.12.3"),
        "uname" => return ok(if linux { "Linux" } else { "Darwin" }),
        "uname -a" => {
            return ok(if linux {
                "Linux nuc-ubuntu 6.8.0-45-generic #45-Ubuntu SMP PREEMPT_DYNAMIC x86_64 x86_64 x86_64 GNU/Linux"
            } else {
                "Darwin mac-studio.local 25.1.0 Darwin Kernel Version 25.1.0 arm64"
            })
        }
        "env" | "printenv" => return ok(&format!("HOME=/home/ci\nSHELL=/bin/bash\nMUXEN_WORKSPACE={}\nNO_COLOR=1\nPWD={here}", info.id)),
        _ => {}
    }
    if first == "ls" {
        let long = cmd.contains("-l");
        let prefix = if cwd.is_empty() { String::new() } else { format!("{cwd}/") };
        let mut names: BTreeSet<String> = BTreeSet::new();
        for path in tree.files.keys().filter(|p| p.starts_with(&prefix) && !p.starts_with(".muxen")) {
            if let Some(name) = path[prefix.len()..].split('/').next() {
                names.insert(name.to_string());
            }
        }
        let lines: Vec<String> = names
            .iter()
            .filter(|n| cmd.contains("-a") || !n.starts_with('.'))
            .map(|n| {
                if long {
                    let dir = tree.files.keys().any(|p| p.starts_with(&format!("{prefix}{n}/")));
                    let size = tree.files.get(&format!("{prefix}{n}")).map(|c| c.len()).unwrap_or(4096);
                    format!("{} 1 ci ci {size:>6} Oct  8 10:12 {n}", if dir { "drwxr-xr-x" } else { "-rw-r--r--" })
                } else {
                    n.clone()
                }
            })
            .collect();
        return ok(&lines.join("\n"));
    }
    if let Some(rest) = cmd.strip_prefix("cat ") {
        let mut out = String::new();
        for f in rest.split_whitespace() {
            let key = if cwd.is_empty() { f.to_string() } else { format!("{cwd}/{f}") };
            match tree.files.get(key.trim_start_matches("./")) {
                Some(c) => out.push_str(c),
                None => return (out, format!("cat: {f}: No such file or directory"), 1),
            }
        }
        return ok(out.trim_end());
    }
    if let Some(rest) = cmd.strip_prefix("echo ") {
        return ok(rest.trim().trim_matches(['"', '\'']));
    }
    if let Some(n) = cmd.strip_prefix("exit ") {
        return (String::new(), String::new(), n.trim().parse().unwrap_or(1));
    }
    if let Some(n) = cmd.strip_prefix("sleep ") {
        let secs: f64 = n.trim().parse().unwrap_or(1.0);
        pause((secs.min(2.0) * 1000.0) as u64).await;
        return ok("");
    }
    if cmd.starts_with("git ") {
        if !tree.git {
            return (String::new(), "fatal: not a git repository (or any of the parent directories): .git".into(), 128);
        }
        if cmd.starts_with("git status") {
            return ok(&status_text(&tree, false));
        }
        if cmd.starts_with("git log") {
            return ok(
                "3f9c2e1 Fix flaky checkout e2e wait\n9b81a04 Bump vitest to 3.2\n77d0c3e Add discount-before-tax rule\nc12e9fa Split checkout routes\n5aa0b17 CI: run e2e on self-hosted Linux",
            );
        }
        if cmd.starts_with("git diff") {
            return ok(&diff_text(&tree));
        }
        if cmd.starts_with("git branch") {
            return ok(&format!("* {}", tree.branch));
        }
        if cmd.starts_with("git rev-parse") {
            return ok("3f9c2e1d8a4b6c0e9f7a5b3c1d2e4f6a8b0c9d7e");
        }
        return ok("");
    }
    let test = ["npm test", "npm run test", "pnpm test", "yarn test", "bun test", "npx vitest", "npx playwright test"];
    if test.iter().any(|t| cmd.starts_with(t)) {
        return ok(" ✓ src/routes/checkout.test.ts (9 tests) 212ms\n ✓ e2e/checkout.spec.ts (12 tests) 9.42s\n\n Test Files  2 passed (2)\n      Tests  21 passed (21)\n   Start at  10:41:07\n   Duration  10.31s");
    }
    if cmd.starts_with("npm ci") || cmd.starts_with("npm install") || cmd.starts_with("pnpm install") {
        return ok("added 412 packages, and audited 413 packages in 9s\n\n68 packages are looking for funding\n  run `npm fund` for details\n\nfound 0 vulnerabilities");
    }
    if cmd.starts_with("cargo test") {
        return ok("running 42 tests\ntest result: ok. 42 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.84s");
    }
    if cmd.starts_with("cargo build") {
        return (String::new(), "   Compiling acme v0.1.0\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.40s".into(), 0);
    }
    if cmd.starts_with("swift test") {
        return ok("Test Suite 'All tests' passed.\n\t Executed 58 tests, with 0 failures (0 unexpected) in 4.211 seconds");
    }
    if cmd.starts_with("swift build") {
        return ok("Building for debugging...\nBuild complete! (41.07s)");
    }
    ok(&format!("(demo) ran on {}: {cmd}", info.machine_name))
}

fn status_text(tree: &DemoTree, short: bool) -> String {
    let mut lines = Vec::new();
    if short {
        lines.push(format!("## {}", tree.branch));
        for f in &tree.changed {
            lines.push(format!("{} {f}", if tree.original.contains(f) { " M" } else { "??" }));
        }
        return lines.join("\n");
    }
    lines.push(format!("On branch {}", tree.branch));
    if tree.changed.is_empty() {
        lines.push("nothing to commit, working tree clean".into());
        return lines.join("\n");
    }
    let modified: Vec<&String> = tree.changed.iter().filter(|f| tree.original.contains(*f)).collect();
    let untracked: Vec<&String> = tree.changed.iter().filter(|f| !tree.original.contains(*f)).collect();
    if !modified.is_empty() {
        lines.push("Changes not staged for commit:".into());
        lines.extend(modified.iter().map(|f| format!("\tmodified:   {f}")));
    }
    if !untracked.is_empty() {
        lines.push("Untracked files:".into());
        lines.extend(untracked.iter().map(|f| format!("\t{f}")));
    }
    lines.join("\n")
}

fn diff_text(tree: &DemoTree) -> String {
    tree.changed
        .iter()
        .filter(|f| tree.original.contains(*f))
        .map(|f| {
            let body: Vec<String> = tree.files.get(f).map(|c| c.lines().take(6).map(|l| format!("+{l}")).collect()).unwrap_or_default();
            format!("diff --git a/{f} b/{f}\n--- a/{f}\n+++ b/{f}\n@@ -1,5 +1,6 @@\n{}", body.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn read(w: &World, rec: &WorkspaceRecord, rel: &str, max: usize) -> Result<FileContent, String> {
    let files = w.workspace_files.lock().unwrap();
    let tree = files.get(&rec.info.id).cloned().unwrap_or_default();
    match tree.files.get(rel) {
        Some(c) => {
            let bytes = c.as_bytes();
            let cut = bytes[..bytes.len().min(max)].to_vec();
            Ok(file_content(rel.to_string(), cut, bytes.len() as u64, max))
        }
        None if tree.files.keys().any(|p| p.starts_with(&format!("{rel}/"))) => Err(format!("{rel} is a folder")),
        None => Err(format!("File {rel} not found in the workspace.")),
    }
}

pub(super) fn write(w: &World, rec: &WorkspaceRecord, rel: &str, content: &str, append: bool) -> u64 {
    let mut files = w.workspace_files.lock().unwrap();
    let tree = files.entry(rec.info.id.clone()).or_default();
    let entry = tree.files.entry(rel.to_string()).or_default();
    if append {
        entry.push_str(content);
    } else {
        *entry = content.to_string();
    }
    let size = entry.len() as u64;
    tree.changed.insert(rel.to_string());
    size
}

pub(super) fn list(w: &World, rec: &WorkspaceRecord, rel: &str, depth: u32) -> Result<Vec<FileEntry>, String> {
    let files = w.workspace_files.lock().unwrap();
    let tree = files.get(&rec.info.id).cloned().unwrap_or_default();
    let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
    if !rel.is_empty() && !tree.files.keys().any(|p| p.starts_with(&prefix)) {
        return Err(format!("Folder {rel} not found in the workspace."));
    }
    let mut entries: BTreeMap<String, FileEntry> = BTreeMap::new();
    for (path, content) in tree.files.iter().filter(|(p, _)| p.starts_with(&prefix) && !p.starts_with(".muxen")) {
        let parts: Vec<&str> = path[prefix.len()..].split('/').collect();
        for i in 0..parts.len().min(depth as usize) {
            let sub = format!("{prefix}{}", parts[..=i].join("/"));
            let is_file = i + 1 == parts.len();
            entries.entry(sub.clone()).or_insert(FileEntry {
                path: sub,
                kind: if is_file { "file" } else { "dir" }.into(),
                size: if is_file { content.len() as u64 } else { 0 },
            });
        }
    }
    Ok(entries.into_values().collect())
}

pub(super) fn changes(w: &World, rec: &WorkspaceRecord) -> String {
    let files = w.workspace_files.lock().unwrap();
    let tree = files.get(&rec.info.id).cloned().unwrap_or_default();
    if !tree.git {
        return "This workspace isn't a git checkout, so there's nothing to diff.".into();
    }
    format!("## git status\n{}\n\n## git diff\n{}", status_text(&tree, true), diff_text(&tree))
}

pub(super) async fn destroy(engine: &Arc<Engine>, op: u64, rec: &WorkspaceRecord, keep: bool) -> Result<(), String> {
    let w = engine.demo_world().cloned().ok_or("not in demo mode")?;
    engine.op_log(op, format!("▸ Cleaning up on {} ({})", rec.info.machine_name, rec.info.host));
    pause(300).await;
    w.reachable(&rec.info.machine_id, &rec.info.host).map_err(|e| format!("{} is unreachable: {e}", rec.info.machine_name))?;
    for pid in &rec.background_pids {
        engine.op_log(op, format!("Stopped background process {pid}"));
    }
    if keep {
        engine.op_log(op, format!("Kept the files in {}", rec.info.path));
    } else {
        w.workspace_files.lock().unwrap().remove(&rec.info.id);
        engine.op_log(op, format!("Deleted {}", rec.info.path));
    }
    Ok(())
}

pub(super) async fn sync(rec: &WorkspaceRecord, local: &Path, direction: SyncDirection, delete: bool) -> String {
    pause(600).await;
    let (from, to) = match direction {
        SyncDirection::Push => (local.display().to_string(), format!("{}:{}", rec.info.host, rec.info.path)),
        SyncDirection::Pull => (format!("{}:{}", rec.info.host, rec.info.path), local.display().to_string()),
    };
    format!(
        "(demo) {from}/ → {to}/{}\nNumber of files: 214 (reg: 187, dir: 27)\nNumber of regular files transferred: 12\nTotal file size: 2.31M bytes\nTotal transferred file size: 48.20K bytes\nsent 51.02K bytes  received 1.18K bytes  34.80K bytes/sec",
        if delete { " (with --delete)" } else { "" }
    )
}
