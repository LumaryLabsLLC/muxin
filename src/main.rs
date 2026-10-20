mod api;
mod cli;
mod cloud;
mod config;
#[path = "../agent/src/crash.rs"]
mod crash;
mod demo;
mod fleet;
mod github;
mod mcp;
mod ops;
mod probe;
#[cfg(feature = "gui")]
mod renamed;
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
mod share;
mod shell_command;
mod update;
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

/// One app for all your machines: agent sessions, desktops, files, GitHub,
/// workspaces and self-hosted runners. Agents can drive it too (`muxin mcp`,
/// or any command with --json).
#[derive(Parser)]
#[command(name = "muxin", version, about, args_conflicts_with_subcommands = true)]
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
    /// Overview: your machines, GitHub state, and what needs attention
    Status(StatusArgs),
    /// List machines with resources and runner summaries
    Machines(AgentArgs),
    /// Everything about one machine: runners, diagnostics, resources
    Machine(MachineArgs),
    /// Add a machine to config.toml (and check it)
    AddMachine(AddMachineArgs),
    /// Remove a machine from config.toml (on the machine, only a direct connection from this app is turned off)
    RemoveMachine(MachineArgs),
    /// Give a machine a new name (its id stays the same)
    RenameMachine(RenameMachineArgs),
    /// Stop connecting to a machine (its sessions go on there) until `muxin connect`
    Disconnect(MachineArgs),
    /// Connect to a machine again (and check it)
    Connect(MachineArgs),
    /// List runners
    Runners(RunnersArgs),
    /// Queued and running self-hosted jobs
    Jobs(AgentArgs),
    /// Cancel the workflow run a queued or running job belongs to (all its jobs)
    CancelRun(CancelRunArgs),
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
    /// What's in a folder on a machine: `muxin files beelink:~/build`
    #[command(alias = "ls")]
    Files(FilesArgs),
    /// Copy files and folders between machines: `muxin cp beelink:~/build/app.tar.gz mac-studio:~/Downloads`
    Cp(CpArgs),
    /// Agent workspaces: persistent work environments on your machines
    #[command(alias = "ws", subcommand)]
    Workspace(WorkspaceCommand),
    /// Your GitHub: what needs you, repositories, issues and pull requests
    #[command(alias = "gh", subcommand)]
    Github(GithubCommand),
    /// Git checkouts on your machines, and fetch/pull/push in them
    Checkouts(CheckoutsArgs),
    /// Whether git on a machine can commit and push, and setting that up
    #[command(subcommand)]
    Git(GitCommand),
    /// How Muxin reaches a machine (direct, ssh link); --direct on|off to change it
    Connection(ConnectionArgs),
    /// A machine's desktop: a screenshot, or mouse and keyboard input
    #[command(subcommand)]
    Desktop(DesktopCommand),
    /// A machine's logs: the system log, a service's, a file's or a container's (--list to see them)
    Logs(LogsArgs),
    /// A machine's services and containers; or start, stop, restart, enable or disable one
    Services(ServicesArgs),
    /// What's listening on a machine (its servers and ports)
    Ports(MachineArgs),
    /// What the agents (Claude Code, Codex) used, and what it'd cost at API prices
    Usage(UsageArgs),
    /// Open a machine's folder in your editor (Zed, Cursor or VS Code) over ssh
    Edit(OpenFolderArgs),
    /// A machine's busiest processes and its GPUs
    Ps(PsArgs),
    /// Stop a process on a machine (--force kills it)
    Kill(KillArgs),
    /// Run a command on several machines at once (--on a,b or --all)
    Run(FleetRunArgs),
    /// Wake a machine that's asleep or off (Wake on LAN)
    Wake(MachineArgs),
    /// Power a machine on or off: a cloud machine, or a VM in Docker on another machine
    Power(CloudPowerArgs),
    /// VMs on your machines: Windows (Docker with KVM) and Linux containers
    #[command(subcommand)]
    Vm(VmCommand),
    /// Schedules: a command or an agent session that runs on a machine at set times
    #[command(alias = "schedules", subcommand)]
    Schedule(ScheduleCommand),
    /// Open a machine's port on this computer (localhost) until Ctrl-C
    Forward(ForwardArgs),
    /// Cloud machines: create DigitalOcean droplets to work in, power them, destroy them
    #[command(subcommand)]
    Cloud(CloudCommand),
    /// Agent sessions: Claude Code, Codex, OpenCode, ... kept running on your machines
    #[command(alias = "sessions", subcommand)]
    Session(SessionCommand),
    /// Run the MCP server on stdio for AI agents (Claude Code, Codex, ...)
    Mcp(McpArgs),
    /// Check every machine once and print a report
    Check(CheckArgs),
    /// Write a starter config.toml
    Init(InitArgs),
    /// Print where Muxin keeps its files
    Paths(CommonArgs),
    /// Put `muxin` in your PATH (macOS: /usr/local/bin, Linux: ~/.local/bin,
    /// Windows: this folder in your user PATH), or take it out
    ShellCommand(ShellCommandArgs),
    /// Whether a newer Muxin is out, and how to update this copy
    Updates(UpdatesArgs),
}

#[derive(Args)]
struct UpdatesArgs {
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ShellCommandArgs {
    /// status (the default), install or remove
    #[arg(value_enum, default_value = "status")]
    action: ShellCommandAction,
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ShellCommandAction {
    Status,
    Install,
    Remove,
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Config directory [default: ~/.config/muxin (%APPDATA%\\muxin on Windows), or $MUXIN_HOME]
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Inventory file [default: <config-dir>/config.toml]
    #[arg(long, short = 'f', value_name = "FILE")]
    inventory: Option<PathBuf>,
}

impl CommonArgs {
    fn paths(&self) -> Paths {
        let paths = Paths::resolve(self.config_dir.clone(), self.inventory.clone());
        crash::log_to(paths.dir.join("crash.log"));
        paths
    }
}

#[derive(Args, Clone)]
struct OpenArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Run against simulated machines instead of your real ones
    #[arg(long)]
    demo: bool,

    /// Open a specific view at launch (for screenshots and debugging), e.g.
    /// "jobs", "settings", "machine:<id>", "runner:<id>", "install:<machine-id>", "add-machine", "logs:<runner-id>",
    /// "sidebar:<width>", "activity-filter:<failed|operations|agents>"; comma-separated
    #[arg(long, hide = true, value_name = "VIEW")]
    view: Option<String>,
}

/// Flags every agent-facing command takes.
#[derive(Args, Clone)]
struct AgentArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Use simulated machines (safe to experiment with; nothing touches real machines or GitHub)
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
struct RenameMachineArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// Machine id (e.g. mac-studio) or name
    machine_id: String,

    /// The new name, e.g. "Studio"
    name: String,
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
struct CancelRunArgs {
    #[command(flatten)]
    agent: AgentArgs,

    /// The job's id (from `muxin jobs --json`)
    job_id: u64,

    /// Return right away with the operation id
    #[arg(long)]
    no_wait: bool,
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
enum GithubCommand {
    /// Review requests, issues assigned to you, your open pull requests
    Inbox(AgentArgs),
    /// Your repositories, most recently pushed first
    Repos(AgentArgs),
    /// A repository's open issues and PRs, recent runs, and local checkouts
    Repo(GhRepoArgs),
    /// The GitHub CLI accounts you're signed in to, and which one Muxin uses
    Accounts(AgentArgs),
    /// Switch Muxin to another of the GitHub CLI's accounts
    Use(GhUseArgs),
    /// Add a repository to your favorites (listed first), or remove it with --remove
    #[command(alias = "fav")]
    Favorite(GhFavoriteArgs),
    /// An issue or pull request with its conversation (URL or owner/name#123)
    #[command(alias = "pr")]
    Issue(GhItemArgs),
}

#[derive(Args)]
struct GhRepoArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// owner/name or a GitHub URL
    repo: String,
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// The schedules on your machines (--on one), with their last runs
    Ls(ScheduleLsArgs),
    /// Add one, e.g. `muxin schedule add beelink --name Tests --every day --at 09:00 --command "make test"`
    Add(ScheduleAddArgs),
    /// Delete one
    Rm(ScheduleRefArgs),
    /// Run one now
    Run(ScheduleRefArgs),
    /// Its last runs, and the newest one's output
    Log(ScheduleRefArgs),
}

#[derive(Args)]
struct ScheduleLsArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// One machine
    #[arg(long)]
    on: Option<String>,
}

