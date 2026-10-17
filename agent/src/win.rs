//! Windows' side of the machine's pages: its services, processes and GPUs,
//! listening ports and event logs, in the same shapes the Linux and macOS
//! code answers with. Windows keeps these behind PowerShell and WMI, so each
//! asks PowerShell once and reads its JSON.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::sys::no_window;

/// Runs a PowerShell script (encoded, so nothing on the way changes it) and
/// returns what it printed.
pub fn ps(script: &str) -> Result<String, String> {
    use base64::Engine as _;
    let full = format!("$ProgressPreference = 'SilentlyContinue'; $ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
    let utf16: Vec<u8> = full.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let out = no_window(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded]))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run PowerShell: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        Err(clixml_error(&String::from_utf8_lossy(&out.stderr)).unwrap_or_else(|| "PowerShell couldn't do that.".into()))
    }
}

/// The JSON a script printed (`ConvertTo-Json`): always an array.
pub fn ps_json(script: &str) -> Result<Vec<Value>, String> {
    let text = ps(script)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Value>(text).map_err(|e| format!("PowerShell said something unexpected: {e}"))? {
        Value::Array(a) => Ok(a),
        v => Ok(vec![v]),
    }
}

/// The message in PowerShell's error output (CLIXML when it isn't a terminal).
fn clixml_error(stderr: &str) -> Option<String> {
    let line = stderr.split("<S S=\"Error\">").nth(1).map(|s| s.split("</S>").next().unwrap_or(s))?;
    let text = line.replace("_x000D__x000A_", " ").replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    Some(text.trim().to_string()).filter(|t| !t.is_empty())
}

