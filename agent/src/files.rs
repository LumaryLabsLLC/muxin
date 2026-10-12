//! Files on this machine, for copying between machines through the app:
//! listing a folder, and packing or unpacking a tar stream on a link channel.
//!
//! A copy is two channels: `pack` on the machine with the files sends a tar
//! stream (DATA frames), `unpack` on the other feeds it to `tar -x`. Each side
//! holds back until the app has caught up (ACK frames), so a slow machine
//! never makes the app buffer more than a window's worth. END ends a stream:
//! from the app it means "that's all the data", from here it carries the
//! result. CLOSE stops it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The app has taken this many more bytes (u64, big-endian).
pub const ACK: u8 = 30;
/// The end of a stream (see the module docs).
pub const END: u8 = 31;

const CHUNK: usize = 64 * 1024;
const WINDOW: u64 = 8 << 20;
/// Most entries a listing returns.
const MAX_ENTRIES: usize = 5000;

pub fn expand_home(path: &str) -> PathBuf {
    crate::sys::expand_home(path)
}

/// `{"op": "files", "path": "~/x"}`: what's in a folder, folders first.
pub fn list(req: &Value) -> Value {
    let path = expand_home(req["path"].as_str().unwrap_or("~"));
    let read = match std::fs::read_dir(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory || path.is_file() => {
            return json!({ "ok": false, "error": format!("{} is a file, not a folder.", path.display()) });
        }
        Err(e) => return json!({ "ok": false, "error": format!("Can't open {}: {e}", path.display()) }),
    };
    let mut entries: Vec<Value> = Vec::new();
    let mut more = false;
    for entry in read.flatten() {
        if entries.len() >= MAX_ENTRIES {
            more = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(meta) = entry.path().symlink_metadata() else { continue };
        let kind = if meta.file_type().is_symlink() {
            "link"
        } else if meta.is_dir() {
            "dir"
        } else {
            "file"
        };
        // A link to a folder can be opened like one.
        let opens = kind == "dir" || (kind == "link" && entry.path().is_dir());
        let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
        entries.push(json!({
            "name": name,
            "kind": kind,
            "folder": opens,
            "size": if kind == "file" { Some(meta.len()) } else { None },
            "modified": modified,
        }));
    }
    entries.sort_by(|a, b| {
        let (fa, fb) = (a["folder"].as_bool().unwrap_or(false), b["folder"].as_bool().unwrap_or(false));
        fb.cmp(&fa).then_with(|| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()))
    });
    let home = std::env::var("HOME").unwrap_or_default();
    json!({ "ok": true, "path": path.display().to_string(), "home": home, "entries": entries, "more": more })
}

/// How much is in these (files and their bytes), within a moment; None when
/// there's too much to count quickly.
fn measure(dir: &Path, names: &[String]) -> (Option<u64>, u64) {
    let started = Instant::now();
    let (mut bytes, mut files) = (0u64, 0u64);
    let mut stack: Vec<PathBuf> = names.iter().map(|n| dir.join(n)).collect();
    while let Some(p) = stack.pop() {
        if started.elapsed() > Duration::from_secs(3) {
            return (None, files);
        }
        let Ok(meta) = p.symlink_metadata() else { continue };
        if meta.is_dir() {
            if let Ok(read) = std::fs::read_dir(&p) {
                stack.extend(read.flatten().map(|e| e.path()));
            }
        } else {
            files += 1;
            bytes += meta.len();
        }
    }
    (Some(bytes), files)
}

/// Bytes sent and bytes the app has taken, for holding back; and the tar
/// process, until it's done.
#[derive(Default)]
pub struct Credit {
    state: Mutex<(u64, bool)>,
    changed: Condvar,
    pid: Mutex<Option<u32>>,
}

impl Credit {
    /// Stops the stream (CLOSE): tar is ended if it's still going.
    pub fn close(&self) {
        self.stop();
        if let Some(pid) = self.pid.lock().unwrap().take() {
            kill(pid);
        }
    }

    pub fn ack(&self, n: u64) {
        self.state.lock().unwrap().0 += n;
        self.changed.notify_all();
    }

