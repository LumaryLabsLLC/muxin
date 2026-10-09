//! Files on the machines: listing folders, and copying files and folders from
//! one machine to another (this computer included).
//!
//! A copy goes through this app: a tar stream from the machine with the files,
//! relayed to `tar -x` on the other, over each machine's live link (see
//! agent/src/files.rs). The machines never need to reach each other. Both ends
//! hold back to a window, so a slow one doesn't pile data up here. Without a
//! link (Windows, no agent) the stream runs through `ssh … tar` instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use super::link::{self, Link};
use super::model::OpProgress;
use super::sessions::agent;
use super::Engine;
use crate::config::MachineSpec;
use crate::util;

/// Frame kinds of a copy (see agent/src/files.rs).
const ACK: u8 = 30;
const END: u8 = 31;
/// What either end may have outstanding.
const WINDOW: u64 = 8 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    /// "file" | "dir" | "link"
    pub kind: String,
    /// Opens like a folder (a folder, or a link to one).
    pub folder: bool,
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub machine_id: String,
    /// The folder, absolute.
    pub path: String,
    /// The machine user's home folder (for showing paths as ~/...).
    pub home: String,
    pub entries: Vec<FileEntry>,
    /// There were more entries than listed.
    pub more: bool,
}

/// What to copy where.
#[derive(Debug, Clone, Deserialize)]
pub struct CopyRequest {
    /// The machine with the files.
    pub from: String,
    /// The folder they're in ("~/build", "/srv/data").
    pub dir: String,
    /// Files and folders in it to copy.
    pub names: Vec<String>,
    /// The machine to copy them to, and the folder there (made if missing;
    /// files with the same names are replaced).
    pub to: String,
    pub to_dir: String,
}

