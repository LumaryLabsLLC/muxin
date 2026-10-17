use std::path::{Path, PathBuf};

use super::*;
use crate::config::Paths;
use crate::fleet::model::{Probe, RunnerSummary};

#[test]
fn keeps_paths_inside_the_workspace() {
    assert_eq!(rel_path("").unwrap(), "");
    assert_eq!(rel_path("./src//lib.rs").unwrap(), "src/lib.rs");
    assert_eq!(rel_path("a\\b").unwrap(), "a/b");
    for bad in ["/etc/passwd", "~/x", "../x", "a/../../b", "C:\\Windows", "\\\\server\\x"] {
        assert!(rel_path(bad).is_err(), "{bad} should be rejected");
    }
}

#[test]
fn validates_env_and_refs() {
    assert!(validate_env("NODE_ENV", "test").is_ok());
    assert!(validate_env("_X1", "").is_ok());
    assert!(validate_env("1X", "a").is_err());
    assert!(validate_env("A-B", "a").is_err());
    assert!(validate_git_ref("main").is_ok());
    assert!(validate_git_ref("fix/flaky-e2e").is_ok());
    assert!(validate_git_ref("v2.4.0").is_ok());
    for bad in ["-x", "a..b", "a b", "x~1", "x:y", "feat/"] {
        assert!(validate_git_ref(bad).is_err(), "{bad}");
    }
}

#[test]
fn normalizes_repos() {
    let r = normalize_repo("acme/api", "github.com").unwrap();
    assert_eq!(r.url, "https://github.com/acme/api.git");
    assert_eq!((r.shown.as_str(), r.short_name.as_str(), r.on_github), ("acme/api", "api", true));
    let r = normalize_repo("https://x:secret@github.com/acme/api.git", "github.com").unwrap();
    assert_eq!(r.shown, "https://github.com/acme/api.git");
    assert!(r.on_github && !r.shown.contains("secret"));
    let r = normalize_repo("git@github.com:acme/api.git", "github.com").unwrap();
    assert!(!r.on_github, "ssh remotes use the machine's own keys");
    assert_eq!(r.short_name, "api");
    assert_eq!(normalize_repo("/srv/git/tool.git", "github.com").unwrap().short_name, "tool");
    assert!(normalize_repo("--upload-pack=x", "github.com").is_err());
    assert!(normalize_repo("just-a-word", "github.com").is_err());
    assert_eq!(github_web_host("https://ghe.acme.internal/api/v3"), "ghe.acme.internal");
}

fn machine(id: &str, platform: &str, cpu: f64, working: usize, disk_free: u64, allowed: bool, status: &str) -> Machine {
    Machine {
        id: id.into(),
        name: id.into(),
        host: id.into(),
        local: false,
        platform: platform.into(),
        kind: "runner".into(),
        status: status.into(),
        checking: false,
        notes: None,
        runner_ids: Vec::new(),
        summary: RunnerSummary { total: 2, working, idle: 2 - working.min(2), offline: 0, unknown: 0 },
        probe: Some(Probe {
            checked_at: Utc::now(),
            duration_ms: 10,
            ok: true,
            error: None,
            hostname: None,
            os: None,
            arch: None,
            os_version: None,
            form_factor: None,
            desktop: None,
            cores: Some(8),
            cpu_pct: Some(cpu),
            mem_total: Some(100),
            mem_used: Some(40),
            disk_total: Some(1_000_000_000_000),
            disk_free: Some(disk_free),
            disk_path: None,
            uptime_sec: None,
            load: Vec::new(),
            docker: None,
            sudo: None,
            last_seen_at: None,
        }),
        diagnostics: Vec::new(),
        workspaces_allowed: allowed,
        agents: Vec::new(),
        agent_version: None,
        sessions_error: None,
        gh: None,
        unreachable: None,
        tailnet: None,
        cloud: None,
        displays: Vec::new(),
        parent: None,
        vm: None,
        direct_address: None,
    }
}

