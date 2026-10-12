# Working with Muxen as an agent

Muxen is one app for all of the user's machines (reached over `ssh`, or directly for `host = "local"`): agent sessions, desktops, files, GitHub, workspaces, cloud machines, and the self-hosted GitHub Actions runners installed on them.

Use the MCP tools when they're connected: start the server with `muxen mcp`, and see the README for setup. Otherwise use the CLI with `--json`. Both front the same API and return the same shapes.

## Ground rules

1. **Start with `status`** (`muxen status --json`) and read `attention` first. It lists what's wrong, most severe first, with the `machine_id` or `runner_id` each item is about.
2. **The state may be stale.** `status` uses the last machine check. Look at `last_check_at` and each machine's `checked_at`. Call `check_machines` (or `status` with `refresh: true`) when you need it fresh. Checks are read-only.
3. **Confirm before changing real things.** `install_runners` and `runner_action` with `stop`, `restart` or `remove` run commands on the user's machines and register or deregister runners on GitHub. Do them when the user asked for exactly that; otherwise say what you'd do and ask. `add_machine`, `remove_machine`, `rename_machine` and `connect_machine` only edit `config.toml`.
4. **Use the hints.** Errors have a `code`:
   - `not_found`
   - `invalid`
   - `busy`
   - `github`
   - `unreachable`
   - `operation_failed`
   - `internal`

   Many errors also have a `hint`, often a command such as `gh auth refresh -h github.com -s admin:org`. Pass hints to the user verbatim; don't improvise around permission problems.
5. **Long operations wait by default** and return a `log_tail`. With `wait: false` you get an `operation_id`; poll `get_operation` until `status` is no longer `running`.

## Ids

- **Machine:** a slug of its name, such as `mac-studio`. The display name is accepted too.
- **Runner:** `<machine-id>/<runner-name>`, such as `mac-studio/mac-studio-01`. A runner that GitHub knows but no machine lists is `github/<name>`. A bare runner name works when it's unique.
- **Runner state:** `working`, `idle`, `offline` or `unknown`. `source` says where the state came from:
  - `github`: live;
  - `probe`: the last machine check;
  - `none`: no information yet.

## Common requests

**"Add my server and put two runners on it."**

