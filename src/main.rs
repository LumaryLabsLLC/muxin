mod api;
mod cli;
mod config;
mod demo;
mod fleet;
mod github;
mod mcp;
mod ops;
mod probe;
#[cfg(feature = "gui")]
mod ui;
mod util;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::api::{ApiError, ApiResult};
use crate::config::{Paths, display_path};
use crate::fleet::Engine;

/// A fast native dashboard for your fleet of self-hosted GitHub Actions runners,
/// built to be driven by AI agents too (`openrunner mcp`, or any command with --json).
#[derive(Parser)]
#[command(name = "openrunner", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    open: OpenArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Open the dashboard window (the default)
    Open(OpenArgs),
    /// Fleet overview: counts, GitHub state, machines, and what needs attention
    Status(StatusArgs),
    /// List machines with resources and runner summaries
    Machines(AgentArgs),
    /// Everything about one machine: runners, diagnostics, resources
    Machine(MachineArgs),
    /// Add a machine to fleet.toml (and check it)
    AddMachine(AddMachineArgs),
    /// Remove a machine from fleet.toml (nothing on the machine changes)
    RemoveMachine(MachineArgs),
    /// List runners
    Runners(RunnersArgs),
    /// Queued and running self-hosted jobs
    Jobs(AgentArgs),
    /// Install runners on a machine and register them with GitHub
    Install(InstallArgs),
    /// Start, stop, restart, show logs for, or remove a runner
    Runner(RunnerArgs),
    /// Watch GitHub orgs or repos (owner/name) for live runner status and jobs
    Watch(ScopesArgs),
    /// Stop watching GitHub orgs or repos
    Unwatch(ScopesArgs),
    /// Show or change settings
    Settings(SettingsArgs),
    /// Host aliases from ~/.ssh/config, for add-machine
    SshHosts(AgentArgs),
    /// Agent workspaces: persistent work environments on your machines
    #[command(alias = "ws", subcommand)]
    Workspace(WorkspaceCommand),
    /// Run the MCP server on stdio for AI agents (Claude Code, Codex, ...)
    Mcp(McpArgs),
    /// Check every machine once and print a fleet report
    Check(CheckArgs),
    /// Write a starter fleet.toml
    Init(InitArgs),
    /// Print where OpenRunner keeps its files
    Paths(CommonArgs),
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Config directory [default: ~/.config/openrunner, or $OPENRUNNER_HOME]
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Inventory file [default: <config-dir>/fleet.toml]
    #[arg(long, short = 'f', value_name = "FILE")]
    inventory: Option<PathBuf>,
}

impl CommonArgs {
    fn paths(&self) -> Paths {
        Paths::resolve(self.config_dir.clone(), self.inventory.clone())
    }
}

#[derive(Args, Clone)]
struct OpenArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Run against a simulated fleet instead of your real machines
    #[arg(long)]
    demo: bool,

    /// Open a specific view at launch (for screenshots and debugging), e.g.
    /// "jobs", "settings", "machine:<id>", "runner:<id>", "install:<machine-id>", "add-machine", "logs:<runner-id>"
    #[arg(long, hide = true, value_name = "VIEW")]
    view: Option<String>,
}

/// Flags every agent-facing command takes.
#[derive(Args, Clone)]
struct AgentArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Use a simulated fleet (safe to experiment with; nothing touches real machines or GitHub)
    #[arg(long)]
    demo: bool,

    /// Print machine-readable JSON on stdout (errors as {"error": {...}})
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StatusArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Re-check every machine over ssh and sync GitHub first
    #[arg(long)]
    refresh: bool,
}

#[derive(Args)]
struct MachineArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Machine id (e.g. mac-studio) or name
    machine_id: String,
}

#[derive(Args)]
struct AddMachineArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Display name, e.g. "Mac Studio"
    #[arg(long)]
    name: String,

    /// ssh destination (user@host, an ~/.ssh/config alias) or "local"
    #[arg(long)]
    host: String,

    /// Don't check the machine after adding it
    #[arg(long)]
    no_check: bool,
}

#[derive(Args)]
struct RunnersArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Only runners on this machine
    #[arg(long, value_name = "ID")]
    machine: Option<String>,

    /// Only runners in this state
    #[arg(long, value_parser = ["working", "idle", "offline", "unknown"])]
    state: Option<String>,
}

