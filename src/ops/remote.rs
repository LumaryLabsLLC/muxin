//! Runs a script on a machine and streams its output into an operation's log.
//! Same transport as machine checks: the script goes over stdin to `sh -s`
//! (locally or through the system `ssh`), or to PowerShell on Windows.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::MachineSpec;
use crate::fleet::Engine;
use crate::probe::{self, ProbeResult, Shell};

/// PowerShell scripts go over stdin and end with this line. (`-Command -` runs
/// stdin line by line and breaks multi-line blocks, so the script is read
/// whole; and Windows' OpenSSH doesn't always pass on the end of stdin once
/// a script is a few KB, so reading to the end could wait forever.)
///
/// The script is read as UTF-8 (`[Console]::In` would read it in the console's
/// code page, and "…" would turn into letters that join a variable's name), and
/// written out as UTF-8: PowerShell's own output, and through the console's code
/// page, programs it runs that honor it.
pub(crate) const PS_END: &str = "#muxen-end";
const PS_READ_STDIN: &str = "$ProgressPreference = 'SilentlyContinue'; $or_utf8 = New-Object Text.UTF8Encoding $false; try { [Console]::OutputEncoding = $or_utf8 } catch {}; $or_in = New-Object IO.StreamReader -ArgumentList ([Console]::OpenStandardInput()), $or_utf8; $b = New-Object Text.StringBuilder; while ((($l = $or_in.ReadLine()) -ne $null) -and ($l -ne '#muxen-end')) { [void]$b.AppendLine($l) }; $b.ToString() | Invoke-Expression";

/// PowerShell's arguments to run a script sent on stdin (see `ps_sendable`),
/// encoded so no shell on the way changes them.
pub(crate) fn ps_stdin_args() -> Vec<String> {
    use base64::Engine as _;
    let utf16: Vec<u8> = PS_READ_STDIN.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded].map(String::from).to_vec()
}

/// A PowerShell script as it's sent: with its end line.
pub(crate) fn ps_sendable(script: &str) -> String {
    format!("{}\n{PS_END}\n", script.trim_end_matches(['\r', '\n']))
}

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

/// A PowerShell single-quoted string. PowerShell takes ‘ ’ ‚ ‛ for single
/// quotes too, so each of those is doubled like ' is, or text with one (a
/// typographic "don’t") would end the string and run the rest as commands.
pub(crate) fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Runs `script` on the machine, forwarding every output line (secrets masked) to
/// the operation log. Errors carry the script's `error: ...` explanation from
/// stderr, else its last stderr line.
pub async fn run(
    engine: &Arc<Engine>,
    op: u64,
    spec: &MachineSpec,
    shell: Shell,
    script: String,
    secrets: &[String],
    timeout: Duration,
) -> Result<(), String> {
    run_lines(engine, op, spec, shell, script, secrets, timeout).await.map(|_| ())
}

/// The same, and returns what it printed (stdout, line by line): a script's
/// results, like the ports it picked.
pub async fn run_lines(
    engine: &Arc<Engine>,
    op: u64,
    spec: &MachineSpec,
    shell: Shell,
    script: String,
    secrets: &[String],
    timeout: Duration,
) -> Result<Vec<String>, String> {
    spec.refuse_if_disconnected()?;
    if cfg!(target_os = "ios") && !spec.is_local() {
        let (stdout, stderr, status) = over_link(spec, script, timeout).await?;
        let (printed, reason) = read_output(&stdout[..], &stderr[..], shell, secrets, |l| engine.op_log(op, l)).await;
        return match status {
            Some(0) => Ok(printed),
            code => Err(reason.unwrap_or_else(|| code.map_or("The script was terminated.".into(), |c| format!("The script exited with status {c}.")))),
        };
    }
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
        let script = if shell == Shell::Powershell { ps_sendable(&script) } else { script };
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);

        let (printed, reason) = read_output(stdout, stderr, shell, secrets, |l| engine.op_log(op, l)).await;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(printed)
        } else {
            Err(reason.unwrap_or_else(|| match status.code() {
                Some(c) => format!("The script exited with status {c}."),
                None => "The script was terminated.".into(),
            }))
        }
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err(format!("Gave up after {} minutes.", timeout.as_secs() / 60)),
    }
}

