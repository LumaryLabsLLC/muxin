//! What differs between Unix and Windows: the user's home, the session
//! server's local connection, and starting and ending processes.
//!
//! The session server listens on a Unix socket (`~/.muxen/agent.sock`) on
//! Unix. Windows gets a port on 127.0.0.1 instead, written with a random
//! token to `~/.muxen/agent.port` (in the user's profile, which only they
//! can read): a client sends the token first, and the server hangs up on
//! anyone who doesn't.

use std::io;
use std::path::PathBuf;
use std::process::Command;

/// The user's home folder.
pub fn user_home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .or_else(|| std::env::var_os("HOME"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/tmp" }))
}

/// `~` and `~/x` (or `~\x`) under the user's home; anything else as it is.
pub fn expand_home(dir: &str) -> PathBuf {
    match dir.trim() {
        "" | "~" => user_home(),
        d if d.starts_with("~/") || d.starts_with("~\\") => user_home().join(&d[2..]),
        d => PathBuf::from(d),
    }
}

/// Whether this runs as the superuser (root; on Windows, never: an
/// administrator's elevated session is its own thing).
pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Keeps going when the terminal that started this hangs up.
pub fn ignore_hangup() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// No console window for a program started on Windows (nothing on Unix).
#[cfg_attr(unix, allow(dead_code))]
pub fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Runs a child in its own process group (its own session on Unix), so it
/// can be ended with everything it starts, and a hangup here doesn't reach it.
pub fn own_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd
}

/// Ends a process started with `own_group` and everything it started.
/// `gently`: asks first (SIGTERM) where that exists.
pub fn kill_tree(pid: u32, gently: bool) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), if gently { libc::SIGTERM } else { libc::SIGKILL });
    }
    #[cfg(windows)]
    {
        let _ = gently;
        let _ = no_window(Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Ends one process (SIGTERM when `gently`, else SIGKILL; on Windows, at once).
pub fn kill_one(pid: u32, gently: bool) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, if gently { libc::SIGTERM } else { libc::SIGKILL });
    }
    #[cfg(windows)]
    {
        let _ = gently;
        let _ = no_window(Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Starts `muxen-agent <args>` in the background so it outlives this
/// connection: in its own session on Unix (logging to `log`); on Windows
/// through WMI, since sshd ends everything a session started when it closes.
pub fn spawn_background(args: &[&str], log: PathBuf) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let home = crate::home_dir();
    #[cfg(unix)]
    {
        use std::process::Stdio;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        let mut cmd = Command::new(exe);
        cmd.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).current_dir(home);
        own_group(&mut cmd).spawn().map(|_| ())
    }
    #[cfg(windows)]
    {
        let _ = log;
        let quote = |s: &str| s.replace('\'', "''");
        let line = std::iter::once(format!("\"{}\"", exe.display())).chain(args.iter().map(|a| a.to_string())).collect::<Vec<_>>().join(" ");
        let script = format!(
            "$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = '{}'; CurrentDirectory = '{}' }}; exit $r.ReturnValue",
            quote(&line),
            quote(&home.display().to_string())
        );
        let status = no_window(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() { Ok(()) } else { Err(io::Error::other(format!("Windows didn't start it ({:?})", status.code()))) }
    }
}

// --- The session server's local connection ---------------------------------

#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Stream = std::net::TcpStream;

/// Connects to the session server.
pub fn connect() -> io::Result<Stream> {
    #[cfg(unix)]
    {
        Stream::connect(crate::socket_path())
    }
    #[cfg(windows)]
    {
        use std::io::Write;
        let text = std::fs::read_to_string(port_file())?;
        let (port, token) = text.trim().split_once(' ').ok_or_else(|| io::Error::other("agent.port is damaged"))?;
        let port: u16 = port.parse().map_err(|_| io::Error::other("agent.port is damaged"))?;
        let mut s = Stream::connect(("127.0.0.1", port))?;
        s.write_all(format!("{token}\n").as_bytes())?;
        Ok(s)
    }
}

#[cfg(windows)]
fn port_file() -> PathBuf {
    crate::home_dir().join("agent.port")
}

/// The session server's listening end.
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    inner: std::net::TcpListener,
    #[cfg(windows)]
    token: String,
}

impl Listener {
    /// Starts listening where `connect` looks (replacing what a stopped server left).
    pub fn bind() -> io::Result<Listener> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = crate::socket_path();
            let _ = std::fs::remove_file(&path);
            let inner = std::os::unix::net::UnixListener::bind(&path)?;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            Ok(Listener { inner })
        }
        #[cfg(windows)]
        {
            use ring::rand::SecureRandom;
            let inner = std::net::TcpListener::bind(("127.0.0.1", 0))?;
            let port = inner.local_addr()?.port();
            let mut bytes = [0u8; 24];
            ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| io::Error::other("no randomness"))?;
            let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let tmp = port_file().with_extension(format!("port.{}", std::process::id()));
            std::fs::write(&tmp, format!("{port} {token}"))?;
            std::fs::rename(&tmp, port_file())?;
            Ok(Listener { inner, token })
        }
    }

    /// The next connection (not yet admitted: see `admit`).
    pub fn accept(&self) -> io::Result<Stream> {
        self.inner.accept().map(|(s, _)| s)
    }

    /// Whether a new connection may talk to the server (on Windows: it sent the token).
    pub fn admit(&self, stream: &mut Stream) -> bool {
        #[cfg(unix)]
        {
            let _ = stream;
            true
        }
        #[cfg(windows)]
        {
            use std::io::Read;
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while line.len() <= self.token.len() {
                match stream.read(&mut byte) {
                    Ok(1) if byte[0] == b'\n' => break,
                    Ok(1) => line.push(byte[0]),
                    _ => return false,
                }
            }
            let _ = stream.set_read_timeout(None);
            line == self.token.as_bytes()
        }
    }
}

/// Removes what `Listener::bind` left for clients (the server is stopping).
pub fn unlisten() {
    #[cfg(unix)]
    let _ = std::fs::remove_file(crate::socket_path());
    #[cfg(windows)]
    let _ = std::fs::remove_file(port_file());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        assert_eq!(expand_home("~"), user_home());
        assert_eq!(expand_home("~/code"), user_home().join("code"));
        assert_eq!(expand_home("/srv"), PathBuf::from("/srv"));
    }
}