impl Engine {
    /// What's in a folder on a machine, folders first.
    pub async fn list_files(self: &Arc<Self>, machine_id: &str, path: &str) -> Result<Listing, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo_listing(&spec, path));
        }
        let l = self.files_link(&spec).await.ok_or_else(|| format!("Browsing {} needs Muxen's agent there (Linux or macOS).", spec.name))?;
        let r = l.request(json!({ "op": "files", "path": if path.trim().is_empty() { "~" } else { path } }), Duration::from_secs(20)).await?;
        if r["ok"].as_bool() != Some(true) {
            return Err(r["error"].as_str().unwrap_or("Couldn't list that folder.").to_string());
        }
        Ok(Listing {
            machine_id: spec.id.clone(),
            path: r["path"].as_str().unwrap_or("").to_string(),
            home: r["home"].as_str().unwrap_or("").to_string(),
            entries: serde_json::from_value(r["entries"].clone()).unwrap_or_default(),
            more: r["more"].as_bool().unwrap_or(false),
        })
    }

    /// Starts copying in the background; returns the operation that tracks it.
    pub fn copy_files(self: &Arc<Self>, req: CopyRequest) -> Result<u64, String> {
        let (from, _) = self.machine(&req.from).ok_or_else(|| format!("No machine called {}.", req.from))?;
        let (to, _) = self.machine(&req.to).ok_or_else(|| format!("No machine called {}.", req.to))?;
        if self.demo_world().is_some() {
            return Err("The demo has no files to copy.".into());
        }
        let names: Vec<String> = req.names.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
        if names.is_empty() {
            return Err("Pick something to copy.".into());
        }
        if let Some(bad) = names.iter().find(|n| n.contains('/') || *n == "." || *n == "..") {
            return Err(format!("\"{bad}\" isn't a name in the folder; give the folder as `dir` and the names inside it."));
        }
        if req.to_dir.trim().is_empty() {
            return Err("Say which folder to copy to.".into());
        }
        let what = match names.as_slice() {
            [one] => one.clone(),
            many => format!("{} items", many.len()),
        };
        let title = if from.id == to.id { format!("Copy {what} on {}", from.name) } else { format!("Copy {what} from {} to {}", from.name, to.name) };
        let id = self.begin_op("copy", title, Some(to.id.clone()), None);
        let cancel = self.op_cancellable(id);
        let me = self.clone();
        self.runtime().spawn(async move {
            let started = Instant::now();
            let result = me.run_copy(id, &from, &req.dir, &names, &to, &req.to_dir, &cancel).await;
            match &result {
                Ok(bytes) => {
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    me.op_log(id, format!("Copied {} in {:.1}s ({}/s).", util::bytes(*bytes), secs, util::bytes((*bytes as f64 / secs) as u64)));
                }
                Err(e) => me.op_log(id, e.clone()),
            }
            me.op_finish(id, result.map(|_| ()));
        });
        Ok(id)
    }

    /// Copies and waits; returns the bytes moved (the tar stream's size).
    #[allow(clippy::too_many_arguments)]
    async fn run_copy(self: &Arc<Self>, id: u64, from: &MachineSpec, dir: &str, names: &[String], to: &MachineSpec, to_dir: &str, cancel: &AtomicBool) -> Result<u64, String> {
        let src_place = format!("{}:{}", from.id, dir);
        let dst_place = format!("{}:{}", to.id, to_dir);
        self.op_log(id, format!("{} → {}", names.iter().map(|n| format!("{src_place}/{n}")).collect::<Vec<_>>().join(", "), dst_place));
        let (mut src, total) = Source::open(self, from, dir, names).await?;
        if let Some(t) = total {
            self.op_log(id, format!("{} to copy.", util::bytes(t)));
        }
        let mut dst = Sink::open(self, to, to_dir).await?;
        let (mut done, started) = (0u64, Instant::now());
        let mut last = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                src.stop();
                dst.stop();
                return Err("Stopped.".into());
            }
            match src.next().await {
                Ok(Some(chunk)) => {
                    dst.write(&chunk).await?;
                    src.ack(chunk.len());
                    done += chunk.len() as u64;
                    if last.elapsed() >= Duration::from_millis(250) {
                        last = Instant::now();
                        let rate = done as f64 / started.elapsed().as_secs_f64().max(0.001);
                        // The tar stream runs a little over the files' size.
                        let total = total.map(|t| t.max(done));
                        self.op_progress(id, OpProgress { done, total, rate: Some(rate) });
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    dst.stop();
                    return Err(e);
                }
            }
        }
        dst.finish().await?;
        self.op_progress(id, OpProgress { done, total: Some(done), rate: None });
        Ok(done)
    }

    /// The machine's live link, with an agent that knows files (updated if
    /// needed). None: no agent there (Windows), or it can't be reached that way.
    async fn files_link(self: &Arc<Self>, spec: &MachineSpec) -> Option<Arc<Link>> {
        let (_, probe) = self.machine(&spec.id)?;
        agent::ensure(spec, probe.as_ref()).await.ok()?;
        let l = link::get(spec).await?;
        // Agents before 0.2.3 don't do files.
        let v: Vec<u32> = l.version().split('.').map(|p| p.parse().unwrap_or(0)).collect();
        (v.as_slice() >= [0, 2, 3].as_slice()).then_some(l)
    }
}

/// Where a copy's tar stream comes from.
enum Source {
    Link { link: Arc<Link>, channel: u32, rx: mpsc::UnboundedReceiver<(u8, Vec<u8>)> },
    Process { child: Child, stdout: ChildStdout, buf: Vec<u8> },
}

