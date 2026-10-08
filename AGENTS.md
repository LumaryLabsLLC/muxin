# Working with OpenRunner as an agent

OpenRunner manages a fleet of self-hosted GitHub Actions runners. The fleet is made up of machines (reached over `ssh`, or directly for `host = "local"`) and the runners installed on them.

Use the MCP tools when they're connected: start the server with `openrunner mcp`, and see the README for setup. Otherwise use the CLI with `--json`. Both front the same API and return the same shapes.

## Ground rules

1. **Start with `fleet_status`** (`openrunner status --json`) and read `attention` first. It lists what's wrong, most severe first, with the `machine_id` or `runner_id` each item is about.
2. **The state may be stale.** `fleet_status` uses the last machine check. Look at `last_check_at` and each machine's `checked_at`. Call `check_machines` (or `fleet_status` with `refresh: true`) when you need it fresh. Checks are read-only.
3. **Confirm before changing real things.** `install_runners` and `runner_action` with `stop`, `restart` or `remove` run commands on the user's machines and register or deregister runners on GitHub. Do them when the user asked for exactly that; otherwise say what you'd do and ask. `add_machine` and `remove_machine` only edit `fleet.toml`.
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

1. Call `list_ssh_hosts` to find the user's alias, or ask for `user@host`.
2. Call `add_machine(name, host)`. Its result includes the first check. If the status is `offline`, the machine isn't reachable with key-based ssh. Show the error and stop.
3. Make sure a GitHub scope is watched (`fleet_status.github.watching`). If none is, ask which org or repo to use and call `github_scopes(add: [...])`. A failure there usually needs `admin:org`; show the hint.
4. Call `install_runners(machine_id, count: 2)`. Pass `scope` if more than one org or repo is watched. The default `mode: "auto"` picks a service where possible, else Docker containers (Linux without passwordless sudo), else a background run.sh; the log says which. Use `mode: "docker"` when the user wants isolated runners; only pass `docker_socket: true` if they explicitly want jobs to use Docker and trust the workflows (it's root-equivalent on the host).
5. Report the new runner ids and their states from the result.

**"Why is runner X offline?"**

1. Call `get_machine` on its machine. Check the machine's `status` first: an unreachable machine explains everything on it. Then check the runner's `state`, `source`, `service_state`, and the diagnostics with its `runner_id`.
2. Call `runner_action(X, "logs")` to read its newest `_diag` log.
3. Explain the cause and propose a fix, such as `runner_action(X, "start")`. Do it only if the user agrees or already asked.

**"Restart everything on machine Y."**

1. Call `get_machine(Y)` to get its runner ids.
2. Call `runner_action(id, "restart")` for each, one at a time. A machine accepts one changing operation at a time; `busy` means wait and retry.
3. Summarize the final states.

**"Why isn't my job starting?"**

1. Call `list_jobs`. A queued job with `matching_online: 0` has no online runner with all of its labels.
2. Compare the job's labels with the runners' labels (`list_runners`). The fix is usually to start an offline runner, or to install one with the missing label (`install_runners(labels: [...])`).

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

The CLI equivalents are `openrunner workspace create|ls|show|exec|cat|write|files|changes|sync|rm|allow`, aliased as `openrunner ws`. `ws exec` exits with the command's own exit code.

## Don'ts

- Don't edit `fleet.toml`, `settings.json` or `state.json` by hand while the tools can do it.
- Don't pass tokens around. OpenRunner gets registration tokens itself from the user's `gh` login.
- Don't use `delete_files: true` unless the user wants the runner's folder gone.
- Don't leave workspaces behind, and don't turn workspaces on for a machine (`workspace_allow`) unless the user asks.
