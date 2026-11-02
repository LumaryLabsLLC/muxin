//! Muxin's own ssh client, for where no `ssh` program can run: Muxin for
//! iPad adding a machine. It installs the agent and sets up the direct
//! connection the way the Mac does over ssh; from then on the iPad reaches
//! the machine directly, as one paired from a Mac.
//!
//! It knows what the `ssh` program would read from your files: this app's
//! own key (`identity/ssh_ed25519`, made on first use), the machines' host
//! keys (`known_hosts`, each confirmed by you the first time), and nothing
//! else: no `~/.ssh/config`, so a machine is named `user@host`, with its port.
//!
//! Errors read like OpenSSH's, so a check that fails says why in the same
//! words (`build::unreachable_kind`).
//!
//! Built for the iPad, and for its tests here (where the rest of Muxin uses
//! the `ssh` program, so some of it is only the iPad's).
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use russh::client::{self, Handle, Msg};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, known_hosts};
use russh::{ChannelMsg, ChannelWriteHalf};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

use crate::config::MachineSpec;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// What a program's output may hold before its reader catches up.
const PIPE: usize = 256 << 10;

/// Where a machine is, as ssh would connect.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub user: String,
    pub host: String,
    pub port: u16,
}

/// `user@host` (or `host` with the machine's `user`), and its port. There's
/// no ssh config to say more, so an alias can't be followed.
pub fn route(spec: &MachineSpec) -> Result<Route, String> {
    let given = spec.host.trim();
    let (user, host) = match given.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (spec.user.clone().filter(|u| !u.is_empty()), given),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let user = user.filter(|u| !u.is_empty()).ok_or_else(|| format!("Say which account to use on {}: name@{host}.", spec.name))?;
    if host.is_empty() {
        return Err(format!("{} has no address.", spec.name));
    }
    Ok(Route { user, host, port: spec.port.unwrap_or(22) })
}

// MARK: This app's key, and the machines'

fn dir() -> Result<PathBuf, String> {
    super::direct::config_dir().ok_or_else(|| "Muxin's config folder isn't set".to_string())
}

/// This app's ssh key, made on first use (ed25519, kept only here).
fn identity() -> Result<PrivateKey, String> {
    static KEY: OnceLock<Mutex<Option<PrivateKey>>> = OnceLock::new();
    let cell = KEY.get_or_init(Default::default);
    if let Some(key) = cell.lock().unwrap().clone() {
        return Ok(key);
    }
    let folder = dir()?.join("identity");
    let path = folder.join("ssh_ed25519");
    let key = match std::fs::read_to_string(&path) {
        Ok(text) => PrivateKey::from_openssh(text.trim()).map_err(|e| format!("This app's ssh key is damaged ({e}).")),
        Err(_) => {
            let mut seed = [0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).map_err(|_| "no random numbers".to_string())?;
            let key = PrivateKey::new(KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)), comment()).map_err(|e| e.to_string())?;
            let text = key.to_openssh(LineEnding::LF).map_err(|e| e.to_string())?;
            crate::config::create_private_dir(&folder).map_err(|e| e.to_string())?;
            write_private(&path, text.as_bytes())?;
            Ok(key)
        }
    }?;
    *cell.lock().unwrap() = Some(key.clone());
    Ok(key)
}

/// "muxin@iPad": what the key says it is, in authorized_keys.
fn comment() -> String {
    let device = crate::config::local_hostname();
    let device: String = device.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    format!("muxin@{}", if device.is_empty() { "ipad".to_string() } else { device })
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path).and_then(|mut f| f.write_all(bytes)).map_err(|e| e.to_string())
}

/// This app's public key, as authorized_keys takes it: `ssh-ed25519 AAAA… muxin@iPad`.
pub fn public_key_line() -> Result<String, String> {
    identity()?.public_key().to_openssh().map_err(|e| e.to_string())
}

fn known_hosts_path() -> Result<PathBuf, String> {
    Ok(dir()?.join("known_hosts"))
}

/// Host keys the user confirmed for the next connection (host, port → fingerprint).
fn confirmed() -> &'static Mutex<HashMap<(String, u16), String>> {
    static CONFIRMED: OnceLock<Mutex<HashMap<(String, u16), String>>> = OnceLock::new();
    CONFIRMED.get_or_init(Default::default)
}

/// The user checked that a machine's host key is this one: it's kept on the
/// next connection that sees it.
pub fn trust_host_key(host: &str, port: u16, fingerprint: &str) {
    confirmed().lock().unwrap().insert((host.to_string(), port), fingerprint.to_string());
}