    pub fn stop(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }

    /// Waits until fewer than `window` bytes past `sent` are unacknowledged.
    /// False once stopped.
    fn wait(&self, sent: u64, window: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        while !s.1 && sent.saturating_sub(s.0) > window {
            s = self.changed.wait_timeout(s, Duration::from_secs(1)).unwrap().0;
        }
        !s.1
    }
}

/// A send on the link: (channel, kind, payload).
pub type Send = Arc<dyn Fn(u32, u8, &[u8]) -> std::io::Result<()> + std::marker::Send + Sync>;

/// `{"op": "pack", "channel": n, "dir": "~/x", "names": ["a", "b"]}`: sends a
/// tar stream of those names (inside `dir`) on the channel. Returns the
/// credit to feed ACKs into, and the process to stop on CLOSE.
pub fn pack(req: &Value, send: Send) -> Result<(Value, Arc<Credit>), String> {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    let dir = expand_home(req["dir"].as_str().unwrap_or("~"));
    let names: Vec<String> = req["names"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    if names.is_empty() || names.iter().any(|n| n.is_empty() || n.contains('/') || n.contains('\\') || n == "." || n == "..") {
        return Err("Name the files and folders to copy (inside the folder, without slashes).".into());
    }
    if let Some(missing) = names.iter().find(|n| dir.join(n).symlink_metadata().is_err()) {
        return Err(format!("{} isn't in {}.", missing, dir.display()));
    }
    let window = req["window"].as_u64().unwrap_or(WINDOW).max(CHUNK as u64);
    let (size, files) = measure(&dir, &names);
    let mut cmd = Command::new("tar");
    cmd.arg("-C").arg(&dir).arg("-cf").arg("-");
    if cfg!(target_os = "macos") {
        // No AppleDouble (._) files or Mac metadata for other systems to trip on.
        cmd.env("COPYFILE_DISABLE", "1").arg("--no-mac-metadata");
    }
    // "./name": a name starting with "-" stays a name.
    cmd.args(names.iter().map(|n| format!("./{n}")));
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start tar: {e}"))?;
    let pid = child.id();
    let (mut stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let credit = Arc::new(Credit::default());
    *credit.pid.lock().unwrap() = Some(pid);
    let errors = collect(stderr);
    let held = credit.clone();
    thread::spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        let mut failed = None;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !held.wait(sent, window) {
                        failed = Some("stopped".to_string());
                        break;
                    }
                    if send(channel, crate::proto::DATA, &buf[..n]).is_err() {
                        failed = Some("the app went away".to_string());
                        break;
                    }
                    sent += n as u64;
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        let result = finish(&mut child, failed, &errors);
        // Gone: nothing to stop any more (its process id may be reused).
        held.pid.lock().unwrap().take();
        let _ = send(channel, END, &serde_json::to_vec(&result).unwrap_or_default());
    });
    Ok((json!({ "ok": true, "size": size, "files": files }), credit))
}

/// The other end of a copy: tar -x into a folder.
pub struct Unpack {
    stdin: Option<ChildStdin>,
    pid: u32,
    done: Arc<AtomicBool>,
    channel: u32,
    send: Send,
}

impl Unpack {
    /// More of the stream.
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else { return Ok(()) };
        stdin.write_all(data)?;
        (self.send)(self.channel, ACK, &(data.len() as u64).to_be_bytes())
    }

    /// That was all of it: tar finishes, and the result goes back.
    pub fn end(&mut self) {
        self.stdin = None;
    }

    pub fn stop(self) {
        if !self.done.load(Ordering::Relaxed) {
            kill(self.pid);
        }
    }
}