#[derive(Args)]
struct InstallArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Machine to install on
    machine_id: String,

    /// GitHub org or owner/repo to register with (optional when exactly one is watched)
    #[arg(long)]
    scope: Option<String>,

    /// How many runners (named <machine-id>-01, -02, ...)
    #[arg(long, conflicts_with = "name")]
    count: Option<usize>,

    /// Runner name (repeat for several)
    #[arg(long = "name", value_name = "NAME")]
    name: Vec<String>,

    /// Extra label (repeat for several)
    #[arg(long = "label", value_name = "LABEL")]
    label: Vec<String>,

    /// Org runner group
    #[arg(long, value_name = "GROUP")]
    group: Option<String>,

    /// How the runners run: auto (service, else docker, else background), service, background, docker
    #[arg(long, value_name = "MODE", default_value = "auto")]
    mode: String,

    /// Docker image for --mode docker [default: ghcr.io/actions/actions-runner:<latest runner version>]
    #[arg(long, value_name = "IMAGE")]
    image: Option<String>,

    /// With --mode docker: give jobs the host's Docker socket (root-equivalent; trusted workflows only)
    #[arg(long)]
    docker_socket: bool,

    /// Same as --mode background (runs run.sh; won't survive a reboot)
    #[arg(long, conflicts_with = "mode")]
    no_service: bool,

    /// Return right away with the operation id
    #[arg(long)]
    no_wait: bool,

    /// Seconds to wait
    #[arg(long, default_value_t = 900)]
    timeout: u64,
}

#[derive(Args)]
struct RunnerArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// start, stop, restart, logs or remove
    #[arg(value_parser = ["start", "stop", "restart", "logs", "remove"])]
    action: String,

    /// Runner id (<machine-id>/<name>, github/<name>) or a unique runner name
    runner_id: String,

    /// With remove: also delete the runner's folder
    #[arg(long)]
    delete_files: bool,

    /// Return right away with the operation id
    #[arg(long)]
    no_wait: bool,

    /// Seconds to wait
    #[arg(long, default_value_t = 180)]
    timeout: u64,
}

#[derive(Args)]
struct ScopesArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Orgs (acme) or repos (acme/api)
    #[arg(required = true)]
    scopes: Vec<String>,
}

#[derive(Args)]
struct SettingsArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// GitHub poll interval in seconds
    #[arg(long, value_name = "SECS")]
    poll: Option<u32>,

    /// Recently pushed repos per org to scan for jobs
    #[arg(long, value_name = "N")]
    scan_repos: Option<u32>,

    /// Automatic machine check interval in minutes (0 = off)
    #[arg(long, value_name = "MINS")]
    check_interval: Option<u32>,

    /// GitHub REST API base (GitHub Enterprise Server)
    #[arg(long, value_name = "URL")]
    api_url: Option<String>,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace on a machine (optionally cloning a repo)
    Create(WsCreateArgs),
    /// List workspaces
    #[command(alias = "list")]
    Ls(AgentArgs),
    /// One workspace with its recent commands
    Show(WsIdArgs),
    /// Run a command in a workspace; exits with the command's exit code
    Exec(WsExecArgs),
    /// Print a file from a workspace
    Cat(WsPathArgs),
    /// Write stdin to a file in a workspace
    Write(WsWriteArgs),
    /// List files in a workspace
    Files(WsFilesArgs),
    /// git status and diff of a workspace
    Changes(WsIdArgs),
    /// Copy a local folder to a workspace (--push) or back (--pull) with rsync
    Sync(WsSyncArgs),
    /// Open an interactive shell in a workspace
    Shell(WsShellArgs),
    /// Destroy a workspace (stops its background commands, deletes its folder)
    #[command(alias = "destroy")]
    Rm(WsRmArgs),
    /// Allow (or with --off, forbid) workspaces on a machine
    Allow(WsAllowArgs),
}