1. Call `list_ssh_hosts` to find the user's alias, or ask for `user@host`. Its `tailnet` list has the computers on the user's Tailscale network, each with the `host` to add it by; one added that way is reachable from anywhere. Its `network` list has computers on the local network that aren't in Muxen yet; `ssh: false` is a Mac with Remote Login off, which the user turns on in System Settings › General › Sharing before it can be added.
2. Call `add_machine(name, host)`. Its result includes the first check. If the status is `offline`, the machine isn't reachable with key-based ssh. Show the error and stop.
3. Make sure a GitHub scope is watched (`status.github.watching`). If none is, ask which org or repo to use and call `github_scopes(add: [...])`. A failure there usually needs `admin:org`; show the hint.
4. Call `install_runners(machine_id, count: 2)`. Pass `scope` if more than one org or repo is watched. The default `mode: "auto"` picks a service where possible, else Docker containers (Linux without passwordless sudo), else a background run.sh; the log says which. Use `mode: "docker"` when the user wants isolated runners; only pass `docker_socket: true` if they explicitly want jobs to use Docker and trust the workflows (it's root-equivalent on the host).
5. Report the new runner ids and their states from the result.

**"Why is runner X offline?"**

1. Call `get_machine` on its machine. Check the machine's `status` first: an unreachable machine explains everything on it. When it's `offline`, `unreachable` says why in plain words. `kind: "offline"` means it didn't answer (shut down, asleep, off the network): there's nothing to fix, so tell the user it's off and when it was `last_seen_at`. Other kinds (`refused`, `auth`, `unknown-host`, `host-key`) come with a `hint` to pass on. Then check the runner's `state`, `source`, `service_state`, and the diagnostics with its `runner_id`.
2. Call `runner_action(X, "logs")` to read its newest `_diag` log.
3. Explain the cause and propose a fix, such as `runner_action(X, "start")`. Do it only if the user agrees or already asked.

**"Restart everything on machine Y."**

1. Call `get_machine(Y)` to get its runner ids.
2. Call `runner_action(id, "restart")` for each, one at a time. A machine accepts one changing operation at a time; `busy` means wait and retry.
3. Summarize the final states.

**"Why isn't my job starting?"**

1. Call `list_jobs`. A queued job with `matching_online: 0` has no online runner with all of its labels.
2. Compare the job's labels with the runners' labels (`list_runners`). The fix is usually to start an offline runner, or to install one with the missing label (`install_runners(labels: [...])`).
3. To stop a run that shouldn't go on, `cancel_run(job_id)` cancels the job's whole workflow run (GitHub cancels runs, not single jobs). Confirm with the user first.

## Workspaces: borrowing a machine to work on

A workspace is a persistent folder on one of the user's machines, optionally a git checkout, that you use as your own work environment. It's like a remote dev box: `workspace_exec` runs shell commands there with the machine user's login environment. Ids look like `ws-7f3a2c`.

- **Reuse before you create.** Call `workspace_list` first. Use one workspace per task and keep using it across steps.
- **Destroy when you're done.** Nothing is deleted automatically. Pull or push anything worth keeping first.
- **A non-zero `exit_code` is a normal result,** not a tool error. Read `stderr`.
- **Long-running things go in the background.** Use `background: true` for servers and watchers, then read the returned `log_path` with `workspace_read_file`.
- **It's a real machine.** Commands run as the user, not in a sandbox. Don't do anything outside the workspace that you wouldn't do in their terminal without asking.

**"Run the test suite on Linux."**

1. Call `workspace_create(platform: "linux", repo: "<owner/repo>", ref: "<branch>")`. Omit `machine_id` to get the least-loaded Linux machine.
2. Install dependencies and run the tests with `workspace_exec`, e.g. `npm ci && npm test`. Raise `timeout_secs` if the suite is slow (max 3600).
3. Report the exit code and the failing tests from `stdout`/`stderr`, then `workspace_destroy` unless the user wants to keep it.

**"Start a dev server and check it."**

1. In an existing workspace, call `workspace_exec(command: "npm run dev", background: true)`. Note the `log_path`.
2. Wait a moment, then read the log with `workspace_read_file(path: <log_path>)` and probe the server with `workspace_exec(command: "curl -sS localhost:3000/health")`.
3. Background processes stop when the workspace is destroyed.

**"Push my local changes, run the tests there, and pull the fix back."**

1. Create a workspace on the target machine (no `repo`, or the same repo so `.git` exists), then call `workspace_sync(local_path: "<absolute path to the user's checkout>", direction: "push")`.
2. Run the tests with `workspace_exec`. Fix things with `workspace_write_file` or `workspace_exec`, and review with `workspace_changes`.
3. Confirm with the user, then `workspace_sync(direction: "pull")`. Only pass `delete: true` when they want deletions mirrored into their checkout.

The CLI equivalents are `muxen workspace create|ls|show|exec|cat|write|files|changes|sync|rm|allow`, aliased as `muxen ws`. `ws exec` exits with the command's own exit code.

## Agent sessions: other agents running on the user's machines

The user runs AI agent CLIs (Claude Code, Codex, OpenCode, Gemini CLI, Amp) or shells in persistent sessions on their machines (held by Muxen's agent there) and watches them in the app. Ids look like `beelink/claude-3f9a`.

- `session_list` shows every session (agent, folder, `status`, `busy` = printed output in the last few seconds) and which agent CLIs each machine has.
- `session_read` returns the screen and recent scrollback as text: use it to see what an agent is doing or asking.
- `session_question` says what a waiting session asks: the question and its `options`, each with a `key` (and the `selected` one), or, when it's simply done, the end of what it said (`options` empty). `session_answer(id, key)` picks an option: it approves or refuses what that agent asked, so only when the user told you to, and say which option.
- `session_send` types text and presses Enter. `session_start` starts one (optionally with a `prompt`); `session_stop` ends it. `session_rename` changes its title (the agent on the machine keeps it; an empty title goes back to the original).
- These are the user's sessions. Start, type into or stop one only when they asked; reading is fine.

The CLI equivalents are `muxen session ls|start|read|question|answer|send|attach|restart|rename|stop`.

## Usage and cost

- `agent_usage(days?, machine_id?)` says what Claude Code and Codex used over the last N days (default 7), from their own logs on each machine: tokens (input, output, cache reads and writes) and requests by model and machine, and the cost at Anthropic's and OpenAI's API list prices (`null` for a model whose price isn't known). With a Claude or ChatGPT plan the user doesn't pay per token: say it's what the same use would cost through the API. Read-only.
- The CLI equivalent is `muxen usage [--days N] [--machine X]`.

## GitHub and the user's code

- `github_inbox` shows what needs the user: review requests, assigned issues, their open PRs. `github_repos` lists their repositories, favorites first (`favorite: true`: the ones they work on most). `github_favorite` stars or unstars one; only when asked.
- Muxen uses one GitHub account at a time. `github_accounts` shows which (`using`) and the alternatives: accounts the GitHub CLI here is signed in to, and other machines' `gh` logins. `github_use_account` switches (only when asked); it changes whose repositories and runners Muxen sees.
- `github_repo` gives a repository's open issues, PRs, recent Actions runs, and its checkouts on the user's machines. `github_item` gives an issue or PR in full (description, comments; for PRs also checks and reviews), from its URL or `owner/name#123`.
- `muxen edit <machine>:<folder>` opens a folder of a machine's in the user's editor here (Zed, Cursor or VS Code, over ssh). It opens a window on the user's screen: only when asked.
- `local_checkouts` lists git checkouts on the machines (branch, ahead/behind, uncommitted files). `git_sync` runs fetch, pull (fast-forward only) or push in one; only when asked.
- To work on an issue: find a checkout with `local_checkouts`, or create a workspace with the repo (`workspace_create(repo, branch: "issue-123-short-name")`; for a pull request `ref: "pull/123/head"` checks out its code, forks included). Then `session_start` an agent in that folder with the issue's URL and text as the prompt, or work there yourself with `workspace_exec`.
- Before an agent commits or pushes on a machine, `git_status(machine_id)` says whether that works (`can_commit`, `push`). `git_setup` fixes it (identity from the user's GitHub account, credentials through the machine's GitHub CLI); only when asked, since it changes the machine's global git config.
- `github_accounts` also says which permissions the login has (`scopes`), the user's organizations (`orgs`), and `grant_command` when watching organizations needs more: pass that to the user verbatim.

## Connections and desktops

- A machine can be disconnected on purpose (`status: "disconnected"`): Muxen doesn't connect to it, check it or read its sessions (they keep running there), and anything that would reach it fails with "<name> is disconnected. Connect to it first." `connect_machine(machine_id)` connects again (and checks it); `connect_machine(machine_id, connected: false)` disconnects. Only when asked.

- `machine_connection` says how Muxen reaches a machine: `direct` (QUIC), `ssh` (one live ssh connection) or `none`, with a measured round trip and the `network` it goes over ("Local network", "Tailscale", ...). `direct_connection(machine_id, enabled)` pairs for direct connections (it installs a user service on the machine; only when asked).
- Tailscale, when it's on this computer, keeps machines reachable away from home: Muxen uses the local network when the machine is on it and Tailscale otherwise, and switches by itself when the network changes. A machine's `tailnet` (in `get_machine` and `machine_connection`) says whether Tailscale sees it online and whether traffic goes peer-to-peer (`path: "direct"`) or through a slower relay (`"relay"`). An `unreachable.kind` of `"tailscale-off"` means Tailscale is off or signed out on this computer: tell the user to turn it on; Muxen never changes Tailscale's settings.
- `machine_ports(machine_id)` lists what's listening on a machine: each port with its program or Docker container, and for web servers (`web: true`) their status and page title. To show the user a server running there, they open it from the machine's page in the app (Servers › Open, at `localhost` on their computer) or run `muxen forward <machine> <port>`; you can check it from the machine itself with `workspace_exec(command: "curl -sS localhost:<port>")`.
- `desktop_screenshot(machine_id)` returns a picture of the machine's screen (Linux with X11, Macs, and Windows with someone signed in). `desktop_input(machine_id, events)` clicks, types, scrolls and presses keys there, in that picture's pixels: look first, act, then look again. It's the user's real screen: only when asked, and never type passwords.
- A machine can have several monitors. `desktop_displays(machine_id)` lists them (each with an `id`; Muxen's virtual ones have `virtual: true`); pass `display` to `desktop_screenshot` and `desktop_input` to use one. `desktop_add_display(machine_id, width?, height?, name?)` adds a virtual monitor to the desktop (Linux and Macs), as if another screen were plugged in, and `desktop_remove_display(machine_id, display)` takes it away. `desktop_set_display(machine_id, display, width?, height?, scale?)` changes a monitor's resolution (one of its `modes`) or the desktop's scale on it (`scales`; on Linux X11 one scale for all monitors): a 4K monitor at scale 2 reads like 1080p and stays sharp. They change the user's real desktop: only when asked.

## Cloud machines

Muxen can create DigitalOcean droplets in the user's account and add them to the user's machines (signed in from Settings › Cloud: an API token, the browser, or doctl).

- `cloud_options` shows the sign-in, regions, sizes with hourly and monthly prices, and the defaults.
- `cloud_create(name, region?, size?)` creates one, sets it up (the user's account and SSH key; Docker, git, gh; Node with Claude Code and Codex; Tailscale, as the user's settings say) and adds it to the user's machines. **It costs money every hour it exists:** tell the user the size and price and get their OK first.
- `cloud_power(machine_id, "on"|"off")`: a powered-off droplet is still billed. Only `cloud_destroy` stops the charges.
- `cloud_usage` says what they cost: each one's hourly rate, monthly cap, this month and since created (destroyed ones this month too), the totals, and the account's DigitalOcean bill when the sign-in may read it.
- `cloud_destroy(machine_id)` deletes the droplet and everything on it. Confirm first and make sure work there is pushed.
- A cloud machine's `cloud` field (in `get_machine`) has its region, size, price, cost so far and how long it has been idle.

The CLI equivalents are `muxen cloud options|create|usage|power|destroy|login|logout`.

## Several machines at once

- `run_on_machines(machine_ids, command, timeout_secs?)` runs one shell command on each machine, in its login shell from the home folder, without a terminal (nothing interactive). It waits for all of them and returns each one's `exit_code`, `error` and `operation.log_tail`. It runs as the user on their real machines: only when asked, and show the command first.
- `status.attention` and `get_machine`'s diagnostics include `reboot-required` (a Linux machine's updates want a restart) and `failed-services` (systemd units that failed, by name). Read their logs (`logs_read(kind: "service")`) before suggesting a fix.
- `wake_machine(machine_id)` sends Wake on LAN to a machine that's asleep or off, from this computer and from online machines on its network. It works once Muxen has seen the machine's network card (while it was on) and when the machine is set up to wake that way. Muxen checks it again after 20 and 40 seconds.
- The CLI equivalents are `muxen run --on a,b -- <command>` (`--all` for every machine online; it exits 1 when any failed) and `muxen wake <machine>`.

## Logs

- `log_sources(machine_id)` lists what a machine has to read: its system log (`kind: "system"`; `"kernel"` on Linux), log files (`readable: false` for root-only ones), Docker containers and running services (`user: true` for the user's own units).
- `logs_read(machine_id, kind, target?, lines?, filter?)` returns a log's last lines (200 by default): a file's path, a container's name or id, a unit as `target`. Use it to find out why something failed before guessing. Read-only.
- The CLI equivalent is `muxen logs <machine> [--list | --kernel | --file <path> | --unit <name> [--user] | --container <name>] [-n N] [--grep X] [--follow]`.

## Services and containers

- `machine_services(machine_id)` lists a machine's services (systemd on Linux, with `at_boot` and `user: true` for the user's own; Homebrew's on a Mac) and Docker containers, failed ones first.
- `service_action(machine_id, name, action)` starts, stops or restarts one, or `enable`/`disable` whether a systemd service starts at boot. It changes the user's machine: only when asked, and say so before stopping anything that could cut the machine off (ssh, tailscaled, the network). The system's services need root: when sudo wants a password it fails with a hint to use the Muxen app, which asks for it; pass that on.
- Each running service and container has `cpu` (percent of one core) and `memory` (bytes); `machine_services` also lists Docker `images` and Compose projects (`compose`, with their files).
- `docker_run(machine_id, image, name?, ports?, env?, volumes?, restart?, command?)`, `docker_pull(machine_id, image)` and `compose_action(machine_id, project, "up" | "down" | "restart" | "pull")` (with `service`: `"start" | "stop" | "restart"` that one service) run as operations and wait by default (their `log_tail` has Docker's output). They change the user's machine: only when asked. `service_action` removes a container (`remove`) too.
- `compose_project(machine_id, project)` describes a Compose project's parts: each service's image, ports, what it waits for (`depends_on`), networks, volumes and mounted files, health check, and its containers' state. Read it before changing a project. The app draws the same as a picture (Services › Compose, a project).
- `machine_processes(machine_id)` lists the busiest few hundred processes (pid, user, name, command, `cpu` in percent of one core, `memory` in bytes, `seconds` running, `gpu_memory` on NVIDIA, `yours`) and the machine's `gpus` (name, `busy` percent, memory used and total). `process_signal(machine_id, pid, signal: "stop"|"kill")` ends one: only when asked, and name the process first. Another user's process needs root; when sudo wants a password it fails with a hint to use the Muxen app.
- Read a failing service's log first with `logs_read(machine_id, kind: "service", target: "<unit>")`; a Compose project's with `kind: "compose", target: "<project>"`.
- The CLI equivalents are `muxen services <machine> [start|stop|restart|enable|disable <name> [--container]]`, `muxen ps <machine>` and `muxen kill <machine> <pid> [--force]`.

## Files

- `list_files(machine_id, path)` lists a folder on any Linux or macOS machine, this computer included.
- `copy_files(from, paths, to, to_dir)` copies files and folders from one machine to another, through Muxen, so the machines don't need to reach each other. Files with the same names in `to_dir` are replaced: `list_files` it first, and confirm with the user before overwriting. It waits by default; with `wait: false`, poll `get_operation`, and `cancel_operation` stops a copy.
- The CLI equivalents are `muxen files <machine>:<folder>` and `muxen cp <machine>:<path>... <machine>:<folder>`.
- Text copied on the user's Mac is shared with machines that have a desktop, and back, when the user has that on (Settings › Clipboard). Don't rely on it to move data; use `copy_files`.

## Don'ts

- Don't edit `config.toml` or `state.json` by hand while the tools can do it.
- Don't pass tokens around. Muxen gets registration tokens itself from the user's `gh` login.
- Don't use `delete_files: true` unless the user wants the runner's folder gone.
- Don't leave workspaces behind, and don't turn workspaces on for a machine (`workspace_allow`) unless the user asks.