/// A PowerShell string literal. PowerShell takes ‘ ’ ‚ ‛ for single quotes
/// too, so each is doubled like ' is (else one would end the string).
pub fn quote(s: &str) -> String {
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

// --- Processes --------------------------------------------------------------

/// What each process uses right now: CPU (percent of one core, as `ps`
/// counts it) and private memory, by pid.
fn usage() -> HashMap<u32, (f64, u64)> {
    let rows = ps_json("Get-CimInstance Win32_PerfFormattedData_PerfProc_Process | Where-Object { $_.IDProcess -ne 0 } | Select-Object IDProcess, PercentProcessorTime, WorkingSetPrivate | ConvertTo-Json -Compress").unwrap_or_default();
    rows.iter()
        .filter_map(|r| Some((r["IDProcess"].as_u64()? as u32, (r["PercentProcessorTime"].as_f64().unwrap_or(0.), r["WorkingSetPrivate"].as_u64().unwrap_or(0)))))
        .collect()
}

/// Every process: pid, parent, user, name, command line, CPU, memory, seconds running.
pub fn processes() -> Result<Vec<Value>, String> {
    let (usage, rows) = std::thread::scope(|scope| {
        let u = scope.spawn(usage);
        // Owners need an elevated session; without one they're left out.
        let rows = ps_json(
            "$owners = @{}; try { Get-Process -IncludeUserName -ErrorAction Stop | ForEach-Object { $owners[$_.Id] = $_.UserName } } catch {}
             $now = Get-Date
             Get-CimInstance Win32_Process | ForEach-Object { [pscustomobject]@{ pid = [int]$_.ProcessId; ppid = [int]$_.ParentProcessId; name = $_.Name; command = $_.CommandLine; seconds = $(if ($_.CreationDate) { [Math]::Max([int64]0, [int64]($now - $_.CreationDate).TotalSeconds) } else { 0 }); user = $owners[[int]$_.ProcessId] } } | ConvertTo-Json -Compress",
        );
        (u.join().unwrap_or_default(), rows)
    });
    let me = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    let agent = std::process::id() as u64;
    Ok(rows?
        .into_iter()
        // Not the PowerShell this asked (its parent is the agent).
        .filter(|r| r["pid"].as_u64().is_some_and(|p| p != 0) && r["ppid"].as_u64() != Some(agent))
        .map(|r| {
            let pid = r["pid"].as_u64().unwrap_or(0) as u32;
            let (cpu, memory) = usage.get(&pid).copied().unwrap_or((0., 0));
            let user = r["user"].as_str().unwrap_or("").to_string();
            let short = user.rsplit('\\').next().unwrap_or("").to_lowercase();
            let name = r["name"].as_str().unwrap_or("").trim_end_matches(".exe").to_string();
            let command = r["command"].as_str().filter(|c| !c.is_empty()).map(str::to_string).unwrap_or_else(|| name.clone());
            let user = user.rsplit('\\').next().unwrap_or("").to_string();
            json!({ "pid": pid, "ppid": r["ppid"], "user": user, "name": name, "command": command, "cpu": cpu, "memory": memory, "seconds": r["seconds"], "yours": !me.is_empty() && short == me })
        })
        .collect())
}

/// The GPUs: NVIDIA's through nvidia-smi (see procs.rs); the others through
/// Windows' own counters (how busy, dedicated memory in use).
pub fn gpus() -> Vec<Value> {
    let rows = ps_json(
        "$busy = @{}; $mem = @{}
         try { (Get-Counter '\\GPU Engine(*)\\Utilization Percentage' -ErrorAction Stop).CounterSamples | ForEach-Object { if ($_.InstanceName -match 'luid_(0x[0-9a-f]+_0x[0-9a-f]+)') { $busy[$matches[1]] += $_.CookedValue } } } catch {}
         try { (Get-Counter '\\GPU Adapter Memory(*)\\Dedicated Usage' -ErrorAction Stop).CounterSamples | ForEach-Object { if ($_.InstanceName -match 'luid_(0x[0-9a-f]+_0x[0-9a-f]+)') { $mem[$matches[1]] = $_.CookedValue } } } catch {}
         $top = ($busy.Values | Measure-Object -Maximum).Maximum
         $used = ($mem.Values | Measure-Object -Maximum).Maximum
         Get-CimInstance Win32_VideoController | Where-Object { $_.Name -notmatch 'Basic Display|Basic Render|Remote Display|Hyper-V' } | ForEach-Object { [pscustomobject]@{ name = $_.Name; memory_total = [int64]$_.AdapterRAM; busy = $top; memory_used = $used } } | ConvertTo-Json -Compress",
    )
    .unwrap_or_default();
    rows.into_iter()
        .filter(|r| !r["name"].as_str().unwrap_or("").contains("NVIDIA"))
        .map(|r| {
            let vendor = match r["name"].as_str().unwrap_or("") {
                n if n.contains("AMD") || n.contains("Radeon") => "amd",
                n if n.contains("Intel") => "intel",
                _ => "other",
            };
            // AdapterRAM is 32 bits: 4 GB and more reads as 4 GB or less; leave those out.
            let total = r["memory_total"].as_u64().filter(|t| *t > 0 && *t < 0xFFFF_0000);
            json!({ "vendor": vendor, "name": r["name"], "busy": r["busy"].as_f64().map(|b| b.min(100.)), "memory_used": r["memory_used"].as_f64().map(|m| m as u64), "memory_total": total })
        })
        .collect()
}

// --- Services ---------------------------------------------------------------

/// Windows services: name, what it is, state, whether it starts at boot, and
/// what its process uses.
pub fn services(stats: bool) -> (Vec<Value>, Option<String>) {
    let rows = match ps_json("Get-CimInstance Win32_Service | Select-Object Name, DisplayName, State, StartMode, ProcessId, ExitCode, DelayedAutoStart | ConvertTo-Json -Compress") {
        Ok(r) => r,
        Err(e) => return (Vec::new(), Some(format!("Couldn't list Windows' services: {e}"))),
    };
    let usage = if stats { usage() } else { HashMap::new() };
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let start = r["StartMode"].as_str().unwrap_or("");
            let exit = r["ExitCode"].as_u64().unwrap_or(0);
            let state = match r["State"].as_str().unwrap_or("") {
                "Running" => "running",
                "Stopped" if start == "Auto" && exit != 0 && exit != 1077 && r["DelayedAutoStart"] != true => "failed",
                "Stopped" => "stopped",
                "Start Pending" => "starting",
                "Stop Pending" => "stopping",
                "Paused" => "paused",
                _ => "unknown",
            };
            let at_boot = match start {
                "Auto" => "enabled",
                "Disabled" => "masked",
                _ => "disabled",
            };
            let pid = r["ProcessId"].as_u64().unwrap_or(0) as u32;
            let (cpu, memory) = (state == "running" && pid != 0).then(|| usage.get(&pid).copied()).flatten().map(|(c, m)| (Some(c), Some(m))).unwrap_or((None, None));
            json!({
                "kind": "service",
                "target": r["Name"],
                "name": r["Name"],
                "detail": r["DisplayName"],
                "state": state,
                "at_boot": at_boot,
                "user": false,
                "cpu": cpu,
                "memory": memory,
            })
        })
        .collect();
    out.sort_by_key(|u| (crate::services::rank(u["state"].as_str()), u["name"].as_str().unwrap_or("").to_lowercase()));
    (out, None)
}