#[derive(Args)]
struct WsCreateArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Machine id; omit to pick the least-loaded machine that allows workspaces
    #[arg(long, value_name = "ID")]
    machine: Option<String>,

    /// When picking: platform prefix (linux, linux-x64, darwin-arm64, windows)
    #[arg(long)]
    platform: Option<String>,

    /// Short label (defaults to the repo name)
    #[arg(long)]
    name: Option<String>,

    /// Git URL or owner/repo to clone
    #[arg(long)]
    repo: Option<String>,

    /// Branch, tag or commit to check out
    #[arg(long = "ref", value_name = "REF")]
    git_ref: Option<String>,

    /// New branch to create after checkout
    #[arg(long)]
    branch: Option<String>,

    /// Run in a fresh Docker container (shorthand for --isolation docker)
    #[arg(long, conflicts_with = "isolation")]
    docker: bool,

    /// "host" (default) or "docker"
    #[arg(long, value_name = "ISOLATION")]
    isolation: Option<String>,

    /// Docker image for docker workspaces, e.g. node:22
    #[arg(long, value_name = "IMAGE")]
    image: Option<String>,

    /// Share the machine's Docker socket with the container (docker-in-docker)
    #[arg(long)]
    docker_socket: bool,
}

#[derive(Args)]
struct WsIdArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Workspace id (ws-…) or unique name
    workspace_id: String,
}

#[derive(Args)]
struct WsExecArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Workspace id (ws-…) or unique name
    workspace_id: String,

    /// Folder relative to the workspace to run in
    #[arg(long)]
    cwd: Option<String>,

    /// Environment variable (repeat for several)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// Kill the command after this many seconds (max 3600)
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// Start it detached and print its pid and log file
    #[arg(long)]
    background: bool,

    /// The command, after `--`. One argument is used as a shell string; several are quoted and joined.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Args)]
struct WsPathArgs {
    #[command(flatten)]
    agent: AgentArgs,

    workspace_id: String,

    /// Path relative to the workspace
    path: String,

    /// Read at most this many bytes
    #[arg(long, value_name = "BYTES")]
    max_bytes: Option<usize>,
}

#[derive(Args)]
struct WsWriteArgs {
    #[command(flatten)]
    agent: AgentArgs,

    workspace_id: String,

    /// Path relative to the workspace
    path: String,

    /// Append instead of replacing
    #[arg(long)]
    append: bool,
}

#[derive(Args)]
struct WsFilesArgs {
    #[command(flatten)]
    agent: AgentArgs,

    workspace_id: String,

    /// Folder relative to the workspace
    path: Option<String>,

    /// How deep to list
    #[arg(long, default_value_t = 2)]
    depth: u32,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("direction").required(true).args(["push", "pull"])))]
struct WsSyncArgs {
    #[command(flatten)]
    agent: AgentArgs,

    workspace_id: String,

    /// Folder on this computer
    local_path: String,

    /// Copy the local folder into the workspace
    #[arg(long)]
    push: bool,

    /// Copy the workspace into the local folder
    #[arg(long)]
    pull: bool,

    /// Also delete files on the receiving side that the sender doesn't have
    #[arg(long)]
    delete: bool,
}

#[derive(Args)]
struct WsShellArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Use a simulated fleet (prints the command instead of running it)
    #[arg(long)]
    demo: bool,

    workspace_id: String,
}

#[derive(Args)]
struct WsRmArgs {
    #[command(flatten)]
    agent: AgentArgs,

    workspace_id: String,

    /// Keep the folder on the machine; only forget the workspace
    #[arg(long)]
    keep_files: bool,
}

#[derive(Args)]
struct WsAllowArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Machine id
    machine_id: String,

    /// Forbid workspaces instead
    #[arg(long)]
    off: bool,
}

#[derive(Args)]
struct McpArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Serve a simulated fleet
    #[arg(long)]
    demo: bool,

    /// Print setup snippets for Claude Code, Codex and other MCP clients, then exit
    #[arg(long)]
    print_config: bool,
}

#[derive(Args)]
struct CheckArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Use a simulated fleet
    #[arg(long)]
    demo: bool,

    /// Print the full snapshot as JSON
    #[arg(long)]
    json: bool,

    /// Skip GitHub; use only what the machines report
    #[arg(long)]
    no_github: bool,
}

#[derive(Args)]
struct InitArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Overwrite an existing fleet.toml
    #[arg(long)]
    force: bool,
}