/// Passwords for the next sign-in, by machine id, in memory only.
fn passwords() -> &'static Mutex<HashMap<String, String>> {
    static PASSWORDS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    PASSWORDS.get_or_init(Default::default)
}

/// A password to sign in to a machine with, when its key isn't let in yet.
pub fn remember_password(machine_id: &str, password: String) {
    passwords().lock().unwrap().insert(machine_id.to_string(), password);
}

pub fn forget_password(machine_id: &str) {
    passwords().lock().unwrap().remove(machine_id);
}

// MARK: Connecting

/// Why a connection didn't go through, beyond what its words say.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// A machine seen for the first time: the user checks its key.
    UnknownHost { host: String, port: u16, fingerprint: String },
    /// Its key changed since it was trusted.
    ChangedHost { host: String, fingerprint: String },
    /// Neither this app's key nor a password let us in.
    NeedsPassword { user: String, host: String },
    /// This app's key isn't let in, and the machine takes no passwords over
    /// ssh: the key goes in its authorized_keys by hand.
    KeyOnly { user: String, host: String },
    Other(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownHost { host, fingerprint, .. } => write!(f, "The authenticity of host '{host}' can't be established: its key is {fingerprint}."),
            Refusal::ChangedHost { host, fingerprint } => {
                write!(f, "Host key verification failed: REMOTE HOST IDENTIFICATION HAS CHANGED for {host} (it's now {fingerprint}).")
            }
            Refusal::NeedsPassword { user, host } => write!(f, "{user}@{host}: Permission denied (publickey,password)."),
            Refusal::KeyOnly { user, host } => write!(f, "{user}@{host}: Permission denied (publickey)."),
            Refusal::Other(e) => f.write_str(e),
        }
    }
}

impl From<Refusal> for String {
    fn from(r: Refusal) -> String {
        r.to_string()
    }
}

/// Checks a machine's host key against known_hosts (or the user's say-so).
pub struct Checker {
    route: Route,
    path: PathBuf,
    seen: Arc<Mutex<Option<Refusal>>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.seen.lock().unwrap() = Some(Refusal::Other(format!("{} offers a host certificate, which Muxin doesn't check yet.", self.route.host)));
            return Ok(false);
        };
        Ok(self.check(key))
    }
}

impl Checker {
    fn check(&self, key: &PublicKey) -> bool {
        let Route { host, port, .. } = &self.route;
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match known_hosts::check_known_hosts_path(host, *port, key, &self.path) {
            Ok(true) => true,
            Err(russh::keys::Error::KeyChanged { .. }) => {
                *self.seen.lock().unwrap() = Some(Refusal::ChangedHost { host: host.clone(), fingerprint });
                false
            }
            _ => {
                let trusted = confirmed().lock().unwrap().get(&(host.clone(), *port)) == Some(&fingerprint);
                if trusted && known_hosts::learn_known_hosts_path(host, *port, key, &self.path).is_ok() {
                    confirmed().lock().unwrap().remove(&(host.clone(), *port));
                    return true;
                }
                *self.seen.lock().unwrap() = Some(Refusal::UnknownHost { host: host.clone(), port: *port, fingerprint });
                false
            }
        }
    }
}

type Session = Arc<Handle<Checker>>;

/// Signed-in connections, by machine id, kept for the next command (as
/// ssh's ControlMaster would).
fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

/// Drops a machine's connection (it was removed, or its password changed).
pub fn disconnect(machine_id: &str) {
    sessions().lock().unwrap().remove(machine_id);
}

/// A machine signed in to before it had its id (while it's being added)
/// takes that connection, and its password, under the id it got.
pub fn adopt(from: &str, to: &str) {
    // One lock each, held once (an `if let` would hold it into its body).
    let mut sessions = sessions().lock().unwrap();
    if let Some(s) = sessions.remove(from) {
        sessions.insert(to.to_string(), s);
    }
    drop(sessions);
    let mut passwords = passwords().lock().unwrap();
    if let Some(p) = passwords.remove(from) {
        passwords.insert(to.to_string(), p);
    }
}

