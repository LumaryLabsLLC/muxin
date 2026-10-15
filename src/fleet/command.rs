//! Running one command on several machines at once (the Run page, `muxin
//! run`): an operation per machine, in the user's login shell from their
//! home folder, its output streamed into the operation's log.

use std::sync::Arc;
use std::time::Duration;

use super::Engine;
use super::workspaces::transport::{self, Limits};
use crate::ops::remote::{self, sh_quote};
use crate::probe::Shell;

/// How long a command may run unless asked otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The longest it may run.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// sh: from the home folder, with the user's own shell when it's an sh-family one.
fn sh_script(command: &str) -> String {
    format!(
        "OR_CMD={}\ncd ~ 2>/dev/null\nexport NO_COLOR=1 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat PAGER=cat\nor_shell=sh\ncase \"$SHELL\" in */bash|*/zsh|*/ksh) or_shell=$SHELL ;; esac\n\"$or_shell\" -c \"$OR_CMD\" < /dev/null\nprintf '%s:exit=%s\\n' \"$OR_MARK\" \"$?\" >&2\n",
        sh_quote(command)
    )
}

/// PowerShell, for Windows machines: the command in a script file of its
/// own, run by its own PowerShell from the home folder, as in a terminal (its
/// output formatted as text, its `exit` its exit status).
fn ps_script(command: &str) -> String {
    format!(
        "$OR_CMD = '{}'\n\
         $f = Join-Path ([IO.Path]::GetTempPath()) (\"muxen-run-$PID.ps1\")\n\
         Set-Content -LiteralPath $f -Value $OR_CMD -Encoding UTF8\n\
         Set-Location $env:USERPROFILE\n\
         & powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $f\n\
         $c = $LASTEXITCODE\n\
         Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue\n\
         [Console]::Error.WriteLine(\"${{OR_MARK}}:exit=$c\")\n",
        command.replace('\'', "''")
    )
}

impl Engine {
    /// Runs `command` on a machine as an operation (kind "command"); its id.
    pub fn run_command(self: &Arc<Self>, machine_id: &str, command: &str, timeout: Duration) -> Result<u64, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Which command?".into());
        }
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(self.demo_command(&spec.id, &spec.name, command, probe.as_ref().and_then(|p| p.os.clone()).unwrap_or_default()));
        }
        spec.refuse_if_disconnected()?;
        let windows = remote::shell_for(&spec, probe.as_ref()) == Shell::Powershell;
        let timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
        let short: String = command.lines().next().unwrap_or("").chars().take(60).collect();
        let op = self.begin_op("command", format!("{short} on {}", spec.name), Some(spec.id.clone()), None);
        let script = if windows { ps_script(command) } else { sh_script(command) };
        let me = self.clone();
        self.runtime().spawn(async move {
            let marker = transport::new_marker();
            let log = me.clone();
            let on_line = move |l: &str| log.op_log(op, l.to_string());
            let out = transport::run(&spec, windows, &marker, transport::with_marker(&marker, windows, &script), timeout, Limits::OUTPUT, &[], Some(&on_line)).await;
            let result = match out {
                Err(e) => Err(e),
                Ok(o) if o.timed_out => Err(format!("Still running after {}s: stopped waiting.", timeout.as_secs())),
                Ok(o) if !o.started => Err(format!("{} is unreachable: {}", spec.name, o.reason())),
                Ok(o) => match o.exit {
                    Some(0) => Ok(()),
                    Some(c) => Err(format!("Exited with status {c}.")),
                    None => Err(o.reason()),
                },
            };
            if let Err(e) = &result {
                me.op_log(op, format!("error: {e}"));
            }
            me.op_finish(op, result);
        });
        Ok(op)
    }

    /// The demo's run: made-up output after a moment.
    fn demo_command(self: &Arc<Self>, id: &str, name: &str, command: &str, os: String) -> u64 {
        let op = self.begin_op("command", format!("{command} on {name}"), Some(id.to_string()), None);
        let mac = os.starts_with("darwin") || os.contains("mac");
        let lines: Vec<String> = if command.starts_with("df") {
            let (dev, size, used, free, pct) = if mac { ("/dev/disk3s1s1", "926Gi", "512Gi", "382Gi", "58%") } else { ("/dev/nvme0n1p2", "1.8T", "412G", "1.3T", "24%") };
            vec!["Filesystem       Size   Used  Avail Capacity  Mounted on".into(), format!("{dev:<16} {size:>5}  {used:>5}  {free:>5}    {pct:>4}   /")]
        } else if command.starts_with("uptime") {
            vec![format!(" 13:45  up {} days,  4:02, 2 users, load averages: 0.42 0.38 0.31", id.len() % 9 + 2)]
        } else if command.starts_with("ls") {
            ["Applications", "Desktop", "Documents", "Downloads", "Library", "Movies", "Music", "Pictures", "Public", "code", "compose", "dotfiles", "go", "models", "notes", "runners", "scratch", "snap", "Templates", "Videos", "work"]
                .iter()
                .map(|s| s.to_string())
                .collect()
                } else if command.starts_with("uname") {
            vec![if mac { "Darwin 25.1.0 arm64".into() } else { "Linux 6.17.0-8-generic x86_64".into() }]
        } else {
            vec!["done".into()]
        };
        let me = self.clone();
        let wait = Duration::from_millis(400 + (id.len() as u64 % 5) * 250);
        self.runtime().spawn(async move {
            tokio::time::sleep(wait).await;
            for l in lines {
                me.op_log(op, l);
            }
            me.op_finish(op, Ok(()));
        });
        op
    }

    /// Runs `command` on each machine; each one's operation id, or why it didn't start.
    pub fn run_on_machines(self: &Arc<Self>, machine_ids: &[String], command: &str, timeout: Duration) -> Vec<(String, Result<u64, String>)> {
        machine_ids.iter().map(|id| (id.clone(), self.run_command(id, command, timeout))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_the_command() {
        let s = sh_script("echo 'hi' && uptime");
        assert!(s.starts_with("OR_CMD='echo '\\''hi'\\'' && uptime'\n"), "{s}");
        assert!(ps_script("Write-Output 'hi'").starts_with("$OR_CMD = 'Write-Output ''hi'''\n"));
    }
}