/// Starts, stops or restarts a service; `enable` starts it at boot, `disable` leaves it to start by hand.
pub fn service_action(name: &str, action: &str) -> Result<(), String> {
    let n = quote(name);
    let script = match action {
        "start" => format!("Start-Service -Name {n}"),
        "stop" => format!("Stop-Service -Name {n} -Force"),
        "restart" => format!("Restart-Service -Name {n} -Force"),
        "enable" => format!("Set-Service -Name {n} -StartupType Automatic"),
        "disable" => format!("Set-Service -Name {n} -StartupType Manual"),
        other => return Err(format!("Can't {other} a service.")),
    };
    ps(&script).map(|_| ()).map_err(|e| if e.contains("ccess is denied") || e.contains("ermission") { "Windows wants an administrator for that.".into() } else { e })
}

// --- Ports ------------------------------------------------------------------

/// TCP ports something listens on: (port, address, program, pid).
pub fn listening() -> Vec<(u16, String, String, Option<u32>)> {
    let rows = ps_json(
        "$names = @{}; Get-Process | ForEach-Object { $names[$_.Id] = $_.ProcessName }
         Get-NetTCPConnection -State Listen | ForEach-Object { [pscustomobject]@{ port = [int]$_.LocalPort; address = $_.LocalAddress; pid = [int]$_.OwningProcess; process = $names[[int]$_.OwningProcess] } } | ConvertTo-Json -Compress",
    )
    .unwrap_or_default();
    rows.into_iter()
        .filter_map(|r| {
            let port = r["port"].as_u64()? as u16;
            let pid = r["pid"].as_u64().map(|p| p as u32).filter(|p| *p > 4);
            // The system itself (pid 4) answers SMB, RPC and http.sys's ports.
            let process = if r["pid"].as_u64() == Some(4) { "System".into() } else { r["process"].as_str().unwrap_or("").to_string() };
            Some((port, r["address"].as_str().unwrap_or("").to_string(), process, pid))
        })
        .collect()
}

// --- Logs -------------------------------------------------------------------

/// The event logs worth reading, as log sources.
pub fn event_logs() -> Vec<Value> {
    [("System", "Windows' system events"), ("Application", "Programs' events"), ("Security", "Sign-ins and audits (administrators)")]
        .iter()
        .map(|(log, detail)| json!({ "kind": "system", "target": log, "name": log, "detail": detail }))
        .collect()
}

/// Windows services, to read their events (System log, by service).
pub fn service_sources() -> Vec<Value> {
    let rows = ps_json("Get-CimInstance Win32_Service | Where-Object { $_.State -eq 'Running' -or $_.StartMode -eq 'Auto' } | Select-Object Name, DisplayName, State | ConvertTo-Json -Compress").unwrap_or_default();
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "kind": "service", "target": r["Name"], "name": r["Name"], "detail": r["DisplayName"], "user": false, "state": if r["State"] == "Running" { "running" } else { "stopped" } }))
        .collect();
    out.sort_by_key(|u| u["name"].as_str().unwrap_or("").to_lowercase());
    out
}