#[derive(Args)]
struct ScheduleAddArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// The machine it runs on
    machine: String,
    #[arg(long)]
    name: String,
    /// 15m (every 15 minutes), 2h, day, weekdays, or days like mon,thu
    #[arg(long)]
    every: String,
    /// HH:MM in the machine's time (for hours: the minute)
    #[arg(long)]
    at: Option<String>,
    /// A shell command to run
    #[arg(long)]
    command: Option<String>,
    /// Or an agent session: claude, codex, opencode, gemini, amp, pi
    #[arg(long = "agent")]
    cli: Option<String>,
    /// What the agent is asked
    #[arg(long)]
    prompt: Option<String>,
    /// The folder it runs in (default ~)
    #[arg(long)]
    folder: Option<String>,
}

#[derive(Args)]
struct ScheduleRefArgs {
    #[command(flatten)]
    agent: AgentArgs,
    machine: String,
    /// The schedule's id (s-...)
    id: String,
}

/// `--every` and `--at` as a schedule's `when`.
fn parse_every(every: &str, at: Option<String>) -> Result<muxen_when::When, String> {
    let e = every.trim().to_ascii_lowercase();
    let num = |s: &str| s.parse::<u32>().map_err(|_| format!("\"{every}\" isn't a schedule Muxin knows."));
    let (kind, n, days): (&str, Option<u32>, Vec<String>) = if let Some(m) = e.strip_suffix('m').filter(|m| m.chars().all(|c| c.is_ascii_digit())) {
        ("minutes", Some(num(m)?), Vec::new())
    } else if let Some(h) = e.strip_suffix('h').filter(|h| h.chars().all(|c| c.is_ascii_digit())) {
        ("hours", Some(num(h)?), Vec::new())
    } else if matches!(e.as_str(), "day" | "daily") {
        ("day", None, Vec::new())
    } else if e == "weekdays" {
        ("weekdays", None, Vec::new())
    } else {
        ("week", None, e.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect())
    };
    let at = match (kind, at) {
        ("minutes", _) => None,
        ("hours", a) => Some(a.unwrap_or_else(|| "00:00".into())),
        (_, Some(a)) => Some(a),
        (_, None) => return Err("Say when with --at HH:MM.".into()),
    };
    Ok(muxen_when::When { every: kind.into(), n, at, days })
}

mod muxen_when {
    pub use crate::fleet::schedules::When;
}

#[derive(Subcommand)]
enum VmCommand {
    /// Where VMs can run, what each machine can run, and the systems
    Options(AgentArgs),
    /// Create a VM and add it to your machines (Windows installs itself: 15-30 minutes)
    Create(VmCreateArgs),
    /// Delete a VM Muxin made: its container, its disk, and the machine in Muxin
    Destroy(CloudMachineArgs),
}

#[derive(Args)]
struct VmCreateArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Its name in Muxin
    name: String,
    /// windows-11, windows-10, windows-server-2025, windows-server-2022, ubuntu-26.04,
    /// ubuntu-24.04, debian-13, fedora, alpine or arch
    #[arg(long)]
    system: String,
    /// The machine to run it on (default: the one with the most free memory that can)
    #[arg(long)]
    on: Option<String>,
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory, in GB
    #[arg(long)]
    memory: Option<u32>,
    /// Windows' disk, in GB
    #[arg(long)]
    disk: Option<u32>,
    /// Agent CLIs to install on a Linux VM, comma-separated: claude, codex, gemini, opencode, amp, pi
    #[arg(long)]
    install: Option<String>,
    /// Return right away with the operation id
    #[arg(long)]
    no_wait: bool,
}

#[derive(Subcommand)]
enum CloudCommand {
    /// The account, regions, sizes and defaults for a new machine
    Options(AgentArgs),
    /// What the cloud machines cost: now, this month, by the month's end, and the account's bill
    Usage(AgentArgs),
    /// Create a droplet, set it up, and add it to your machines (it costs money while it exists)
    Create(CloudCreateArgs),
    /// Delete a cloud machine's droplet and remove it from Muxin
    Destroy(CloudMachineArgs),
    /// Power a cloud machine on or off (DigitalOcean still bills it while off)
    Power(CloudPowerArgs),
    /// Sign in to DigitalOcean: --token - (reads the token from stdin), --doctl, or --browser
    Login(CloudLoginArgs),
    /// Forget the DigitalOcean sign-in
    Logout(AgentArgs),
}

#[derive(Args)]
struct CloudCreateArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// The machine's name
    name: String,
    /// Region slug, e.g. nyc3 (default: Settings, else the nearest)
    #[arg(long)]
    region: Option<String>,
    /// Size slug, e.g. s-2vcpu-4gb (default: Settings)
    #[arg(long)]
    size: Option<String>,
    /// What it comes with, comma-separated: docker, gh, node, python, go, rust, php, ruby,
    /// java, bun, deno, dotnet, claude, codex, gemini, opencode, amp, pi, tailscale
    /// ("" for nothing; default: Settings)
    #[arg(long)]
    install: Option<String>,
    /// Return right away with the operation id
    #[arg(long)]
    no_wait: bool,
}

#[derive(Args)]
struct CloudMachineArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
}

#[derive(Args)]
struct CloudPowerArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// on or off
    #[arg(value_parser = ["on", "off"])]
    state: String,
}

#[derive(Args)]
struct CloudLoginArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// A DigitalOcean API token (kept in the Keychain). `-` reads it from stdin, which
    /// keeps it out of your shell's history and the process list:
    /// `pbpaste | muxin cloud login --token -`
    #[arg(long, conflicts_with_all = ["doctl", "browser"])]
    token: Option<String>,
    /// Use doctl's login
    #[arg(long, conflicts_with = "browser")]
    doctl: bool,
    /// Sign in in the browser
    #[arg(long)]
    browser: bool,
}

#[derive(Subcommand)]
enum DesktopCommand {
    /// Save a screenshot of the machine's screen (JPEG)
    Shot(DesktopShotArgs),
    /// Send input: a JSON list like '[{"type":"click","x":100,"y":200},{"type":"text","text":"hi"}]'
    Input(DesktopInputArgs),
    /// List the machine's monitors, virtual ones included
    Displays(MachineArgs),
    /// Add a virtual monitor (like plugging in another screen)
    Add(DesktopAddArgs),
    /// Remove a virtual monitor Muxin added
    Remove(DesktopRemoveArgs),
    /// Change a monitor's resolution or scale
    Set(DesktopSetArgs),
    /// Open the machine's own Remote Desktop (RDP) in your app for it, through Muxin, until Ctrl-C
    Rdp(MachineArgs),
    /// Open the machine's own VNC (a Mac's Screen Sharing) in your app for it, through Muxin, until Ctrl-C
    Vnc(MachineArgs),
}

#[derive(Args)]
struct PsArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// How many to show
    #[arg(short = 'n', long, default_value_t = 20)]
    lines: usize,
}

