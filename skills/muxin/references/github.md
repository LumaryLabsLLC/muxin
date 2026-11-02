# GitHub and git

Muxin uses the GitHub CLI's login (`gh auth login`) or `GITHUB_TOKEN`, one account at a time.

## What needs the user

```sh
muxin github inbox --json                          # review requests, assigned issues, their open PRs
muxin github repos --json                          # their repositories, favorites first (`favorite: true`)
muxin github repo acme/api --json                  # open issues and PRs, recent Actions runs, checkouts on their machines
muxin github issue acme/api#142 --json             # an issue or PR in full: description, comments; for PRs also checks and reviews
muxin github issue https://github.com/acme/api/pull/142 --json
```

`muxin github pr` is the same command as `github issue`.

## Checkouts on the machines

```sh
muxin checkouts --json                             # every git checkout Muxin found on the machines
muxin checkouts --repo acme/api --json             # just this repository's
muxin checkouts --machine beelink --json
muxin checkouts --machine beelink --path '~/code/api' --git pull --json   # fetch, pull (fast-forward only) or push there
```

Each checkout has its `machineId`, `path`, `repo`, `branch`, `upstream`, `ahead`, `behind` and `dirty` (uncommitted files). Run `--git` only when the user asked.

To work on an issue or review a PR there, see [sessions and boxes](sessions-and-boxes.md#start-one-on-an-issue-or-a-pull-request).

## Git on a machine

```sh
muxin git status beelink --json
muxin git setup beelink --identity --credentials --json
```

`git status` says whether an agent there can commit (`can_commit`) and how pushes reach GitHub (`push`: `gh`, `other` or `none`). `git setup --identity` sets git's name and email from the user's GitHub account; `--credentials` makes git push with the machine's GitHub CLI login (`gh auth setup-git`). It changes the machine's global git config: only when asked.

## Accounts and favorites

```sh
muxin github accounts --json                       # who Muxin uses (`using`), the alternatives, `scopes`, `orgs`
muxin github use work-login --json                 # switch accounts
muxin github use --machine work-macbook --json     # use that machine's gh login
muxin github favorite acme/api --json              # star it in Muxin (--remove to unstar)
```

Switching changes whose repositories and runners Muxin sees. When watching organizations needs more permissions, `github accounts` has a `grant_command`: pass it to the user verbatim. Switch accounts or change favorites only when asked.
