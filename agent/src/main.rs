//! muxen-agent: the small program Muxen installs on each machine it
//! manages (`~/.muxen/bin`). It keeps agent sessions (terminals running
//! Claude Code, Codex, a shell, ...) alive in a background server, the way tmux
//! does, so they outlive ssh connections and the app, and lets Muxen
//! attach to them.
//!
//! ```text
//! muxen-agent version         print the version
//! muxen-agent hash            a hash of this binary, to tell builds apart
//! muxen-agent ctl             one JSON request on stdin, one JSON reply on stdout
//! muxen-agent link            a long-lived, multiplexed connection for the app (stdin/stdout)
//! muxen-agent listen          the same over QUIC, for paired apps (a user service)
//! muxen-agent attach <name>   connect this terminal to a session
//! muxen-agent serve           the session server (started on demand)
//! ```

mod client;
mod clipboard;
mod daemon;
mod detect;
mod desktop;
#[cfg(unix)]
mod displays;
mod logs;
mod ports;
mod procs;
mod usage;
mod wake;
mod root;
mod services;
mod direct;
mod files;
mod gui;
mod link;
mod proto;
mod repos;
mod screen;
mod sys;
#[cfg_attr(not(windows), allow(dead_code))]
mod win;
#[cfg(windows)]
mod win_screen;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("version" | "--version") => {
            println!("muxen-agent {VERSION}");
            0
        }
        Some("hash") => {
            println!("{}", self_hash());
            0
        }
        Some("ctl") => client::ctl(),
        Some("link") => link::run(),
        Some("listen") => direct::listen(),
        Some("migrate") => direct::migrate(),
        Some("screen") => desktop::run(&args[1..]),
        #[cfg(unix)]
        Some("displays") => displays::run_list(),
        #[cfg(windows)]
        Some("displays") => {
            println!("{}", win_screen::displays());
            0
        }
        #[cfg(windows)]
        Some("gui-serve") => win_screen::gui_serve(),
        #[cfg(target_os = "macos")]
        Some("vdisplay") => displays::run_holder(&args[1..]),
        #[cfg(target_os = "macos")]
        Some("display-mode") => displays::run_mode(&args[1..]),
        Some("clip") => clipboard::run(),
        Some("attach") => match args.get(1) {
            Some(name) => client::attach(name),
            None => usage(),
        },
        Some("serve") => daemon::serve(),
        _ => usage(),
    };
    std::process::exit(code);
}

fn usage() -> i32 {
    eprintln!("usage: muxen-agent version | hash | ctl | link | listen | attach <name> | serve");
    2
}

/// FNV-1a of this executable. Muxen compares it with the build it ships to
/// decide whether to upload a new one.
pub fn self_hash() -> String {
    let bytes = std::env::current_exe().and_then(std::fs::read).unwrap_or_default();
    format!("{:016x}", fnv(&bytes))
}

pub fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `~/.muxen`, private to the user.
pub fn home_dir() -> PathBuf {
    let dir = sys::user_home().join(".muxen");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// The session server's socket: `~/.muxen/agent.sock`, or a private
/// folder under /tmp when the home path is too long for a Unix socket.
/// (Windows: see sys.rs.)
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    let path = home_dir().join("agent.sock");
    if path.as_os_str().len() < 100 {
        return path;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = unsafe { libc::getuid() };
        let dir = PathBuf::from(format!("/tmp/muxen-{uid}"));
        let _ = std::fs::create_dir(&dir);
        // Only a folder we own and nobody else can enter.
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            return dir.join("agent.sock");
        }
    }
    path
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