#[derive(Args)]
struct FleetRunArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine ids or names, comma-separated
    #[arg(long, value_delimiter = ',', required_unless_present = "all")]
    on: Vec<String>,
    /// Every machine that's online
    #[arg(long, conflicts_with = "on")]
    all: bool,
    /// Seconds each may run
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    /// The command (quote it, or put it after --)
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct KillArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    pid: u32,
    /// SIGKILL instead of SIGTERM
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct OpenFolderArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// <machine>:<folder>, e.g. beelink:~/code/api
    target: String,
    /// zed, cursor or vscode (default: Settings › General › Editor, or the first installed)
    #[arg(long, default_value = "")]
    editor: String,
    /// Print the command instead of running it
    #[arg(long)]
    print: bool,
}

#[derive(Args)]
struct UsageArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// The last N days
    #[arg(long, default_value_t = 7)]
    days: u32,
    /// Just this machine
    machine: Option<String>,
}

#[derive(Args)]
struct ForwardArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// The port there
    port: u16,
    /// The port here (default: the same one, or the next free one)
    #[arg(long)]
    local: Option<u16>,
}

#[derive(Args)]
struct ServicesArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// start, stop, restart, enable or disable (none: list them)
    action: Option<String>,
    /// The service or container
    name: Option<String>,
    /// It's a container (when a service has the same name)
    #[arg(long)]
    container: bool,
}

#[derive(Args)]
struct LogsArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// What there is to read instead
    #[arg(long)]
    list: bool,
    /// The kernel's messages (Linux)
    #[arg(long, conflicts_with_all = ["file", "unit", "container"])]
    kernel: bool,
    /// A log file
    #[arg(long, conflicts_with_all = ["unit", "container"])]
    file: Option<String>,
    /// A systemd unit (nginx, actions.runner...)
    #[arg(long, conflicts_with = "container")]
    unit: Option<String>,
    /// The unit is the user's own (systemctl --user)
    #[arg(long, requires = "unit")]
    user: bool,
    /// A Docker container (name or id)
    #[arg(long)]
    container: Option<String>,
    /// How many lines (default 200)
    #[arg(short = 'n', long, default_value_t = 200)]
    lines: u64,
    /// Only lines holding this (any case)
    #[arg(long)]
    grep: Option<String>,
    /// Keep printing new lines (Ctrl-C to stop)
    #[arg(long)]
    follow: bool,
}

#[derive(Args)]
struct DesktopSetArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// The display's id (see `muxin desktop displays`)
    display: String,
    /// Its resolution, e.g. 3840x2160 (one it offers)
    #[arg(long)]
    size: Option<String>,
    /// How much the desktop scales things up: 1, 2, ...
    #[arg(long)]
    scale: Option<f64>,
}

#[derive(Args)]
struct DesktopAddArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// Its size, e.g. 2560x1440 (default: the main monitor's)
    #[arg(long)]
    size: Option<String>,
    /// What to call it
    #[arg(long)]
    name: Option<String>,
}

#[derive(Args)]
struct DesktopRemoveArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// The display's id (see `muxin desktop displays`)
    display: String,
}

#[derive(Args)]
struct DesktopShotArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// Where to save it (default: <machine>-desktop.jpg)
    #[arg(short, long)]
    output: Option<std::path::PathBuf>,
    /// One of its displays (see `muxin desktop displays`)
    #[arg(long)]
    display: Option<String>,
}

#[derive(Args)]
struct DesktopInputArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// The events, as JSON
    events: String,
    /// One of its displays (see `muxin desktop displays`)
    #[arg(long)]
    display: Option<String>,
}

#[derive(Args)]
struct FilesArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// <machine>[:<folder>], e.g. beelink:~/build (default: the home folder)
    target: String,
}

#[derive(Args)]
struct CpArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// <machine>:<path>... <machine>:<folder>: files and folders on one machine,
    /// then the folder to copy them into (made if missing; same names are replaced)
    #[arg(required = true, num_args = 2..)]
    paths: Vec<String>,
    /// Start the copy and return without waiting for it
    #[arg(long)]
    no_wait: bool,
}

#[derive(Args)]
struct ConnectionArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// Connect directly over QUIC (on) or only through ssh (off)
    #[arg(long, value_name = "on|off")]
    direct: Option<String>,
}

#[derive(Subcommand)]
enum GitCommand {
    /// git's name and email, and how it logs in to GitHub, on a machine
    Status(GitMachineArgs),
    /// Set git's name and email (from your GitHub account) and/or its GitHub login (via gh)
    Setup(GitSetupArgs),
}

#[derive(Args)]
struct GitMachineArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
}

#[derive(Args)]
struct GitSetupArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// Set git's name and email: your GitHub name and private noreply address, unless given
    #[arg(long)]
    identity: bool,
    #[arg(long, requires = "identity")]
    name: Option<String>,
    #[arg(long, requires = "identity")]
    email: Option<String>,
    /// Make git use the machine's GitHub CLI login for github.com (gh auth setup-git)
    #[arg(long)]
    credentials: bool,
}

#[derive(Args)]
struct GhUseArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// A GitHub login (see `muxin github accounts`)
    #[arg(required_unless_present = "machine")]
    login: Option<String>,
    /// Use this machine's GitHub CLI login instead
    #[arg(long, value_name = "MACHINE")]
    machine: Option<String>,
}

#[derive(Args)]
struct GhFavoriteArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// owner/name or a GitHub URL
    repo: String,
    /// Remove it from the favorites
    #[arg(long)]
    remove: bool,
}

#[derive(Args)]
struct GhItemArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Issue or PR URL, or owner/name#123
    reference: String,
}

#[derive(Args)]
struct CheckoutsArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Only this machine
    #[arg(long, value_name = "MACHINE")]
    machine: Option<String>,
    /// Only checkouts of this repository (owner/name)
    #[arg(long)]
    repo: Option<String>,
    /// Run git fetch, pull or push in --path on --machine
    #[arg(long, value_name = "ACTION", requires_all = ["machine", "path"])]
    git: Option<String>,
    /// The checkout's folder (with --git)
    #[arg(long)]
    path: Option<String>,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List sessions (and which agents each machine has)
    #[command(alias = "list")]
    Ls(SessionLsArgs),
    /// Start an agent (claude, codex, opencode, gemini, amp, pi) or a shell on a machine
    Start(SessionStartArgs),
    /// Print a session's screen and recent scrollback
    Read(SessionReadArgs),
    /// Type text into a session (and press Enter)
    Send(SessionSendArgs),
    /// Ask a session's agent to commit, commit_push, pull_request, review, fix_comments, conflicts or recap
    Do(SessionDoArgs),
    /// What a session's agent is waiting for: a choice and its options, or what it said last
    Question(SessionIdArgs),
    /// Pick one of a waiting session's options (its key from `question`)
    Answer(SessionAnswerArgs),
    /// Open a session in this terminal (detach with the window's close, or kill the ssh)
    Attach(SessionIdArgs),
    /// Run a session's agent again
    Restart(SessionIdArgs),
    /// Give a session a new title ("" goes back to the one it started with)
    Rename(SessionRenameArgs),
    /// End a session
    #[command(alias = "rm")]
    Stop(SessionIdArgs),
}

#[derive(Args)]
struct SessionLsArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Only this machine
    #[arg(long, value_name = "MACHINE")]
    machine: Option<String>,
}

#[derive(Args)]
struct SessionStartArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Machine id or name
    machine: String,
    /// claude, codex, opencode, gemini, amp, pi or shell
    #[arg(value_name = "AGENT")]
    kind: String,
    /// Folder on the machine (default: home)
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long)]
    title: Option<String>,
    /// First message for the agent
    #[arg(long)]
    prompt: Option<String>,
}

#[derive(Args)]
struct SessionIdArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Session id (<machine-id>/<name>) or unique name
    session_id: String,
}

#[derive(Args)]
struct SessionAnswerArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Session id (<machine-id>/<name>) or unique name
    session_id: String,
    /// The option's key, as `muxin session question` shows it (1, 2, … or #0, #1, …)
    choice: String,
}

