# Agent sessions and boxes

## Agent sessions

The user runs agent CLIs (Claude Code, Codex, OpenCode, Gemini CLI, Amp, Pi) and shells in sessions that keep running on their machines, held by Muxin's agent there. They watch them in the app. These are the user's sessions: start one, type into it, answer it or stop it only when they asked. Reading is always fine.

### See them

```sh
muxin session ls --json
muxin session ls --machine beelink --json
```

Session, box and checkout JSON uses camelCase keys (`machineId`, `agentName`, `sessionId`). Each session has:

- `id`, `agent` (`claude`, `codex`, ..., `shell`), `title`, `cwd`.
- `status`: `running` or `exited` (with `exitCode`).
- `busy`: it printed something in the last few seconds, so it's working.
- `attention`: `waiting`, `bell` or `exited`, until the user opens it.
- `git`: what the work in its folder changed. `branch`; `base` (the default branch it's compared with); `files`, `added`, `removed` and `commits` since the base; `uncommitted` files; `ahead` and `behind` its upstream; `worktree`; and `pull`, the branch's pull request (`number`, `state`, `url`, and `checks` with `passed`, `failed`, `pending` and the `failing` ones).

`machines` lists which agent CLIs each machine has.

### Read it, and answer it

```sh
muxin session read beelink/claude-3f9a --json                 # the screen and recent scrollback, as `text`
muxin session read beelink/claude-3f9a --lines 50 --json      # less scrollback
muxin session question beelink/claude-3f9a --json             # what it's waiting for
muxin session answer beelink/claude-3f9a 1 --json             # pick an option by its key
muxin session send beelink/claude-3f9a "Yes, and add a test" --json   # type it, then Enter
```

- `question` returns the question as `text` and its `options`, each with a `key` (and the `selected` one). Empty `options` means it's simply done or waiting for a message: `text` has the end of what it said. Reply to that with `send`.
- `answer` approves or refuses what the agent asked. Only when the user told you to, and say which option you picked.
- `send --no-enter` types without pressing Enter.

### Actions: review, fix comments, conflicts, commit, PR

```sh
muxin session do beelink/claude-3f9a review --json
```

The action is one of:

- `commit`, `commit_push`
- `pull_request`: open the branch's pull request, or update it.
- `review`: review the branch's changes.
- `fix_comments`: fix the pull request's review comments.
- `conflicts`: resolve merge conflicts with the base branch.
- `recap`: write a recap of the work.

Each one types that action's prompt into the agent; the user can change the prompts in the app (Settings › Prompts). Only when the user asked. Then follow the session with `read` and `question`.

### Start one on an issue or a pull request

1. Find the repository on the user's machines: `muxin checkouts --repo acme/api --json` (each with its `machineId`, `path`, `branch`, `ahead`, `behind`, and `dirty`: uncommitted files).
2. Give the work a folder of its own:
   - An issue, with a checkout there: a git worktree of it on a new branch. `muxin workspace create --machine beelink --worktree-of '~/code/api' --branch issue-123-login-redirect --json`
   - A pull request: a clone at its code, forks included. `muxin workspace create --machine beelink --repo acme/api --ref pull/142/head --json`
   - `workspace.path` in the result is the folder.
3. Start the agent there with the issue's or PR's URL and text: `muxin session start beelink claude --cwd '~/muxen/workspaces/ws-7f3a2c' --prompt "Review https://github.com/acme/api/pull/142 and list what should change" --json`

Before an agent commits or pushes on a machine, `muxin git status beelink --json` says whether that works (`can_commit`, `push`). See [GitHub and git](github.md).

### Start, rename, restart, stop

```sh
muxin session start beelink claude --cwd '~/code/api' --prompt "Fix the failing test" --json
muxin session start beelink codex --cwd '~/code/api' --title "API tests" --json
muxin session start beelink shell --json
muxin session rename beelink/claude-3f9a "API review" --json   # "" goes back to the original title
muxin session restart beelink/claude-3f9a --json               # run its agent again
muxin session stop beelink/claude-3f9a --json
```

The agent is `claude`, `codex`, `opencode`, `gemini`, `amp`, `pi` or `shell`. `muxin session attach` opens one in a terminal; it's interactive, for the user, not for you.

### Hand a session's work to another agent

```sh
muxin session handoff beelink/claude-3f9a codex --json                 # Codex continues; the old session ends
muxin session handoff beelink/claude-3f9a claude --keep --json         # a fresh conversation; the old one keeps running
muxin session handoff this-mac/claude-3f9a codex --on beelink --json   # move the work to beelink, and continue there
muxin session handoff box-7k2m9q gemini --json                         # a box changes agents
```

Muxin writes `.muxin/handoff.md` in the session's folder (the git state, the agent's to-do list, the conversation from its transcript), starts the new agent there asked to read it, and ends the old session unless `--keep`. `source` in the result says what the note came from: `transcript`, `terminal` (an agent whose transcript Muxin can't read) or `none`. With `--on`, the branch's commits that aren't on origin, its uncommitted changes and new files move into a workspace on that machine; the folder it left stays as it was. Git checkouts only. Only when the user asked.

## Boxes

A box is one task handed to an agent that works on it by itself. It gets a fresh checkout of a repository on a new branch, in a Docker container of its own on one of the user's machines. The agent runs there with its permission prompts off (the container is its sandbox), and it can push its branch and open pull requests with the user's GitHub login. Make one only when the user asked.

```sh
muxin box new acme/api "Fix the flaky webhook test and open a pull request" --json
muxin box new acme/api "Review this pull request and write up what should change" --ref pull/142/head --agent codex --json
muxin box new '~/code/app' "Finish this refactor" --on beelink --json   # a checkout there, uncommitted work included
muxin box ls --json
muxin box send box-7k2m9q "Also update the API docs" --json             # starts a paused box again
muxin box resume box-7k2m9q --json
muxin box pause box-7k2m9q --json
muxin box rm box-7k2m9q --json
```

- `box new` takes `owner/name`, a git URL, or a checkout's folder on the `--on` machine. `--agent` is `claude` (the default), `codex`, `amp`, `opencode`, `gemini` or `pi`. Without `--on` it goes on the online machine with Docker and the most free memory. `--ref` is what it starts from (a branch, tag, commit or `pull/<n>/head`); `--branch` is the branch it works on (default `box/<name>`).
- The first box on a machine builds Muxin's image there, which takes a few minutes. The repository's `.agents/setup` runs first.
- `box ls` gives each box's `state` (`running`, `starting`, `paused`), `busy`, `attention` (`waiting`: it asks something), `sessionId` while it runs, its `task`, and `git` (what it changed, its pull request).
- `muxin session read`, `question` and `answer` work on its `sessionId`.
- It pauses by itself after 15 minutes without output from its agent (`--idle <minutes>` on `box new`; 0 never), or when its agent quits. Its files and conversation stay; dev servers and shells it ran end.
- `box rm` deletes the container and the checkout, with any work that wasn't pushed. Check its `git` (`ahead`, `uncommitted`) and confirm with the user first, unless they asked for it.
- A box's agent is signed in as it is on that machine. If it says it isn't, tell the user to sign it in once from the box's terminal (`/login`). Don't ask for tokens.
- `muxin box attach` and `muxin box shell` are interactive, for the user.

## What the agents used

```sh
muxin usage --json                   # the last 7 days, every machine
muxin usage beelink --days 30 --json
```

Claude Code's and Codex's tokens and requests by model and machine, from their own logs, and the cost at API list prices (`null` for a model whose price isn't known). With a Claude or ChatGPT plan the user doesn't pay per token: say it's what the same use would cost through the API.