/// Connects and signs in, or reuses the connection there is.
pub async fn session(spec: &MachineSpec) -> Result<Session, Refusal> {
    if let Some(s) = sessions().lock().unwrap().get(&spec.id).filter(|s| !s.is_closed()).cloned() {
        return Ok(s);
    }
    let route = route(spec).map_err(Refusal::Other)?;
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(5)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(None));
    let checker = Checker { route: route.clone(), path: known_hosts_path().map_err(Refusal::Other)?, seen: seen.clone() };
    let at = format!("ssh: connect to host {} port {}", route.host, route.port);
    let addrs: Vec<std::net::SocketAddr> = match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((route.host.as_str(), route.port))).await {
        Ok(Ok(a)) => a.collect(),
        Ok(Err(_)) => return Err(Refusal::Other(format!("ssh: Could not resolve hostname {}: nodename nor servname provided, or not known", route.host))),
        Err(_) => return Err(Refusal::Other(format!("{at}: Operation timed out"))),
    };
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, &addrs[..], checker)).await;
    let mut handle = match connected {
        Err(_) => return Err(Refusal::Other(format!("{at}: Operation timed out"))),
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            if let Some(why) = seen.lock().unwrap().take() {
                return Err(why);
            }
            return Err(Refusal::Other(match e {
                russh::Error::IO(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => format!("{at}: Connection refused"),
                russh::Error::IO(io) => format!("{at}: {io}"),
                e => format!("{at}: {e}"),
            }));
        }
    };
    let key = Arc::new(identity().map_err(Refusal::Other)?);
    let (mut signed_in, offered) = match handle.authenticate_publickey(&route.user, PrivateKeyWithHashAlg::new(key, None)).await {
        Ok(client::AuthResult::Success) => (true, Vec::new()),
        Ok(client::AuthResult::Failure { remaining_methods, .. }) => (false, remaining_methods.to_vec()),
        Err(_) => (false, Vec::new()),
    };
    // What the machine takes besides keys (none said: try both).
    let (password_ok, interactive_ok) = if offered.is_empty() {
        (true, true)
    } else {
        (offered.contains(&russh::MethodKind::Password), offered.contains(&russh::MethodKind::KeyboardInteractive))
    };
    if !signed_in && (password_ok || interactive_ok) {
        let password = passwords().lock().unwrap().get(&spec.id).cloned();
        if let Some(password) = password {
            if password_ok {
                signed_in = handle.authenticate_password(&route.user, password.clone()).await.is_ok_and(|r| r.success());
            }
            // Machines that ask through PAM (keyboard-interactive): the
            // password answers each prompt that hides what's typed.
            if !signed_in && interactive_ok {
                signed_in = keyboard_interactive(&mut handle, &route.user, &password).await;
            }
        }
    }
    if !signed_in {
        let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
        if !password_ok && !interactive_ok {
            return Err(Refusal::KeyOnly { user: route.user, host: route.host });
        }
        return Err(Refusal::NeedsPassword { user: route.user, host: route.host });
    }
    let session = Arc::new(handle);
    sessions().lock().unwrap().insert(spec.id.clone(), session.clone());
    Ok(session)
}

/// Signs in with keyboard-interactive, answering password prompts.
async fn keyboard_interactive(handle: &mut Handle<Checker>, user: &str, password: &str) -> bool {
    use russh::client::KeyboardInteractiveAuthResponse as R;
    let mut reply = handle.authenticate_keyboard_interactive_start(user, None).await;
    for _ in 0..4 {
        match reply {
            Ok(R::Success) => return true,
            Ok(R::InfoRequest { prompts, .. }) => {
                let answers = prompts.iter().map(|p| if p.echo { String::new() } else { password.to_string() }).collect();
                reply = handle.authenticate_keyboard_interactive_respond(answers).await;
            }
            _ => return false,
        }
    }
    false
}

// MARK: Running things

/// A command running on a machine over ssh, as a child process would: its
/// stdin, stdout and stderr, and how it ended.
pub struct RemoteChild {
    pub stdin: Option<RemoteStdin>,
    pub stdout: Option<DuplexStream>,
    pub stderr: Option<DuplexStream>,
    status: tokio::sync::watch::Receiver<Option<i32>>,
    channel: Arc<ChannelWriteHalf<Msg>>,
    _session: Session,
}

/// A remote command's stdin. Dropping it (or shutting it down) tells the
/// command there's no more, as closing a pipe would.
pub struct RemoteStdin {
    writer: std::pin::Pin<Box<dyn AsyncWrite + Send>>,
    channel: Arc<ChannelWriteHalf<Msg>>,
    ended: bool,
}

impl AsyncWrite for RemoteStdin {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        self.writer.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        self.writer.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        let done = self.writer.as_mut().poll_shutdown(cx);
        if done.is_ready() {
            self.ended = true;
        }
        done
    }
}