impl Source {
    /// Starts packing; also returns how many bytes there are, when known.
    async fn open(engine: &Arc<Engine>, spec: &MachineSpec, dir: &str, names: &[String]) -> Result<(Source, Option<u64>), String> {
        if let Some(l) = engine.files_link(spec).await {
            let (channel, rx) = l.open_channel();
            let r = l.request(json!({ "op": "pack", "channel": channel, "dir": dir, "names": names, "window": WINDOW }), Duration::from_secs(30)).await;
            return match r {
                Ok(r) if r["ok"].as_bool() == Some(true) => Ok((Source::Link { link: l, channel, rx }, r["size"].as_u64())),
                Ok(r) => {
                    l.close_channel(channel);
                    Err(format!("{}: {}", spec.name, r["error"].as_str().unwrap_or("couldn't read the files")))
                }
                Err(e) => {
                    l.close_channel(channel);
                    Err(format!("{}: {e}", spec.name))
                }
            };
        }
        let script = format!("tar -C {} -cf - {}", shell_path(dir), names.iter().map(|n| crate::ops::remote::sh_quote(&format!("./{n}"))).collect::<Vec<_>>().join(" "));
        let mut child = shell(spec, &script).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| format!("{}: {e}", spec.name))?;
        let stdout = child.stdout.take().ok_or("no output")?;
        Ok((Source::Process { child, stdout, buf: vec![0; 64 * 1024] }, None))
    }

    /// The next piece of the stream; None at its end.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        match self {
            Source::Link { rx, .. } => loop {
                match rx.recv().await {
                    Some((link::DATA, data)) => return Ok(Some(data)),
                    Some((END, result)) => return end_result(&result).map(|_| None),
                    Some(_) => continue,
                    None => return Err("The connection to the machine with the files dropped.".into()),
                }
            },
            Source::Process { child, stdout, buf } => {
                let n = stdout.read(buf).await.map_err(|e| e.to_string())?;
                if n > 0 {
                    return Ok(Some(buf[..n].to_vec()));
                }
                let out = child.wait_with_output_ref().await?;
                if out.0.success() { Ok(None) } else { Err(format!("Reading the files failed: {}", out.1.trim())) }
            }
        }
    }

    /// The other end took this much; more may come.
    fn ack(&self, n: usize) {
        if let Source::Link { link, channel, .. } = self {
            link.send(*channel, ACK, &(n as u64).to_be_bytes());
        }
    }

    fn stop(&mut self) {
        match self {
            Source::Link { link, channel, .. } => link.close_channel(*channel),
            Source::Process { child, .. } => {
                let _ = child.start_kill();
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Source::Link { link, channel, .. } = self {
            link.close_channel(*channel);
        }
    }
}

/// Where a copy's tar stream goes.
enum Sink {
    Link { link: Arc<Link>, channel: u32, rx: mpsc::UnboundedReceiver<(u8, Vec<u8>)>, inflight: u64, ended: Option<Vec<u8>> },
    Process { child: Child, stdin: Option<ChildStdin> },
}

impl Sink {
    async fn open(engine: &Arc<Engine>, spec: &MachineSpec, dir: &str) -> Result<Sink, String> {
        if let Some(l) = engine.files_link(spec).await {
            let (channel, rx) = l.open_channel();
            let r = l.request(json!({ "op": "unpack", "channel": channel, "dir": dir }), Duration::from_secs(30)).await;
            return match r {
                Ok(r) if r["ok"].as_bool() == Some(true) => Ok(Sink::Link { link: l, channel, rx, inflight: 0, ended: None }),
                Ok(r) => {
                    l.close_channel(channel);
                    Err(format!("{}: {}", spec.name, r["error"].as_str().unwrap_or("couldn't write there")))
                }
                Err(e) => {
                    l.close_channel(channel);
                    Err(format!("{}: {e}", spec.name))
                }
            };
        }
        let script = format!("mkdir -p {d} && tar -C {d} -xf -", d = shell_path(dir));
        let mut child = shell(spec, &script).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().map_err(|e| format!("{}: {e}", spec.name))?;
        let stdin = child.stdin.take();
        Ok(Sink::Process { child, stdin })
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), String> {
        match self {
            Sink::Link { link, channel, rx, inflight, ended } => {
                link.send(*channel, link::DATA, data);
                *inflight += data.len() as u64;
                // Hold on while the machine is behind.
                while *inflight > WINDOW {
                    match rx.recv().await {
                        Some((ACK, n)) if n.len() >= 8 => *inflight = inflight.saturating_sub(u64::from_be_bytes(n[..8].try_into().unwrap_or_default())),
                        Some((END, result)) => {
                            *ended = Some(result.clone());
                            return end_result(&result).and(Err("Writing the files stopped early.".into()));
                        }
                        Some(_) => {}
                        None => return Err("The connection to the machine receiving the files dropped.".into()),
                    }
                }
                Ok(())
            }
            Sink::Process { stdin, .. } => match stdin.as_mut() {
                Some(s) => s.write_all(data).await.map_err(|e| format!("Writing the files failed: {e}")),
                None => Ok(()),
            },
        }
    }

    /// That was all: waits for the files to be written.
    async fn finish(&mut self) -> Result<(), String> {
        match self {
            Sink::Link { link, channel, rx, ended, .. } => {
                if let Some(result) = ended.take() {
                    return end_result(&result);
                }
                link.send(*channel, END, &[]);
                loop {
                    match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
                        Ok(Some((END, result))) => return end_result(&result),
                        Ok(Some(_)) => {}
                        Ok(None) => return Err("The connection to the machine receiving the files dropped.".into()),
                        Err(_) => return Err("The machine receiving the files didn't finish writing them.".into()),
                    }
                }
            }
            Sink::Process { child, stdin } => {
                drop(stdin.take());
                let out = child.wait_with_output_ref().await?;
                if out.0.success() { Ok(()) } else { Err(format!("Writing the files failed: {}", out.1.trim())) }
            }
        }
    }

    fn stop(&mut self) {
        match self {
            Sink::Link { link, channel, .. } => link.close_channel(*channel),
            Sink::Process { child, .. } => {
                let _ = child.start_kill();
            }
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Sink::Link { link, channel, .. } = self {
            link.close_channel(*channel);
        }
    }
}

/// An END frame's result: ok, or why not.
fn end_result(payload: &[u8]) -> Result<(), String> {
    let v: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    if v["ok"].as_bool() == Some(true) { Ok(()) } else { Err(v["error"].as_str().unwrap_or("The copy failed.").to_string()) }
}

/// A folder for a shell command: `~/x` stays relative to the home folder.
fn shell_path(dir: &str) -> String {
    let dir = dir.trim();
    match dir.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", crate::ops::remote::sh_quote(rest)),
        None if dir == "~" || dir.is_empty() => "\"$HOME\"".into(),
        None => crate::ops::remote::sh_quote(dir),
    }
}