#[test]
fn picks_the_quietest_matching_machine() {
    let machines = vec![
        machine("busy-linux", "linux-x64", 70.0, 2, 500_000_000_000, true, "online"),
        machine("quiet-linux", "linux-x64", 5.0, 0, 500_000_000_000, true, "online"),
        machine("quiet-mac", "darwin-arm64", 1.0, 0, 500_000_000_000, true, "online"),
        machine("full-disk", "linux-x64", 0.0, 0, 1_000_000_000, true, "online"),
        machine("forbidden", "linux-x64", 0.0, 0, 500_000_000_000, false, "online"),
        machine("asleep", "linux-x64", 0.0, 0, 500_000_000_000, true, "offline"),
    ];
    assert_eq!(pick_machine(&machines, None).unwrap().id, "quiet-mac");
    assert_eq!(pick_machine(&machines, Some("linux")).unwrap().id, "quiet-linux");
    assert_eq!(pick_machine(&machines, Some("macos")).unwrap().id, "quiet-mac");
    assert_eq!(pick_machine(&machines, Some("Linux_x86_64")).unwrap().id, "quiet-linux");
    // Docker workspaces prefer machines whose check found docker.
    let mut with_docker = machines.clone();
    if let Some(p) = with_docker.iter_mut().find(|m| m.id == "quiet-linux").and_then(|m| m.probe.as_mut()) {
        p.docker = Some("29.8.1".into());
    }
    assert_eq!(pick_machine_for(&with_docker, None, true).unwrap().id, "quiet-linux");
    assert_eq!(pick_machine_for(&machines, None, true).unwrap().id, "quiet-mac", "no docker info anywhere: any machine");
    let err = pick_machine(&machines, Some("windows")).unwrap_err();
    assert!(err.contains("windows") && err.contains("forbidden (linux-x64, workspaces off)"), "{err}");
}

#[test]
fn quotes_env_and_builds_commands() {
    let env = vec![("MSG".to_string(), "it's \"fine\" $HOME".to_string())];
    assert_eq!(env_exports(Shell::Sh, &env), "export MSG='it'\\''s \"fine\" $HOME'\n");
    assert_eq!(env_exports(Shell::Powershell, &env), "$env:MSG = 'it''s \"fine\" $HOME'\n");

    let spec: MachineSpec = toml::from_str("name = \"Box\"\nhost = \"ci@box\"\nport = 2222\nssh_options = [\"-i\", \"~/.ssh/ci key\"]").unwrap();
    let shell = shell_command(&spec, "/home/ci/muxen/workspaces/ws-abc123", false);
    assert_eq!(shell, "ssh -t -p 2222 -i '~/.ssh/ci key' ci@box \"cd '/home/ci/muxen/workspaces/ws-abc123' && exec \\$SHELL -l\"");
    let local: MachineSpec = toml::from_str("name = \"Here\"\nhost = \"local\"").unwrap();
    assert_eq!(shell_command(&local, "/tmp/w/ws-1", false), "cd '/tmp/w/ws-1' && exec \"$SHELL\" -l");

    let args = rsync_args(&spec, "/w/ws-1", "/src/app/", SyncDirection::Push, true);
    assert!(args.contains(&"--delete".to_string()) && args.contains(&"--exclude=/.muxen/".to_string()));
    assert_eq!(&args[args.len() - 2..], &["/src/app/".to_string(), "ci@box:/w/ws-1/".to_string()]);
    let e = args.iter().position(|a| a == "-e").unwrap();
    assert!(args[e + 1].starts_with("ssh -T -o BatchMode=yes") && args[e + 1].ends_with("-i '~/.ssh/ci key'"), "{}", args[e + 1]);
    let pull = rsync_args(&local, "/w/ws-1", "/dst", SyncDirection::Pull, false);
    assert_eq!(&pull[pull.len() - 2..], &["/w/ws-1/".to_string(), "/dst/".to_string()]);
    assert!(!pull.contains(&"-e".to_string()));
    // A pull never brings a .git back (its config could run commands here); a push still sends yours.
    assert!(pull.contains(&"--exclude=.git".to_string()));
    assert!(!args.contains(&"--exclude=.git".to_string()));
}

#[test]
fn names_github_repos_for_the_privacy_check() {
    let name = |raw: &str| github_full_name(&normalize_repo(raw, "github.com").unwrap());
    assert_eq!(name("acme/api").as_deref(), Some("acme/api"));
    assert_eq!(name("https://github.com/acme/api.git").as_deref(), Some("acme/api"));
    assert_eq!(name("https://x:secret@github.com/acme/api").as_deref(), Some("acme/api"));
    assert_eq!(name("https://github.com/acme/api/tree/main"), None);
}