fn main() -> ExitCode {
    // Logs always go to stderr: stdout is reserved for --json output and MCP messages.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPENRUNNER_LOG").unwrap_or_else(|_| "openrunner=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new().context("starting the async runtime") {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let code = match cli.command {
        None => open(cli.open, &runtime),
        Some(Command::Open(args)) => open(args, &runtime),
        Some(Command::Status(a)) => {
            let req = api::StatusRequest { refresh: a.refresh };
            run(&runtime, &a.agent, async move |e| api::status(&e, req, api::Surface::Cli).await, cli::status)
        }
        Some(Command::Machines(a)) => run(&runtime, &a, async move |e| api::machines(&e).await, |m| cli::machines(m)),
        Some(Command::Machine(a)) => {
            let req = api::MachineRequest { machine_id: a.machine_id };
            run(&runtime, &a.agent, async move |e| api::machine(&e, req).await, cli::machine)
        }
        Some(Command::AddMachine(a)) => {
            let req = api::AddMachineRequest { name: a.name, host: a.host, check: !a.no_check };
            run(&runtime, &a.agent, async move |e| api::add_machine(&e, req, api::Surface::Cli).await, cli::added_machine)
        }
        Some(Command::RemoveMachine(a)) => {
            let req = api::MachineRequest { machine_id: a.machine_id };
            run(&runtime, &a.agent, async move |e| api::remove_machine(&e, req).await, |m| {
                format!("Removed {} ({}) from fleet.toml. Nothing on the machine changed.\n", m.name, m.id)
            })
        }
        Some(Command::Runners(a)) => {
            let req = api::RunnersRequest { machine_id: a.machine, state: a.state };
            run(&runtime, &a.agent, async move |e| api::runners(&e, req).await, |r| cli::runners(r))
        }
        Some(Command::Jobs(a)) => run(&runtime, &a, async move |e| api::jobs(&e, api::Surface::Cli).await, cli::jobs),
        Some(Command::Install(a)) => {
            let req = api::InstallRunnersRequest {
                machine_id: a.machine_id,
                scope: a.scope,
                names: a.name,
                count: a.count,
                labels: a.label,
                runner_group: a.group,
                as_service: !a.no_service,
                mode: Some(if a.no_service { "background".into() } else { a.mode }),
                image: a.image,
                docker_socket: a.docker_socket,
                wait: !a.no_wait,
                timeout_secs: Some(a.timeout),
            };
            run(&runtime, &a.agent, async move |e| api::install_runners(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        Some(Command::Runner(a)) => {
            let action = match a.action.parse::<api::ActionName>() {
                Ok(action) => action,
                Err(e) => return ExitCode::from(report_error(&a.agent, &e)),
            };
            let req = api::RunnerActionRequest {
                runner_id: a.runner_id,
                action,
                delete_files: a.delete_files,
                wait: !a.no_wait,
                timeout_secs: Some(a.timeout),
            };
            run(&runtime, &a.agent, async move |e| api::runner_action(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        Some(Command::Watch(a)) => {
            let req = api::GitHubScopesRequest { add: a.scopes, remove: vec![] };
            run(&runtime, &a.agent, async move |e| api::github_scopes(&e, req).await, cli::scopes)
        }
        Some(Command::Unwatch(a)) => {
            let req = api::GitHubScopesRequest { add: vec![], remove: a.scopes };
            run(&runtime, &a.agent, async move |e| api::github_scopes(&e, req).await, cli::scopes)
        }
        Some(Command::Settings(a)) => {
            let req = api::UpdateSettingsRequest {
                github_poll_seconds: a.poll,
                scan_repos: a.scan_repos,
                check_interval_minutes: a.check_interval,
                api_url: a.api_url,
            };
            let changing = req.github_poll_seconds.is_some()
                || req.scan_repos.is_some()
                || req.check_interval_minutes.is_some()
                || req.api_url.is_some();
            run(
                &runtime,
                &a.agent,
                async move |e| if changing { api::update_settings(&e, req) } else { Ok(api::settings(&e)) },
                cli::settings,
            )
        }
        Some(Command::SshHosts(a)) => run(&runtime, &a, async move |_| Ok(api::ssh_hosts()), |h| {
            if h.hosts.is_empty() { "No hosts in ~/.ssh/config.\n".into() } else { format!("{}\n", h.hosts.join("\n")) }
        }),
        Some(Command::Workspace(cmd)) => workspace(cmd, &runtime),
        Some(Command::Mcp(a)) => mcp(a, &runtime),
        Some(Command::Check(args)) => {
            let _guard = runtime.enter();
            let engine = Engine::new(args.common.paths(), args.demo);
            let snapshot = runtime.block_on(engine.check_once(!args.no_github));
            if args.json {
                println!("{}", serde_json::to_string_pretty(&snapshot).unwrap_or_default());
            } else {
                let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                print!("{}", fleet::report::render(&snapshot, color));
            }
            0
        }
        Some(Command::Init(args)) => init(args),
        Some(Command::Paths(args)) => {
            let p = args.paths();
            println!("config dir  {}", display_path(&p.dir));
            println!("inventory   {}", display_path(&p.inventory));
            println!("settings    {}", display_path(&p.settings));
            println!("state       {}", display_path(&p.state));
            0
        }
    };
    ExitCode::from(code)
}

fn open(args: OpenArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    // The engine runs on tokio; the window runs on GPUI's main-thread executor.
    let _guard = runtime.enter();
    let engine = Engine::new(args.common.paths(), args.demo);
    engine.start();
    if engine.is_demo() {
        tracing::info!("demo mode: simulated fleet, nothing is read from or written to disk");
    } else {
        tracing::info!("inventory {}", display_path(&engine.paths().inventory));
    }
    #[cfg(feature = "gui")]
    {
        ui::run(engine, args.view);
        0
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (engine, args.view);
        eprintln!("This build of OpenRunner has no window (built without the `gui` feature).");
        eprintln!("Use the commands (`openrunner --help`) or the MCP server (`openrunner mcp`).");
        2
    }
}

/// Runs one agent-facing command: JSON or human output on stdout, errors with a
/// code (0 ok, 1 failed, 2 bad input / not found).
fn run<T: Serialize>(
    runtime: &tokio::runtime::Runtime,
    args: &AgentArgs,
    f: impl AsyncFnOnce(Arc<Engine>) -> ApiResult<T>,
    human: impl FnOnce(&T) -> String,
) -> u8 {
    let _guard = runtime.enter();
    let engine = Engine::new(args.common.paths(), args.demo);
    match runtime.block_on(f(engine)) {
        Ok(value) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
            } else {
                print!("{}", human(&value));
            }
            0
        }
        Err(e) => report_error(args, &e),
    }
}

fn report_error(args: &AgentArgs, e: &ApiError) -> u8 {
    if args.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "error": e })).unwrap_or_default());
    } else {
        eprint!("{}", cli::error(e));
    }
    e.exit_code()
}