/// A shell command on the machine: here, or over ssh.
fn shell(spec: &MachineSpec, script: &str) -> Command {
    let mut cmd = if spec.is_local() {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    } else {
        let mut c = Command::new("ssh");
        c.args(crate::probe::ssh_args(spec)).arg(script);
        c
    };
    // No AppleDouble (._) files from a Mac.
    cmd.env("COPYFILE_DISABLE", "1").stderr(std::process::Stdio::piped()).kill_on_drop(true);
    cmd
}

trait WaitOutput {
    /// Waits for the process; its status and what it printed on stderr.
    async fn wait_with_output_ref(&mut self) -> Result<(std::process::ExitStatus, String), String>;
}

impl WaitOutput for Child {
    async fn wait_with_output_ref(&mut self) -> Result<(std::process::ExitStatus, String), String> {
        let mut err = String::new();
        if let Some(mut stderr) = self.stderr.take() {
            let _ = stderr.read_to_string(&mut err).await;
        }
        let status = self.wait().await.map_err(|e| e.to_string())?;
        Ok((status, err))
    }
}

/// A made-up home folder for the demo: a few projects to browse.
fn demo_listing(spec: &MachineSpec, path: &str) -> Listing {
    let home = if spec.platform.as_deref().is_some_and(|p| p.starts_with("darwin")) { "/Users/ci" } else { "/home/ci" };
    let path = path.trim();
    let abs = match path.strip_prefix('~') {
        Some(rest) => format!("{home}{rest}"),
        None if path.is_empty() => home.to_string(),
        None if !path.starts_with('/') => format!("{home}/{path}"),
        None => path.to_string(),
    };
    let abs = if abs.len() > 1 { abs.trim_end_matches('/').to_string() } else { abs };
    let (folders, files): (&[&str], &[&str]) = match abs.strip_prefix(home).map(|r| r.trim_start_matches('/')) {
        Some("") => (&[".config", "code", "Documents", "Downloads", "notes"], &["todo.md"]),
        Some("code") => (&["api", "checkout", "desktop", "infra", "ios-app", "muxen", "web"], &[]),
        Some(p) if p.starts_with("code/") && !p["code/".len()..].contains('/') => (&[".github", "docs", "src", "tests"], &["README.md", "package.json"]),
        _ => (&[], &[]),
    };
    let entry = |name: &&str, folder: bool| FileEntry { name: name.to_string(), kind: if folder { "dir" } else { "file" }.into(), folder, size: (!folder).then_some(2048), modified: None };
    Listing {
        machine_id: spec.id.clone(),
        path: abs,
        home: home.to_string(),
        entries: folders.iter().map(|n| entry(n, true)).chain(files.iter().map(|n| entry(n, false))).collect(),
        more: false,
    }
}