/// `{"op": "unpack", "channel": n, "dir": "~/dest"}`: extracts the tar stream
/// the app sends on the channel into `dir` (made if missing). Files with the
/// same names are replaced.
pub fn unpack(req: &Value, send: Send) -> Result<(Value, Unpack), String> {
    let channel = req["channel"].as_u64().unwrap_or(0) as u32;
    let dir = expand_home(req["dir"].as_str().unwrap_or("~"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't make {}: {e}", dir.display()))?;
    let mut child = Command::new("tar")
        .arg("-C")
        .arg(&dir)
        .arg("-xf")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't start tar: {e}"))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let errors = collect(child.stderr.take().unwrap());
    let (reply, done) = (send.clone(), Arc::new(AtomicBool::new(false)));
    let finished = done.clone();
    thread::spawn(move || {
        let result = finish(&mut child, None, &errors);
        finished.store(true, Ordering::Relaxed);
        let _ = reply(channel, END, &serde_json::to_vec(&result).unwrap_or_default());
    });
    Ok((json!({ "ok": true, "path": dir.display().to_string() }), Unpack { stdin, pid, done, channel, send }))
}

/// What a tar process printed on stderr (the last of it).
fn collect(mut stderr: std::process::ChildStderr) -> Arc<Mutex<String>> {
    let text = Arc::new(Mutex::new(String::new()));
    let into = text.clone();
    thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        *into.lock().unwrap() = s;
    });
    text
}

fn finish(child: &mut Child, failed: Option<String>, errors: &Arc<Mutex<String>>) -> Value {
    if failed.is_some() {
        let _ = child.kill();
    }
    let status = child.wait();
    // Let stderr's reader catch up.
    thread::sleep(Duration::from_millis(50));
    let stderr = errors.lock().unwrap().clone();
    let tail: String = stderr.lines().filter(|l| !l.contains("Ignoring unknown extended header")).collect::<Vec<_>>().join("\n");
    match (failed, status) {
        (Some(e), _) => json!({ "ok": false, "error": e }),
        (None, Ok(s)) if s.success() => json!({ "ok": true }),
        (None, Ok(s)) => json!({ "ok": false, "error": if tail.trim().is_empty() { format!("tar exited with {s}") } else { tail.trim().to_string() } }),
        (None, Err(e)) => json!({ "ok": false, "error": e.to_string() }),
    }
}

pub fn kill(pid: u32) {
    crate::sys::kill_one(pid, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks_through_channels() {
        let base = std::env::temp_dir().join(format!("muxen-files-{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        std::fs::create_dir_all(src.join("folder/inner")).unwrap();
        std::fs::write(src.join("folder/inner/a.txt"), b"hello").unwrap();
        std::fs::write(src.join("-dash.bin"), vec![7u8; 300_000]).unwrap();

        // What pack sends lands in unpack, the way the app relays it.
        let frames: Arc<Mutex<Vec<(u8, Vec<u8>)>>> = Arc::default();
        let sink = frames.clone();
        let send: Send = Arc::new(move |_, kind, payload| {
            sink.lock().unwrap().push((kind, payload.to_vec()));
            Ok(())
        });
        let req = json!({ "channel": 5, "dir": src.display().to_string(), "names": ["folder", "-dash.bin"] });
        let (reply, credit) = pack(&req, send).unwrap();
        assert_eq!(reply["files"], 2);
        assert_eq!(reply["size"], 300_005);
        let acks: Send = Arc::new(|_, _, _| Ok(()));
        let (_, mut into) = unpack(&json!({ "channel": 6, "dir": dst.display().to_string() }), acks).unwrap();
        let started = Instant::now();
        loop {
            let batch: Vec<(u8, Vec<u8>)> = std::mem::take(&mut *frames.lock().unwrap());
            let mut ended = None;
            for (kind, payload) in batch {
                match kind {
                    crate::proto::DATA => {
                        into.write(&payload).unwrap();
                        credit.ack(payload.len() as u64);
                    }
                    END => ended = Some(serde_json::from_slice::<Value>(&payload).unwrap()),
                    _ => {}
                }
            }
            if let Some(result) = ended {
                assert_eq!(result["ok"], true, "{result}");
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(5));
        }
        into.end();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(std::fs::read(dst.join("folder/inner/a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::metadata(dst.join("-dash.bin")).unwrap().len(), 300_000);
        let listed = list(&json!({ "path": dst.display().to_string() }));
        assert_eq!(listed["entries"][0]["name"], "folder", "folders first");
        let _ = std::fs::remove_dir_all(&base);
    }
}