#[test]
fn parses_script_output() {
    assert_eq!(field("noise\nsize=12\ndata=abc\n", "size").as_deref(), Some("12"));
    let entries = parse_listing("d\t4096\tsrc\nf\t120\tsrc/main.rs\nl\t9\tlatest\n", "app");
    assert_eq!(entries.len(), 3);
    assert_eq!((entries[0].path.as_str(), entries[0].kind.as_str(), entries[0].size), ("app/src", "dir", 0));
    assert_eq!((entries[1].path.as_str(), entries[1].size), ("app/src/main.rs", 120));
    assert_eq!(entries[2].kind, "link");

    let tail = output_tail(&(0..40).map(|i| format!("out {i}")).collect::<Vec<_>>().join("\n"), "err!");
    assert_eq!(tail.len(), registry::TAIL_LINES);
    assert_eq!(tail.last().unwrap(), "err!");

    let text = file_content("a.txt".into(), "héllo".as_bytes()[..2].to_vec(), 6, 2);
    assert!(text.truncated && !text.binary && text.content == "h", "a cut inside a character keeps the valid prefix");
    let bin = file_content("a.bin".into(), vec![0x89, b'P', b'N', b'G', 0, 1], 6, 100);
    assert!(bin.binary && bin.content.is_empty());
    assert!(random_id(6).chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    assert_ne!(random_id(8), random_id(8));
}

#[tokio::test(flavor = "multi_thread")]
async fn demo_flow() {
    let dir = std::env::temp_dir().join(format!("muxen-ws-demo-{}", std::process::id()));
    let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), true);
    engine.run_check(None).await;
    assert_eq!(engine.workspaces().len(), 4, "demo seeds four workspaces");
    assert!(engine.workspaces().iter().any(|w| w.isolation == "docker" && w.image.as_deref() == Some("node:22")), "one is a container");
    assert!(engine.workspaces().iter().any(|w| w.worktree_of.as_deref() == Some("/Users/ci/code/ios-app")), "one is a worktree");
    assert!(!dir.join("workspaces.json").exists(), "demo never writes to disk");

    let info = engine
        .create_workspace(CreateWorkspace {
            platform: Some("linux".into()),
            repo: Some("acme/api".into()),
            branch: Some("agent/try-fix".into()),
            owner: Some("test-agent".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(info.status, "ready");
    assert!(info.platform.starts_with("linux"), "{info:?}");
    assert!(info.shell.starts_with("ssh -t"), "{}", info.shell);

    let r = engine.workspace_exec(&info.id, "pwd", ExecOptions::default()).await.unwrap();
    assert_eq!((r.exit_code, r.stdout.as_str()), (Some(0), info.path.as_str()));
    engine.workspace_write_file(&info.id, "notes/plan.md", "1. fix it\n", false).await.unwrap();
    let size = engine.workspace_write_file(&info.id, "notes/plan.md", "2. test it\n", true).await.unwrap();
    assert_eq!(size, 21);
    let f = engine.workspace_read_file(&info.id, "notes/plan.md", 0).await.unwrap();
    assert_eq!(f.content, "1. fix it\n2. test it\n");
    let files = engine.workspace_list_files(&info.id, "", 1).await.unwrap();
    assert!(files.iter().any(|e| e.path == "notes" && e.kind == "dir"), "{files:?}");
    assert!(files.iter().any(|e| e.path == "package.json"), "{files:?}");
    let changes = engine.workspace_changes(&info.id).await.unwrap();
    assert!(changes.contains("?? notes/plan.md"), "{changes}");
    let bg = engine.workspace_exec(&info.id, "npm run dev", ExecOptions { background: true, ..Default::default() }).await.unwrap();
    assert!(bg.background_pid.is_some() && bg.log_path.as_deref() == Some(".muxen/logs/1.log"));

    let ws = engine.workspaces().into_iter().find(|w| w.id == info.id).unwrap();
    assert_eq!(ws.commands, 2);
    assert_eq!(ws.history[0].command, "pwd");
    assert!(engine.workspace_exec(&info.id, "ls", ExecOptions { cwd: Some("../..".into()), ..Default::default() }).await.is_err());

    // The asleep laptop can't host one; workspaces can be turned off per machine.
    let err = engine.create_workspace(CreateWorkspace { machine: Some("harbor".into()), ..Default::default() }).await.unwrap_err();
    assert!(err.contains("unreachable"), "{err}");
    engine.set_workspaces_allowed("beacon", false).unwrap();
    let err = engine.create_workspace(CreateWorkspace { machine: Some("beacon".into()), ..Default::default() }).await.unwrap_err();
    assert!(err.contains("turned off"), "{err}");
    engine.set_workspaces_allowed("beacon", true).unwrap();

    engine.destroy_workspace(&info.id, false).await.unwrap();
    assert!(!engine.workspaces().iter().any(|w| w.id == info.id));
    let err = engine.workspace_exec(&info.id, "pwd", ExecOptions::default()).await.unwrap_err();
    assert!(err.contains("not found"), "{err}");
}

// ---------------------------------------------------------------------------
// A real workspace on this computer (host = "local"), in a temp dir.

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A bare repo with one commit on `main` and a `feature` branch.
fn bare_repo(tmp: &Path) -> PathBuf {
    let src = tmp.join("src-repo");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("README.md"), "# tool\n").unwrap();
    git(&src, &["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]);
    git(&src, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "first"]);
    git(&src, &["branch", "feature"]);
    let bare = tmp.join("tool.git");
    git(tmp, &["clone", "-q", "--bare", src.to_str().unwrap(), bare.to_str().unwrap()]);
    bare
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn local_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("muxen-ws-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp = tmp.canonicalize().unwrap();
    let root = tmp.join("workspaces");
    std::fs::write(
        tmp.join("config.toml"),
        format!("[[machine]]\nname = \"Here\"\nhost = \"local\"\nworkspace_root = \"{}\"\n", root.display()),
    )
    .unwrap();
    let repo = bare_repo(&tmp);
    let paths = Paths::resolve(Some(tmp.clone()), None);
    let engine = Engine::new(paths.clone(), false);
    engine.run_check(None).await;

    let info = engine
        .create_workspace(CreateWorkspace {
            machine: Some("here".into()),
            repo: Some(repo.display().to_string()),
            git_ref: Some("feature".into()),
            branch: Some("agent/work".into()),
            owner: Some("test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(info.status, "ready");
    assert_eq!(info.name, "tool");
    assert_eq!(Path::new(&info.path), root.join(&info.id));
    assert!(root.join(&info.id).join("README.md").exists());

    // Another process (an agent's MCP server) sees it right away.
    let other = Engine::new(paths.clone(), false);
    assert!(other.workspaces().iter().any(|w| w.id == info.id));

    // stdout, stderr and the exit code come back separately; quoting survives.
    let r = engine
        .workspace_exec(&info.id, "echo \"hi $GREETING\"; echo oops >&2; git branch --show-current; exit 3", ExecOptions {
            env: vec![("GREETING".into(), "it's me".into())],
            by: Some("test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(3), "{r:?}");
    assert_eq!(r.stdout, "hi it's me\nagent/work");
    assert_eq!(r.stderr, "oops");

    // cwd and MUXEN_WORKSPACE.
    std::fs::create_dir_all(root.join(&info.id).join("sub")).unwrap();
    let r = other.workspace_exec(&info.id, "basename \"$PWD\"; echo $MUXEN_WORKSPACE", ExecOptions { cwd: Some("sub".into()), ..Default::default() }).await.unwrap();
    assert_eq!(r.stdout, format!("sub\n{}", info.id));

    // A timeout kills the whole tree, not just the shell.
    let r = engine
        .workspace_exec(&info.id, "sleep 30 & echo $! > child.pid; wait", ExecOptions { timeout_secs: Some(1), ..Default::default() })
        .await
        .unwrap();
    assert!(r.timed_out && r.exit_code.is_none(), "{r:?}");
    let child: u32 = std::fs::read_to_string(root.join(&info.id).join("child.pid")).unwrap().trim().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!alive(child), "the timed-out command's child was killed");

    // Background commands keep running until the workspace is destroyed.
    let bg = engine.workspace_exec(&info.id, "echo started; sleep 300", ExecOptions { background: true, ..Default::default() }).await.unwrap();
    let pid = bg.background_pid.expect("pid");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive(pid));
    let log = engine.workspace_read_file(&info.id, bg.log_path.as_deref().unwrap(), 0).await.unwrap();
    assert_eq!(log.content.trim(), "started");

    // Files.
    engine.workspace_write_file(&info.id, "src/a.txt", "line 1\n", false).await.unwrap();
    assert_eq!(engine.workspace_write_file(&info.id, "src/a.txt", "line 2\n", true).await.unwrap(), 14);
    let f = engine.workspace_read_file(&info.id, "src/a.txt", 0).await.unwrap();
    assert_eq!((f.content.as_str(), f.size, f.truncated), ("line 1\nline 2\n", 14, false));
    let f = engine.workspace_read_file(&info.id, "src/a.txt", 3).await.unwrap();
    assert_eq!((f.content.as_str(), f.truncated), ("lin", true));
    assert!(engine.workspace_read_file(&info.id, "missing.txt", 0).await.unwrap_err().contains("no file"));
    assert!(engine.workspace_read_file(&info.id, "../../etc/passwd", 0).await.is_err());
    let list = engine.workspace_list_files(&info.id, "", 2).await.unwrap();
    assert!(list.iter().any(|e| e.path == "src/a.txt" && e.kind == "file" && e.size == 14), "{list:?}");
    assert!(list.iter().any(|e| e.path == "src" && e.kind == "dir"));
    assert!(!list.iter().any(|e| e.path.starts_with(".git") || e.path.starts_with(".muxen")), "{list:?}");

    // Changes.
    std::fs::write(root.join(&info.id).join("README.md"), "# tool\nedited\n").unwrap();
    let changes = engine.workspace_changes(&info.id).await.unwrap();
    assert!(changes.contains("## agent/work") && changes.contains(" M README.md") && changes.contains("+edited"), "{changes}");

    // Sync both ways (when rsync is around).
    if std::process::Command::new("rsync").arg("--version").output().is_ok_and(|o| o.status.success()) {
        let local = tmp.join("local-copy");
        let summary = engine.workspace_sync(&info.id, local.to_str().unwrap(), SyncDirection::Pull, false).await.unwrap();
        assert!(local.join("README.md").exists(), "{summary}");
        assert!(!local.join(".muxen").exists(), "Muxin's own folder isn't synced");
        std::fs::write(local.join("pushed.txt"), "from local\n").unwrap();
        engine.workspace_sync(&info.id, local.to_str().unwrap(), SyncDirection::Push, false).await.unwrap();
        assert!(root.join(&info.id).join("pushed.txt").exists());
    }

    let ws = engine.workspaces().into_iter().find(|w| w.id == info.id).unwrap();
    assert_eq!(ws.commands, 4);
    assert_eq!(ws.history[0].exit_code, Some(3));
    assert!(ws.history[0].output_tail.contains(&"oops".to_string()));
    assert!(ws.history[2].timed_out);
    assert!(ws.history[3].background);

    // Destroy stops background commands and deletes the folder.
    other.destroy_workspace(&info.id, false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!alive(pid), "background command stopped");
    assert!(!root.join(&info.id).exists());
    engine.refresh_from_disk();
    assert!(engine.workspaces().is_empty());

    // A bad repo fails cleanly and stays listed as failed until destroyed.
    let err = engine
        .create_workspace(CreateWorkspace { machine: Some("here".into()), repo: Some(tmp.join("nope.git").display().to_string()), ..Default::default() })
        .await
        .unwrap_err();
    assert!(err.contains("clone"), "{err}");
    let failed = engine.workspaces();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, "failed");
    assert!(root.join(&failed[0].id).exists(), "the partial folder is recorded");
    assert!(engine.workspace_exec(&failed[0].id, "pwd", ExecOptions::default()).await.unwrap_err().contains("failed to set up"));
    engine.destroy_workspace(&failed[0].id, false).await.unwrap();
    assert!(!root.join(&failed[0].id).exists(), "destroy cleans up the partial folder");

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Docker isolation

#[test]
fn docker_names_images_and_commands() {
    assert_eq!(container_name("ws-7f3a2c"), "muxen-ws-7f3a2c");
    for ok in ["node:22", "rust:1", "python:3.13-slim", "ghcr.io/acme/dev:latest", "busybox@sha256:abc123"] {
        assert_eq!(validate_image(ok).unwrap(), ok);
    }
    for bad in ["", "-it", "node 22", "node;rm -rf /", "$(id)", ":tag", "../x"] {
        assert!(validate_image(bad).is_err(), "{bad:?}");
    }
    let script = create_script(true);
    let check = script.find("or_docker_check || exit 6").unwrap();
    let clone = script.find("git clone").unwrap();
    let up = script.find("or_docker_up \"$(pwd -P)\"").unwrap();
    assert!(check < clone && clone < up, "docker is checked before cloning and started after");
    assert!(!create_script(false).contains("or_docker"));

    let spec: MachineSpec = toml::from_str("name = \"Box\"\nhost = \"beelink\"\nport = 2222").unwrap();
    let shell = docker_shell_command(&spec, "muxen-ws-abc");
    assert!(shell.starts_with("ssh -t -p 2222 beelink \"docker start muxen-ws-abc"), "{shell}");
    assert!(shell.contains("docker exec -it -w /workspace muxen-ws-abc"), "{shell}");
    let local: MachineSpec = toml::from_str("name = \"Here\"\nhost = \"local\"").unwrap();
    assert!(docker_shell_command(&local, "muxen-ws-abc").starts_with("docker start"));
}

#[tokio::test(flavor = "multi_thread")]
async fn demo_docker_flow() {
    let dir = std::env::temp_dir().join(format!("muxen-ws-demo-docker-{}", std::process::id()));
    let engine = Engine::new(Paths::resolve(Some(dir), None), true);
    engine.run_check(None).await;
    let docker = |image: Option<&str>| WorkspaceIsolation { isolation: Some("docker".into()), image: image.map(str::to_string), docker_socket: false };

    let info = engine
        .create_workspace_with(CreateWorkspace { machine: Some("beacon".into()), repo: Some("acme/web".into()), ..Default::default() }, docker(Some("node:22")))
        .await
        .unwrap();
    assert_eq!((info.isolation.as_str(), info.image.as_deref()), ("docker", Some("node:22")));
    assert_eq!(info.container.as_deref(), Some(container_name(&info.id).as_str()));
    assert!(info.shell.contains("docker exec -it -w /workspace"), "{}", info.shell);
    let r = engine.workspace_exec(&info.id, "pwd", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "/workspace", "commands run inside the container");

    // The default image, and the validation errors agents can hit.
    let plain = engine.create_workspace_with(CreateWorkspace { machine: Some("beacon".into()), ..Default::default() }, docker(None)).await.unwrap();
    assert_eq!(plain.image.as_deref(), Some(DEFAULT_IMAGE));
    let err = engine
        .create_workspace_with(CreateWorkspace::default(), WorkspaceIsolation { isolation: Some("vm".into()), ..Default::default() })
        .await
        .unwrap_err();
    assert!(err.contains("isn't an isolation"), "{err}");
    let err = engine
        .create_workspace_with(CreateWorkspace::default(), WorkspaceIsolation { image: Some("node:22".into()), ..Default::default() })
        .await
        .unwrap_err();
    assert!(err.contains("only applies to docker"), "{err}");
    let err = engine.create_workspace_with(CreateWorkspace::default(), docker(Some("node 22"))).await.unwrap_err();
    assert!(err.contains("isn't a docker image"), "{err}");
    // The demo has no Windows machine; this one only exists for tests.
    let id = engine.add_machine(crate::ops::NewMachine { name: "Test Windows".into(), host: "test-windows".into() }).unwrap();
    engine.run_check(Some(vec![id.clone()])).await;
    let err = engine.create_workspace_with(CreateWorkspace { machine: Some(id), ..Default::default() }, docker(None)).await.unwrap_err();
    assert!(err.contains("Windows"), "{err}");

    engine.destroy_workspace(&info.id, false).await.unwrap();
    engine.destroy_workspace(&plain.id, false).await.unwrap();
}

/// A real container workspace on this computer. Needs docker (and pulls
/// busybox); run with `cargo test -- --ignored local_docker`.
#[cfg(unix)]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn local_docker_end_to_end() {
    let docker_ok = std::process::Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().is_ok_and(|o| o.status.success());
    if !docker_ok {
        eprintln!("skipping: docker isn't available here");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("muxen-ws-docker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp = tmp.canonicalize().unwrap();
    let root = tmp.join("workspaces");
    std::fs::write(tmp.join("config.toml"), format!("[[machine]]\nname = \"Here\"\nhost = \"local\"\nworkspace_root = \"{}\"\n", root.display())).unwrap();
    let engine = Engine::new(Paths::resolve(Some(tmp.clone()), None), false);
    engine.run_check(None).await;
    let info = engine
        .create_workspace_with(
            CreateWorkspace { machine: Some("here".into()), ..Default::default() },
            WorkspaceIsolation { isolation: Some("docker".into()), image: Some("busybox:1.36".into()), docker_socket: false },
        )
        .await
        .unwrap();
    let container = info.container.clone().unwrap();

    let r = engine.workspace_exec(&info.id, "pwd; id -u; echo $GREETING > made.txt", ExecOptions { env: vec![("GREETING".into(), "hi".into())], ..Default::default() }).await.unwrap();
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert!(r.stdout.starts_with("/workspace"), "{r:?}");
    let me = String::from_utf8(std::process::Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    assert!(r.stdout.contains(me.trim()), "runs as the host user: {r:?}");
    assert_eq!(std::fs::read_to_string(root.join(&info.id).join("made.txt")).unwrap(), "hi\n", "the folder is mounted");

    let r = engine.workspace_exec(&info.id, "exit 7", ExecOptions::default()).await.unwrap();
    assert_eq!(r.exit_code, Some(7));
    let r = engine.workspace_exec(&info.id, "sleep 60", ExecOptions { timeout_secs: Some(2), ..Default::default() }).await.unwrap();
    assert!(r.timed_out, "{r:?}");
    let ps = std::process::Command::new("docker").args(["exec", &container, "ps"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&ps.stdout).contains("sleep 60"), "the timed-out command died inside the container");

    // A missing container is recreated on the next command.
    std::process::Command::new("docker").args(["rm", "-f", &container]).output().unwrap();
    let r = engine.workspace_exec(&info.id, "cat made.txt", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "hi");

    engine.destroy_workspace(&info.id, false).await.unwrap();
    let gone = std::process::Command::new("docker").args(["inspect", &container]).output().unwrap();
    assert!(!gone.status.success(), "the container was removed");
    assert!(!root.join(&info.id).exists());
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The full docker flow on a real remote machine over ssh. Set
/// MUXEN_TEST_DOCKER_HOST to an ssh destination with docker (it pulls
/// node:22) and run `cargo test -- --ignored remote_docker`.
#[cfg(unix)]
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn remote_docker_end_to_end() {
    let Ok(host) = std::env::var("MUXEN_TEST_DOCKER_HOST") else {
        eprintln!("skipping: set MUXEN_TEST_DOCKER_HOST");
        return;
    };
    let ssh = |cmd: &str| {
        let out = std::process::Command::new("ssh").args(["-o", "BatchMode=yes", &host, cmd]).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let tmp = std::env::temp_dir().join(format!("muxen-ws-remote-docker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("config.toml"),
        format!("[[machine]]\nname = \"Remote\"\nhost = \"{host}\"\nworkspace_root = \"~/muxen-test/workspaces\"\n"),
    )
    .unwrap();
    let engine = Engine::new(Paths::resolve(Some(tmp.clone()), None), false);
    engine.run_check(None).await;

    let info = engine
        .create_workspace_with(
            CreateWorkspace { machine: Some("remote".into()), repo: Some("octocat/Hello-World".into()), owner: Some("test".into()), ..Default::default() },
            WorkspaceIsolation { isolation: Some("docker".into()), image: Some("node:22".into()), docker_socket: false },
        )
        .await
        .unwrap();
    let container = info.container.clone().unwrap();
    assert_eq!(info.status, "ready");
    let (running, _) = ssh(&format!("docker inspect -f '{{{{.State.Running}}}}' {container}"));
    assert!(running, "the container is up");

    // Inside the container: node, git on the clone, the host user's uid, /workspace.
    let r = engine.workspace_exec(&info.id, "node -v; git status --short --branch; id -u; pwd", ExecOptions::default()).await.unwrap();
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    assert!(r.stdout.starts_with("v22."), "{r:?}");
    assert!(r.stdout.contains("## master") && r.stdout.ends_with("/workspace"), "{r:?}");
    let (_, uid) = ssh("id -u");
    assert!(r.stdout.contains(&format!("\n{uid}\n")), "runs as the ssh user: {r:?}");

    // A background server, reached from inside the container.
    let bg = engine
        .workspace_exec(&info.id, "node -e \"require('http').createServer((q,s)=>s.end('pong')).listen(8765)\"", ExecOptions { background: true, ..Default::default() })
        .await
        .unwrap();
    assert!(bg.background_pid.is_some() && bg.log_path.is_some(), "{bg:?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let r = engine.workspace_exec(&info.id, "curl -s http://127.0.0.1:8765/", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "pong", "{r:?}");

    // A timeout kills the command inside the container, children included.
    let r = engine.workspace_exec(&info.id, "sh -c 'sleep 61' & sleep 62; wait", ExecOptions { timeout_secs: Some(2), ..Default::default() }).await.unwrap();
    assert!(r.timed_out && r.exit_code.is_none(), "{r:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = engine
        .workspace_exec(&info.id, "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline 2>/dev/null; echo; done | grep -c '^sleep 6[12]' || true", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(r.stdout.trim(), "0", "nothing left running: {r:?}");

    // Files (through the host path) and git changes.
    engine.workspace_write_file(&info.id, "notes/plan.md", "1. fix it\n", false).await.unwrap();
    let r = engine.workspace_exec(&info.id, "cat notes/plan.md", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "1. fix it");
    let (_, owner) = ssh(&format!("stat -c %U {}/notes/plan.md", info.path));
    let (_, me) = ssh("id -un");
    assert_eq!(owner, me, "files aren't root-owned");
    engine.workspace_exec(&info.id, "echo 'from the container' >> README", ExecOptions::default()).await.unwrap();
    let changes = engine.workspace_changes(&info.id).await.unwrap();
    assert!(changes.contains("+from the container"), "{changes}");

    // Sync both ways.
    let local = tmp.join("local");
    engine.workspace_sync(&info.id, local.to_str().unwrap(), SyncDirection::Pull, false).await.unwrap();
    assert!(local.join("README").exists());
    std::fs::write(local.join("pushed.txt"), "from my laptop\n").unwrap();
    engine.workspace_sync(&info.id, local.to_str().unwrap(), SyncDirection::Push, false).await.unwrap();
    let r = engine.workspace_exec(&info.id, "cat pushed.txt", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "from my laptop");

    // The container disappears (reboot, prune): the next command brings it back.
    ssh(&format!("docker rm -f {container}"));
    let r = engine.workspace_exec(&info.id, "cat notes/plan.md", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "1. fix it", "{r:?}");

    // Destroy removes the container and the folder.
    engine.destroy_workspace(&info.id, false).await.unwrap();
    let (exists, _) = ssh(&format!("docker inspect {container}"));
    assert!(!exists, "container removed");
    let (folder, _) = ssh(&format!("test -e {}", info.path));
    assert!(!folder, "folder removed");

    // docker_socket shares the host's docker with the (non-root) container user.
    let dind = engine
        .create_workspace_with(
            CreateWorkspace { machine: Some("remote".into()), ..Default::default() },
            WorkspaceIsolation { isolation: Some("docker".into()), image: Some("node:22".into()), docker_socket: true },
        )
        .await
        .unwrap();
    let r = engine.workspace_exec(&dind.id, "test -S /var/run/docker.sock && test -w /var/run/docker.sock && echo writable", ExecOptions::default()).await.unwrap();
    assert_eq!(r.stdout, "writable", "{r:?}");
    engine.destroy_workspace(&dind.id, false).await.unwrap();

    ssh("rmdir ~/muxen-test/workspaces ~/muxen-test 2>/dev/null");
    let _ = std::fs::remove_dir_all(&tmp);
}
