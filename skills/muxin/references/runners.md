# Runners and jobs

Self-hosted GitHub Actions runners on the user's machines.

- A runner's `state` is `working`, `idle`, `offline` or `unknown`. `source` says where that came from: `github` (live), `probe` (the last machine check) or `none` (nothing yet).
- Installing, stopping, restarting and removing runners run commands on the user's machines and register or deregister runners on GitHub. Do them when the user asked for exactly that; otherwise say what you'd do and ask.

```sh
muxin runners --json
muxin runners --machine mac-studio --state offline --json
muxin jobs --json                      # queued and running jobs
```

## "Add my server and put two runners on it"

1. `muxin ssh-hosts --json` lists the user's `~/.ssh/config` aliases. Its `tailnet` list has the computers on their Tailscale network, each with the `host` to add it by; `network` has computers on this network that aren't in Muxin yet (`ssh: false` is a Mac with Remote Login off: the user turns it on in System Settings › General › Sharing). Or ask for `user@host`.
2. `muxin add-machine --name "Mac Studio" --host ci@mac-studio.local --json`. The result includes the first check. If its status is `offline`, the machine isn't reachable with key-based ssh: show the error and stop.
3. A GitHub org or repo must be watched (`github.watching` in `muxin status --json`). If none is, ask which one and run `muxin watch acme --json` (or `acme/api`). A failure there usually needs `admin:org`: show the hint.
4. `muxin install mac-studio --count 2 --json`. Add `--scope acme` when more than one org or repo is watched. The default `--mode auto` picks a service where it can, else Docker containers (Linux without passwordless sudo), else a background `run.sh`; the log says which. Use `--mode docker` when the user wants isolated runners, and only add `--docker-socket` if they explicitly want jobs to use Docker and trust the workflows (it's root-equivalent on the host).
5. Report the new runner ids and their states from the result.

More `install` flags: `--name` (repeat for several, instead of `--count`), `--label` (repeat), `--group` (an org runner group), `--image` (with `--mode docker`), `--timeout` (seconds).

## "Why is runner X offline?"

1. `muxin machine mac-studio --json`. Check the machine's `status` first: an unreachable machine explains everything on it. When it's `offline`, `unreachable.kind` says why: `offline` means it didn't answer (shut down, asleep, off the network), so tell the user, with `last_seen_at`. The other kinds are in [machines](machines.md#when-a-machine-cant-be-reached), most with a `hint` to pass on. Then look at the runner's `state`, `source`, `service_state` and the machine's diagnostics.
2. `muxin runner logs mac-studio/mac-studio-01 --json` reads its newest `_diag` log.
3. Explain the cause and propose a fix, such as `muxin runner start mac-studio/mac-studio-01 --json`. Run it only if the user agrees or already asked.

## "Restart everything on machine Y"

1. `muxin machine Y --json` for its runner ids.
2. `muxin runner restart <runner-id> --json` for each, one at a time. A machine takes one changing operation at a time; `busy` means wait and retry.
3. Summarize the final states.

The actions are `start`, `stop`, `restart`, `logs` and `remove`. `remove` deregisters it; add `--delete-files` only when the user wants the runner's folder gone.

## "Why isn't my job starting?"

1. `muxin jobs --json`. A queued job with `matching_online: 0` has no online runner with all of its labels.
2. Compare the job's `labels` with the runners' (`muxin runners --json`). The fix is usually to start an offline runner, or to install one with the missing label (`muxin install beelink --count 1 --label gpu --json`).
3. To stop a run that shouldn't go on: `muxin cancel-run 9000105 --json` (the job's `id`) cancels the job's whole workflow run; GitHub cancels runs, not single jobs. Confirm with the user first.

## GitHub scopes and settings

```sh
muxin watch acme acme/api --json       # watch orgs or repos (live runner status and jobs)
muxin unwatch acme/api --json
muxin settings --json                  # show; --poll, --scan-repos, --check-interval, --api-url change them
```

Change what's watched or the settings only when asked.
