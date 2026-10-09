//! Runs a script on a machine and streams its output into an operation's log.
//! Same transport as machine checks: the script goes over stdin to `sh -s`
//! (locally or through the system `ssh`), or to PowerShell on Windows.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::MachineSpec;
use crate::fleet::Engine;
use crate::probe::{self, ProbeResult, Shell};

/// `-Command -` runs stdin line by line and breaks multi-line blocks; read it whole instead.
const PS_READ_STDIN: &str = "[Console]::In.ReadToEnd() | Invoke-Expression";
const STDERR_TAIL: usize = 4;

/// The shell a machine speaks: pinned in config.toml, else what the last check used, else by OS.
pub fn shell_for(spec: &MachineSpec, probe: Option<&ProbeResult>) -> Shell {
    spec.shell
        .or_else(|| probe.and_then(|p| p.shell))
        .unwrap_or_else(|| match probe.and_then(|p| p.os.as_deref()) {
            Some("windows") => Shell::Powershell,
            _ if spec.is_local() && cfg!(windows) => Shell::Powershell,
            _ => Shell::Sh,
        })
}

/// A script parameter, quoted for the target shell.
pub enum Param<'a> {
    Str(&'a str),
    List(&'a [String]),
    Bool(bool),
}

/// Prepends `name='value'` assignments (sh) or `$name = 'value'` (PowerShell) to a script.
pub fn with_params(shell: Shell, params: &[(&str, Param)], script: &str) -> String {
    let mut out = String::new();
    for (name, value) in params {
        let line = match (shell, value) {
            (Shell::Sh, Param::Str(s)) => format!("{name}={}\n", sh_quote(s)),
            (Shell::Sh, Param::List(l)) => format!("{name}={}\n", sh_quote(&l.join(" "))),
            (Shell::Sh, Param::Bool(b)) => format!("{name}={}\n", if *b { 1 } else { 0 }),
            (Shell::Powershell, Param::Str(s)) => format!("${name} = {}\n", ps_quote(s)),
            (Shell::Powershell, Param::List(l)) => {
                format!("${name} = @({})\n", l.iter().map(|s| ps_quote(s)).collect::<Vec<_>>().join(", "))
            }
            (Shell::Powershell, Param::Bool(b)) => format!("${name} = ${b}\n"),
        };
        out.push_str(&line);
    }
    out.push_str(script);
    out
}

pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub(crate) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Runs `script` on the machine, forwarding every output line (secrets masked) to
/// the operation log. Errors carry the last stderr lines, which is where the
/// scripts put their `error: ...` explanation.
pub async fn run(
    engine: &Arc<Engine>,
    op: u64,
    spec: &MachineSpec,
    shell: Shell,
    script: String,
    secrets: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let mut cmd = command(spec, shell);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        if spec.is_local() { format!("Couldn't start the script: {e}") } else { format!("Couldn't run ssh: {e}") }
    })?;

    let mut stdin = child.stdin.take().ok_or("No stdin for the script")?;
    let stdout = child.stdout.take().ok_or("No stdout for the script")?;
    let stderr = child.stderr.take().ok_or("No stderr for the script")?;

    let work = async {
        // Ignore write errors (e.g. ssh exiting early); stderr says why.
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);

        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();
        let mut tail: Vec<String> = Vec::new();
        let (mut out_open, mut err_open) = (true, true);
        while out_open || err_open {
            tokio::select! {
                line = out_lines.next_line(), if out_open => match line {
                    Ok(Some(l)) => log_line(engine, op, &l, secrets),
                    _ => out_open = false,
                },
                line = err_lines.next_line(), if err_open => match line {
                    Ok(Some(l)) => {
                        let clean = clean_line(&l, secrets);
                        if !clean.trim().is_empty() {
                            tail.push(clean.clone());
                            if tail.len() > STDERR_TAIL {
                                tail.remove(0);
                            }
                            engine.op_log(op, clean);
                        }
                    }
                    _ => err_open = false,
                },
            }
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            let reason = tail
                .iter()
                .rev()
                .find_map(|l| l.strip_prefix("error: ").map(str::to_string))
                .or_else(|| tail.last().cloned())
                .unwrap_or_else(|| match status.code() {
                    Some(c) => format!("The script exited with status {c}."),
                    None => "The script was terminated.".into(),
                });
            Err(reason.trim_start_matches("Write-Error: ").to_string())
        }
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err(format!("Gave up after {} minutes.", timeout.as_secs() / 60)),
    }
}

fn command(spec: &MachineSpec, shell: Shell) -> Command {
    if spec.is_local() {
        match shell {
            Shell::Sh => {
                let mut c = Command::new("/bin/sh");
                c.arg("-s");
                c
            }
            Shell::Powershell => {
                let mut c = Command::new("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", PS_READ_STDIN]);
                c
            }
        }
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(match shell {
            Shell::Sh => "sh -s".to_string(),
            Shell::Powershell => {
                format!("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{PS_READ_STDIN}\"")
            }
        });
        c
    }
}

fn log_line(engine: &Arc<Engine>, op: u64, line: &str, secrets: &[String]) {
    let clean = clean_line(line, secrets);
    if !clean.trim().is_empty() {
        engine.op_log(op, clean);
    }
}

/// Masks secrets, drops ANSI escapes and carriage-return progress redraws.
pub fn clean_line(line: &str, secrets: &[String]) -> String {
    let line = line.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
    let mut out = strip_ansi(line);
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(s.as_str(), "••••••");
    }
    out.trim_end().to_string()
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameters, then one final byte in @..~
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_params() {
        let names = vec!["a-01".to_string(), "b-02".to_string()];
        let sh = with_params(
            Shell::Sh,
            &[("OR_TOKEN", Param::Str("it's")), ("OR_NAMES", Param::List(&names)), ("OR_SERVICE", Param::Bool(true))],
            "echo hi\n",
        );
        assert_eq!(sh, "OR_TOKEN='it'\\''s'\nOR_NAMES='a-01 b-02'\nOR_SERVICE=1\necho hi\n");
        let ps = with_params(
            Shell::Powershell,
            &[("OR_TOKEN", Param::Str("it's")), ("OR_NAMES", Param::List(&names)), ("OR_SERVICE", Param::Bool(false))],
            "",
        );
        assert_eq!(ps, "$OR_TOKEN = 'it''s'\n$OR_NAMES = @('a-01', 'b-02')\n$OR_SERVICE = $false\n");
    }

    #[test]
    fn cleans_lines() {
        let secrets = vec!["AABBCCDDEEFF".to_string()];
        assert_eq!(clean_line("--token AABBCCDDEEFF --name x", &secrets), "--token •••••• --name x");
        assert_eq!(clean_line("\u{1b}[32m√ Connected\u{1b}[0m  ", &[]), "√ Connected");
        assert_eq!(clean_line("10%\r50%\r100%", &[]), "100%");
    }

    #[test]
    fn picks_shells() {
        let spec: MachineSpec = toml::from_str("name = \"w\"\nhost = \"win\"").unwrap();
        let probe = ProbeResult { os: Some("windows".into()), ok: true, ..Default::default() };
        assert_eq!(shell_for(&spec, Some(&probe)), Shell::Powershell);
        assert_eq!(shell_for(&spec, None), Shell::Sh);
    }
}
