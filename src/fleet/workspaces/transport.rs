//! Runs workspace scripts on a machine. Unlike machine checks, these run in the
//! user's *login* shell, so Homebrew, nvm, asdf and friends are on PATH just as
//! in their own terminal.
//!
//! Login profiles may print things, so every script first prints a random
//! marker on both streams and everything before it is dropped. Scripts report
//! the command's exit status as `<marker>:exit=N` on stderr, which can't be
//! confused with ssh's own exit codes.

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::config::MachineSpec;
use crate::ops::remote::clean_line;
use crate::probe;

/// sh-family login shells read the script from stdin as a login shell; anything
/// else (fish, nushell, xonsh) falls back to a login `sh`. Valid in every one
/// of those shells, since ssh hands the remote command to the user's shell.
const LOGIN_WRAPPER: &str =
    r#"case "$SHELL" in */bash|*/zsh|*/sh|*/ksh|*/dash) exec "$SHELL" -l -s;; *) exec sh -l -s;; esac"#;
const PS_READ_STDIN: &str = "[Console]::In.ReadToEnd() | Invoke-Expression";
/// After the script reports its exit status, give stragglers holding the
/// output pipes (a server the command left running) this long, then hang up.
const AFTER_EXIT_GRACE: Duration = Duration::from_secs(2);

/// How much output to keep. The middle of anything longer is dropped.
#[derive(Clone, Copy)]
pub struct Limits {
    pub head: usize,
    pub tail: usize,
    /// Longer lines are cut.
    pub line: usize,
}

impl Limits {
    pub const OUTPUT: Limits = Limits { head: 16 * 1024, tail: 48 * 1024, line: 16 * 1024 };
    pub const DATA: Limits = Limits { head: 8 * 1024 * 1024, tail: 0, line: 8 * 1024 * 1024 };
}

#[derive(Debug, Default)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    /// The process's own exit status (ssh or the local shell).
    pub status: Option<i32>,
    /// From `<marker>:exit=N`, when the script reports it.
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    /// The script started (the marker came through).
    pub started: bool,
    /// stderr from before the script started: ssh errors, broken profiles.
    pub preamble: String,
}

impl Output {
    /// A short reason for a failure: the script's `error: ...` line, else the last stderr lines.
    pub fn reason(&self) -> String {
        let lines: Vec<&str> = self.stderr.lines().chain(self.preamble.lines()).map(str::trim).filter(|l| !l.is_empty()).collect();
        if let Some(e) = lines.iter().find_map(|l| l.strip_prefix("error: ")) {
            return e.to_string();
        }
        let tail: Vec<&str> = lines.iter().rev().take(3).rev().copied().collect();
        match (tail.is_empty(), self.status) {
            (false, _) => tail.join(" "),
            (true, Some(c)) => format!("The command on the machine exited with status {c}."),
            (true, None) => "The command on the machine was terminated.".into(),
        }
    }
}

pub fn new_marker() -> String {
    format!("__muxen_{}__", super::random_id(12))
}

/// Prepends the marker preamble for `windows` (PowerShell) or sh scripts.
pub fn with_marker(marker: &str, windows: bool, script: &str) -> String {
    if windows {
        format!("$OR_MARK = '{marker}'\nWrite-Output $OR_MARK\n[Console]::Error.WriteLine($OR_MARK)\n{script}")
    } else {
        // zsh as a login shell: behave like sh for the rest of the script.
        format!(
            "if [ -n \"$ZSH_VERSION\" ]; then emulate sh 2>/dev/null; fi\nOR_MARK='{marker}'\nprintf '%s\\n' \"$OR_MARK\"\nprintf '%s\\n' \"$OR_MARK\" >&2\n{script}"
        )
    }
}

fn command(spec: &MachineSpec, windows: bool) -> Command {
    if spec.is_local() {
        if windows {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", PS_READ_STDIN]);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", LOGIN_WRAPPER]);
            c
        }
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(if windows {
            format!("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \"{PS_READ_STDIN}\"")
        } else {
            format!("sh -c '{LOGIN_WRAPPER}'")
        });
        c
    }
}

