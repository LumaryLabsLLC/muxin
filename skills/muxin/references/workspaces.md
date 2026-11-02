# Workspaces: borrowing a machine to work on

A workspace is a persistent folder on one of the user's machines, optionally a git checkout, that you use as your own work environment, like a remote dev box. `muxin workspace exec` runs shell commands there with the machine user's login environment. `muxin ws` is the same command.

- **Reuse before you create.** Run `muxin ws ls --json` first. Use one workspace per task and keep using it across steps.
- **A worktree when the repository is already there.** When `muxin checkouts --json` shows a checkout of the repository on the machine, `--worktree-of <its folder> --branch <name>` makes the workspace a git worktree of it: quicker than a clone, on its own branch, so several agents can work on one repository at once. Removing the workspace removes the worktree and keeps the branch.
- **Destroy it when you're done.** Nothing is deleted automatically. Pull or push anything worth keeping first.
- **A non-zero exit code is a normal result,** not a Muxin error. Read stderr.
- **Long-running things go in the background** (`--background`): servers and watchers. Read the log file it prints with `muxin ws cat`.
- **It's a real machine.** Commands run as the user, not in a sandbox (unless the workspace is a Docker one). Don't do anything outside the workspace that you wouldn't do in their terminal without asking.

## Create

```sh
muxin ws create --platform linux --repo acme/api --ref main --json          # the least-loaded Linux machine
muxin ws create --machine beelink --repo acme/api --ref pull/142/head --json  # a pull request's code, forks included
muxin ws create --machine beelink --worktree-of '~/code/api' --branch fix-login --json
muxin ws create --docker --image node:22 --repo acme/web --json             # a fresh container of its own
```

- Without `--machine`, Muxin picks the least-loaded online machine that allows workspaces; `--platform` narrows it (`linux`, `linux-x64`, `darwin-arm64`, `windows`).
- `--repo` clones `owner/name` or a git URL (private repos with the user's `gh` login), `--ref` checks out a branch, tag, commit or `pull/<n>/head`, and `--branch` makes a new branch after that.
- `--docker` runs commands in a container (`--image`; the folder is at `/workspace`). Only add `--docker-socket` when the task needs Docker inside it: that's root-equivalent on the host.
- The result's `workspace` has its `id` and its `path` on the machine.

## Work in it

```sh
muxin ws exec ws-7f3a2c -- npm ci
muxin ws exec ws-7f3a2c --timeout 1800 -- "npm test 2>&1 | tail -50"
muxin ws exec ws-7f3a2c --cwd packages/web --env CI=1 -- npm run build
muxin ws exec ws-7f3a2c --background -- npm run dev
muxin ws exec ws-7f3a2c -- curl -sS localhost:3000/health
muxin ws cat ws-7f3a2c .muxen/logs/1.log
muxin ws files ws-7f3a2c src --depth 1
muxin ws changes ws-7f3a2c --json          # git status and diff
muxin ws show ws-7f3a2c --json             # with its recent commands
```

- Put the command after `--`. One argument is used as a shell string (pipes and `&&` work); several are quoted and joined.
- Without `--json`, `exec` passes stdout and stderr through and exits with the command's exit code (124 on timeout). With `--json` you get `exit_code`, `stdout`, `stderr`, `timed_out` and `duration_ms`.
- `--timeout` is in seconds: 600 by default, 3600 at most.
- `muxin ws write ws-7f3a2c notes.txt` writes stdin to a file there (`--append` adds to it).

## Move code between the user's checkout and a workspace

```sh
muxin ws sync ws-7f3a2c '/Users/me/code/api' --push --json   # this computer's folder into the workspace
muxin ws sync ws-7f3a2c '/Users/me/code/api' --pull --json   # the workspace back into it
```

The folder is on this computer: use its absolute path. A pull brings files back but never the workspace's `.git`, so commits made there stay there: leave the changes uncommitted, or commit them in the user's checkout. Confirm with the user before a pull, and only add `--delete` (mirror deletions) when they want that.

## Clean up

```sh
muxin ws rm ws-7f3a2c --json
muxin ws rm ws-7f3a2c --keep-files --json   # forget it, keep the folder
```

Background processes stop when the workspace is removed. `muxin ws allow <machine>` turns workspaces on or off for a machine (`--off`); only when the user asks. `muxin ws shell` is interactive, for the user.

## "Run the test suite on Linux"

1. `muxin ws create --platform linux --repo acme/api --ref my-branch --json`
2. `muxin ws exec <id> --timeout 1800 -- "npm ci && npm test"`
3. Report the exit code and the failing tests, then `muxin ws rm <id> --json` unless the user wants to keep it.