/// Operation output streams to stderr while a command waits.
fn stream_line(line: &str) {
    eprintln!("  {line}");
}

/// Who the CLI records as a workspace's owner and a command's author.
const CLI_OWNER: &str = "cli";
const CLI: api::Surface = api::Surface::Cli;

fn workspace(cmd: WorkspaceCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        WorkspaceCommand::Create(a) => {
            let req = api::WorkspaceCreateRequest {
                machine_id: a.machine,
                platform: a.platform,
                name: a.name,
                repo: a.repo,
                git_ref: a.git_ref,
                branch: a.branch,
                isolation: if a.docker { Some("docker".into()) } else { a.isolation },
                image: a.image,
                docker_socket: a.docker_socket,
            };
            run(runtime, &a.agent, async move |e| api::workspace_create(&e, req, CLI_OWNER, CLI).await, cli::workspace_created)
        }
        WorkspaceCommand::Ls(a) => run(runtime, &a, async move |e| Ok(api::workspaces_list(&e)), cli::workspaces),
        WorkspaceCommand::Show(a) => {
            let req = api::WorkspaceRequest { workspace_id: a.workspace_id };
            run(runtime, &a.agent, async move |e| api::workspace_get(&e, req, CLI), cli::workspace)
        }
        WorkspaceCommand::Exec(a) => workspace_exec(a, runtime),
        WorkspaceCommand::Cat(a) => {
            let req = api::WorkspaceReadRequest { workspace_id: a.workspace_id, path: a.path, max_bytes: a.max_bytes };
            run(runtime, &a.agent, async move |e| api::workspace_read_file(&e, req, CLI).await, cli::file)
        }
        WorkspaceCommand::Write(a) => {
            let mut content = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut content) {
                return report_error(&a.agent, &ApiError::invalid(format!("Couldn't read stdin as UTF-8 text: {e}")));
            }
            let req = api::WorkspaceWriteRequest { workspace_id: a.workspace_id, path: a.path, content, append: a.append };
            run(runtime, &a.agent, async move |e| api::workspace_write_file(&e, req, CLI).await, |w| {
                format!("Wrote {} ({} bytes) in {}.\n", w.path, w.size, w.workspace_id)
            })
        }
        WorkspaceCommand::Files(a) => {
            let req = api::WorkspaceFilesRequest { workspace_id: a.workspace_id, path: a.path, depth: Some(a.depth) };
            run(runtime, &a.agent, async move |e| api::workspace_list_files(&e, req, CLI).await, cli::files)
        }
        WorkspaceCommand::Changes(a) => {
            let req = api::WorkspaceRequest { workspace_id: a.workspace_id };
            run(runtime, &a.agent, async move |e| api::workspace_changes(&e, req, CLI).await, |c| {
                let mut out = c.changes.clone();
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out
            })
        }
        WorkspaceCommand::Sync(a) => {
            let direction = if a.push { fleet::workspaces::SyncDirection::Push } else { fleet::workspaces::SyncDirection::Pull };
            let req = api::WorkspaceSyncRequest { workspace_id: a.workspace_id, local_path: a.local_path, direction, delete: a.delete };
            run(runtime, &a.agent, async move |e| api::workspace_sync(&e, req, CLI).await, |r| {
                let mut out = r.summary.clone();
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out
            })
        }
        WorkspaceCommand::Shell(a) => workspace_shell(a, runtime),
        WorkspaceCommand::Rm(a) => {
            let req = api::WorkspaceDestroyRequest { workspace_id: a.workspace_id, keep_files: a.keep_files };
            run(runtime, &a.agent, async move |e| api::workspace_destroy(&e, req, CLI).await, |d| {
                if d.kept_files {
                    format!("Forgot {}; its folder is still on the machine.\n", d.workspace_id)
                } else {
                    format!("Destroyed {}.\n", d.workspace_id)
                }
            })
        }
        WorkspaceCommand::Allow(a) => {
            let req = api::WorkspaceAllowRequest { machine_id: a.machine_id, allowed: !a.off };
            run(runtime, &a.agent, async move |e| api::workspace_allow(&e, req, CLI), |r| {
                format!("Workspaces are now {} on {}.\n", if r.workspaces_allowed { "allowed" } else { "off" }, r.machine_id)
            })
        }
    }
}