impl Drop for RemoteStdin {
    fn drop(&mut self) {
        if !self.ended
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let channel = self.channel.clone();
            rt.spawn(async move { channel.eof().await });
        }
    }
}

impl RemoteChild {
    /// How it ended: its exit code (255 when ssh couldn't tell, as OpenSSH says).
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        drop(self.stdin.take());
        let code = loop {
            if let Some(code) = *self.status.borrow() {
                break code;
            }
            if self.status.changed().await.is_err() {
                break self.status.borrow().unwrap_or(255);
            }
        };
        Ok(exit_status(code))
    }

    /// Reads all it prints, then how it ended.
    pub async fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        drop(self.stdin.take());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut o, mut e) = (self.stdout.take(), self.stderr.take());
        let read_out = async {
            if let Some(o) = o.as_mut() {
                let _ = o.read_to_end(&mut out).await;
            }
        };
        let read_err = async {
            if let Some(e) = e.as_mut() {
                let _ = e.read_to_end(&mut err).await;
            }
        };
        tokio::join!(read_out, read_err);
        let status = self.wait().await?;
        Ok(std::process::Output { status, stdout: out, stderr: err })
    }

    /// Ends it (the channel closes; the command there gets a hangup).
    pub fn kill(&self) {
        let channel = self.channel.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { channel.close().await });
        }
    }
}

impl Drop for RemoteChild {
    fn drop(&mut self) {
        // As tokio's kill_on_drop: a command nobody waits for anymore goes.
        if self.status.borrow().is_none() {
            self.kill();
        }
    }
}

fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw((code & 0xff) << 8)
}

/// Starts `command` on the machine, as `ssh <machine> <command>` would.
pub async fn spawn(spec: &MachineSpec, command: &str) -> Result<RemoteChild, String> {
    let session = session(spec).await?;
    let channel = match session.channel_open_session().await {
        Ok(c) => c,
        // A kept connection that just went: once more, on a new one.
        Err(_) => {
            disconnect(&spec.id);
            session_or_err(spec).await?.channel_open_session().await.map_err(|e| format!("ssh: {e}"))?
        }
    };
    channel.exec(true, command).await.map_err(|e| format!("ssh: couldn't start the command: {e}"))?;
    let (mut read, write) = channel.split();
    let write = Arc::new(write);
    let (out_tx, out_rx) = tokio::io::duplex(PIPE);
    let (err_tx, err_rx) = tokio::io::duplex(PIPE);
    let (status_tx, status_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        let (mut out_tx, mut err_tx) = (Some(out_tx), Some(err_tx));
        let mut code = None;
        while let Some(msg) = read.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if let Some(w) = out_tx.as_mut() {
                        let _ = w.write_all(&data).await;
                    }
                }
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    if let Some(w) = err_tx.as_mut() {
                        let _ = w.write_all(&data).await;
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                ChannelMsg::ExitSignal { .. } => code = Some(255),
                ChannelMsg::Eof => {
                    out_tx = None;
                    err_tx = None;
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        drop((out_tx, err_tx));
        let _ = status_tx.send(Some(code.unwrap_or(255)));
    });
    let stdin = RemoteStdin { writer: Box::pin(write.make_writer()), channel: write.clone(), ended: false };
    Ok(RemoteChild { stdin: Some(stdin), stdout: Some(out_rx), stderr: Some(err_rx), status: status_rx, channel: write, _session: session })
}

async fn session_or_err(spec: &MachineSpec) -> Result<Session, String> {
    session(spec).await.map_err(String::from)
}

/// Copies `bytes` to `path` on the machine over SFTP (Windows' sshd stalls
/// on a big stdin). `path` is relative to the account's home; its folders
/// are made first.
pub async fn sftp_put(spec: &MachineSpec, bytes: &[u8], path: &str) -> Result<(), String> {
    let session = session_or_err(spec).await?;
    let channel = session.channel_open_session().await.map_err(|e| format!("ssh: {e}"))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| format!("sftp isn't available there: {e}"))?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await.map_err(|e| format!("sftp: {e}"))?;
    let parts: Vec<&str> = path.split('/').collect();
    for i in 1..parts.len() {
        let folder = parts[..i].join("/");
        if !sftp.try_exists(&folder).await.unwrap_or(false) {
            sftp.create_dir(&folder).await.map_err(|e| format!("sftp: couldn't make {folder}: {e}"))?;
        }
    }
    let mut file = sftp.create(path).await.map_err(|e| format!("sftp: couldn't write {path}: {e}"))?;
    file.write_all(bytes).await.map_err(|e| format!("sftp: the copy stopped: {e}"))?;
    file.shutdown().await.map_err(|e| format!("sftp: {e}"))?;
    let _ = sftp.close().await;
    Ok(())
}

