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
    #[cfg(test)]
    isolate_home();
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .or_else(|| std::env::var_os("HOME"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/tmp" }))
}

/// Tests never touch the user's own home (~/.muxen, ~/.claude, ~/.codex): a
/// test run's home is a folder of its own, from the first look at it on.
#[cfg(test)]
fn isolate_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Short: the session server's socket goes in it (about 100 bytes at most).
        let home = std::env::temp_dir().join(format!("mxa-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        // SAFETY: once per test run, before the home is first used; Rust's own reads of
        // the environment take the same lock as set_var.
        unsafe {
            std::env::set_var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }, &home);
            std::env::set_var("HOME", &home);
            for var in crate::detect::DIR_VARS {
                std::env::remove_var(var);
            }
        }
    });
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
    #[cfg(target_os = "macos")]
    {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        mac::spawn_disclaimed(&exe, args, &log, &home)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::process::Stdio;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        let mut cmd = Command::new(exe);
        cmd.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).current_dir(home);
        own_group(&mut cmd).spawn().map(|_| ())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let _ = log;
        // Out of whatever job this runs in, without a window. That works
        // unless the job forbids it (sshd's ends everything in it when the
        // session closes): then through WMI, which starts it outside.
        let direct = Command::new(&exe)
            .args(args)
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
            .spawn();
        if direct.is_ok() {
            return Ok(());
        }
        let quote = crate::win::quote;
        let line = std::iter::once(format!("\"{}\"", exe.display())).chain(args.iter().map(|a| a.to_string())).collect::<Vec<_>>().join(" ");
        // Hidden (ShowWindow 0), should this be a desktop session.
        let script = format!(
            "$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{{ ShowWindow = [uint16]0 }}; $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = {}; CurrentDirectory = {}; ProcessStartupInformation = $startup }}; exit $r.ReturnValue",
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

/// Starting the agent's background processes on a Mac.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    unsafe extern "C" {
        /// libSystem, macOS 10.14 and later, as Terminal and iTerm use it: the
        /// new process is responsible for itself, not for whoever started it.
        fn responsibility_spawnattrs_setdisclaim(attrs: *mut libc::posix_spawnattr_t, disclaim: libc::c_int) -> libc::c_int;
        fn posix_spawn_file_actions_addchdir_np(actions: *mut libc::posix_spawn_file_actions_t, path: *const libc::c_char) -> libc::c_int;
    }
    /// A session of its own, like `setsid` (<spawn.h>).
    const POSIX_SPAWN_SETSID: libc::c_short = 0x0400;

    fn c(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::other("a NUL in an argument"))
    }

    fn check(what: &str, rc: libc::c_int) -> io::Result<()> {
        if rc == 0 { Ok(()) } else { Err(io::Error::other(format!("{what}: {}", io::Error::from_raw_os_error(rc)))) }
    }

    /// `exe args...` in a session of its own, with no files open but /dev/null
    /// in and `log` out, started in `dir`, and responsible for itself.
    ///
    /// macOS asks about the camera, the microphone, contacts and the like on
    /// behalf of the process "responsible" for the one asking, and that's
    /// inherited: the session server started by Muxin.app would ask as
    /// Muxin.app, by sshd as sshd, by launchd as itself. Disclaimed, it's
    /// always the agent (Muxin Agent.app, with a word on why for each), so a
    /// program in any session gets the same prompt, and one answer covers all.
    pub fn spawn_disclaimed(exe: &Path, args: &[&str], log: &std::fs::File, dir: &Path) -> io::Result<()> {
        let program = c(exe.as_os_str())?;
        let mut owned = vec![program.clone()];
        for a in args {
            owned.push(c(OsStr::new(a))?);
        }
        let mut argv: Vec<*mut libc::c_char> = owned.iter().map(|a| a.as_ptr() as *mut libc::c_char).collect();
        argv.push(std::ptr::null_mut());
        let null = CString::new("/dev/null").unwrap();
        let dir = c(dir.as_os_str())?;
        unsafe {
            let mut attrs: libc::posix_spawnattr_t = std::ptr::null_mut();
            let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
            check("posix_spawnattr_init", libc::posix_spawnattr_init(&mut attrs))?;
            let spawned = (|| {
                check("posix_spawn_file_actions_init", libc::posix_spawn_file_actions_init(&mut actions))?;
                check("posix_spawnattr_setflags", libc::posix_spawnattr_setflags(&mut attrs, POSIX_SPAWN_SETSID | libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short))?;
                // Where it's not there (an old system), the agent asks as before.
                let _ = responsibility_spawnattrs_setdisclaim(&mut attrs, 1);
                check("addopen", libc::posix_spawn_file_actions_addopen(&mut actions, 0, null.as_ptr(), libc::O_RDONLY, 0))?;
                check("adddup2", libc::posix_spawn_file_actions_adddup2(&mut actions, log.as_raw_fd(), 1))?;
                check("adddup2", libc::posix_spawn_file_actions_adddup2(&mut actions, log.as_raw_fd(), 2))?;
                check("addchdir", posix_spawn_file_actions_addchdir_np(&mut actions, dir.as_ptr()))?;
                let mut pid: libc::pid_t = 0;
                check("posix_spawn", libc::posix_spawn(&mut pid, program.as_ptr(), &actions, &attrs, argv.as_ptr(), *libc::_NSGetEnviron() as *const *mut libc::c_char))
            })();
            if !actions.is_null() {
                libc::posix_spawn_file_actions_destroy(&mut actions);
            }
            libc::posix_spawnattr_destroy(&mut attrs);
            spawned
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// It starts the program in a session of its own, in `dir`, with its
        /// output in the log and nothing on its input.
        #[test]
        fn starts_a_program_on_its_own() {
            let tmp = std::env::temp_dir().join(format!("muxen-spawn-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();
            let log_path = tmp.join("log");
            let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).unwrap();
            let script = "echo \"dir=$(pwd -P) sid=$(ps -o sess= -p $$ | tr -d ' ') pid=$$\"; read x || echo stdin-empty";
            spawn_disclaimed(Path::new("/bin/sh"), &["-c", script], &log, &tmp).unwrap();
            let mut out = String::new();
            for _ in 0..50 {
                out = std::fs::read_to_string(&log_path).unwrap_or_default();
                if out.contains("stdin-empty") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            assert!(out.contains(&format!("dir={}", tmp.canonicalize().unwrap().display())), "{out}");
            assert!(out.contains("stdin-empty"), "{out}");
            let _ = std::fs::remove_dir_all(&tmp);
        }
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