/// Runs `script` (already wrapped by `with_marker`) and collects its output.
/// `on_line` sees each cleaned stdout/stderr line as it arrives (for streaming
/// into an operation log). `secrets` are masked everywhere.
pub async fn run(
    spec: &MachineSpec,
    windows: bool,
    marker: &str,
    script: String,
    timeout: Duration,
    limits: Limits,
    secrets: &[String],
    on_line: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<Output, String> {
    spec.refuse_if_disconnected()?;
    // Over the machine's live link when there is one: no ssh handshake. Scripts
    // that stream their output (operation logs) and big uploads still use ssh.
    if !windows && !spec.is_local() && on_line.is_none() && script.len() < 8 << 20 {
        if let Some(link) = crate::fleet::link::get(spec).await {
            let req = serde_json::json!({ "op": "sh", "script": script, "timeout": timeout.as_secs() });
            if let Ok(reply) = link.request(req, timeout + Duration::from_secs(10)).await {
                if reply["ok"].as_bool() == Some(true) {
                    return Ok(from_link(marker, limits, secrets, &reply));
                }
            }
        }
    }
    let mut cmd = command(spec, windows);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        if spec.is_local() { format!("Couldn't start a shell: {e}") } else { format!("Couldn't run ssh: {e}") }
    })?;
    let mut stdin = child.stdin.take().ok_or("No stdin for the script")?;
    let mut stdout = child.stdout.take().ok_or("No stdout for the script")?;
    let mut stderr = child.stderr.take().ok_or("No stderr for the script")?;

    let mut out = Collector::new(marker, limits, secrets, false);
    let mut err = Collector::new(marker, Limits::OUTPUT, secrets, true);

    let work = async {
        // The script is small (or a bounded file upload); write it whole, then close stdin.
        let writer = async {
            let _ = stdin.write_all(script.as_bytes()).await;
            drop(stdin);
        };
        let reader = async {
            let (mut out_open, mut err_open) = (true, true);
            let mut ob = [0u8; 8192];
            let mut eb = [0u8; 8192];
            let mut exit_deadline: Option<tokio::time::Instant> = None;
            while out_open || err_open {
                let grace = async {
                    match exit_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    n = read(&mut stdout, &mut ob), if out_open => match n {
                        Some(n) => out.push(&ob[..n], on_line),
                        None => out_open = false,
                    },
                    n = read(&mut stderr, &mut eb), if err_open => match n {
                        Some(n) => {
                            err.push(&eb[..n], on_line);
                            if err.exit.is_some() && exit_deadline.is_none() {
                                exit_deadline = Some(tokio::time::Instant::now() + AFTER_EXIT_GRACE);
                            }
                        }
                        None => err_open = false,
                    },
                    _ = grace => break,
                }
            }
        };
        tokio::join!(writer, reader);
    };

    let finished = tokio::time::timeout(timeout, work).await.is_ok();
    let status = if finished {
        match tokio::time::timeout(AFTER_EXIT_GRACE, child.wait()).await {
            Ok(Ok(s)) => s.code(),
            _ => {
                let _ = child.kill().await;
                None
            }
        }
    } else {
        let _ = child.kill().await;
        None
    };

    out.finish(on_line);
    err.finish(on_line);
    let started = out.started || err.started;
    let (stdout, out_trunc) = out.text();
    let (stderr, err_trunc) = err.text();
    Ok(Output {
        stdout,
        stderr,
        status,
        exit: err.exit,
        timed_out: err.timed_out || !finished,
        truncated: out_trunc || err_trunc,
        started,
        preamble: err.preamble.join("\n"),
    })
}