// MARK: Letting this app in

/// Runs a short script there and says what it printed, or why it failed.
async fn run(spec: &MachineSpec, command: &str) -> Result<String, String> {
    let out = spawn(spec, command).await?.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("it failed").to_string())
    }
}

/// Puts this app's key in the account's authorized_keys (on Windows, the
/// administrators' file for an administrator), so it signs in without a
/// password from now on. Already there: nothing changes.
pub async fn authorize(spec: &MachineSpec, windows: bool) -> Result<(), String> {
    let line = public_key_line()?;
    let script = if windows { authorize_ps(&line) } else { authorize_sh(&line) };
    let command = if windows { powershell(&script) } else { format!("sh -c {}", crate::ops::remote::sh_quote(&script)) };
    run(spec, &command).await.map(drop).map_err(|e| format!("Couldn't let this iPad in on {}: {e}", spec.name))
}

/// Takes this app's key off the machine again (removing it from Muxin).
pub fn deauthorize_script(windows: bool) -> Result<String, String> {
    let line = public_key_line()?;
    let key = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    Ok(if windows { deauthorize_ps(&key) } else { deauthorize_sh(&key) })
}

fn authorize_sh(line: &str) -> String {
    let q = crate::ops::remote::sh_quote(line);
    format!(
        "set -e; umask 077; mkdir -p \"$HOME/.ssh\"; chmod 700 \"$HOME/.ssh\"; f=\"$HOME/.ssh/authorized_keys\"; touch \"$f\"; chmod 600 \"$f\"; \
         key=$(printf '%s' {q} | cut -d' ' -f1,2); grep -qF \"$key\" \"$f\" || printf '%s\\n' {q} >> \"$f\""
    )
}

fn deauthorize_sh(key: &str) -> String {
    let q = crate::ops::remote::sh_quote(key);
    format!("f=\"$HOME/.ssh/authorized_keys\"; [ -f \"$f\" ] || exit 0; grep -vF {q} \"$f\" > \"$f.muxin\" || true; cat \"$f.muxin\" > \"$f\"; rm -f \"$f.muxin\"")
}

/// Administrators sign in with C:\ProgramData\ssh\administrators_authorized_keys,
/// readable only by Administrators and SYSTEM; other accounts with their own.
const WINDOWS_KEYS: &str = "$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)\n\
     if ($admin) { $f = Join-Path $env:ProgramData 'ssh\\administrators_authorized_keys' } else { $d = Join-Path $env:USERPROFILE '.ssh'; New-Item -ItemType Directory -Force $d | Out-Null; $f = Join-Path $d 'authorized_keys' }\n";

// The file is written whole, line by line: appending would join the key to a
// last line without a newline. Administrators and SYSTEM by their SIDs, which
// a Windows in another language names otherwise.
fn authorize_ps(line: &str) -> String {
    let q = crate::ops::remote::ps_quote(line);
    format!(
        "$ErrorActionPreference = 'Stop'\n$line = {q}\n{WINDOWS_KEYS}\
         $key = ($line -split ' ')[0..1] -join ' '\n\
         $have = @(if (Test-Path $f) {{ Get-Content $f }})\n\
         if (-not ($have | Where-Object {{ $_.StartsWith($key) }})) {{ Set-Content -Path $f -Value ($have + $line) -Encoding ascii }}\n\
         if ($admin) {{ icacls $f /inheritance:r /grant '*S-1-5-32-544:F' /grant '*S-1-5-18:F' | Out-Null; if ($LASTEXITCODE -ne 0) {{ throw \"icacls couldn't limit $f to Administrators and SYSTEM\" }} }}\n"
    )
}

fn deauthorize_ps(key: &str) -> String {
    let q = crate::ops::remote::ps_quote(key);
    format!("$key = {q}\n{WINDOWS_KEYS}if (Test-Path $f) {{ $keep = @(Get-Content $f | Where-Object {{ -not $_.StartsWith($key) }}); Set-Content -Path $f -Value $keep -Encoding ascii }}\n")
}

/// A PowerShell script as one command line (whatever the shell sshd starts).
pub fn powershell(script: &str) -> String {
    use base64::Engine as _;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!("powershell -NoProfile -NonInteractive -EncodedCommand {}", base64::engine::general_purpose::STANDARD.encode(utf16))
}

#[cfg(test)]
mod tests;
