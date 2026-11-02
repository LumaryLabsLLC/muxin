//! The client against an ssh server of russh's own, on this computer: it runs
//! commands for real, in a home of its own, and lets in the keys in that
//! home's authorized_keys (or the password "secret").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

use super::*;

#[derive(Clone)]
struct TestServer {
    home: PathBuf,
    /// It takes the password "secret" (else keys only, as many servers).
    passwords: bool,
    channels: Arc<AsyncMutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl server::Server for TestServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if self.passwords && user == "tester" && password == "secret" { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: server::VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        let keys = std::fs::read_to_string(self.home.join(".ssh/authorized_keys")).unwrap_or_default();
        let wanted = key.public_key.to_openssh().unwrap_or_default();
        let wanted = wanted.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let methods: russh::MethodSet = if self.passwords { (&[russh::MethodKind::PublicKey, russh::MethodKind::Password][..]).into() } else { (&[russh::MethodKind::PublicKey][..]).into() };
        Ok(if user == "tester" && keys.lines().any(|l| l.starts_with(&wanted)) { Auth::Accept } else { Auth::Reject { proceed_with_methods: Some(methods), partial_success: false } })
    }

    async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: server::ChannelOpenHandle, _: &mut ServerSession) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(&mut self, id: ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
        let Some(channel) = self.channels.lock().await.remove(&id) else { return Ok(()) };
        session.channel_success(id)?;
        let handle = session.handle();
        let command = String::from_utf8_lossy(data).into_owned();
        let home = self.home.clone();
        tokio::spawn(async move {
            let mut child = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .env("HOME", &home)
                .current_dir(&home)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let (mut read, write) = channel.split();
            let mut stdin = child.stdin.take();
            tokio::spawn(async move {
                while let Some(msg) = read.wait().await {
                    match msg {
                        ChannelMsg::Data { data } => {
                            if let Some(s) = stdin.as_mut() {
                                let _ = s.write_all(&data).await;
                            }
                        }
                        ChannelMsg::Eof => stdin = None,
                        _ => {}
                    }
                }
            });
            let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
            let (mut ow, mut ew) = (write.make_writer(), write.make_writer_ext(Some(1)));
            let _ = tokio::join!(tokio::io::copy(&mut out, &mut ow), tokio::io::copy(&mut err, &mut ew));
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(255);
            let _ = handle.exit_status_request(id, code as u32).await;
            let _ = handle.eof(id).await;
            let _ = handle.close(id).await;
        });
        Ok(())
    }
}

struct Running {
    port: u16,
    home: PathBuf,
    key: PrivateKey,
}

/// A server on a port of its own, in a home of its own.
async fn server(tag: &str) -> Running {
    server_with(tag, true).await
}

async fn server_with(tag: &str, passwords: bool) -> Running {
    let home = std::env::temp_dir().join(format!("muxin-ssh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let key = fixed_key(tag);
    let config = Arc::new(server::Config { keys: vec![key.clone()], auth_rejection_time: Duration::from_millis(1), auth_rejection_time_initial: Some(Duration::ZERO), ..Default::default() });
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let mut s = TestServer { home: home.clone(), passwords, channels: Default::default() };
    tokio::spawn(async move {
        let _ = s.run_on_socket(config, &socket).await;
    });
    Running { port, home, key }
}

fn fixed_key(tag: &str) -> PrivateKey {
    let mut seed = [0u8; 32];
    for (i, b) in tag.bytes().enumerate().take(32) {
        seed[i] = b;
    }
    PrivateKey::new(KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed)), "test").unwrap()
}