#[derive(Args)]
struct SessionRenameArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Session id (<machine-id>/<name>) or unique name
    session_id: String,
    /// The new title
    title: String,
}

#[derive(Args)]
struct SessionReadArgs {
    #[command(flatten)]
    agent: AgentArgs,
    session_id: String,
    /// Scrollback lines above the screen
    #[arg(long, default_value_t = 200)]
    lines: u32,
}

#[derive(Args)]
struct SessionSendArgs {
    #[command(flatten)]
    agent: AgentArgs,
    session_id: String,
    /// Text to type
    text: String,
    /// Don't press Enter afterwards
    #[arg(long)]
    no_enter: bool,
}

#[derive(Args)]
struct SessionDoArgs {
    #[command(flatten)]
    agent: AgentArgs,
    session_id: String,
    /// commit, commit_push, pull_request, review, fix_comments, conflicts or recap
    action: String,
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

    /// A git worktree of this checkout on the machine (its folder) instead of
    /// a clone, on --branch, so several agents can work on one repository
    #[arg(long, value_name = "FOLDER", requires = "machine", conflicts_with_all = ["repo", "docker"])]
    worktree_of: Option<String>,
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

    /// Use simulated machines (prints the command instead of running it)
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

    /// Serve simulated machines
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

    /// Use simulated machines
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

    /// Overwrite an existing config.toml
    #[arg(long)]
    force: bool,
}

