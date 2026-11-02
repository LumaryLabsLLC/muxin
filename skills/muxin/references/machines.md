# Machines

Everything here that changes a machine (starting or stopping services and processes, running commands, copying over files, the desktop, schedules, power) runs as the user on their real machines: only when they asked, and say what you'll run first. Reading is always fine.

## Adding, renaming, connecting

```sh
muxin machines --json
muxin ssh-hosts --json                                   # ~/.ssh/config aliases, their tailnet, this network
muxin add-machine --name "Mac Studio" --host ci@mac-studio.local --json
muxin rename-machine mac-studio "Studio" --json          # the id stays the same
muxin remove-machine mac-studio --json
muxin disconnect beelink --json                          # stop connecting; its sessions go on there
muxin connect beelink --json                             # connect again, and check it
```

These only edit Muxin's `config.toml` (`remove-machine` also unpairs a machine paired for direct connections from this app). Don't edit `config.toml` by hand. A disconnected machine (`status: "disconnected"`) fails everything that would reach it until `muxin connect`; connect or disconnect only when asked.

## When a machine can't be reached

`muxin machine beelink --json`: when `status` is `offline`, `unreachable` has a `kind`, a `title` and `detail`, often a `hint`, and `last_seen_at`. The kinds:

- `offline`: it didn't answer (shut down, asleep, off the network). Nothing to fix from here; tell the user when it was last seen. `muxin wake beelink` sends Wake on LAN, when Muxin has seen its network card and the machine is set up to wake that way.
- `refused`, `auth`, `unknown-host`, `host-key`: an ssh problem. Pass the `hint` on.
- `tailscale-off`: Tailscale is off or signed out on this computer. Tell the user to turn it on; Muxin never changes Tailscale's settings.
- `host-down`: the machine it runs on (or is reached through) is off. `stopped`: its container there isn't running. `powered-off`: a cloud machine that's off. `installing`: a VM Muxin just made.

`parent` in the machine names the machine it runs on, with `relation` `vm`, `container`, `inside` or `through` (ssh goes through it).

```sh
muxin connection beelink --json             # direct (QUIC) or ssh, round trip, which network, Tailscale's view
muxin connection beelink --direct on --json # pair for direct connections (installs a user service there; only when asked)
```

## What's running

```sh
muxin ports beelink --json                  # what's listening: programs, containers, web servers with their page titles
muxin ps beelink -n 30 --json               # busiest processes (cpu, memory, gpu_memory) and the GPUs
muxin kill beelink 4242 --json              # stop a process (--force: SIGKILL); name it to the user first
muxin services beelink --json               # services (systemd, Homebrew) and Docker containers, failed first
muxin services beelink restart nginx --json # start, stop, restart, enable or disable one
muxin services beelink stop postgres --container --json
```

- Say so before stopping anything that could cut the machine off (ssh, tailscaled, the network).
- A system service or another user's process needs root. When sudo wants a password, the command fails with a hint to use the Muxin app, which asks for it: pass that on.
- To show the user a web server there: `muxin forward beelink 3000` opens it at `localhost:3000` here until Ctrl-C (or the machine's page in the app, Servers › Open). To check it yourself, `curl` it on the machine (`muxin run`, below).
- Running containers, pulling images and Compose projects (up, down, a project's parts) are in the app and in Muxin's MCP tools (`docker_run`, `docker_pull`, `compose_action`, `compose_project`), not the CLI.

## Logs

```sh
muxin logs beelink --list --json                            # what there is to read
muxin logs beelink --json                                   # the system log's last 200 lines
muxin logs beelink --unit nginx -n 100 --json               # a systemd unit's (--user for the user's own)
muxin logs beelink --container postgres --grep error --json
muxin logs beelink --file '~/app/storage/logs/laravel.log' -n 50 --json
muxin logs beelink --kernel --json                          # Linux
```

Read a failing service's log before suggesting a fix. `--follow` keeps printing new lines until Ctrl-C; it's for a person watching, not for you.

## Commands on several machines

```sh
muxin run --on beelink,mac-studio --json -- df -h /
muxin run --all --timeout 120 --json -- "uptime && free -m"
```

Runs one shell command on each machine, in its login shell from the home folder, without a terminal (nothing interactive). It waits for all of them and returns each one's `exit_code`, `error` and output (`operation.log_tail`). `--all` is every machine that's online. Show the command to the user first. For work in one place over several steps, use a [workspace](workspaces.md) instead.

## Files

```sh
muxin files beelink:~/build --json                                # what's in a folder (default: the home folder)
muxin cp beelink:~/build/app.tar.gz mac-studio:~/Downloads --json # copy between machines, through Muxin
```

`cp` takes files and folders on one machine, then the folder to copy them into (made if missing). Files with the same names there are replaced: list the folder first and confirm with the user before overwriting.

## Desktop

```sh
muxin desktop shot beelink -o beelink.jpg               # then look at beelink.jpg
muxin desktop displays beelink --json                   # its monitors (`virtual: true` for Muxin's)
muxin desktop windows beelink --json                    # the apps' windows
muxin desktop shot beelink --display window:1312130 -o window.jpg
muxin desktop input beelink '[{"type":"click","x":640,"y":400},{"type":"text","text":"hello"},{"type":"key","key":"enter"}]' --json
```

- `desktop shot` saves the picture only without `--json` (with it you get just the sizes). Linux with X11, Macs, and Windows with someone signed in. When nobody is signed in it says so at once: tell the user. They can sign in at the machine, or open its own Remote Desktop or Screen Sharing (`muxin desktop rdp beelink`, `muxin desktop vnc beelink`), which shows the sign-in screen.
- `desktop input` events, in the screenshot's pixels: `click` (`x`, `y`, `button`, `double`), `move`, `scroll` (`x`, `y`, `dy`; positive scrolls down), `key` (`"enter"`, `"ctrl+c"`), `text`, `wait` (`ms`). Look first, act, then look again. It's the user's real screen: only when asked, and never type passwords.
- `muxin desktop add`, `remove` and `set` add a virtual monitor, take it away, or change a monitor's resolution (`--size 3840x2160`) or scale (`--scale 2`). They change the user's real desktop: only when asked.

## Schedules

A schedule runs a command, or starts an agent session, on one machine at set times, through the machine's own scheduler (cron, launchd, Task Scheduler), so it runs whether or not Muxin is open. Times are the machine's own.

```sh
muxin schedule ls --json                                  # every machine's (--on beelink for one)
muxin schedule add beelink --name "Disk report" --every weekdays --at 09:00 --command "df -h /" --json
muxin schedule add beelink --name Triage --every day --at 08:30 --agent claude --prompt "Label new issues in acme/api" --folder '~/code/api' --json
muxin schedule run beelink s-ab12cd --json                # run it now
muxin schedule log beelink s-ab12cd --json                # its last runs and the newest one's output
muxin schedule rm beelink s-ab12cd --json
```

- `--every` is `15m`, `2h`, `day`, `weekdays`, or days like `mon,thu`; `--at` is `HH:MM` (for hours, the minute).
- An `--agent` schedule starts a session the user sees like any other, left waiting for them when it's done. For an agent run that finishes by itself, schedule a command such as `claude -p "..."`.
- `problem` in `schedule ls` says when a machine can't run schedules (no cron, as in a container).
- They run unattended: create one only when asked, and say what it runs and when.

## Muxin itself

```sh
muxin updates --json     # whether a newer Muxin is out, and how to update this copy
muxin paths              # where Muxin keeps its files
```

Installing an update is the user's call.