/// `workspace exec`: passes the command's output through and exits with its exit code
/// (124 when it timed out, like `timeout(1)`), so it composes in scripts.
fn workspace_exec(a: WsExecArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let mut env = std::collections::BTreeMap::new();
    for pair in &a.env {
        match pair.split_once('=') {
            Some((k, v)) => {
                env.insert(k.to_string(), v.to_string());
            }
            None => return report_error(&a.agent, &ApiError::invalid(format!("--env needs KEY=VALUE, got \"{pair}\"."))),
        }
    }
    let req = api::WorkspaceExecRequest {
        workspace_id: a.workspace_id,
        command: shell_join(&a.command),
        cwd: a.cwd,
        env,
        timeout_secs: Some(a.timeout),
        background: a.background,
    };
    let _guard = runtime.enter();
    let engine = Engine::new(a.agent.common.paths(), a.agent.demo);
    match runtime.block_on(api::workspace_exec(&engine, req, CLI_OWNER, CLI)) {
        Ok(out) => {
            if a.agent.json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            } else {
                use std::io::Write;
                // Captured output loses its final newline; put it back so the shell prompt
                // (or the next line) doesn't run into the last line of output.
                let newline = |s: &str| if s.is_empty() || s.ends_with('\n') { "" } else { "\n" };
                let _ = write!(std::io::stdout(), "{}{}", out.stdout, newline(&out.stdout));
                let _ = write!(std::io::stderr(), "{}{}", out.stderr, newline(&out.stderr));
                if let Some(log) = &out.log_path {
                    eprintln!("started in the background (pid {}), log: {log}", out.background_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
                } else if out.timed_out {
                    eprintln!("openrunner: timed out after {}s", out.duration_ms / 1000);
                } else if out.truncated {
                    eprintln!("openrunner: output was truncated (the middle was dropped)");
                }
            }
            match (out.exit_code, out.timed_out) {
                (_, true) => 124,
                (Some(code), _) => u8::try_from(code).unwrap_or(1),
                (None, _) => 0,
            }
        }
        Err(e) => report_error(&a.agent, &e),
    }
}