fn main() -> ExitCode {
    crash::install(concat!("muxin ", env!("CARGO_PKG_VERSION")), config::dir_from_env().join("crash.log"));
    // Logs always go to stderr: stdout is reserved for --json output and MCP messages.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MUXEN_LOG").unwrap_or_else(|_| "muxin=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .without_time()
        .init();

    // A share link opened with Muxin: Linux and Windows hand it over as the
    // argument (macOS through the app, see ui::open_link).
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some(url) = argv.get(1).and_then(|a| a.to_str()).filter(|a| a.starts_with("muxin://join")).map(String::from) {
        argv = vec![argv[0].clone(), "open".into(), "--view".into(), format!("join:{url}").into()];
    }
    let cli = Cli::parse_from(argv);
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
                format!("Removed {} ({}) from config.toml. On the machine, only a direct connection from this app (if it had one) is turned off.\n", m.name, m.id)
            })
        }
        Some(Command::Connect(a)) => {
            let req = api::ConnectMachineRequest { machine_id: a.machine_id, connected: true };
            run(&runtime, &a.agent, async move |e| api::connect_machine(&e, req).await, |m| format!("Connected to {} ({}).\n", m.name, m.status))
        }
        Some(Command::Disconnect(a)) => {
            let req = api::ConnectMachineRequest { machine_id: a.machine_id, connected: false };
            run(&runtime, &a.agent, async move |e| api::connect_machine(&e, req).await, |m| format!("Disconnected from {}. Its sessions go on there; `muxin connect {}` connects again.\n", m.name, m.id))
        }
        Some(Command::RenameMachine(a)) => {
            let req = api::RenameMachineRequest { machine_id: a.machine_id, name: a.name };
            run(&runtime, &a.agent, async move |e| api::rename_machine(&e, req).await, |m| format!("Renamed {} to {}.\n", m.id, m.name))
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
        Some(Command::CancelRun(a)) => {
            let req = api::CancelRunRequest { job_id: a.job_id, wait: !a.no_wait, timeout_secs: None };
            run(&runtime, &a.agent, async move |e| api::cancel_run(&e, req, api::Surface::Cli, stream_line).await, cli::action)
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
        Some(Command::SshHosts(a)) => run(&runtime, &a, async move |e| Ok(api::ssh_hosts(&e).await), |h| {
            let mut out = if h.hosts.is_empty() { "No hosts in ~/.ssh/config.\n".to_string() } else { format!("{}\n", h.hosts.join("\n")) };
            if !h.network.is_empty() {
                out.push_str("\nOn this network:\n");
                for n in &h.network {
                    let note = if n.ssh { "" } else { "  (a Mac with Remote Login off: turn it on in System Settings › General › Sharing)" };
                    out.push_str(&format!("{}  {}{note}\n", n.host, n.name));
                }
            }
            if !h.tailnet.is_empty() {
                out.push_str("\nOn your tailnet:\n");
                for t in &h.tailnet {
                    let state = if t.online { "online" } else { "offline" };
                    let added = t.machine_id.as_ref().map(|id| format!("  (added as {id})")).unwrap_or_default();
                    out.push_str(&format!("{}  {}  {}{added}\n", t.host, t.os, state));
                }
            }
            out
        }),
        Some(Command::Workspace(cmd)) => workspace(cmd, &runtime),
        Some(Command::Session(cmd)) => session(cmd, &runtime),
        Some(Command::Github(cmd)) => github(cmd, &runtime),
        Some(Command::Checkouts(a)) => checkouts(a, &runtime),
        Some(Command::Git(cmd)) => git(cmd, &runtime),
        Some(Command::Desktop(cmd)) => desktop(cmd, &runtime),
        Some(Command::Logs(a)) => logs(a, &runtime),
        Some(Command::Services(a)) => services(a, &runtime),
        Some(Command::Ps(a)) => {
            let (req, n) = (api::ProcessesRequest { machine_id: a.machine }, a.lines);
            run(&runtime, &a.agent, async move |e| api::machine_processes(&e, req).await, move |l| cli::processes(l, n))
        }
        Some(Command::Kill(a)) => {
            let req = api::ProcessSignalRequest { machine_id: a.machine, pid: a.pid, signal: Some(if a.force { "kill".into() } else { "stop".into() }) };
            run(&runtime, &a.agent, async move |e| api::process_signal(&e, req).await, |v| format!("{} {}.\n", if v["done"] == "killed" { "Killed" } else { "Stopped" }, v["pid"]))
        }
        Some(Command::Run(a)) => fleet_run(a, &runtime),
        Some(Command::Power(a)) => {
            let req = api::CloudMachineRequest { machine_id: a.machine, power: Some(a.state), wait: true };
            run(&runtime, &a.agent, async move |e| api::machine_power(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        Some(Command::Wake(a)) => {
            let req = api::WakeRequest { machine_id: a.machine_id };
            run(&runtime, &a.agent, async move |e| api::wake_machine(&e, req).await, |v| format!("Sent the wake signal from {}. Muxin checks it again in 20 and 40 seconds.\n", v["sent_from"].as_str().unwrap_or("here")))
        }
        Some(Command::Edit(a)) => {
            let (machine, path) = a.target.split_once(':').map(|(m, p)| (m.to_string(), p.to_string())).unwrap_or((a.target.clone(), "~".into()));
            let (editor, print) = (a.editor.clone(), a.print);
            run(
                &runtime,
                &a.agent,
                async move |e| {
                    let m = api::find_machine(&e.current(), &machine)?.clone();
                    e.editor_command(&m.id, &path, &editor, !print).await.map_err(api::ApiError::invalid)
                },
                move |(name, line)| if print { format!("{}\n", line.join(" ")) } else { format!("Opening it in {name}.\n") },
            )
        }
        Some(Command::Usage(a)) => {
            let req = api::UsageRequest { days: Some(a.days), machine_id: a.machine };
            run(&runtime, &a.agent, async move |e| api::usage(&e, req).await, cli::usage)
        }
        Some(Command::Ports(a)) => {
            let req = api::PortsRequest { machine_id: a.machine_id };
            run(&runtime, &a.agent, async move |e| api::machine_ports(&e, req).await, |p| cli::ports(p))
        }
        Some(Command::Forward(a)) => {
            let (machine, port, local) = (a.machine.clone(), a.port, a.local);
            run(
                &runtime,
                &a.agent,
                async move |e| {
                    let f = api::forward_port(&e, api::ForwardRequest { machine_id: machine, port, local_port: local }).await?;
                    eprintln!("Port {} of {} is at {} here. Ctrl-C stops.", f.port, f.machine_id, f.url());
                    let _ = tokio::signal::ctrl_c().await;
                    e.stop_forward(&f.machine_id, f.port);
                    Ok(f)
                },
                |_| String::new(),
            )
        }
        Some(Command::Cloud(cmd)) => cloud(cmd, &runtime),
        Some(Command::Schedule(cmd)) => match cmd {
            ScheduleCommand::Ls(a) => {
                let req = api::ScheduleListRequest { machine_id: a.on };
                run(&runtime, &a.agent, async move |e| api::schedule_list(&e, req).await, cli::schedules)
            }
            ScheduleCommand::Add(a) => {
                let when = match parse_every(&a.every, a.at) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return std::process::ExitCode::from(2);
                    }
                };
                let req = api::ScheduleSaveRequest { machine_id: a.machine, id: None, name: a.name, when, command: a.command, agent: a.cli, prompt: a.prompt, folder: a.folder, enabled: true };
                run(&runtime, &a.agent, async move |e| api::schedule_save(&e, req).await, |s| format!("✓ {} ({}): {}\n", s.name, s.id, s.when_text))
            }
            ScheduleCommand::Rm(a) => {
                let req = api::ScheduleRequest { machine_id: a.machine, schedule_id: a.id, started: None };
                run(&runtime, &a.agent, async move |e| api::schedule_delete(&e, req).await, |_| "✓ Deleted\n".to_string())
            }
            ScheduleCommand::Run(a) => {
                let req = api::ScheduleRequest { machine_id: a.machine, schedule_id: a.id, started: None };
                run(&runtime, &a.agent, async move |e| api::schedule_run(&e, req).await, |_| "✓ Started. See it with `muxin schedule log`.\n".to_string())
            }
            ScheduleCommand::Log(a) => {
                let req = api::ScheduleRequest { machine_id: a.machine, schedule_id: a.id, started: None };
                run(&runtime, &a.agent, async move |e| api::schedule_log(&e, req).await, cli::schedule_log)
            }
        },
        Some(Command::Vm(cmd)) => match cmd {
            VmCommand::Options(a) => run(&runtime, &a, async move |e| Ok::<_, api::ApiError>(api::vm_options(&e).await), cli::vm_options),
            VmCommand::Create(a) => {
                let install = a.install.map(|l| l.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
                let req = api::VmCreateRequest { name: a.name, system: a.system, host: a.on, cpus: a.cpus, memory_gb: a.memory, disk_gb: a.disk, install, wait: !a.no_wait, timeout_secs: Some(100 * 60) };
                run(&runtime, &a.agent, async move |e| api::vm_create(&e, req, api::Surface::Cli, stream_line).await, cli::action)
            }
            VmCommand::Destroy(a) => {
                let req = api::CloudMachineRequest { machine_id: a.machine, power: None, wait: true };
                run(&runtime, &a.agent, async move |e| api::vm_destroy(&e, req, api::Surface::Cli, stream_line).await, cli::action)
            }
        },
        Some(Command::Files(a)) => {
            let (machine, path) = a.target.split_once(':').map(|(m, p)| (m.to_string(), p.to_string())).unwrap_or((a.target.clone(), String::new()));
            let req = api::ListFilesRequest { machine_id: machine, path };
            run(&runtime, &a.agent, async move |e| api::list_files(&e, req).await, cli::listing)
        }
        Some(Command::Cp(a)) => cp(a, &runtime),
        Some(Command::Connection(a)) => {
            let machine = a.machine.clone();
            match a.direct.as_deref() {
                Some(v @ ("on" | "off")) => {
                    let req = api::DirectRequest { machine_id: machine, enabled: v == "on" };
                    run(&runtime, &a.agent, async move |e| api::set_direct(&e, req).await, cli::connection)
                }
                Some(other) => {
                    eprintln!("error: --direct takes on or off, not \"{other}\"");
                    2
                }
                None => run(&runtime, &a.agent, async move |e| api::machine_connection(&e, &machine).await, cli::connection),
            }
        }
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
            println!("config      {}", display_path(&p.config));
            if p.inventory != p.config {
                println!("machines    {}", display_path(&p.inventory));
            }
            println!("state       {}", display_path(&p.state));
            0
        }
        Some(Command::Updates(args)) => {
            let current = env!("CARGO_PKG_VERSION");
            match (runtime.block_on(update::check()), args.json) {
                (Ok(latest), true) => {
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "current": current, "update": latest })).unwrap_or_default());
                    0
                }
                (Ok(None), false) => {
                    println!("Muxin {current} is the latest version.");
                    0
                }
                (Ok(Some(u)), false) => {
                    println!("Muxin {} is out (you have {current}).", u.version);
                    match (&u.command, &u.download) {
                        (Some(command), _) => println!("Update it with: {command}"),
                        (None, Some(url)) => println!("Download: {url}"),
                        (None, None) => {}
                    }
                    println!("What's new: {}", u.page);
                    0
                }
                (Err(e), true) => {
                    println!("{}", serde_json::json!({ "error": { "code": "unreachable", "message": e } }));
                    1
                }
                (Err(e), false) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        Some(Command::ShellCommand(args)) => {
            let result = match args.action {
                ShellCommandAction::Status => Ok(shell_command::status()),
                ShellCommandAction::Install => shell_command::install(),
                ShellCommandAction::Remove => shell_command::remove(),
            };
            match (result, args.json) {
                (Ok(s), true) => {
                    println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
                    0
                }
                (Ok(s), false) => {
                    let line = match s.state {
                        "installed" => format!("muxin is installed: {}", s.path),
                        "missing" => format!("Not installed (it would go in {}). Run `muxin shell-command install`.", s.path),
                        "other" => format!("{} is something else{}", s.path, s.target.as_deref().map(|t| format!(" (-> {t})")).unwrap_or_default()),
                        _ => "Not available here.".to_string(),
                    };
                    println!("{line}");
                    if let Some(note) = s.note {
                        println!("{note}");
                    }
                    0
                }
                (Err(e), true) => {
                    println!("{}", serde_json::json!({ "error": { "code": "operation_failed", "message": e } }));
                    1
                }
                (Err(e), false) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
    };
    ExitCode::from(code)
}

/// Windows: started from Explorer or the Start menu, the window comes with a
/// console of its own (a terminal window). Start again without one: a
/// console that has no window, which every ssh and PowerShell Muxin starts
/// then shares instead of each opening a window. True when it did (this
/// process should end).
#[cfg(windows)]
fn without_console_window() -> bool {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if std::env::var_os("MUXEN_NO_CONSOLE").is_some() {
        return false;
    }
    // Someone else on this console: started from a terminal, which stays.
    let mut ids = [0u32; 2];
    if unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) } != 1 {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env("MUXEN_NO_CONSOLE", "1").creation_flags(CREATE_NO_WINDOW);
    cmd.spawn().is_ok()
}

fn open(args: OpenArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    #[cfg(windows)]
    if without_console_window() {
        return 0;
    }
    // The engine runs on tokio; the window runs on GPUI's main-thread executor.
    let _guard = runtime.enter();
    let engine = Engine::new(args.common.paths(), args.demo);
    engine.start();
    if engine.is_demo() {
        tracing::info!("demo mode: simulated machines, nothing is read from or written to disk");
    } else {
        tracing::info!("inventory {}", display_path(&engine.paths().inventory));
    }
    #[cfg(feature = "gui")]
    {
        // The window is the one process that watches cloud machines for idleness.
        engine.start_cloud_idle_watch();
        ui::run(engine, args.view);
        0
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (engine, args.view);
        eprintln!("This build of Muxin has no window (built without the `gui` feature).");
        eprintln!("Use the commands (`muxin --help`) or the MCP server (`muxin mcp`).");
        2
    }
}