fn app_home() {
    let dir = std::env::temp_dir().join(format!("muxin-ssh-app-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    crate::fleet::direct::init(dir);
}

fn spec(id: &str, port: u16) -> MachineSpec {
    toml::from_str(&format!("name = \"{id}\"\nid = \"{id}\"\nhost = \"tester@127.0.0.1\"\nport = {port}")).unwrap()
}

fn fingerprint(key: &PrivateKey) -> String {
    key.public_key().fingerprint(HashAlg::Sha256).to_string()
}

/// Signed in (trusting its key, and with the password when given), or why not.
async fn signed_in(spec: &MachineSpec, s: &Running, password: Option<&str>) -> Result<Session, Refusal> {
    trust_host_key("127.0.0.1", s.port, &fingerprint(&s.key));
    if let Some(p) = password {
        remember_password(&spec.id, p.to_string());
    }
    let got = session(spec).await;
    forget_password(&spec.id);
    got
}

#[test]
fn routes_come_from_the_address() {
    let mut s = spec("r", 2200);
    assert_eq!(route(&s).unwrap(), Route { user: "tester".into(), host: "127.0.0.1".into(), port: 2200 });
    s.host = "[fd7a::1]".into();
    s.user = Some("adib".into());
    s.port = None;
    assert_eq!(route(&s).unwrap(), Route { user: "adib".into(), host: "fd7a::1".into(), port: 22 });
    s.user = None;
    assert!(route(&s).unwrap_err().contains("name@fd7a::1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_machine_is_trusted_only_once_its_key_is_checked() {
    app_home();
    let s = server("hostkey").await;
    let m = spec("ssh-hostkey", s.port);
    remember_password(&m.id, "secret".into());
    match session(&m).await {
        Err(Refusal::UnknownHost { host, port, fingerprint: fp }) => {
            assert_eq!((host.as_str(), port), ("127.0.0.1", s.port));
            assert_eq!(fp, fingerprint(&s.key));
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    // A wrong fingerprint isn't enough.
    trust_host_key("127.0.0.1", s.port, "SHA256:nope");
    assert!(matches!(session(&m).await, Err(Refusal::UnknownHost { .. })));
    trust_host_key("127.0.0.1", s.port, &fingerprint(&s.key));
    assert!(session(&m).await.is_ok());
    // Kept in known_hosts: no asking again.
    disconnect(&m.id);
    assert!(session(&m).await.is_ok());
    forget_password(&m.id);
    disconnect(&m.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_host_key_is_refused() {
    app_home();
    let s = server("changed").await;
    let other = fixed_key("someone else");
    known_hosts::learn_known_hosts_path("127.0.0.1", s.port, other.public_key(), known_hosts_path().unwrap()).unwrap();
    let m = spec("ssh-changed", s.port);
    let got = signed_in(&m, &s, Some("secret")).await;
    let Err(why @ Refusal::ChangedHost { .. }) = got else { panic!("{:?}", got.map(|_| ())) };
    assert_eq!(crate::fleet::build::unreachable_kind(&why.to_string()), "host-key");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_password_lets_this_app_in_with_its_key() {
    app_home();
    let s = server("password").await;
    let m = spec("ssh-password", s.port);
    let refused = signed_in(&m, &s, None).await;
    let Err(why @ Refusal::NeedsPassword { .. }) = refused else { panic!("{:?}", refused.map(|_| ())) };
    assert_eq!(crate::fleet::build::unreachable_kind(&why.to_string()), "auth");
    assert!(matches!(signed_in(&m, &s, Some("wrong")).await, Err(Refusal::NeedsPassword { .. })));
    signed_in(&m, &s, Some("secret")).await.unwrap();
    authorize(&m, false).await.unwrap();
    authorize(&m, false).await.unwrap();
    let keys = std::fs::read_to_string(s.home.join(".ssh/authorized_keys")).unwrap();
    assert_eq!(keys.matches(&public_key_line().unwrap()).count(), 1, "{keys}");
    // No password now: the key.
    disconnect(&m.id);
    signed_in(&m, &s, None).await.unwrap();
    // And off again.
    run(&m, &format!("sh -c {}", crate::ops::remote::sh_quote(&deauthorize_script(false).unwrap()))).await.unwrap();
    assert!(!std::fs::read_to_string(s.home.join(".ssh/authorized_keys")).unwrap().contains("muxin@"));
    disconnect(&m.id);
    assert!(matches!(signed_in(&m, &s, None).await, Err(Refusal::NeedsPassword { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_that_takes_keys_only_says_so() {
    app_home();
    let s = server_with("keyonly", false).await;
    let m = spec("ssh-keyonly", s.port);
    let got = signed_in(&m, &s, Some("secret")).await;
    let Err(why @ Refusal::KeyOnly { .. }) = got else { panic!("{:?}", got.map(|_| ())) };
    assert_eq!(crate::fleet::build::unreachable_kind(&why.to_string()), "auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_run_as_child_processes_would() {
    app_home();
    let s = server("exec").await;
    let m = spec("ssh-exec", s.port);
    signed_in(&m, &s, Some("secret")).await.unwrap();
    remember_password(&m.id, "secret".into());

    let out = spawn(&m, "printf out; printf err >&2; exit 3").await.unwrap().wait_with_output().await.unwrap();
    assert_eq!((out.stdout.as_slice(), out.stderr.as_slice(), out.status.code()), (&b"out"[..], &b"err"[..], Some(3)));

    // A megabyte in, counted there.
    let mut child = spawn(&m, "wc -c").await.unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let big = vec![b'x'; 1 << 20];
    stdin.write_all(&big).await.unwrap();
    drop(stdin);
    let mut said = String::new();
    child.stdout.take().unwrap().read_to_string(&mut said).await.unwrap();
    assert_eq!(said.trim(), (1 << 20).to_string());
    assert!(child.wait().await.unwrap().success());

    // Both ways at once, as a link runs.
    let mut child = spawn(&m, "cat").await.unwrap();
    let (mut stdin, mut stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
    let mut back = [0u8; 5];
    stdin.write_all(b"hello").await.unwrap();
    stdout.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"hello");
    drop(stdin);
    assert!(child.wait().await.unwrap().success());

    // Several on one connection.
    let runs = (0..4).map(|i| {
        let m = m.clone();
        async move { run(&m, &format!("echo {i}")).await.unwrap() }
    });
    let said: Vec<String> = futures::future::join_all(runs).await;
    assert_eq!(said, vec!["0\n", "1\n", "2\n", "3\n"]);
    forget_password(&m.id);
    disconnect(&m.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_that_isnt_there_says_so_in_ssh_words() {
    app_home();
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let refused = session(&spec("ssh-refused", free)).await.err().unwrap().to_string();
    assert_eq!(crate::fleet::build::unreachable_kind(&refused), "refused", "{refused}");
    let mut gone = spec("ssh-unknown", 22);
    gone.host = "tester@no-such-host.invalid".into();
    let unknown = session(&gone).await.err().unwrap().to_string();
    assert_eq!(crate::fleet::build::unreachable_kind(&unknown), "unknown-host", "{unknown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_takes_the_connection_it_was_added_with() {
    app_home();
    let s = server("adopt").await;
    let adding = spec("ssh-adding", s.port);
    let session = signed_in(&adding, &s, Some("secret")).await.unwrap();
    adopt("ssh-adding", "ssh-added");
    // The same connection, under its id now (no password needed again).
    let added = spec("ssh-added", s.port);
    assert!(Arc::ptr_eq(&super::session(&added).await.unwrap(), &session));
    assert!(sessions().lock().unwrap().get("ssh-adding").is_none());
    disconnect("ssh-added");
}

#[test]
fn this_apps_key_is_kept() {
    app_home();
    let line = public_key_line().unwrap();
    assert!(line.starts_with("ssh-ed25519 AAAA") && line.contains(" muxin@"), "{line}");
    assert_eq!(public_key_line().unwrap(), line);
    let saved = std::fs::read_to_string(crate::fleet::direct::config_dir().unwrap().join("identity/ssh_ed25519")).unwrap();
    assert!(saved.contains("OPENSSH PRIVATE KEY"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(crate::fleet::direct::config_dir().unwrap().join("identity/ssh_ed25519")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn windows_scripts_name_the_right_file() {
    let s = authorize_ps("ssh-ed25519 AAAA muxin@ipad");
    assert!(s.contains("administrators_authorized_keys") && s.contains("icacls") && s.contains("authorized_keys"));
    assert!(deauthorize_ps("ssh-ed25519 AAAA").contains("StartsWith($key)"));
    let _ = Path::new("");
}

/// The Windows scripts on a real Windows machine, as an administrator, with
/// ProgramData pointed at a scratch folder there so the machine's own keys
/// stay as they are. `MUXIN_TEST_WINDOWS=<ssh host> cargo test
/// windows_scripts_on_windows -- --ignored`, from a computer with ssh.
#[test]
#[ignore]
fn windows_scripts_on_windows() {
    let host = std::env::var("MUXIN_TEST_WINDOWS").expect("MUXIN_TEST_WINDOWS=<ssh host of a Windows machine>");
    let run = |script: &str| {
        let pre = "$ErrorActionPreference = 'Stop'\n$env:ProgramData = Join-Path $env:TEMP 'muxin-keys-test'\n\
                   New-Item -ItemType Directory -Force (Join-Path $env:ProgramData 'ssh') | Out-Null\n\
                   $t = Join-Path $env:ProgramData 'ssh\\administrators_authorized_keys'\n";
        let out = std::process::Command::new("ssh").arg(&host).arg(powershell(&format!("{pre}{script}"))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
    };
    let show = "\"--\"; Get-Content $t; \"--\"; icacls $t | Select-Object -Skip 0 -First 3\n";
    let ipad = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIipad muxin@ThisiPad";
    run("Remove-Item -Recurse -Force $env:ProgramData\n");

    // A file another tool wrote, without a newline at its end.
    let got = run(&format!("Set-Content -Path $t -Value 'ssh-ed25519 AAAAother other@mac' -NoNewline\n{}{show}", authorize_ps(ipad)));
    let lines: Vec<&str> = got.split("--\n").nth(1).unwrap().lines().collect();
    assert_eq!(lines, ["ssh-ed25519 AAAAother other@mac", ipad], "{got}");
    let acl = got.split("--\n").nth(2).unwrap();
    // "NT AUTHORITY\SYSTEM:(F)" splits at its space.
    let mut entries: Vec<&str> = acl.split_whitespace().filter(|w| w.contains(":(")).collect();
    entries.sort();
    assert_eq!(entries, ["AUTHORITY\\SYSTEM:(F)", "BUILTIN\\Administrators:(F)"], "{acl}");

    // Twice: once in the file.
    let got = run(&format!("{}{show}", authorize_ps(ipad)));
    assert_eq!(got.matches("muxin@ThisiPad").count(), 1, "{got}");

    // Taken off again, the other key stays; then the last key leaves an empty file.
    let key = ipad.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let got = run(&format!("{}{show}", deauthorize_ps(&key)));
    assert_eq!(got.split("--\n").nth(1).unwrap().trim(), "ssh-ed25519 AAAAother other@mac", "{got}");
    let got = run(&format!("{}{}{show}", deauthorize_ps("ssh-ed25519 AAAAother"), deauthorize_ps(&key)));
    assert_eq!(got.split("--\n").nth(1).unwrap().trim(), "", "{got}");
    run("Remove-Item -Recurse -Force $env:ProgramData\n");
}
