# OpenRunner

A fast, native dashboard for your own fleet of self-hosted GitHub Actions runners, built to be run by you **or by your AI agents**.

Point it at the machines you own (a Mac Studio under the desk, a NUC, a Linux box, a Windows laptop) and OpenRunner shows:

- which runners are working, idle or offline;
- what each machine's CPU, memory, disk and uptime look like;
- which jobs are queued, and which of them no runner can take.

It can also set the machines up. It installs runners, registers them with GitHub, and can start, stop, restart, inspect or remove them.

And it lets agents **borrow a machine as a work environment**: an [agent workspace](#agent-workspaces) is a persistent folder (optionally a git checkout) on one of your machines where an agent runs commands, edits files and syncs code.

- **Native and fast.** The window is built with [GPUI](https://www.gpui.rs), the GPU-accelerated UI framework behind the Zed editor. It runs on macOS, Linux and Windows.
- **Agent-first.** Every capability is available as an MCP server (`openrunner mcp`) and as scriptable commands with `--json`. Claude Code, Codex and other agents can check the fleet, explain problems and fix them.
- **Uses what you already have.** Machines are reached with your system `ssh`, so your keys, agent and `~/.ssh/config` aliases just work. GitHub access uses your `gh` login.

## Install

```sh
git clone https://github.com/adibhanna/openrunner && cd openrunner
cargo build --release
./target/release/openrunner            # opens the window
```

Requirements:

- A recent stable Rust (edition 2024).
- On macOS, Xcode (GPUI compiles Metal shaders).
- On Linux, GPUI's system libraries. On Ubuntu/Debian:
  `sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libfontconfig-dev libx11-xcb-dev libxcb1-dev`.

For servers, CI boxes and agent machines there's a headless build with no GUI libraries: just the CLI and the MCP server.

```sh
cargo build --release --no-default-features
```

There's nothing else to set up. Fonts and icons are compiled into the binary.

To get a real app with the OpenRunner icon:

- **macOS:** `./scripts/bundle-macos.sh` builds `dist/OpenRunner.app` (ad-hoc signed). Drag it to Applications. Agents can still use the binary inside it: `OpenRunner.app/Contents/MacOS/openrunner mcp`.
- **Linux:** install `assets/linux/openrunner.desktop` and `assets/brand/icon-512.png` (as `openrunner.png`).
- **Windows:** the icon is `assets/brand/openrunner.ico`.

## Quick start

```sh
openrunner --demo                       # explore a simulated fleet first; nothing real is touched
openrunner                              # your fleet: starts with this computer in fleet.toml
openrunner add-machine --name "Mac Studio" --host ci@mac-studio.local
openrunner watch acme                   # live runner status and jobs for the acme org
openrunner install mac-studio --count 2 # two runners, registered with acme, as services
openrunner status
```

## The app

- **Overview:** a *fleet pulse* chart of how busy your online runners are, counts of working, idle, offline and queued, what needs attention (with a button that fixes or explains each item), and a card per machine with live CPU, memory and disk.
- **Machines and runners:** install runners on a machine, and start, stop, restart, show logs for or remove each one.
- **Activity (⌘J):** one timeline of everything happening on the fleet: operations started in the app (with live output) and every command agents ran in workspaces, filterable by kind or failures.
- **Workspaces:** see what agents are doing on your machines (every command, its exit code and output), review a workspace's git changes, copy a shell command to step in, or destroy it.
- **Jobs:** queued and running self-hosted jobs, and which queued jobs no online runner can take.
- **Appearance:** follow the system, or pin light or dark, each with its own theme. Built-in themes:
  - dark: OpenRunner Graphite, macOS Dark, Gruvbox Dark, Catppuccin Mocha / Macchiato / Frappé, Tokyo Night, Nord, Rosé Pine, Solarized Dark, Dracula, Everforest Dark, Kanagawa Wave, GitHub Dark;
  - light: OpenRunner Paper, macOS Light, Gruvbox Light, Catppuccin Latte, Tokyo Night Day, Nord Light, Rosé Pine Dawn, Solarized Light, Everforest Light, Kanagawa Lotus, GitHub Light.

  The macOS themes also switch to the system fonts (SF Pro, and SF Mono when it's installed). The choice is saved in `ui.json` next to `fleet.toml`.

Keyboard: ⌘R check now · ⇧⌘R sync GitHub · ⌘N add machine · ⌘1 / ⌘2 overview / jobs · ⌘F or `/` search the fleet · ⌘J activity · ⌘[ / ⌘] back / forward · ⌘, settings. On Linux and Windows, use Ctrl instead of ⌘.

## The inventory: `fleet.toml`

OpenRunner keeps its files in `~/.config/openrunner/`, or in `$OPENRUNNER_HOME` if that's set. Run `openrunner paths` to see where they are.

- `fleet.toml` lists your machines. You can edit it by hand. OpenRunner also edits it when you add machines or install runners, and keeps your comments when it does.
- `settings.json` holds the watched GitHub orgs and repos, and the polling intervals.
- `state.json` holds the last machine check.

```toml
[[machine]]
name = "Mac Studio"
host = "ci@mac-studio.local"      # anything `ssh` accepts, or "local" for this computer
runners = ["mac-studio-01", "mac-studio-02"]   # expected runners; missing ones are flagged
# runner_dirs = ["/Volumes/Fast/runners/*"]   # extra places to look (globs)
# shell = "powershell"                          # Windows; auto-detected when omitted
# user = "ci"   port = 2222   ssh_options = ["-i", "~/.ssh/ci"]
# discover = false                              # don't list runners found but not declared
# notes = "Under the desk"

[[machine]]
name = "StudioNAS"   # no runners: shown under "Other machines"
host = "nas"
```

Runners installed in the usual places are discovered automatically: `~/actions-runner*`, `~/actions-runner/*`, `~/runners/*`, `/opt/actions-runner*` and `C:\actions-runner\*`. A runner is found from its `.runner` file and its running `Runner.Listener` / `Runner.Worker` processes.

## GitHub

Watch an org or a repo in Settings, or with `openrunner watch acme` (or `acme/api`). OpenRunner then reads live runner status (online, busy) and the queued and running jobs of the most recently pushed repos.

The token comes from `OPENRUNNER_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`. Polling uses conditional requests, so unchanged responses don't count against your rate limit.

Reading and registering **org** runners needs an org admin token with the `admin:org` scope:

```sh
gh auth refresh -h github.com -s admin:org
```

Repo runners need admin access to the repo. For GitHub Enterprise Server, set the API URL in Settings or run `openrunner settings --api-url https://HOST/api/v3`.

## Use it from AI agents

OpenRunner ships an [MCP](https://modelcontextprotocol.io) server. Print setup snippets that point at your binary:

```sh
openrunner mcp --print-config
```

**Claude Code**

```sh
claude mcp add openrunner -- /path/to/openrunner mcp
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.openrunner]
command = "/path/to/openrunner"
args = ["mcp"]
```

**Other MCP clients** (Claude Desktop, Cursor, …)

```json
{ "mcpServers": { "openrunner": { "command": "/path/to/openrunner", "args": ["mcp"] } } }
```

Add `"--demo"` after `"mcp"` to let an agent practice on the simulated fleet.

The MCP server has these tools:

| Tool | What it does |
| --- | --- |
| `fleet_status` | Counts, GitHub state, every machine, and the `attention` list. Start here. |
| `get_machine` | One machine's resources, runners and diagnostics, with hints. |
| `list_runners` | Runners, optionally filtered by machine or state. |
| `list_jobs` | Queued and running jobs, including which ones no runner can take. |
| `check_machines` | Runs the read-only machine check over ssh now. |
| `add_machine`, `remove_machine` | Edit `fleet.toml`. Nothing on the machine changes. |
| `install_runners` | Downloads, registers and starts runners on a machine. |
| `runner_action` | Starts, stops, restarts or removes a runner, or reads its logs. |
| `get_operation`, `list_operations` | Progress and output of long operations. |
| `github_scopes` | Watches or unwatches orgs and repos, with access checks. |
| `get_settings`, `update_settings`, `list_ssh_hosts` | Settings, and the hosts from your `~/.ssh/config`. |
| `workspace_create`, `workspace_list`, `workspace_get` | Check out a machine as a work environment; see what exists. |
| `workspace_exec` | Runs a command in a workspace (or starts it in the background). |
| `workspace_read_file`, `workspace_write_file`, `workspace_list_files` | Work with a workspace's files. |
| `workspace_changes`, `workspace_sync` | Review the git diff; rsync code to or from your local checkout. |
| `workspace_destroy`, `workspace_allow` | Clean up; turn workspaces on or off for a machine. |

Every tool returns structured JSON. Failures come back with a `code` an agent can branch on:

- `not_found`
- `invalid`
- `busy`
- `github`
- `unreachable`
- `operation_failed`
- `internal`

Many failures also include a `hint` with the exact command that fixes them. [AGENTS.md](AGENTS.md) has guidance written for agents.

### The CLI, for scripts and agents without MCP

Every command takes `--json`. In that mode stdout carries JSON only, and errors are printed as `{"error": {...}}`. Progress and logs go to stderr. Exit codes:

- `0`: success.
- `1`: the operation failed.
- `2`: bad input, or the thing wasn't found.

```sh
openrunner status [--refresh]                 # overview + what needs attention
openrunner machines | machine <id>
openrunner runners [--machine <id>] [--state offline]
openrunner jobs
openrunner add-machine --name <name> --host <ssh-host> [--no-check]
openrunner remove-machine <id>
openrunner install <machine-id> [--scope acme] [--count N | --name a --name b] [--label gpu] [--mode auto|service|background|docker] [--image IMAGE] [--docker-socket] [--no-wait]
openrunner runner <start|stop|restart|logs|remove> <runner-id> [--delete-files]
openrunner watch <org|owner/repo>... / unwatch ...
openrunner settings [--poll 20] [--scan-repos 10] [--check-interval 15]
openrunner ssh-hosts
openrunner check [--json]                     # one-shot full report
openrunner workspace …                        # agent workspaces, see below (alias: ws)
```

Ids:

- Machines are slugs of their names, such as `mac-studio`.
- Runners are `<machine-id>/<runner-name>`, such as `mac-studio/mac-studio-01`.
- A runner that GitHub knows but no machine lists is `github/<name>`.

Every command also accepts `--demo`.

The window, the CLI and MCP servers can all run at once. They share the files in the config directory and pick up each other's changes: settings, edits to `fleet.toml`, and newer machine checks.

## Agent workspaces

Think of [Amp's Orbs](https://ampcode.com/news/agents-in-orbs), but on your own hardware. An agent asks for a workspace and gets a persistent folder on one of your machines, optionally with a repo cloned at a branch or ref. It then works there like it would on a remote dev box:

- **Run commands** with your login environment (Homebrew, nvm, asdf and the rest of your `PATH`). Long-running things such as dev servers go in the background, with their output in a log file inside the workspace.
- **Read, write and list files.**
- **Review** with `git status` and the diff.
- **Sync code** both ways with your local checkout over rsync: push uncommitted local changes to test them on Linux, pull a fix back.

**Host or Docker.** By default commands run directly on the machine. With `isolation: "docker"` (CLI `--docker`), each workspace gets its own fresh container instead, from an image the agent picks (`node:22`, `python:3.13`, `rust:1`, …; default `buildpack-deps:bookworm`). The workspace folder is mounted at `/workspace`, the container runs as the ssh user so files aren't root-owned, and a container that disappears (reboot, `docker system prune`) is recreated on the next command with the files intact. `docker_socket` shares the machine's Docker for docker-in-docker; that's root-equivalent, so only use it when the task needs it. Docker workspaces need Docker on the machine and the ssh user in the `docker` group. They aren't available on Windows machines yet.

The machine can be named, or picked for you: the least-loaded online machine that allows workspaces, optionally matching a platform such as `linux` or `darwin-arm64`. Private GitHub repos are cloned with your `gh` login; the token is passed through the environment for that one command and never written to disk or logs.

Every command an agent runs is recorded with its exit code, duration and the end of its output. You can see that history in the app's Workspaces page, or open a terminal in any workspace with `openrunner workspace shell <id>`.

```sh
openrunner ws create --platform linux --repo acme/api --ref main
openrunner ws create --docker --image node:22 --repo acme/web   # an isolated container
openrunner ws exec ws-7f3a2c -- cargo test          # exits with the command's exit code
openrunner ws exec ws-7f3a2c --background -- npm run dev
openrunner ws cat ws-7f3a2c .openrunner/logs/1.log
echo 'hello' | openrunner ws write ws-7f3a2c notes.txt
openrunner ws files ws-7f3a2c src --depth 1
openrunner ws changes ws-7f3a2c
openrunner ws sync ws-7f3a2c ~/code/api --push      # or --pull, add --delete to mirror deletions
openrunner ws shell ws-7f3a2c                       # an interactive shell in the workspace, for you
openrunner ws ls
openrunner ws rm ws-7f3a2c [--keep-files]
openrunner ws allow studionas --off                 # forbid workspaces on a machine
```

`ws exec` passes the command's stdout and stderr through and exits with its exit code (124 if it timed out), so it composes in scripts. With `--json` it prints the full result instead.

**Safety.** Host workspace commands run as the ssh user on your machine, with that user's permissions; they aren't sandboxed. Use Docker isolation for code you don't trust. Workspaces are on by default for every machine with a host. Turn them off per machine with `workspaces = false` in `fleet.toml`, the toggle on the machine's page in the app, or `openrunner workspace allow <machine> --off`. Nothing is deleted automatically: workspaces stay until you or an agent destroys them. Folders live under `~/openrunner/workspaces/<id>` on each machine (`workspace_root` in `fleet.toml` changes that).

## How checks and installs work

- **Checks are read-only.** A small script is sent over `ssh` (`sh` on macOS and Linux, PowerShell on Windows), or run directly for `host = "local"`. It reports CPU, memory, disk, uptime and load, and finds the installed runners, their versions, their services and whether they're running. Checks run on a schedule (every 15 minutes by default) or on request.
- **Installs only happen when you ask.** OpenRunner checks the machine, gets a short-lived registration token from GitHub, and installs each runner in one of these modes (`--mode` / `mode`, default `auto`):
  - **service:** downloads the latest `actions-runner` for the machine's platform into `~/actions-runner/<name>`, runs `config.sh --unattended`, and installs a service: launchd on macOS (the user must be logged in), systemd on Linux (needs **passwordless sudo** for `svc.sh`), a Windows service on Windows.
  - **docker:** one container per runner, `openrunner-<name>`, from GitHub's official image (`ghcr.io/actions/actions-runner:<latest runner version>`, or `--image`). The runner's files live in a named volume of the same name, registration happens in a throwaway container that reads the token from stdin (so it never shows up in `docker inspect` or a process list), and the container runs with `--restart unless-stopped`, so it comes back with the Docker daemon. No sudo needed; the user just has to be in the `docker` group. The runners get the `docker` label, and jobs run in a clean Ubuntu userland. `--docker-socket` shares the host's Docker socket so jobs can use Docker; that's root-equivalent access to the machine, so only use it for trusted workflows.
  - **background:** `run.sh` started with `nohup`; no root needed, but it doesn't come back after a reboot (`--no-service` is the same).
  - **auto** picks service when the machine allows it (macOS, Windows, Linux with passwordless sudo or as root), else docker when Docker is usable, else background, and says which it picked in the operation log.
- **Remove** stops the runner, uninstalls its service (or removes its container) and deregisters it with a removal token. It deletes the folder (or the container's volume) only if you ask.
- Docker runners show up in checks with their container's state; a container that keeps restarting is flagged with the `docker logs` command to look at.
- Registration and removal tokens are masked in all output and never logged.
- `ssh` runs with `BatchMode=yes`, so a machine that needs a password just shows up as unreachable. Make sure `ssh <host>` works with keys first.

## Develop

```sh
cargo test                 # unit + demo-mode integration tests
cargo run -- --demo        # the window, simulated fleet
cargo run -- status --demo --json
```

Source layout:

- `src/fleet`: the engine and snapshot model.
- `src/probe`: machine checks.
- `src/github`: the GitHub API client.
- `src/ops`: installs and runner control.
- `src/fleet/workspaces/`: agent workspaces (host and Docker).
- `src/api.rs`: the agent API.
- `src/mcp.rs`: the MCP server.
- `src/ui`: the GPUI window.

`Cargo.lock` pins `libc` to 0.2.189: GPUI's HTTP client pulls in `xattr` 0.2, which doesn't build on Linux against newer `libc` releases. Don't run a blanket `cargo update` without rebuilding on Linux.

## License

Copyright © 2026 Adib Hanna. All rights reserved. OpenRunner is proprietary software; see `LICENSE`.

It includes third-party components under their own licenses, listed in `THIRD_PARTY_NOTICES.md`: the Geist fonts (SIL Open Font License, `assets/fonts/OFL.txt`), the Lucide icons (ISC, `assets/icons/LICENSE.txt`), GPUI (Apache-2.0) and the Rust crates in `Cargo.lock`.