/// The PowerShell that prints a log's last `lines` (then, when `follow`,
/// what comes, checking every second): an event log, a service's events, a file.
pub fn log_script(kind: &str, target: &str, lines: u64, follow: bool) -> Result<String, String> {
    // An event as one line: time, level, source, message.
    let show = "ForEach-Object { '{0} {1,-7} {2}: {3}' -f $_.TimeCreated.ToString('yyyy-MM-ddTHH:mm:ss'), $_.LevelDisplayName, $_.ProviderName, (($_.Message -replace '\\s+', ' ').Trim()) }";
    let events = |filter: String| {
        let mut s = format!("$f = {filter}\n$e = @(Get-WinEvent -FilterHashtable $f -MaxEvents {lines} -ErrorAction SilentlyContinue)\n[array]::Reverse($e)\n$e | {show}\n");
        if follow {
            s.push_str(&format!("$last = if ($e.Count) {{ $e[-1].RecordId }} else {{ 0 }}\nwhile ($true) {{ Start-Sleep 1; $new = @(Get-WinEvent -FilterHashtable $f -MaxEvents 200 -ErrorAction SilentlyContinue | Where-Object {{ $_.RecordId -gt $last }}); if ($new.Count) {{ [array]::Reverse($new); $new | {show}; $last = $new[-1].RecordId }} }}\n"));
        }
        s
    };
    match kind {
        "system" | "kernel" => {
            let log = if target.is_empty() { "System" } else { target };
            if !log.chars().all(|c| c.is_ascii_alphanumeric() || "-/ ".contains(c)) {
                return Err("That log's name has odd characters.".into());
            }
            Ok(events(format!("@{{ LogName = {} }}", quote(log))))
        }
        "service" => {
            if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric() || "._-$".contains(c)) {
                return Err("Which service?".into());
            }
            // Its events: the Service Control Manager's about it (started,
            // stopped, failed), and its own when it writes any, by time.
            // (Asking for a source that has never written one fails, so each is asked alone.)
            let script = format!(
                "$n = {name}\n\
                 $disp = (Get-Service -Name $n -ErrorAction SilentlyContinue).DisplayName; if (-not $disp) {{ $disp = $n }}\n\
                 function Get-Its($since, $max) {{\n\
                   $a = @(Get-WinEvent -FilterHashtable @{{ LogName = 'System'; ProviderName = 'Service Control Manager'; StartTime = $since }} -MaxEvents 3000 -ErrorAction SilentlyContinue | Where-Object {{ $_.Message -match [regex]::Escape($disp) }} | Select-Object -First $max)\n\
                   $b = @(try {{ Get-WinEvent -FilterHashtable @{{ LogName = 'System', 'Application'; ProviderName = $n; StartTime = $since }} -MaxEvents $max -ErrorAction Stop }} catch {{ }})\n\
                   @($a + $b) | Sort-Object TimeCreated | Select-Object -Last $max\n\
                 }}\n\
                 $e = @(Get-Its ([datetime]::MinValue) {lines})\n\
                 $e | {show}\n",
                name = quote(target)
            );
            let follow_part = format!("$last = if ($e.Count) {{ $e[-1].TimeCreated }} else {{ Get-Date }}\nwhile ($true) {{ Start-Sleep 2; $new = @(Get-Its $last.AddMilliseconds(1) 200); if ($new.Count) {{ $new | {show}; $last = $new[-1].TimeCreated }} }}\n");
            Ok(if follow { script + &follow_part } else { script })
        }
        "file" => {
            let path = crate::sys::expand_home(target);
            std::fs::File::open(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => format!("{target} is readable only by administrators."),
                std::io::ErrorKind::NotFound => format!("There's no {target} on this machine."),
                _ => format!("Couldn't open {target}: {e}"),
            })?;
            let wait = if follow { " -Wait" } else { "" };
            Ok(format!("Get-Content -LiteralPath {} -Tail {lines}{wait}", quote(&path.display().to_string())))
        }
        other => Err(format!("No log source called \"{other}\".")),
    }
}

/// The program and arguments that run a log script (for `logs::command`).
pub fn log_command(script: String) -> (String, Vec<String>) {
    use base64::Engine as _;
    let full = format!("$ProgressPreference = 'SilentlyContinue'; [Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
    let utf16: Vec<u8> = full.encode_utf16().flat_map(u16::to_le_bytes).collect();
    ("powershell".into(), vec!["-NoProfile".into(), "-NonInteractive".into(), "-EncodedCommand".into(), base64::engine::general_purpose::STANDARD.encode(utf16)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_powershell_errors() {
        let err = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\"><S S=\"Error\">Cannot find any service with service name 'nope'._x000D__x000A_</S></Objs>";
        assert_eq!(clixml_error(err).as_deref(), Some("Cannot find any service with service name 'nope'."));
        assert_eq!(quote("it's"), "'it''s'");
        // PowerShell's other single quotes are doubled too.
        assert_eq!(quote("don\u{2019}t \u{2018}x\u{201A}\u{201B}"), "'don\u{2019}\u{2019}t \u{2018}\u{2018}x\u{201A}\u{201A}\u{201B}\u{201B}'");
    }
}
