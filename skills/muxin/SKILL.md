---
name: muxin
description: Drive Muxin, the app for all of the user's machines, through its `muxin` CLI with --json. Use it when the user asks about their machines or what runs on them, here or from their phone. That covers checking machines; agent sessions (Claude Code, Codex, OpenCode, ...) running on them, what those sessions are asking, and asking them to review a pull request, fix its review comments, resolve its conflicts, commit, push or open a PR; boxes (tasks an agent works on alone in a container); workspaces; self-hosted GitHub Actions runners and queued jobs; the user's GitHub inbox, issues and checkouts; logs, services, processes, files and desktops; schedules; cloud machines and VMs.
license: Apache-2.0
compatibility: Needs Muxin's `muxin` command on the PATH (Settings › General › Command line, or `muxin shell-command install`).
metadata:
  source: https://github.com/LumaryLabsLLC/muxin/tree/main/skills/muxin
---

# Muxin

Muxin is one app for all of the user's machines, reached over ssh (or directly for this computer): agent sessions, boxes, workspaces, desktops, files, GitHub, cloud machines and VMs, and the self-hosted GitHub Actions runners on them. The `muxin` command drives all of it, as the app and its MCP server do.

Add `--json` to every command. stdout then carries JSON only, progress and logs go to stderr, and a failure prints `{"error": {"code", "message", "hint"}}`. If Muxin's MCP tools are connected (`muxin mcp`), they do the same things with the same shapes.

## Ground rules

1. **Start with `muxin status --json`** and read `attention` first. It lists what's wrong, most severe first, with the `machine_id` or `runner_id` each item is about.
2. **The state may be stale.** `status` uses the last machine check: look at `last_check_at` and each machine's `checked_at`. `muxin status --refresh --json` checks every machine first. Checks are read-only.
3. **Confirm before changing real things.** Reading is always fine. Typing into a session, answering its questions, starting or stopping anything, installing, deleting, pushing and anything that costs money need the user's go-ahead, unless they asked for exactly that. Then say what you did.
4. **Pass hints on verbatim.** Errors have a `code`: `not_found`, `invalid`, `busy`, `github`, `unreachable`, `operation_failed` or `internal`. Many also have a `hint`, often a command such as `gh auth refresh -h github.com -s admin:org`. Give the user the hint as it is; don't improvise around permission problems.
5. **Let long commands wait.** Installs, copies, boxes and VMs wait by default and stream progress to stderr. Operation ids belong to the process that started them, so a later `muxin` call can't look one up: don't pass `--no-wait`. Run slow ones (a Windows VM takes 15-30 minutes) in the background of your shell tool.
6. **Exit codes:** `0` success, `1` the operation failed, `2` bad input or not found. `muxin workspace exec` exits with the command's own code (124 when it timed out).
7. **Quote remote paths that start with `~`** (`--cwd '~/code/api'`), so your shell doesn't turn them into this computer's home folder.
8. **`busy` means wait and retry.** A machine takes one changing operation at a time.

## Ids

- **Machine:** a slug of its name, such as `mac-studio`. The name works too.
- **Session:** `<machine-id>/<name>`, such as `beelink/claude-3f9a`. A unique name works too.
- **Box:** `box-7k2m9q`. **Workspace:** `ws-7f3a2c`. **Schedule:** `s-ab12cd`.
- **Runner:** `<machine-id>/<runner-name>`, such as `mac-studio/mac-studio-01`. A runner GitHub knows but no machine lists is `github/<name>`.

## Looking around (read-only)

```sh
muxin status --json                    # counts, GitHub, every machine, attention
muxin machine beelink --json           # one machine: resources, runners, diagnostics, why it's unreachable
muxin session ls --json                # agent sessions, what each changed (git), its pull request and checks
muxin box ls --json                    # boxes: running, waiting or paused, and their git
muxin github inbox --json              # review requests, assigned issues, the user's open PRs
muxin checkouts --repo acme/api --json # where a repository is checked out, and its state
muxin jobs --json                      # queued and running GitHub Actions jobs
```

## Common requests

**"Review PR 142" or "fix the conflicts on my PR"** (from a session that's already on that branch):

1. `muxin session ls --json`. Find the session whose `git.pull.number` is 142, or whose `git.branch` is the PR's branch.
2. `muxin session do beelink/claude-3f9a review --json`. The action is one of `commit`, `commit_push`, `pull_request`, `review`, `fix_comments`, `conflicts` or `recap`. It types the user's prompt for that action into the agent.
3. Follow it with `muxin session read beelink/claude-3f9a --json`. When it waits for a choice, see [sessions and boxes](references/sessions-and-boxes.md).

No session on that branch? Start one in a checkout of the repository (`muxin checkouts --repo acme/api --json` finds them), or hand the task to a box. Both are in [sessions and boxes](references/sessions-and-boxes.md).

**"What's my agent on beelink doing?"** `muxin session read beelink/claude-3f9a --json` returns its screen as text. `muxin session question beelink/claude-3f9a --json` says what it's waiting for.

**"Why is machine X offline?"** `muxin machine X --json`. When `status` is `offline`, `unreachable` says why. `kind: "offline"` means it didn't answer (off, asleep, off the network): tell the user, with `last_seen_at`; `muxin wake X` may bring it back. Other kinds come with a `hint` to pass on.

## More

Read the one that fits the task:

- [Sessions and boxes](references/sessions-and-boxes.md): start, read, answer and steer agent sessions; actions (review, conflicts, PRs); hand-offs; boxes.
- [GitHub and git](references/github.md): inbox, repositories, issues and PRs, checkouts, git setup on a machine.
- [Workspaces](references/workspaces.md): a folder on one of the user's machines to work in yourself: run commands, read and write files, sync code.
- [Runners and jobs](references/runners.md): self-hosted GitHub Actions runners, installing them, jobs that don't start.
- [Machines](references/machines.md): adding machines, services and containers, logs, processes, ports, files, desktops, commands on several machines, schedules.
- [Cloud machines and VMs](references/cloud-and-vms.md): DigitalOcean droplets (they cost money) and VMs on the user's machines.

Every command has `--help`. `--demo` runs any command against simulated machines, safe to practice on.