/// A script's result from the agent's link, read the same way as ssh output.
fn from_link(marker: &str, limits: Limits, secrets: &[String], reply: &serde_json::Value) -> Output {
    let mut out = Collector::new(marker, limits, secrets, false);
    let mut err = Collector::new(marker, Limits::OUTPUT, secrets, true);
    out.push(reply["stdout"].as_str().unwrap_or("").as_bytes(), None);
    err.push(reply["stderr"].as_str().unwrap_or("").as_bytes(), None);
    out.finish(None);
    err.finish(None);
    let started = out.started || err.started;
    let (stdout, out_trunc) = out.text();
    let (stderr, err_trunc) = err.text();
    Output {
        stdout,
        stderr,
        status: reply["status"].as_i64().map(|s| s as i32),
        exit: err.exit,
        timed_out: err.timed_out || reply["timed_out"].as_bool().unwrap_or(false),
        truncated: out_trunc || err_trunc,
        started,
        preamble: err.preamble.join("\n"),
    }
}

async fn read(r: &mut (impl AsyncRead + Unpin), buf: &mut [u8]) -> Option<usize> {
    match r.read(buf).await {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(n),
    }
}

/// Splits a byte stream into cleaned lines, drops the login-profile preamble,
/// picks out marker lines, and keeps a bounded head and tail.
struct Collector<'a> {
    marker: &'a str,
    limits: Limits,
    secrets: &'a [String],
    is_stderr: bool,
    partial: Vec<u8>,
    partial_cut: bool,
    started: bool,
    preamble: Vec<String>,
    head: Vec<String>,
    head_bytes: usize,
    tail: VecDeque<String>,
    tail_bytes: usize,
    dropped_bytes: usize,
    cut: bool,
    exit: Option<i32>,
    timed_out: bool,
}

impl<'a> Collector<'a> {
    fn new(marker: &'a str, limits: Limits, secrets: &'a [String], is_stderr: bool) -> Self {
        Collector {
            marker,
            limits,
            secrets,
            is_stderr,
            partial: Vec::new(),
            partial_cut: false,
            started: false,
            preamble: Vec::new(),
            head: Vec::new(),
            head_bytes: 0,
            tail: VecDeque::new(),
            tail_bytes: 0,
            dropped_bytes: 0,
            cut: false,
            exit: None,
            timed_out: false,
        }
    }

    fn push(&mut self, bytes: &[u8], on_line: Option<&(dyn Fn(&str) + Send + Sync)>) {
        for &b in bytes {
            if b == b'\n' {
                let raw = std::mem::take(&mut self.partial);
                if std::mem::take(&mut self.partial_cut) {
                    self.cut = true;
                }
                self.line(&String::from_utf8_lossy(&raw), on_line);
            } else if self.partial.len() < self.line_cap() {
                self.partial.push(b);
            } else {
                self.partial_cut = true;
            }
        }
    }

    /// Until the marker shows up, lines may be login-profile noise or the marker
    /// itself, so a tiny cap can't be allowed to cut the marker.
    fn line_cap(&self) -> usize {
        if self.started { self.limits.line } else { self.limits.line.max(4096) }
    }

    fn finish(&mut self, on_line: Option<&(dyn Fn(&str) + Send + Sync)>) {
        if !self.partial.is_empty() {
            let raw = std::mem::take(&mut self.partial);
            self.line(&String::from_utf8_lossy(&raw), on_line);
        }
    }

    fn line(&mut self, raw: &str, on_line: Option<&(dyn Fn(&str) + Send + Sync)>) {
        let mut text = raw.to_string();
        if !self.started {
            match text.find(self.marker) {
                // Profile output can share the marker's line if it didn't end with a newline.
                Some(i) if text[i + self.marker.len()..].trim().is_empty() => {
                    self.started = true;
                    return;
                }
                _ => {
                    if self.preamble.len() < 20 {
                        let clean = clean_line(&text, self.secrets);
                        if !clean.trim().is_empty() {
                            self.preamble.push(clean);
                        }
                    }
                    return;
                }
            }
        }
        if self.is_stderr {
            let exit_tag = format!("{}:exit=", self.marker);
            if let Some(i) = text.find(&exit_tag) {
                self.exit = text[i + exit_tag.len()..].trim().parse().ok();
                text.truncate(i);
                if text.trim().is_empty() {
                    return;
                }
            }
            let timeout_tag = format!("{}:timeout", self.marker);
            if let Some(i) = text.find(&timeout_tag) {
                self.timed_out = true;
                text.truncate(i);
                if text.trim().is_empty() {
                    return;
                }
            }
        }
        let clean = clean_line(&text, self.secrets);
        if let Some(cb) = on_line {
            if !clean.trim().is_empty() {
                cb(&clean);
            }
        }
        self.store(clean);
    }