/// Runs one agent-facing command: JSON or human output on stdout, errors with a
/// code (0 ok, 1 failed, 2 bad input / not found).
/// `muxin cp <machine>:<path>... <machine>:<folder>`
fn cp(a: CpArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let place = |s: &str| s.split_once(':').map(|(m, p)| (m.to_string(), p.to_string()));
    let (dest, sources) = a.paths.split_last().expect("clap requires two");
    let Some((to, to_dir)) = place(dest) else {
        eprintln!("error: the destination is <machine>:<folder>, e.g. mac-studio:~/Downloads");
        return 2;
    };
    let mut from: Option<String> = None;
    let mut paths = Vec::new();
    for s in sources {
        match place(s) {
            Some((m, p)) if from.as_ref().is_none_or(|f| *f == m) => {
                from = Some(m);
                paths.push(p);
            }
            Some(_) => {
                eprintln!("error: copy from one machine at a time");
                return 2;
            }
            None => {
                eprintln!("error: \"{s}\" needs its machine: <machine>:<path>, e.g. beelink:~/build/app.tar.gz");
                return 2;
            }
        }
    }
    let req = api::CopyFilesRequest { from: from.unwrap_or_default(), paths, to, to_dir, wait: !a.no_wait, timeout_secs: None };
    let json = a.agent.json;
    run(runtime, &a.agent, async move |e| api::copy_files(&e, req, api::Surface::Cli, |line| if !json { eprintln!("{line}") }).await, cli::copied)
}

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
                worktree_of: a.worktree_of,
            };
            run(runtime, &a.agent, async move |e| api::workspace_create(&e, req, CLI_OWNER, CLI).await, cli::workspace_created)
        }
        WorkspaceCommand::Ls(a) => run(runtime, &a, async move |e| Ok(api::workspaces_list(&e).await), cli::workspaces),
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
/// `muxin run`: each machine's output under its name; exits 1 when any failed.
fn fleet_run(a: FleetRunArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let _guard = runtime.enter();
    let engine = Engine::new(a.agent.common.paths(), a.agent.demo);
    let machine_ids = if a.all {
        runtime.block_on(api::status(&engine, api::StatusRequest::default(), CLI)).ok();
        engine.current().machines.iter().filter(|m| m.status == "online").map(|m| m.id.clone()).collect()
    } else {
        a.on.clone()
    };
    let req = api::RunRequest { machine_ids, command: shell_join(&a.command), timeout_secs: Some(a.timeout), wait: Some(true) };
    match runtime.block_on(api::run_on_machines(&engine, req)) {
        Ok(out) => {
            if a.agent.json {
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            } else {
                let snap = engine.current();
                let ops = &snap.operations;
                for r in &out {
                    let how = match (r.exit_code, &r.error) {
                        (Some(0), _) => "ok".to_string(),
                        (Some(c), _) => format!("exit {c}"),
                        (None, Some(e)) => e.clone(),
                        (None, None) => "running".into(),
                    };
                    println!("── {} ({how})", r.machine_id);
                    let log = r.operation.as_ref().and_then(|o| ops.iter().find(|x| x.id == o.id)).map(|o| o.log.clone()).unwrap_or_default();
                    for line in log.iter().filter(|l| !l.starts_with("error: ")) {
                        println!("{line}");
                    }
                }
            }
            u8::from(out.iter().any(|r| r.exit_code != Some(0)))
        }
        Err(e) => report_error(&a.agent, &e),
    }
}

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
                    eprintln!("muxin: timed out after {}s", out.duration_ms / 1000);
                } else if out.truncated {
                    eprintln!("muxin: output was truncated (the middle was dropped)");
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

fn github(cmd: GithubCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        GithubCommand::Inbox(a) => run(runtime, &a, async move |e| api::github_inbox(&e).await, cli::github_inbox),
        GithubCommand::Repos(a) => run(runtime, &a, async move |e| api::github_repos(&e).await, |r| cli::github_repos(r)),
        GithubCommand::Repo(a) => {
            let req = api::RepoRequest { repo: a.repo };
            run(runtime, &a.agent, async move |e| api::github_repo(&e, req).await, cli::github_repo)
        }
        GithubCommand::Accounts(a) => run(runtime, &a, async move |e| api::github_accounts(&e).await, cli::github_accounts),
        GithubCommand::Use(a) => {
            let req = api::UseAccountRequest { login: a.login.unwrap_or_default(), machine_id: a.machine };
            run(runtime, &a.agent, async move |e| api::github_use_account(&e, req).await, cli::github_accounts)
        }
        GithubCommand::Favorite(a) => {
            let req = api::FavoriteRequest { repo: a.repo, favorite: !a.remove };
            run(runtime, &a.agent, async move |e| api::github_favorite(&e, req), cli::github_favorites)
        }
        GithubCommand::Issue(a) => {
            let req = api::ItemRequest { reference: a.reference };
            run(runtime, &a.agent, async move |e| api::github_item(&e, req).await, cli::github_item)
        }
    }
}