/// Joins `exec -- cmd args...`: a single argument is taken as a shell string
/// ("make test && make lint"); several are quoted as needed and joined.
fn shell_join(args: &[String]) -> String {
    if let [one] = args {
        return one.clone();
    }
    args.iter()
        .map(|a| {
            let safe = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
            if safe { a.clone() } else { format!("'{}'", a.replace('\'', r"'\''")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `workspace shell`: replaces this process with an interactive shell in the workspace.
fn workspace_shell(a: WsShellArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let agent = AgentArgs { common: a.common.clone(), demo: a.demo, json: false };
    let _guard = runtime.enter();
    let engine = Engine::new(a.common.paths(), a.demo);
    let w = match api::workspace_get(&engine, api::WorkspaceRequest { workspace_id: a.workspace_id }, CLI) {
        Ok(w) => w,
        Err(e) => return report_error(&agent, &e),
    };
    if a.demo {
        eprintln!("demo: would run: {}", w.shell);
        return 0;
    }
    eprintln!("{} on {} · {}", w.id, w.machine_name, w.path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("/bin/sh").arg("-c").arg(&w.shell).exec();
        eprintln!("error: couldn't start the shell: {err}");
        1
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new("cmd").args(["/C", &w.shell]).status() {
            Ok(status) => status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1),
            Err(err) => {
                eprintln!("error: couldn't start the shell: {err}");
                1
            }
        }
    }
}

fn mcp(args: McpArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    if args.print_config {
        print!("{}", mcp::print_config());
        return 0;
    }
    let _guard = runtime.enter();
    // No background loops: tools refresh what they need on demand.
    let engine = Engine::new(args.common.paths(), args.demo);
    match runtime.block_on(mcp::serve_stdio(engine)) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("mcp: {e:#}");
            1
        }
    }
}

fn init(args: InitArgs) -> u8 {
    let paths = args.common.paths();
    if paths.inventory.exists() && !args.force {
        eprintln!("error: {} already exists (use --force to overwrite)", display_path(&paths.inventory));
        return 2;
    }
    if let Err(e) = config::write_atomic(&paths.inventory, config::default_inventory().as_bytes()) {
        eprintln!("error: {e:#}");
        return 1;
    }
    println!("Wrote {}", display_path(&paths.inventory));
    println!("Add your machines, then run `openrunner` to open the dashboard.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn joins_exec_commands() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(shell_join(&v(&["make test && make lint"])), "make test && make lint");
        assert_eq!(shell_join(&v(&["echo", "hello world", "it's"])), "echo 'hello world' 'it'\\''s'");
        assert_eq!(shell_join(&v(&["cargo", "test", "--release"])), "cargo test --release");
    }

    #[test]
    fn parses_workspace_commands() {
        let cli = Cli::try_parse_from(["openrunner", "ws", "exec", "ws-abc123", "--env", "A=1", "--", "npm", "test"]).unwrap();
        match cli.command {
            Some(Command::Workspace(WorkspaceCommand::Exec(a))) => {
                assert_eq!(a.workspace_id, "ws-abc123");
                assert_eq!(a.command, vec!["npm", "test"]);
                assert_eq!(a.env, vec!["A=1"]);
            }
            _ => panic!("expected workspace exec"),
        }
        assert!(Cli::try_parse_from(["openrunner", "workspace", "sync", "ws-1", "."]).is_err(), "needs --push or --pull");
        assert!(Cli::try_parse_from(["openrunner", "workspace", "sync", "ws-1", ".", "--push", "--pull"]).is_err());
        assert!(Cli::try_parse_from(["openrunner", "workspace", "create", "--platform", "linux", "--repo", "acme/api", "--ref", "main", "--json"]).is_ok());
    }
}