/// Reads a script's stdout and stderr to their ends, passing each cleaned line
/// to `log`. Returns what it printed (stdout, line by line) and what stderr
/// says went wrong, if anything (see `Reason`).
///
/// Lines are read as bytes: Windows programs write in the console's code page
/// (config.cmd's "√ Connected to GitHub"), which isn't always UTF-8, and a line
/// like that mustn't end the reading (or leave the script blocked on a full pipe).
async fn read_output(
    stdout: impl AsyncRead + Unpin,
    stderr: impl AsyncRead + Unpin,
    shell: Shell,
    secrets: &[String],
    mut log: impl FnMut(String),
) -> (Vec<String>, Option<String>) {
    let (mut stdout, mut stderr) = (BufReader::new(stdout), BufReader::new(stderr));
    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let mut printed: Vec<String> = Vec::new();
    let mut reason = Reason::default();
    let (mut out_open, mut err_open) = (true, true);
    while out_open || err_open {
        tokio::select! {
            line = read_line(&mut stdout, &mut out_buf), if out_open => match line {
                Some(l) => {
                    let clean = clean_line(&l, secrets);
                    if !clean.trim().is_empty() {
                        log(clean);
                    }
                    printed.push(l);
                }
                None => out_open = false,
            },
            line = read_line(&mut stderr, &mut err_buf), if err_open => match line {
                Some(l) => {
                    for clean in stderr_lines(shell, &l, secrets) {
                        reason.see(shell, &clean);
                        log(clean);
                    }
                }
                None => err_open = false,
            },
        }
    }
    (printed, reason.get())
}

/// The next line (without its line ending), with bytes that aren't UTF-8
/// replaced; None at the end. Partial lines stay in `buf`, so a `select!` can
/// drop this and call it again.
async fn read_line(r: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>) -> Option<String> {
    match r.read_until(b'\n', buf).await {
        Ok(_) if !buf.is_empty() => {
            let line = String::from_utf8_lossy(buf).trim_end_matches(['\n', '\r']).to_string();
            buf.clear();
            Some(line)
        }
        _ => None,
    }
}

/// A stderr line as the lines of text it holds, cleaned, blank ones left out.
/// PowerShell writes its errors as CLIXML when stderr isn't a terminal.
fn stderr_lines(shell: Shell, line: &str, secrets: &[String]) -> Vec<String> {
    let text = if shell == Shell::Powershell { clixml_text(line) } else { line.to_string() };
    text.lines().map(|l| clean_line(l, secrets)).filter(|l| !l.trim().is_empty()).collect()
}

/// Why a script failed, from its stderr: its last `error: ...` line, else its
/// last line. Any `error:` line counts, not only the last few: PowerShell writes
/// its error records when it exits, after the trap's `error: ...` line.
#[derive(Default)]
struct Reason {
    error: Option<String>,
    last: Option<String>,
}

impl Reason {
    fn see(&mut self, shell: Shell, line: &str) {
        // The lines PowerShell puts under an error's message (`At line:1 char:5`,
        // `+ <the code>`, `+ CategoryInfo ...`) never say why.
        let line = line.trim();
        if shell == Shell::Powershell && (line.starts_with("At line:") || line.starts_with('+')) {
            return;
        }
        let line = line.trim_start_matches("Write-Error: ");
        // PowerShell can put the command's name first: "Name : error: ...".
        if let Some(message) = line.strip_prefix("error: ").or_else(|| line.split_once(" : error: ").map(|(_, m)| m)) {
            self.error = Some(message.to_string());
        }
        self.last = Some(line.to_string());
    }

    fn get(self) -> Option<String> {
        self.error.or(self.last)
    }
}