    fn store(&mut self, line: String) {
        let size = line.len() + 1;
        if self.head_bytes + size <= self.limits.head {
            self.head_bytes += size;
            self.head.push(line);
            return;
        }
        self.tail_bytes += size;
        self.tail.push_back(line);
        while self.tail_bytes > self.limits.tail {
            match self.tail.pop_front() {
                Some(l) => {
                    self.tail_bytes -= l.len() + 1;
                    self.dropped_bytes += l.len() + 1;
                }
                None => break,
            }
        }
    }

    /// The kept text and whether anything was dropped.
    fn text(&self) -> (String, bool) {
        let mut lines: Vec<&str> = self.head.iter().map(String::as_str).collect();
        let note;
        if self.dropped_bytes > 0 {
            note = format!("[… {} of output omitted …]", crate::util::bytes(self.dropped_bytes as u64));
            lines.push(&note);
        }
        lines.extend(self.tail.iter().map(String::as_str));
        (lines.join("\n"), self.dropped_bytes > 0 || self.cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(marker: &str, limits: Limits, input: &str, stderr: bool) -> (Collector<'static>, String, bool) {
        let marker: &'static str = Box::leak(marker.to_string().into_boxed_str());
        let mut c = Collector::new(marker, limits, &[], stderr);
        c.push(input.as_bytes(), None);
        c.finish(None);
        let (t, cut) = c.text();
        (c, t, cut)
    }

    #[test]
    fn drops_profile_noise_and_reads_markers() {
        let m = "__muxen_test__";
        let (c, text, _) = collect(m, Limits::OUTPUT, &format!("Welcome!{m}\nreal error\nlast{m}:exit=3\n"), true);
        assert!(c.started);
        assert_eq!(c.preamble, Vec::<String>::new(), "noise on the marker's line is dropped");
        assert_eq!(c.exit, Some(3));
        assert_eq!(text, "real error\nlast");

        let (c, text, _) = collect(m, Limits::OUTPUT, &format!("motd line\n{m}\n\u{1b}[31mred\u{1b}[0m\n{m}:timeout\n"), true);
        assert_eq!(c.preamble, vec!["motd line"]);
        assert!(c.timed_out);
        assert_eq!(text, "red");
    }

    #[test]
    fn caps_long_output_keeping_head_and_tail() {
        let m = "__muxen_cap__";
        let limits = Limits { head: 20, tail: 20, line: 1000 };
        let body: String = (0..100).map(|i| format!("line {i:03}\n")).collect();
        let (_, text, truncated) = collect(m, limits, &format!("{m}\n{body}"), false);
        assert!(truncated);
        assert!(text.starts_with("line 000\nline 001"), "{text}");
        assert!(text.ends_with("line 098\nline 099"), "{text}");
        assert!(text.contains("omitted"), "{text}");
    }

    #[test]
    fn cuts_huge_lines() {
        let m = "__muxen_line__";
        let limits = Limits { head: 1 << 20, tail: 0, line: 10 };
        let (_, text, truncated) = collect(m, limits, &format!("{m}\n{}\nok\n", "x".repeat(50)), false);
        assert!(truncated);
        assert_eq!(text, "xxxxxxxxxx\nok");
    }

    #[test]
    fn wraps_scripts_with_markers() {
        let sh = with_marker("MK", false, "echo hi\n");
        assert!(sh.contains("OR_MARK='MK'") && sh.ends_with("echo hi\n"));
        let ps = with_marker("MK", true, "Write-Output hi\n");
        assert!(ps.starts_with("$OR_MARK = 'MK'"));
    }
}
