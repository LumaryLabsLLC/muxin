# Muxen

A fast, native dashboard for your own fleet of self-hosted GitHub Actions runners, built to be run by you **or by your AI agents**.

Point it at the machines you own (a Mac Studio under the desk, a NUC, a Linux box, a Windows laptop) and Muxen shows:

- which runners are working, idle or offline;
- what each machine's CPU, memory, disk and uptime look like;
- which jobs are queued, and which of them no runner can take.

It can also set the machines up. It installs runners, registers them with GitHub, and can start, stop, restart, inspect or remove them.

And it lets agents **borrow a machine as a work environment**: an [agent workspace](#agent-workspaces) is a persistent folder (optionally a git checkout) on one of your machines where an agent runs commands, edits files and syncs code.

- **Native and fast.** The window is built with [GPUI](https://www.gpui.rs), the GPU-accelerated UI framework behind the Zed editor. It runs on macOS, Linux and Windows.
- **Agent-first.** Every capability is available as an MCP server (`muxen mcp`) and as scriptable commands with `--json`. Claude Code, Codex and other agents can check the fleet, explain problems and fix them.
- **Uses what you already have.** Machines are reached with your system `ssh`, so your keys, agent and `~/.ssh/config` aliases just work. GitHub access uses your `gh` login.

## Install

```sh
git clone https://github.com/LumaryLabsLLC/openrunner && cd openrunner
cargo build --release
./target/release/muxen            # opens the window
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

To get a real app with the Muxen icon:

- **macOS:** `./scripts/bundle-macos.sh` builds `dist/Muxen.app` (ad-hoc signed). Drag it to Applications. Agents can still use the binary inside it: `Muxen.app/Contents/MacOS/muxen mcp`.
- **Linux:** install `assets/linux/muxen.desktop` and `assets/brand/icon-512.png` (as `muxen.png`).
- **Windows:** the icon is `assets/brand/muxen.ico`.

## Quick start

```sh
muxen --demo                       # explore a simulated fleet first; nothing real is touched
muxen                              # your fleet: starts with this computer in fleet.toml
muxen add-machine --name "Mac Studio" --host ci@mac-studio.local
muxen watch acme                   # live runner status and jobs for the acme org
muxen install mac-studio --count 2 # two runners, registered with acme, as services
muxen status
```

## The app

- **Overview:** a *fleet pulse* chart of how busy your online runners are, counts of working, idle, offline and queued, what needs attention (with a button that fixes or explains each item), and a card per machine with live CPU, memory and disk.
- **Machines and runners:** install runners on a machine, and start, stop, restart, show logs for or remove each one.
- **Agent sessions (⌘T):** a real terminal on any machine running Claude Code, Codex, OpenCode, Gemini CLI, Amp or a shell. Sessions are listed under their machine in the sidebar (a spinner while the agent works, a colored dot when it wants you) and keep running when you close the app.
- **Activity (⌘J):** one timeline of everything happening on the fleet: operations started in the app (with live output) and every command agents ran in workspaces, filterable by kind or failures.
- **Workspaces:** see what agents are doing on your machines (every command, its exit code and output), review a workspace's git changes, copy a shell command to step in, or destroy it.
- **Jobs:** queued and running self-hosted jobs, and which queued jobs no online runner can take.
- **Appearance:** follow the system, or pin light or dark, each with its own theme. Built-in themes:
  - dark: Muxen Graphite, macOS Dark, Gruvbox Dark, Catppuccin Mocha / Macchiato / Frappé, Tokyo Night, Nord, Rosé Pine, Solarized Dark, Dracula, Everforest Dark, Kanagawa Wave, GitHub Dark;
  - light: Muxen Paper, macOS Light, Gruvbox Light, Catppuccin Latte, Tokyo Night Day, Nord Light, Rosé Pine Dawn, Solarized Light, Everforest Light, Kanagawa Lotus, GitHub Light.

  The macOS themes also switch to the system fonts (SF Pro, and SF Mono when it's installed). The choice is saved in `ui.json` next to `fleet.toml`.

Keyboard: ⌘R check now · ⇧⌘R sync GitHub · ⌘N add machine · ⌘T new session · ⌘1 / ⌘2 overview / jobs · ⌘F or `/` search the fleet · ⌘J activity · ⌘[ / ⌘] back / forward · ⌘, settings. On Linux and Windows, use Ctrl instead of ⌘.

## The inventory: `fleet.toml`

Muxen keeps its files in `~/.config/muxen/`, or in `$MUXEN_HOME` if that's set. Run `muxen paths` to see where they are.

- `fleet.toml` lists your machines. You can edit it by hand. Muxen also edits it when you add machines or install runners, and keeps your comments when it does.
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

Watch an org or a repo in Settings, or with `muxen watch acme` (or `acme/api`). Muxen then reads live runner status (online, busy) and the queued and running jobs of the most recently pushed repos.

The token comes from `MUXEN_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`. Polling uses conditional requests, so unchanged responses don't count against your rate limit.

Reading and registering **org** runners needs an org admin token with the `admin:org` scope:

```sh
gh auth refresh -h github.com -s admin:org
```

Repo runners need admin access to the repo. For GitHub Enterprise Server, set the API URL in Settings or run `muxen settings --api-url https://HOST/api/v3`.

## Use it from AI agents

Muxen ships an [MCP](https://modelcontextprotocol.io) server. Print setup snippets that point at your binary:

```sh
muxen mcp --print-config
```

**Claude Code**

```sh
claude mcp add muxen -- /path/to/muxen mcp
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.muxen]
command = "/path/to/muxen"
args = ["mcp"]
```

**Other MCP clients** (Claude Desktop, Cursor, …)

```json
{ "mcpServers": { "muxen": { "command": "/path/to/muxen", "args": ["mcp"] } } }
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
muxen status [--refresh]                 # overview + what needs attention
muxen machines | machine <id>
muxen runners [--machine <id>] [--state offline]
muxen jobs
muxen add-machine --name <name> --host <ssh-host> [--no-check]
muxen remove-machine <id>
muxen install <machine-id> [--scope acme] [--count N | --name a --name b] [--label gpu] [--mode auto|service|background|docker] [--image IMAGE] [--docker-socket] [--no-wait]
muxen runner <start|stop|restart|logs|remove> <runner-id> [--delete-files]
muxen watch <org|owner/repo>... / unwatch ...
muxen settings [--poll 20] [--scan-repos 10] [--check-interval 15]
muxen ssh-hosts
muxen check [--json]                     # one-shot full report
muxen workspace …                        # agent workspaces, see below (alias: ws)
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

Every command an agent runs is recorded with its exit code, duration and the end of its output. You can see that history in the app's Workspaces page, or open a terminal in any workspace with `muxen workspace shell <id>`.

```sh
muxen ws create --platform linux --repo acme/api --ref main
muxen ws create --docker --image node:22 --repo acme/web   # an isolated container
muxen ws exec ws-7f3a2c -- cargo test          # exits with the command's exit code
muxen ws exec ws-7f3a2c --background -- npm run dev
muxen ws cat ws-7f3a2c .muxen/logs/1.log
echo 'hello' | muxen ws write ws-7f3a2c notes.txt
muxen ws files ws-7f3a2c src --depth 1
muxen ws changes ws-7f3a2c
muxen ws sync ws-7f3a2c ~/code/api --push      # or --pull, add --delete to mirror deletions
muxen ws shell ws-7f3a2c                       # an interactive shell in the workspace, for you
muxen ws ls
muxen ws rm ws-7f3a2c [--keep-files]
muxen ws allow studionas --off                 # forbid workspaces on a machine
```

`ws exec` passes the command's stdout and stderr through and exits with its exit code (124 if it timed out), so it composes in scripts. With `--json` it prints the full result instead.

**Safety.** Host workspace commands run as the ssh user on your machine, with that user's permissions; they aren't sandboxed. Use Docker isolation for code you don't trust. Workspaces are on by default for every machine with a host. Turn them off per machine with `workspaces = false` in `fleet.toml`, the toggle on the machine's page in the app, or `muxen workspace allow <machine> --off`. Nothing is deleted automatically: workspaces stay until you or an agent destroys them. Folders live under `~/muxen/workspaces/<id>` on each machine (`workspace_root` in `fleet.toml` changes that).

## Your GitHub, and handing issues to agents

The GitHub page is built for the loop of picking an issue, handing it to an agent, and moving on:

- **For you:** pull requests waiting for your review, issues assigned to you, and your open pull requests.
- **Repositories:** everything you own, collaborate on or reach through your orgs, with each one's open issues, pull requests and GitHub Actions runs. Star the ones you work on to keep them at the top, and show the list as one list or grouped by owner (you, then your organizations, then repositories other people shared with you).
- **On your machines:** each machine's agent finds git checkouts in your code folders (`~/Developer`, `~/code`, `~/src`, … set in `settings.json` as `codeFolders`) and links them to their GitHub repository, with branch, commits ahead/behind and uncommitted changes. Fetch, Pull and Push from there.
- **Fix with agent:** on any issue (or **Review with agent** on a pull request), one click opens a new session in that repository's checkout on the machine you pick (this computer first), with the issue's link, title and description as the agent's first message. Right-click an issue or pull request for **Fix in a new workspace…**: pick any machine and agent, and Muxen clones the repository there (an issue gets its own branch, like `issue-910-content-excalidraw`; a pull request checks out its branch, forks included), then starts the agent in it.

Muxen uses the GitHub CLI's login (`gh auth login`) or `GITHUB_TOKEN`. Settings has a **Sign in with GitHub** button that runs the CLI's browser sign-in; each machine's page shows whether its own `gh` is signed in (agents there use it for issues and pull requests) and can sign it in too.

**Git on each machine.** A machine's page has a Git card: whether its GitHub CLI is signed in, who commits there, and how pushes reach GitHub. When something's missing, one click fixes it: **Use my GitHub identity** sets git's name and email to your GitHub name and private noreply address, and **Use the GitHub CLI** runs `gh auth setup-git` so pushes use the machine's `gh` login. Agents working there can then commit and push.

```sh
muxen git status beelink
muxen git setup beelink --identity --credentials
```

**Permissions.** Watching an organization's runners needs the `admin:org` permission on your GitHub login. Settings shows when it's missing, with a **Grant access** button that runs `gh auth refresh -s admin:org` in a terminal (it finishes in your browser), and lists your organizations so watching one is a click.

**More than one account.** Click your name at the top of the GitHub page to switch. The menu lists every account the GitHub CLI here is signed in to, plus each machine's own `gh` login: with a work laptop signed in to your work account, pick "@you-at-work · Work MacBook" and Muxen sees that account's repositories, inbox and runners (the token is fetched over ssh when needed and kept in memory only). Fix with agent then prefers checkouts on that machine. Switching doesn't change which account `gh` uses in your terminal. "Add another account…" runs `gh auth login`.

```sh
muxen github inbox                         # what needs you
muxen github repos                         # favorites first (★)
muxen github favorite ZenNotes/zennotes    # --remove to unstar
muxen github accounts
muxen github use work-login                # or: --machine work-macbook
muxen github repo ZenNotes/zennotes        # issues, PRs, runs, and where it's checked out
muxen github issue ZenNotes/zennotes#632   # or the issue's URL
muxen checkouts --repo ZenNotes/zennotes
muxen checkouts --machine beelink --path ~/code/api --git pull
```

## Agent sessions

Start an AI agent on one of your machines and come back to it whenever you like, from the app, from your own terminal, or through another agent.

- **Keeps running.** Sessions live in Muxen's agent on the machine (`~/.muxen/bin/muxen-agent`), a small background program that holds each session's terminal, the way tmux would. Closing the app, sleeping your laptop or losing the network only detaches; the agent keeps working until you end the session.
- **Nothing to install.** When Muxen connects to a machine it uploads the agent build for that OS and CPU (macOS and Linux, x86_64 and arm64; static binaries, about 1 MB) and keeps it up to date. Windows machines run sessions in the app's terminal for now, so they end with the window.
- **A real terminal in the app.** Everything goes to the agent: Esc, Ctrl-C, Tab, arrows, Option/Shift+Enter for newlines. ⌘C copies a selection, ⌘V pastes, the wheel scrolls back. Right-click a machine for a new terminal or agent session.
- **Your environment.** The agent starts in your login shell, in the folder you pick, so nvm/fnm/asdf and the CLI's own login work as they do in your terminal. Muxen finds which agent CLIs each machine has: `claude`, `codex`, `opencode`, `gemini`, `amp`.
- **Agents can drive agents.** `session_start`, `session_read` (the screen as text), `session_send` (type into it) and `session_stop` are MCP tools and CLI commands too.

```sh
muxen session ls                                   # every session, and which agents each machine has
muxen session start beelink claude --cwd ~/code/api --prompt "fix the failing test"
muxen session read beelink/claude-3f9a             # what's on its screen
muxen session send beelink/claude-3f9a "yes"       # type, then Enter (--no-enter to skip it)
muxen session attach beelink/claude-3f9a           # open it in this terminal; detach with Ctrl-] then d
muxen session restart beelink/claude-3f9a
muxen session stop beelink/claude-3f9a
```

Build the agent for every platform with `scripts/build-agents.sh` (needs `cargo-zigbuild` and Zig for the Linux builds); `scripts/bundle-macos.sh` runs it and ships the builds inside the app.

## Connections and desktops

Muxen first reaches a machine with `ssh`, installs its small agent there, and from then on talks to that agent over **one live connection** instead of a new ssh connection per request: requests take milliseconds instead of ~0.4 s, terminals open instantly, and the sidebar updates the moment a session changes.

- **Over ssh** (the default): one ssh connection per machine, kept open and reconnected when it drops.
- **Direct**: on a machine's page, **Connect directly** pairs the app with its agent (each trusts only the other's certificate, swapped over ssh) and runs the agent's listener as a user service there (systemd or launchd) on UDP port 47821. Muxen then connects over QUIC, falling back to ssh whenever the direct connection is down.
- `MUXEN_NO_LINK=1` turns all of this off: one ssh connection per request, as before.

**Desktop.** On a Linux machine with an X11 desktop, **Show desktop** (on its page, or its right-click menu) shows its screen live and sends your mouse and keyboard to it; ⌘ works as Ctrl there.

- Pick 720p, 1080p, 1440p, 4K or Native from the bar above it. On a local network it starts at the screen's own resolution, further away at 1080p.
- **Open in window** gives it a window of its own (put it on another screen, or full screen).
- On an agent session's page, **Desktop** shows the machine's screen beside the terminal (drag the divider to resize), to check what the agent did without leaving it.
- The bar says how it travels: direct or over ssh, and on which network (local network, Tailscale, internet). Paired machines are reached on their local-network address first, so a machine at home isn't reached through a VPN.
- Only changed parts of the screen are sent (captured through shared memory, compressed on all of the machine's cores), so a still screen costs nothing.

```sh
muxen connection beelink                  # how it's reached, and how fast
muxen connection beelink --direct on      # pair and connect directly
muxen desktop shot beelink -o screen.jpg
muxen desktop input beelink '[{"type":"click","x":100,"y":200},{"type":"text","text":"hello"}]'
```

AI agents get the same: `machine_connection`, `direct_connection`, `desktop_screenshot` (an image they can look at) and `desktop_input`.

## How checks and installs work

- **Checks are read-only.** A small script is sent over `ssh` (`sh` on macOS and Linux, PowerShell on Windows), or run directly for `host = "local"`. It reports CPU, memory, disk, uptime and load, and finds the installed runners, their versions, their services and whether they're running. Checks run on a schedule (every 15 minutes by default) or on request.
- **Installs only happen when you ask.** Muxen checks the machine, gets a short-lived registration token from GitHub, and installs each runner in one of these modes (`--mode` / `mode`, default `auto`):
  - **service:** downloads the latest `actions-runner` for the machine's platform into `~/actions-runner/<name>`, runs `config.sh --unattended`, and installs a service: launchd on macOS (the user must be logged in), systemd on Linux (needs **passwordless sudo** for `svc.sh`), a Windows service on Windows.
  - **docker:** one container per runner, `muxen-<name>`, from GitHub's official image (`ghcr.io/actions/actions-runner:<latest runner version>`, or `--image`). The runner's files live in a named volume of the same name, registration happens in a throwaway container that reads the token from stdin (so it never shows up in `docker inspect` or a process list), and the container runs with `--restart unless-stopped`, so it comes back with the Docker daemon. No sudo needed; the user just has to be in the `docker` group. The runners get the `docker` label, and jobs run in a clean Ubuntu userland. `--docker-socket` shares the host's Docker socket so jobs can use Docker; that's root-equivalent access to the machine, so only use it for trusted workflows.
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

Copyright © 2026 Adib Hanna. All rights reserved. Muxen is proprietary software; see `LICENSE`.

It includes third-party components under their own licenses, listed in `THIRD_PARTY_NOTICES.md`: the Geist fonts (SIL Open Font License, `assets/fonts/OFL.txt`), the Lucide icons (ISC, `assets/icons/LICENSE.txt`), GPUI (Apache-2.0) and the Rust crates in `Cargo.lock`.