/// The text of PowerShell's CLIXML output: `#< CLIXML`, then `<Objs …>` lines
/// whose `<S S="Error">` (and other streams') strings hold the text, one or more
/// lines each. Progress records are left out, other lines kept as they are.
pub fn clixml_text(text: &str) -> String {
    if !text.contains("#< CLIXML") && !text.contains("<Objs ") {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "#< CLIXML" {
            continue;
        }
        if !trimmed.starts_with("<Objs ") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        for part in trimmed.split("<S S=\"").skip(1) {
            let Some((stream, s)) = part.split_once("\">") else { continue };
            let label = match stream {
                "warning" => "WARNING: ",
                "verbose" => "VERBOSE: ",
                "debug" => "DEBUG: ",
                _ => "",
            };
            for l in clixml_unescape(s.split("</S>").next().unwrap_or("")).lines() {
                if let Some(l) = without_reader(l) {
                    out.push_str(label);
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// PowerShell 5.1 starts an error record with the command it ran, Muxin's stdin
/// reader, wrapped to the console's width: "$ProgressPreference = ...", ...,
/// "}; $b.ToString() | Invoke-Expression : <the message>". Keeps the message.
fn without_reader(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.len() >= 20 && PS_READ_STDIN.contains(trimmed) {
        return None;
    }
    match trimmed.split_once(" : ") {
        Some((head, message)) if head.len() >= "Invoke-Expression".len() && PS_READ_STDIN.ends_with(head) => Some(message),
        _ => Some(line),
    }
}

/// A CLIXML string's text: XML's entities, then PowerShell's `_xHHHH_` escapes
/// (UTF-16 code units: line breaks, escape characters; `_x005F_` is "_").
fn clixml_unescape(s: &str) -> String {
    let xml = s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&");
    let mut out = String::with_capacity(xml.len());
    let mut units: Vec<u16> = Vec::new();
    let mut rest = xml.as_str();
    while let Some(c) = rest.chars().next() {
        let b = rest.as_bytes();
        let escaped = (b.len() >= 7 && b.starts_with(b"_x") && b[6] == b'_' && b[2..6].iter().all(u8::is_ascii_hexdigit))
            .then(|| u16::from_str_radix(&rest[2..6], 16).ok())
            .flatten();
        if let Some(unit) = escaped {
            units.push(unit);
            rest = &rest[7..];
            continue;
        }
        out.extend(char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out.extend(char::decode_utf16(units.drain(..)).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
    out
}

/// Runs `script` on the machine and returns what it printed, or why it
/// failed (its last `error: ...` line, else its last stderr line).
pub async fn output(spec: &MachineSpec, shell: Shell, script: String, timeout: Duration) -> Result<String, String> {
    spec.refuse_if_disconnected()?;
    if cfg!(target_os = "ios") && !spec.is_local() {
        let (stdout, stderr, status) = over_link(spec, script, timeout).await?;
        if status == Some(0) {
            return Ok(String::from_utf8_lossy(&stdout).to_string());
        }
        let mut reason = Reason::default();
        for line in String::from_utf8_lossy(&stderr).lines().flat_map(|l| stderr_lines(shell, l, &[])) {
            reason.see(shell, &line);
        }
        return Err(reason.get().unwrap_or_else(|| format!("It exited with status {}.", status.unwrap_or(-1))));
    }
    let mut cmd = command(spec, shell);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't run ssh: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("No stdin for the script")?;
    let script = if shell == Shell::Powershell { ps_sendable(&script) } else { script };
    let work = async move {
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
        child.wait_with_output().await.map_err(|e| e.to_string())
    };
    let out = tokio::time::timeout(timeout, work).await.map_err(|_| format!("Gave up after {} seconds.", timeout.as_secs()))??;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }
    let mut reason = Reason::default();
    for line in String::from_utf8_lossy(&out.stderr).lines().flat_map(|l| stderr_lines(shell, l, &[])) {
        reason.see(shell, &line);
    }
    Err(reason.get().unwrap_or_else(|| format!("It exited with status {}.", out.status.code().unwrap_or(-1))))
}

/// Muxin on an iPad can't run ssh: a script goes over the machine's direct
/// link instead, and its output comes when it's done, not line by line. Its
/// stdout, its stderr, and its exit status (None: terminated). The agent runs
/// it as ssh would (`sh -s`, PowerShell on Windows), not in the user's login
/// shell: these scripts are sh, and zsh, say, reads them otherwise (it
/// doesn't split `$LIST` into words).
async fn over_link(spec: &MachineSpec, script: String, timeout: Duration) -> Result<(Vec<u8>, Vec<u8>, Option<i32>), String> {
    let link = crate::fleet::link::current_or_direct(spec).await.ok_or_else(|| format!("Can't reach {}. It may be off, or not paired with this iPad.", spec.name))?;
    let req = serde_json::json!({ "op": "sh", "script": script, "timeout": timeout.as_secs(), "login": false });
    let reply = link.request(req, timeout + Duration::from_secs(10)).await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(reply["error"].as_str().unwrap_or("The script couldn't start.").to_string());
    }
    if reply["timed_out"].as_bool() == Some(true) {
        return Err(format!("Gave up after {} minutes.", timeout.as_secs() / 60));
    }
    let bytes = |k: &str| reply[k].as_str().unwrap_or("").as_bytes().to_vec();
    Ok((bytes("stdout"), bytes("stderr"), reply["status"].as_i64().map(|s| s as i32)))
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
                c.args(ps_stdin_args());
                c
            }
        }
    } else {
        let mut c = Command::new("ssh");
        c.args(probe::ssh_args(spec));
        c.arg(match shell {
            Shell::Sh => "sh -s".to_string(),
            Shell::Powershell => format!("powershell {}", ps_stdin_args().join(" ")),
        });
        c
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

    /// How PowerShell reads a single-quoted string (its tokenizer): any of
    /// ' ‘ ’ ‚ ‛ ends it, unless another one follows, which stands for that
    /// second one. Returns the text and whatever comes after the string.
    fn ps_read(quoted: &str) -> (String, String) {
        let is_quote = |c: char| matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}');
        let mut chars = quoted.chars().peekable();
        assert!(chars.next().is_some_and(is_quote));
        let mut text = String::new();
        while let Some(c) = chars.next() {
            if is_quote(c) {
                match chars.peek() {
                    Some(&n) if is_quote(n) => text.push(chars.next().unwrap()),
                    _ => return (text, chars.collect()),
                }
            } else {
                text.push(c);
            }
        }
        panic!("unterminated string: {quoted}");
    }

    #[test]
    fn powershell_strings_hold_any_quote() {
        for s in ["it's", "don’t", "‘quoted’", "a‚b‛c", "x’; Remove-Item -Recurse ~; ’", "''’’", "", "$(whoami) `n"] {
            let (text, rest) = ps_read(&ps_quote(s));
            assert_eq!(text, s);
            assert_eq!(rest, "", "{s:?} leaked out of its string");
        }
        assert_eq!(ps_quote("don’t"), "'don’’t'");
    }

    #[test]
    fn cleans_lines() {
        let secrets = vec!["AABBCCDDEEFF".to_string()];
        assert_eq!(clean_line("--token AABBCCDDEEFF --name x", &secrets), "--token •••••• --name x");
        assert_eq!(clean_line("\u{1b}[32m√ Connected\u{1b}[0m  ", &[]), "√ Connected");
        assert_eq!(clean_line("10%\r50%\r100%", &[]), "100%");
    }

    /// Reads `stdout` and `stderr` like `run_lines`: what it printed, what it
    /// logged, and the failure reason.
    async fn read(shell: Shell, stdout: &[u8], stderr: &[u8]) -> (Vec<String>, Vec<String>, Option<String>) {
        let mut logged = Vec::new();
        let (printed, reason) = read_output(stdout, stderr, shell, &[], |l| logged.push(l)).await;
        (printed, logged, reason)
    }

    /// An error record as Windows PowerShell 5.1 writes it to a redirected
    /// stderr (seen on Windows 11): the command that ran (the stdin reader) and
    /// the message, wrapped at spaces to the console's width, then where it was.
    fn clixml_record(message: &str) -> String {
        let mut pieces = vec![String::new()];
        for word in format!("{PS_READ_STDIN} : {message}").split_inclusive(' ') {
            if pieces.last().unwrap().len() + word.len() > 100 {
                pieces.push(String::new());
            }
            pieces.last_mut().unwrap().push_str(word);
        }
        pieces.extend(
            [
                "At line:3 char:8",
                "+ trap { Write-Error (\"error: \" + $_.Exception.Message); exit 1 }",
                "+        ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
                "    + CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException",
                "    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException",
                " ",
            ]
            .map(String::from),
        );
        let strings: String = pieces
            .iter()
            .map(|p| format!("<S S=\"Error\">{}_x000D__x000A_</S>", p.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")))
            .collect();
        format!("<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">{strings}</Objs>")
    }

    #[tokio::test]
    async fn powershell_errors_are_their_message() {
        // What Muxin 0.5.8 got from a failing script on Windows 11 (its reader).
        let seen = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\"><S S=\"Error\">$ProgressPreference = 'SilentlyContinue'; $b = New-Object Text.StringBuilder; while ((($l = _x000D__x000A_</S><S S=\"Error\">[Console]::In.ReadLine()) -ne $null) -and ($l -ne '#muxen-end')) { [void]$b.AppendLine($l) }; _x000D__x000A_</S><S S=\"Error\">$b.ToString() | Invoke-Expression : error: the folder doesn't exist &amp; &lt;stuff&gt;_x000D__x000A_</S><S S=\"Error\">At line:3 char:8_x000D__x000A_</S><S S=\"Error\">+ trap { Write-Error (\"error: \" + $_.Exception.Message); exit 1 }_x000D__x000A_</S><S S=\"Error\">+        ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~_x000D__x000A_</S><S S=\"Error\">    + CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException_x000D__x000A_</S><S S=\"Error\">    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException_x000D__x000A_</S><S S=\"Error\"> _x000D__x000A_</S></Objs>\r\n";
        let (_, logged, reason) = read(Shell::Powershell, b"", seen.as_bytes()).await;
        assert_eq!(reason.as_deref(), Some("the folder doesn't exist & <stuff>"));
        assert!(logged.iter().all(|l| !l.contains("<Objs") && !l.contains("CLIXML")), "{logged:?}");

        // With this reader, its wrapped text goes and the message stays.
        let long = format!("C:\\actions-runner\\x belongs to someone else, {}", "so that account could change what the runners run. ".repeat(3));
        let (_, logged, reason) = read(Shell::Powershell, b"", format!("#< CLIXML\r\n{}\r\n", clixml_record(&format!("error: {long}"))).as_bytes()).await;
        assert!(logged[0].starts_with("error: C:\\actions-runner\\x belongs"), "{logged:?}");
        assert!(logged.iter().all(|l| !l.contains("Invoke-Expression") && !l.contains("$ProgressPreference")), "{logged:?}");
        assert!(long.starts_with(reason.as_deref().unwrap()), "{reason:?}");

        // The traps print their error as a plain line; PowerShell writes its
        // records (a warning, a non-terminating error) as it exits, after it.
        let records = clixml_record("a non-terminating one").replacen("\">", "\"><S S=\"warning\">a _x005F_x000D_ warning</S>", 1);
        let stderr = format!("#< CLIXML\r\nerror: C:\\actions-runner\\café is in use\r\n{records}\r\n");
        let (_, logged, reason) = read(Shell::Powershell, b"", stderr.as_bytes()).await;
        assert_eq!(reason.as_deref(), Some("C:\\actions-runner\\café is in use"));
        assert!(logged.contains(&"WARNING: a _x000D_ warning".to_string()), "{logged:?}");
        assert!(logged.contains(&"a non-terminating one".to_string()), "{logged:?}");

        // Without an `error:` line: the last line that isn't the code or the category.
        let (_, _, reason) = read(Shell::Powershell, b"", clixml_record("Get-Item : Cannot find path").as_bytes()).await;
        assert_eq!(reason.as_deref(), Some("Get-Item : Cannot find path"));
        // sh's stderr is read as it is.
        let (_, logged, reason) = read(Shell::Sh, b"", b"error: no docker\n+ set -x line\n").await;
        assert_eq!((logged.len(), reason.as_deref()), (2, Some("no docker")));
    }

    #[test]
    fn unescapes_clixml() {
        assert_eq!(clixml_unescape("a_x000D__x000A_b"), "a\r\nb");
        assert_eq!(clixml_unescape("_x005F_x000D_ stays"), "_x000D_ stays");
        assert_eq!(clixml_unescape("_x001B_[31mred_x001B_[0m"), "\u{1b}[31mred\u{1b}[0m");
        assert_eq!(clixml_unescape("_xD83D__xDE00_ and _xD83D_ alone"), "😀 and \u{FFFD} alone");
        assert_eq!(clixml_unescape("_xZZZZ_ _x12_ é_x00E9_ &amp;lt; &quot;q&quot;"), "_xZZZZ_ _x12_ éé &lt; \"q\"");
        assert_eq!(clixml_text("plain\nlines"), "plain\nlines");
    }

    #[tokio::test]
    async fn keeps_reading_past_lines_that_are_not_utf8() {
        // config.cmd prints its check marks in the console's code page (here 437).
        let stdout = b"# Authentication\r\n\r\n\xfb Connected to GitHub\r\n\r\n# Runner Registration\r\nStarted r1 in the background\r\n\xe2\x9c\x93 r1 is installed";
        let (printed, logged, _) = read(Shell::Powershell, stdout, b"\xfb from stderr\r\nerror: it broke\r\n").await;
        assert_eq!(printed[2], "\u{FFFD} Connected to GitHub");
        assert_eq!(printed.last().map(String::as_str), Some("✓ r1 is installed"));
        assert_eq!(printed.len(), 7);
        for l in ["\u{FFFD} Connected to GitHub", "Started r1 in the background", "✓ r1 is installed", "\u{FFFD} from stderr", "error: it broke"] {
            assert!(logged.contains(&l.to_string()), "{l:?} not in {logged:?}");
        }
    }

    #[tokio::test]
    async fn lines_split_across_reads_stay_whole() {
        use tokio::io::AsyncWriteExt as _;
        let (mut out_w, out_r) = tokio::io::duplex(64);
        let (mut err_w, err_r) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            let pause = || tokio::time::sleep(Duration::from_millis(20));
            out_w.write_all(b"half a li").await.unwrap();
            pause().await;
            err_w.write_all(b"a warning\n").await.unwrap();
            pause().await;
            out_w.write_all(b"ne\n\xfb\n").await.unwrap();
        });
        let mut logged = Vec::new();
        let (printed, _) = read_output(out_r, err_r, Shell::Sh, &[], |l| logged.push(l)).await;
        writer.await.unwrap();
        assert_eq!(printed, ["half a line", "\u{FFFD}"]);
        assert_eq!(logged, ["a warning", "half a line", "\u{FFFD}"]);
    }

    #[test]
    fn picks_shells() {
        let spec: MachineSpec = toml::from_str("name = \"w\"\nhost = \"win\"").unwrap();
        let probe = ProbeResult { os: Some("windows".into()), ok: true, ..Default::default() };
        assert_eq!(shell_for(&spec, Some(&probe)), Shell::Powershell);
        assert_eq!(shell_for(&spec, None), Shell::Sh);
    }
}