fn cloud(cmd: CloudCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        CloudCommand::Options(a) => run(runtime, &a, async move |e| Ok::<_, api::ApiError>(api::cloud_options(&e).await), cli::cloud_options),
        CloudCommand::Usage(a) => run(runtime, &a, async move |e| Ok::<_, api::ApiError>(api::cloud_usage(&e).await), cli::cloud_usage),
        CloudCommand::Create(a) => {
            let req = api::CloudCreateRequest {
                name: a.name,
                region: a.region,
                size: a.size,
                install: a.install.map(|list| list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
                wait: !a.no_wait,
                timeout_secs: None,
            };
            run(runtime, &a.agent, async move |e| api::cloud_create(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        CloudCommand::Destroy(a) => {
            let req = api::CloudMachineRequest { machine_id: a.machine, power: None, wait: true };
            run(runtime, &a.agent, async move |e| api::cloud_destroy(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        CloudCommand::Power(a) => {
            let req = api::CloudMachineRequest { machine_id: a.machine, power: Some(a.state), wait: true };
            run(runtime, &a.agent, async move |e| api::cloud_power(&e, req, api::Surface::Cli, stream_line).await, cli::action)
        }
        CloudCommand::Login(a) => {
            let method = if a.doctl { "doctl" } else if a.browser { "oauth" } else { "token" };
            let mut token = a.token.clone();
            if method == "token" && token.is_none() {
                eprintln!("error: pass --token - (and the token on stdin), --doctl or --browser");
                return 2;
            }
            if token.as_deref() == Some("-") {
                let stdin = std::io::stdin();
                if stdin.is_terminal() {
                    eprint!("DigitalOcean API token: ");
                }
                token = match read_token(&mut stdin.lock()) {
                    Ok(t) => Some(t),
                    Err(e) => return report_error(&a.agent, &ApiError::invalid(e)),
                };
            }
            run(runtime, &a.agent, async move |e| e.cloud_sign_in(method, token, |url| crate::util::open_url(url)).await.map_err(api::ApiError::invalid), |email| {
                format!("Signed in to DigitalOcean as {email}.\n")
            })
        }
        CloudCommand::Logout(a) => run(runtime, &a, async move |e| Ok::<_, api::ApiError>(e.cloud_sign_out()), |_| "Signed out of DigitalOcean.\n".to_string()),
    }
}

/// `--token -`: the first line on stdin, so the token stays out of the shell's
/// history and the process list.
fn read_token(input: &mut impl std::io::BufRead) -> Result<String, String> {
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| format!("Couldn't read the token from stdin: {e}"))?;
    match line.trim() {
        "" => Err("No token came in on stdin. Pipe it in, e.g. muxin cloud login --token - < token.txt".into()),
        token => Ok(token.to_string()),
    }
}

fn desktop(cmd: DesktopCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        DesktopCommand::Shot(a) => {
            let machine = a.machine.clone();
            let path = a.output.clone().unwrap_or_else(|| format!("{machine}-desktop.jpg").into());
            let display = a.display.clone();
            run(runtime, &a.agent, async move |e| api::desktop_screenshot(&e, &machine, display.as_deref()).await, move |shot| match std::fs::write(&path, &shot.jpeg) {
                Ok(()) => format!("Saved {} ({}×{}, screen {}×{}).\n", path.display(), shot.width, shot.height, shot.screen_width, shot.screen_height),
                Err(e) => format!("Couldn't save {}: {e}\n", path.display()),
            })
        }
        DesktopCommand::Input(a) => match serde_json::from_str(&a.events) {
            Ok(events) => {
                let req = api::DesktopInputRequest { machine_id: a.machine, display: a.display, events };
                run(runtime, &a.agent, async move |e| api::desktop_input(&e, req).await, |v| format!("Sent {} events.\n", v["events"]))
            }
            Err(e) => {
                eprintln!("error: the events aren't valid: {e}");
                2
            }
        },
        DesktopCommand::Displays(a) => {
            let req = api::MachineRequest { machine_id: a.machine_id };
            run(runtime, &a.agent, async move |e| api::desktop_displays(&e, req).await, |displays| {
                displays
                    .iter()
                    .map(|d| {
                        let tags = [d.main.then_some("main"), d.is_virtual.then_some("virtual")].into_iter().flatten().collect::<Vec<_>>().join(", ");
                        format!("{:<16} {:<20} {:>5}×{:<5} at {},{}{}\n", d.id, d.name, d.width, d.height, d.x, d.y, if tags.is_empty() { String::new() } else { format!("  ({tags})") })
                    })
                    .collect::<String>()
            })
        }
        DesktopCommand::Add(a) => {
            let size = match a.size.as_deref().map(parse_size) {
                Some(Some(size)) => Some(size),
                Some(None) => {
                    eprintln!("error: --size looks like 1920x1080");
                    return 2;
                }
                None => None,
            };
            let req = api::AddDisplayRequest { machine_id: a.machine, width: size.map(|s| s.0), height: size.map(|s| s.1), name: a.name };
            run(runtime, &a.agent, async move |e| api::desktop_add_display(&e, req).await, |d| format!("Added {} ({}×{}) as display {}.\n", d.name, d.width, d.height, d.id))
        }
        DesktopCommand::Set(a) => {
            let size = match a.size.as_deref().map(parse_size) {
                Some(Some(size)) => Some(size),
                Some(None) => {
                    eprintln!("error: --size looks like 3840x2160");
                    return 2;
                }
                None => None,
            };
            let req = api::SetDisplayRequest { machine_id: a.machine, display: a.display, width: size.map(|s| s.0), height: size.map(|s| s.1), scale: a.scale };
            run(runtime, &a.agent, async move |e| api::desktop_set_display(&e, req).await, |d| format!("{} is now {}×{}.\n", d.name, d.width, d.height))
        }
        DesktopCommand::Remove(a) => {
            let req = api::RemoveDisplayRequest { machine_id: a.machine, display: a.display };
            run(runtime, &a.agent, async move |e| api::desktop_remove_display(&e, req).await, |v| format!("Removed display {}.\n", v["removed"].as_str().unwrap_or("")))
        }
        DesktopCommand::Rdp(a) => remote_desktop(a, crate::fleet::remote_desktop::Protocol::Rdp, runtime),
        DesktopCommand::Vnc(a) => remote_desktop(a, crate::fleet::remote_desktop::Protocol::Vnc, runtime),
    }
}

/// `muxin desktop rdp|vnc`: the machine's own remote desktop in this
/// computer's app for it, its port open here until Ctrl-C.
fn remote_desktop(a: MachineArgs, protocol: crate::fleet::remote_desktop::Protocol, runtime: &tokio::runtime::Runtime) -> u8 {
    let req = api::RemoteDesktopRequest { machine_id: a.machine_id, protocol: Some(protocol) };
    run(
        runtime,
        &a.agent,
        async move |e| {
            let opened = api::open_remote_desktop(&e, req).await?;
            let name = e.machine(&opened.machine_id).map(|(spec, _)| spec.name).unwrap_or_else(|| opened.machine_id.clone());
            eprintln!("{} Ctrl-C closes it.", opened.summary(&name));
            let _ = tokio::signal::ctrl_c().await;
            e.stop_forward(&opened.machine_id, opened.port);
            Ok(opened)
        },
        |_| String::new(),
    )
}

fn services(a: ServicesArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let machine = a.machine.clone();
    match (a.action.clone(), a.name.clone()) {
        (None, _) => run(runtime, &a.agent, async move |e| api::machine_services(&e, api::ServicesRequest { machine_id: machine }).await, cli::services),
        (Some(action), Some(name)) => {
            let req = api::ServiceActionRequest { machine_id: machine, name, action, container: a.container };
            run(runtime, &a.agent, async move |e| api::service_action(&e, req).await, |v| format!("{} {}.\n", past(v["action"].as_str().unwrap_or("")), v["name"].as_str().unwrap_or("")))
        }
        (Some(_), None) => {
            eprintln!("error: which service or container? muxin services <machine> <action> <name>");
            2
        }
    }
}

/// "restart" as "Restarted", for saying what was done.
fn past(action: &str) -> &'static str {
    match action {
        "start" => "Started",
        "stop" => "Stopped",
        "restart" => "Restarted",
        "enable" => "It starts at boot now:",
        "disable" => "It doesn't start at boot now:",
        _ => "Done:",
    }
}

fn logs(a: LogsArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    use crate::fleet::logs::LogSource;
    let machine = a.machine.clone();
    if a.list {
        return run(runtime, &a.agent, async move |e| api::log_sources(&e, api::LogSourcesRequest { machine_id: machine }).await, cli::log_sources);
    }
    let (kind, target) = match (&a.file, &a.unit, &a.container) {
        (Some(f), _, _) => ("file", f.clone()),
        (_, Some(u), _) => ("service", if u.contains('.') { u.clone() } else { format!("{u}.service") }),
        (_, _, Some(c)) => ("docker", c.clone()),
        _ if a.kernel => ("kernel", String::new()),
        _ => ("system", String::new()),
    };
    if !a.follow {
        let req = api::ReadLogsRequest { machine_id: machine, kind: kind.into(), target, user: a.user, lines: Some(a.lines), filter: a.grep.clone() };
        return run(runtime, &a.agent, async move |e| api::logs_read(&e, req).await, |v| {
            v["lines"].as_array().map(|l| l.iter().filter_map(|x| x.as_str()).map(|x| format!("{x}\n")).collect()).unwrap_or_default()
        });
    }
    // Following: the machine's link, printing lines as they come.
    let source = LogSource { kind: kind.into(), target, user: a.user, ..Default::default() };
    let grep = a.grep.as_deref().map(str::to_lowercase);
    let _guard = runtime.enter();
    let engine = Engine::new(a.agent.common.paths(), a.agent.demo);
    let result: Result<(), String> = runtime.block_on(async {
        let m = api::find_machine(&engine.current(), &a.machine).map_err(|e| e.message.clone())?.clone();
        let (spec, probe) = engine.machine(&m.id).ok_or("No such machine.")?;
        crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let link = crate::fleet::link::get(&spec).await.ok_or_else(|| format!("Can't reach the agent on {}.", spec.name))?;
        let (channel, mut frames) = link.open_channel();
        let request = serde_json::json!({ "op": "logs", "channel": channel, "source": source.request(), "lines": a.lines });
        let reply = link.request(request, std::time::Duration::from_secs(20)).await?;
        if reply["ok"].as_bool() != Some(true) {
            return Err(reply["error"].as_str().unwrap_or("The machine couldn't open that log.").to_string());
        }
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        while let Some((kind, bytes)) = frames.recv().await {
            if kind == crate::fleet::link::CLOSE {
                break;
            }
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines().filter(|l| grep.as_ref().is_none_or(|g| l.to_lowercase().contains(g))) {
                if writeln!(stdout, "{line}").is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// "1920x1080" as (1920, 1080).
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once(['x', '×'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn git(cmd: GitCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        GitCommand::Status(a) => {
            let machine = a.machine;
            run(runtime, &a.agent, async move |e| api::git_status(&e, &machine).await, cli::git_setup)
        }
        GitCommand::Setup(a) => {
            let req = api::GitSetupRequest { machine_id: a.machine, identity: a.identity, name: a.name, email: a.email, credentials: a.credentials };
            run(runtime, &a.agent, async move |e| api::git_setup(&e, req).await, cli::git_setup)
        }
    }
}

fn checkouts(a: CheckoutsArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    if let (Some(action), Some(machine), Some(path)) = (a.git.clone(), a.machine.clone(), a.path.clone()) {
        let req = api::GitRequest { machine_id: machine, path, action };
        return run(runtime, &a.agent, async move |e| api::checkout_git(&e, req).await, |r| format!("{}\n", r.output));
    }
    let req = api::CheckoutsRequest { machine_id: a.machine, repo: a.repo };
    run(runtime, &a.agent, async move |e| api::checkouts(&e, req).await, |r| cli::checkouts(r))
}

fn session(cmd: SessionCommand, runtime: &tokio::runtime::Runtime) -> u8 {
    match cmd {
        SessionCommand::Ls(a) => {
            let req = api::SessionListRequest { machine_id: a.machine };
            run(runtime, &a.agent, async move |e| api::sessions_list(&e, req).await, cli::sessions)
        }
        SessionCommand::Start(a) => {
            let req = api::SessionStartRequest { machine_id: a.machine, agent: a.kind, cwd: a.cwd, title: a.title, prompt: a.prompt };
            run(runtime, &a.agent, async move |e| api::session_start(&e, req).await, cli::session_started)
        }
        SessionCommand::Read(a) => {
            let req = api::SessionReadRequest { session_id: a.session_id, lines: Some(a.lines) };
            run(runtime, &a.agent, async move |e| api::session_read(&e, req).await, |r| format!("{}\n", r.text))
        }
        SessionCommand::Question(a) => {
            let req = api::SessionRequest { session_id: a.session_id };
            run(runtime, &a.agent, async move |e| api::session_question(&e, req).await, |q| {
                let mut out: String = q.text.iter().map(|l| format!("{l}\n")).collect();
                if q.options.is_empty() {
                    out.push_str("(waiting for a message: muxin session send)\n");
                }
                for o in &q.options {
                    let mark = if q.selected.as_deref() == Some(o.key.as_str()) { "›" } else { " " };
                    out.push_str(&format!("{mark} {:<3} {}\n", o.key, o.label));
                }
                out
            })
        }
        SessionCommand::Answer(a) => {
            let req = api::SessionAnswerRequest { session_id: a.session_id, choice: a.choice };
            run(runtime, &a.agent, async move |e| api::session_answer(&e, req).await, |d| format!("Answered {}.\n", d.session_id))
        }
        SessionCommand::Do(a) => {
            let req = api::SessionActionRequest { session_id: a.session_id, action: a.action };
            run(runtime, &a.agent, async move |e| api::session_action(&e, req).await, |r| format!("Asked {}: {}\n", r.session_id, r.prompt))
        }
        SessionCommand::Send(a) => {
            let req = api::SessionSendRequest { session_id: a.session_id, text: a.text, enter: !a.no_enter };
            run(runtime, &a.agent, async move |e| api::session_send(&e, req).await, |r| format!("Sent to {}.\n", r.session_id))
        }
        SessionCommand::Restart(a) => {
            let req = api::SessionRequest { session_id: a.session_id };
            run(runtime, &a.agent, async move |e| api::session_restart(&e, req).await, |r| format!("Restarted {}.\n", r.session_id))
        }
        SessionCommand::Rename(a) => {
            let req = api::SessionRenameRequest { session_id: a.session_id, title: a.title };
            run(runtime, &a.agent, async move |e| api::session_rename(&e, req).await, |s| format!("{} is now \"{}\".\n", s.id, s.title))
        }
        SessionCommand::Stop(a) => {
            let req = api::SessionRequest { session_id: a.session_id };
            run(runtime, &a.agent, async move |e| api::session_stop(&e, req).await, |r| format!("Stopped {}.\n", r.session_id))
        }
        SessionCommand::Attach(a) => session_attach(a, runtime),
    }
}

/// Replaces this process with ssh (or the local agent) attached to the session.
fn session_attach(a: SessionIdArgs, runtime: &tokio::runtime::Runtime) -> u8 {
    let _guard = runtime.enter();
    let engine = Engine::new(a.agent.common.paths(), a.agent.demo);
    let req = api::SessionRequest { session_id: a.session_id };
    let cmd = match runtime.block_on(api::session_attach(&engine, req)) {
        Ok(c) => c,
        Err(e) => return report_error(&a.agent, &e),
    };
    if a.agent.demo {
        eprintln!("demo: would run: {} {}", cmd.program, cmd.args.join(" "));
        return 0;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&cmd.program).args(&cmd.args).exec();
        eprintln!("error: couldn't start {}: {err}", cmd.program);
        1
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&cmd.program).args(&cmd.args).status() {
            Ok(status) => status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1),
            Err(err) => {
                eprintln!("error: couldn't start {}: {err}", cmd.program);
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
    println!("Add your machines, then run `muxin` to open the dashboard.");
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
    fn crash_log_goes_in_the_config_dir_asked_for() {
        let dir = std::env::temp_dir().join(format!("muxen-crash-dir-{}", std::process::id()));
        let cli = Cli::try_parse_from(["muxin", "machines", "--config-dir", dir.to_str().unwrap()]).unwrap();
        let Some(Command::Machines(a)) = cli.command else { panic!("not machines") };
        assert_eq!(a.common.paths().dir, dir);
        assert_eq!(crash::current_log(), Some(dir.join("crash.log")));
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
        let cli = Cli::try_parse_from(["muxin", "ws", "exec", "ws-abc123", "--env", "A=1", "--", "npm", "test"]).unwrap();
        match cli.command {
            Some(Command::Workspace(WorkspaceCommand::Exec(a))) => {
                assert_eq!(a.workspace_id, "ws-abc123");
                assert_eq!(a.command, vec!["npm", "test"]);
                assert_eq!(a.env, vec!["A=1"]);
            }
            _ => panic!("expected workspace exec"),
        }
        assert!(Cli::try_parse_from(["muxin", "workspace", "sync", "ws-1", "."]).is_err(), "needs --push or --pull");
        assert!(Cli::try_parse_from(["muxin", "workspace", "sync", "ws-1", ".", "--push", "--pull"]).is_err());
        assert!(Cli::try_parse_from(["muxin", "workspace", "create", "--platform", "linux", "--repo", "acme/api", "--ref", "main", "--json"]).is_ok());
    }

    #[test]
    fn cloud_login_reads_the_token_from_stdin() {
        let token = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Some(Command::Cloud(CloudCommand::Login(a))) => a.token,
            _ => panic!("expected cloud login"),
        };
        assert_eq!(token(&["muxin", "cloud", "login", "--token", "-"]).as_deref(), Some("-"));
        assert_eq!(token(&["muxin", "cloud", "login", "--token", "dop_v1_abc"]).as_deref(), Some("dop_v1_abc"));
        assert!(Cli::try_parse_from(["muxin", "cloud", "login", "--token", "-", "--doctl"]).is_err());

        let read = |input: &str| read_token(&mut std::io::Cursor::new(input.as_bytes()));
        assert_eq!(read("dop_v1_abc\n").as_deref(), Ok("dop_v1_abc"));
        assert_eq!(read("  dop_v1_abc\r\nmore\n").as_deref(), Ok("dop_v1_abc"), "only the first line, trimmed");
        assert!(read("").is_err());
        assert!(read("\n").is_err());
    }
}
