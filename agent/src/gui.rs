//! The logged-in session's side of the agent. On a Mac, capturing the screen
//! (and the clipboard) works from the user's own session, not from an ssh
//! session: the agent's background service (`listen`, a launchd agent) runs
//! there, so it starts those programs, and links that came in over ssh reach
//! them through a local socket. Nothing crosses the network but ssh, so the
//! Mac's Local Network privacy doesn't come into it.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[cfg(unix)]
fn socket() -> PathBuf {
    crate::home_dir().join("gui.sock")
}

#[cfg(unix)]
/// Whether programs that need the desktop session must be started by the
/// background service: a Mac, from a link that came in over ssh.
pub fn needed() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("SSH_CONNECTION").is_some()
}

#[cfg(unix)]
/// Serves the socket from the background service, on a thread of its own.
pub fn serve() {
    let path = socket();
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("muxen-agent: can't serve {}: {e}", path.display());
            return;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            thread::spawn(move || run(conn));
        }
    });
}

#[cfg(unix)]
/// One connection: `{"run": ["screen", ...]}`, then the program's stdin and
/// stdout are the connection, until either side closes.
fn run(mut conn: UnixStream) {
    let Ok(req) = crate::proto::read_line(&mut conn) else { return };
    let args: Vec<String> = req["run"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    // Only the programs that need the session.
    if !matches!(args.first().map(String::as_str), Some("screen" | "clip" | "displays" | "windows" | "display-mode" | "vdisplay")) {
        return;
    }
    // A virtual display's holder outlives the connection: start it, say its id.
    #[cfg(target_os = "macos")]
    if args.first().map(String::as_str) == Some("vdisplay") {
        let line = crate::displays::start_holder(&args).unwrap_or_else(|e| json!({ "error": e }).to_string());
        let _ = writeln!(conn, "{line}");
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let log = std::fs::OpenOptions::new().create(true).append(true).open(crate::home_dir().join("screen.log"));
    let mut cmd = Command::new(exe);
    cmd.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped());
    match log {
        Ok(l) => cmd.stderr(l),
        Err(_) => cmd.stderr(Stdio::null()),
    };
    let Ok(mut child) = cmd.spawn() else { return };
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return;
    };
    let Ok(mut from_link) = conn.try_clone() else {
        let _ = child.kill();
        return;
    };
    thread::spawn(move || {
        let _ = io::copy(&mut from_link, &mut stdin);
    });
    let _ = io::copy(&mut stdout, &mut conn);
    // A Mac's screen capture ends by itself, maybe a while later (see
    // desktop.rs's `captures`): read it to the end rather than kill it.
    if cfg!(target_os = "macos") && args.first().map(String::as_str) == Some("screen") {
        let _ = io::copy(&mut stdout, &mut io::sink());
    } else {
        let _ = child.kill();
    }
    // The other side reads to the end: tell it this is the end, before the
    // program has gone (a Mac's capture may linger a while). (The thread
    // above holds the socket open, so dropping ours alone doesn't, and a
    // program that answers once, like `displays`, would leave it waiting.)
    let _ = conn.shutdown(std::net::Shutdown::Write);
    let _ = child.wait();
}

#[cfg(unix)]
/// Runs a program in the logged-in session (see `needed`): its stdin and
/// stdout, as a socket. Starts the background service when it isn't running.
pub fn connect(args: &[String]) -> Result<UnixStream, String> {
    let mut stream = match UnixStream::connect(socket()) {
        Ok(s) => s,
        Err(_) => {
            crate::direct::start_service();
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                match UnixStream::connect(socket()) {
                    Ok(s) => break s,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(200)),
                    Err(e) => return Err(format!("the agent's background service isn't running here ({e})")),
                }
            }
        }
    };
    crate::proto::write_line(&mut stream, &json!({ "run": args })).map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Reads one frame (for the HELLO a program starts with).
pub fn read_hello(r: &mut impl Read) -> Value {
    match crate::proto::read_frame(r) {
        Ok((crate::desktop::HELLO, p)) => serde_json::from_slice::<Value>(&p).unwrap_or(Value::Null),
        _ => json!({ "error": "the screen capture stopped right away" }),
    }
}

/// Writes a frame and flushes.
pub fn write(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    crate::proto::write_frame(w, kind, payload)?;
    w.flush()
}
