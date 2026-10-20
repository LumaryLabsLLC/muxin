//! Shares end to end: a share server, an agent with sessions, a host and
//! guests, all on this computer. Needs the server running and an agent
//! build, and moves HOME (so it runs alone):
//!
//! ```text
//! cargo run -p muxin-share-server &
//! MUXIN_TEST_SHARE_SERVER=http://127.0.0.1:8787 MUXIN_TEST_AGENT=target/debug/muxen-agent \
//!   cargo test share::tests -- --ignored --test-threads=1
//! ```

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use super::guest::{self, Frames, Update};
use super::host::{self, Desktop, Options, Port, Source, Target};
use crate::config::MachineSpec;
use crate::fleet::link::{self, Link};

struct Local(MachineSpec);

#[async_trait::async_trait]
impl Source for Local {
    async fn link(&self) -> Result<Arc<Link>, String> {
        link::get(&self.0).await.ok_or_else(|| "no link".to_string())
    }

    async fn start(&self, agent: &str) -> Result<String, String> {
        let l = self.link().await?;
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) % 100_000;
        let name = format!("{agent}-{n}");
        let home = std::env::var("HOME").unwrap_or_default();
        let r = l.request(json!({ "op": "start", "name": name, "agent": agent, "title": "Started by a guest", "cwd": home, "cols": 100, "rows": 30 }), Duration::from_secs(15)).await?;
        if r["ok"] != true {
            return Err(r["error"].as_str().unwrap_or("didn't start").to_string());
        }
        Ok(name)
    }

    async fn stop(&self, name: &str, restart: bool) -> Result<(), String> {
        let l = self.link().await?;
        let r = l.request(json!({ "op": if restart { "restart" } else { "stop" }, "name": name }), Duration::from_secs(15)).await?;
        if r["ok"] == true { Ok(()) } else { Err(r["error"].as_str().unwrap_or("didn't stop").to_string()) }
    }

    fn agents(&self) -> Vec<String> {
        vec!["shell".into()]
    }
}

/// Waits for an update that `want` accepts, skipping others.
async fn next(rx: &mut mpsc::UnboundedReceiver<Update>, what: &str, want: impl Fn(&Update) -> bool) -> Update {
    let wait = async {
        loop {
            match rx.recv().await {
                Some(u) if want(&u) => return u,
                Some(_) => {}
                None => panic!("updates ended waiting for {what}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait).await.unwrap_or_else(|_| panic!("no {what}"))
}

/// Everything a guest's terminal got, until `needle` shows up (or not).
async fn output_until(frames: &mut Frames, needle: &str, within: Duration) -> (String, bool) {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some((kind, b))) = tokio::time::timeout_at(deadline, frames.recv()).await {
        if kind == super::DATA {
            text.push_str(&String::from_utf8_lossy(&b));
            if text.contains(needle) {
                return (text, true);
            }
        }
    }
    (text, false)
}

/// A home of its own with the test agent in it (the agent and its sessions
/// live there), and a link to that agent.
async fn test_machine(tag: &str) -> (std::path::PathBuf, MachineSpec, Arc<Link>) {
    let agent = std::env::var("MUXIN_TEST_AGENT").expect("set MUXIN_TEST_AGENT");
    let home = std::env::temp_dir().join(format!("muxin-share-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".muxen/bin")).unwrap();
    std::fs::copy(&agent, home.join(".muxen/bin/muxen-agent")).unwrap();
    unsafe { std::env::set_var("HOME", &home) };
    let spec: MachineSpec = toml::from_str("name = \"Here\"\nid = \"here\"\nhost = \"local\"").unwrap();
    // Not a link to another test's agent (in its home, gone now).
    link::reset("here");
    let l = link::get(&spec).await.expect("link to the test agent");
    (home, spec, l)
}

async fn start_session(l: &Link, home: &std::path::Path, name: &str, title: &str) {
    let started = l.request(json!({ "op": "start", "name": name, "title": title, "agent": "shell", "cwd": home.display().to_string(), "cols": 100, "rows": 30 }), Duration::from_secs(15)).await.unwrap();
    assert_eq!(started["ok"], true, "{started}");
}

fn target(session: &str, title: &str, agent: &str, machine_name: &str) -> Target {
    Target {
        session_id: if session.is_empty() { String::new() } else { format!("here/{session}") },
        machine_id: "here".into(),
        name: session.into(),
        title: title.into(),
        agent: agent.into(),
        machine_name: machine_name.into(),
        platform: String::new(),
        by: "Adib".into(),
    }
}

fn options(approve: bool, desktop: Desktop, ports: Vec<Port>, ttl: u64) -> Options {
    Options { approve, can_type: false, manage: false, desktop, ports, ttl: Duration::from_secs(ttl) }
}

async fn screen(l: &Link, name: &str) -> String {
    let r = l.request(json!({ "op": "read", "name": name, "lines": 50 }), Duration::from_secs(10)).await.unwrap();
    r["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn a_guest_watches_then_types_when_let() {
    let server = std::env::var("MUXIN_TEST_SHARE_SERVER").expect("set MUXIN_TEST_SHARE_SERVER");
    let (home, spec, l) = test_machine("session").await;
    start_session(&l, &home, "shared", "").await;
    let share = host::start(&server, target("shared", "Checkout fix", "shell", "Here"), options(true, Desktop::Control, vec![Port { port: 22, name: "ssh".into(), ..Default::default() }], 120), Arc::new(Local(spec.clone())))
        .await
        .expect("share");
    // A session's share has no desktop or servers, whatever was asked.
    assert_eq!((share.desktop, share.ports.len()), (Desktop::None, 0));
    assert!(share.link.contains("/s#1."));

    // A link with the wrong secret gets nowhere.
    let mut forged = super::ticket::Ticket::parse(&share.link).unwrap();
    forged.secret = [0; 32];
    match guest::join(&forged.link(), "Mallory").await {
        Ok((_, mut rx)) => match next(&mut rx, "the end", |u| matches!(u, Update::Ended(_))).await {
            Update::Ended(why) => assert!(why.contains("doesn't work"), "{why}"),
            _ => unreachable!(),
        },
        Err(e) => panic!("forged link: {e}"),
    }
    // Nor does one naming another certificate (the server standing in).
    let mut swapped = super::ticket::Ticket::parse(&share.link).unwrap();
    swapped.fingerprint = [7; 32];
    let refused = guest::join(&swapped.link(), "Mallory").await.err().expect("refused");
    assert!(refused.contains("verified"), "{refused}");

    // Sam joins and waits with a code the host sees too.
    let (joined, mut rx) = guest::join(&share.link, "Sam").await.expect("join");
    let code = match next(&mut rx, "the code", |u| matches!(u, Update::Code(_))).await {
        Update::Code(c) => c,
        _ => unreachable!(),
    };
    let info = match next(&mut rx, "what's shared", |u| matches!(u, Update::Info(_))).await {
        Update::Info(i) => i,
        _ => unreachable!(),
    };
    assert_eq!((info.title.as_str(), info.by.as_str(), info.can_type), ("Checkout fix", "Adib", false));
    next(&mut rx, "waiting", |u| matches!(u, Update::Waiting)).await;
    let waiting = host::list().into_iter().find(|s| s.id == share.id).unwrap();
    let sam = waiting.guests.iter().find(|g| g.name == "Sam").expect("Sam knocks");
    assert_eq!((sam.code.as_str(), sam.state.as_str(), sam.client.as_str()), (code.as_str(), "waiting", "app"));
    host::admit(&share.id, sam.id, true);
    next(&mut rx, "admitted", |u| matches!(u, Update::Admitted { can_type: false, can_control: false, can_manage: false })).await;
    // The shared session, whatever name is asked for.
    let (channel, reply, mut frames) = joined.open(json!({ "op": "watch", "session": "other" })).await.expect("watch");
    assert_eq!(reply["session"], "shared");
    match tokio::time::timeout(Duration::from_secs(10), frames.recv()).await {
        Ok(Some((super::SIZE, b))) => assert_eq!(b, [0, 100, 0, 30]),
        other => panic!("the size first: {other:?}"),
    }
    // Nothing else is there.
    let desktop = joined.open(json!({ "op": "desktop" })).await.err().expect("no desktop");
    assert!(desktop.contains("isn't shared"), "{desktop}");
    let tcp = joined.open(json!({ "op": "tcp", "port": 22 })).await.err().expect("no servers");
    assert!(tcp.contains("isn't shared"), "{tcp}");

    // On this computer the connection is direct (no relay offered anyway).
    let mut path = String::new();
    for _ in 0..30 {
        path = host::list().into_iter().find(|s| s.id == share.id).and_then(|s| s.guests.into_iter().find(|g| g.id == sam.id)).map(|g| g.path).unwrap_or_default();
        if !path.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(path, "direct");

    // Watching: the owner's typing shows; Sam's keys go nowhere.
    let (owner, _owner_rx) = l.open_channel();
    let attached = l.request(json!({ "op": "attach", "name": "shared", "channel": owner }), Duration::from_secs(10)).await.unwrap();
    assert_eq!(attached["ok"], true);
    l.send(owner, link::DATA, b"echo FROM_OWNER\r");
    let (_, seen) = output_until(&mut frames, "FROM_OWNER", Duration::from_secs(10)).await;
    assert!(seen, "the guest sees the session");
    // Bypassing the app's own check: the host drops it.
    joined.send(channel, super::DATA, b"echo FROM_SAM_EARLY\r");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!screen(&l, "shared").await.contains("FROM_SAM_EARLY"));

    // Let Sam type.
    host::set_typing(&share.id, sam.id, true);
    next(&mut rx, "typing allowed", |u| matches!(u, Update::Typing(true))).await;
    // The session attaches again, typing: its frames come on the same channel.
    tokio::time::sleep(Duration::from_millis(500)).await;
    joined.send_keys(channel, b"echo FROM_SAM\r");
    let (_, typed) = output_until(&mut frames, "FROM_SAM", Duration::from_secs(10)).await;
    assert!(typed, "Sam's keys reach the session once let");
    assert!(screen(&l, "shared").await.contains("FROM_SAM"));

    // Someone the user doesn't let in hears so.
    let (_, mut kim) = guest::join(&share.link, "Kim").await.expect("join");
    next(&mut kim, "waiting", |u| matches!(u, Update::Waiting)).await;
    let knock = host::list().into_iter().find(|s| s.id == share.id).unwrap().guests.into_iter().find(|g| g.name == "Kim").expect("Kim knocks");
    host::admit(&share.id, knock.id, false);
    next(&mut kim, "denied", |u| matches!(u, Update::Denied)).await;

    // Stopping the share lets Sam go.
    host::stop(&share.id);
    match next(&mut rx, "the end", |u| matches!(u, Update::Ended(_))).await {
        Update::Ended(why) => assert_eq!(why, "They stopped sharing."),
        _ => unreachable!(),
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(host::list().is_empty());
    let after = guest::join(&share.link, "Sam").await;
    assert!(after.is_err(), "a stopped share's link stops working");

    let _ = l.request(json!({ "op": "stop", "name": "shared" }), Duration::from_secs(10)).await;
    let _ = std::fs::remove_dir_all(&home);
}

/// A small server on this computer: says back what it hears.
async fn echo_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn a_guest_uses_a_shared_machine() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = std::env::var("MUXIN_TEST_SHARE_SERVER").expect("set MUXIN_TEST_SHARE_SERVER");
    let (home, spec, l) = test_machine("machine").await;
    start_session(&l, &home, "api", "API server").await;
    start_session(&l, &home, "web", "Web").await;
    let (echo, closed) = (echo_server().await, echo_server().await);
    let ports = vec![Port { port: echo, name: "Echo".into(), ..Default::default() }];
    let share = host::start(&server, target("", "Here", "", "Here"), options(true, Desktop::View, ports, 120), Arc::new(Local(spec.clone()))).await.expect("share");

    let (joined, mut rx) = guest::join(&share.link, "Sam").await.expect("join");
    let info = match next(&mut rx, "what's shared", |u| matches!(u, Update::Info(_))).await {
        Update::Info(i) => i,
        _ => unreachable!(),
    };
    assert!(info.whole_machine);
    assert_eq!((info.title.as_str(), info.desktop, info.can_control, info.ports.len()), ("Here", Desktop::View, false, 1));
    // Nothing opens before the user lets Sam in.
    next(&mut rx, "waiting", |u| matches!(u, Update::Waiting)).await;
    let early = tokio::time::timeout(Duration::from_secs(3), joined.open(json!({ "op": "watch", "session": "api" }))).await;
    assert!(!matches!(early, Ok(Ok(_))), "nothing before being let in");
    let sam = host::list().into_iter().find(|s| s.id == share.id).unwrap().guests.into_iter().find(|g| g.name == "Sam").unwrap();
    host::admit(&share.id, sam.id, true);
    next(&mut rx, "admitted", |u| matches!(u, Update::Admitted { .. })).await;

    // The machine's sessions, as they are.
    let names = |u: &Update| match u {
        Update::Sessions(list) => list.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let list = next(&mut rx, "the sessions", |u| matches!(u, Update::Sessions(_))).await;
    assert_eq!(names(&list), ["api", "web"]);

    // Two sessions at once, each on its channel; a made-up one is refused.
    let (api, _, mut api_frames) = joined.open(json!({ "op": "watch", "session": "api" })).await.expect("watch api");
    let (_, _, mut web_frames) = joined.open(json!({ "op": "watch", "session": "web" })).await.expect("watch web");
    let nope = joined.open(json!({ "op": "watch", "session": "nope" })).await.err().expect("refused");
    assert!(nope.contains("isn't there"), "{nope}");
    let (owner, _owner_rx) = l.open_channel();
    l.request(json!({ "op": "attach", "name": "web", "channel": owner }), Duration::from_secs(10)).await.unwrap();
    l.send(owner, link::DATA, b"echo ON_WEB\r");
    assert!(output_until(&mut web_frames, "ON_WEB", Duration::from_secs(10)).await.1, "web's output on web's channel");
    assert!(!output_until(&mut api_frames, "ON_WEB", Duration::from_secs(1)).await.1, "not on api's");

    // Typing only once let.
    joined.send(api, super::DATA, b"echo SAM_EARLY\r");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!screen(&l, "api").await.contains("SAM_EARLY"));
    host::set_typing(&share.id, sam.id, true);
    next(&mut rx, "typing allowed", |u| matches!(u, Update::Typing(true))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    joined.send_keys(api, b"echo SAM_TYPED\r");
    assert!(output_until(&mut api_frames, "SAM_TYPED", Duration::from_secs(10)).await.1);

    // Starting and ending sessions: not until let.
    let refused = joined.call(json!({ "op": "start", "agent": "shell" })).await.err().expect("not let yet");
    assert!(refused.contains("Ask"), "{refused}");
    host::set_managing(&share.id, sam.id, true);
    next(&mut rx, "starting sessions allowed", |u| matches!(u, Update::Manage(true))).await;
    assert!(joined.can_manage());
    let started = joined.call(json!({ "op": "start", "agent": "shell" })).await.expect("a terminal started");
    next(&mut rx, "the session started", |u| names(u).contains(&started)).await;
    assert!(joined.call(json!({ "op": "start", "agent": "claude" })).await.is_err(), "only agents installed there");
    assert!(joined.call(json!({ "op": "end", "session": "made-up" })).await.is_err(), "only sessions that are there");
    assert_eq!(joined.call(json!({ "op": "end", "session": started.clone() })).await.as_deref(), Ok(started.as_str()));
    next(&mut rx, "the session gone", |u| matches!(u, Update::Sessions(_)) && !names(u).contains(&started)).await;

    // A session started later shows up.
    start_session(&l, &home, "later", "Later").await;
    let list = next(&mut rx, "the new session", |u| names(u).contains(&"later".to_string())).await;
    assert_eq!(names(&list).len(), 3);

    // The shared server, and only it.
    let (echo_ch, _, mut echo_frames) = joined.open(json!({ "op": "tcp", "port": echo })).await.expect("tcp");
    joined.send(echo_ch, super::DATA, b"ping");
    match tokio::time::timeout(Duration::from_secs(5), echo_frames.recv()).await {
        Ok(Some((super::DATA, b))) => assert_eq!(b, b"ping"),
        other => panic!("the echo: {other:?}"),
    }
    // A frame bigger than a data channel message goes in pieces, and comes
    // back (in the server's own reads) whole.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    joined.send(echo_ch, super::DATA, &big);
    let mut back = Vec::new();
    while back.len() < big.len() {
        match tokio::time::timeout(Duration::from_secs(10), echo_frames.recv()).await {
            Ok(Some((super::DATA, b))) => back.extend(b),
            other => panic!("the big echo, {} bytes in: {other:?}", back.len()),
        }
    }
    assert!(back == big, "300 KB through and back, in order");
    let refused = joined.open(json!({ "op": "tcp", "port": closed })).await.err().expect("not shared");
    assert!(refused.contains("isn't shared"), "{refused}");
    // Forwarded to a port here, as the app's Open does.
    let local = joined.forward(echo).await.expect("forward");
    let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", local)).await.unwrap();
    conn.write_all(b"hello through").await.unwrap();
    let mut got = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(5), conn.read_exact(&mut got)).await.expect("echo in time").unwrap();
    assert_eq!(&got, b"hello through");
    // Viewing the desktop never lets the mouse through.
    host::set_control(&share.id, sam.id, true);
    assert!(!host::list()[0].guests[0].can_control);

    // Taking the servers and the desktop back closes what's open on them.
    host::set_access(&share.id, Desktop::None, Vec::new());
    match next(&mut rx, "the access", |u| matches!(u, Update::Access { .. })).await {
        Update::Access { desktop, ports } => assert_eq!((desktop, ports.len()), (Desktop::None, 0)),
        _ => unreachable!(),
    }
    let closed_too = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some((kind, _)) = echo_frames.recv().await {
            if kind == super::CLOSE {
                return true;
            }
        }
        true
    });
    assert!(closed_too.await.unwrap_or(false), "the server's channel closes");
    assert!(joined.open(json!({ "op": "tcp", "port": echo })).await.is_err());
    assert!(joined.open(json!({ "op": "desktop" })).await.is_err());
    assert!(joined.forwarded(echo).is_none(), "the forward here stops too");

    host::stop(&share.id);
    next(&mut rx, "the end", |u| matches!(u, Update::Ended(_))).await;
    for name in ["api", "web", "later"] {
        let _ = l.request(json!({ "op": "stop", "name": name }), Duration::from_secs(10)).await;
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// Hosts a share for a browser to join by hand (or by a browser driver):
/// writes the link to `MUXIN_TEST_LINK_FILE`, lets guests in, lets them type
/// 15 seconds later, and keeps the sessions busy. Stops after 10 minutes.
/// `MUXIN_TEST_SHARE_MACHINE=1` shares the machine (two sessions and a small
/// web server) instead of one session; `MUXIN_TEST_SHARE_DESKTOP=view` or
/// `control` shares its desktop too (this computer's screen: not on yours).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn hosts_for_a_browser() {
    let (Ok(server), Ok(link_file)) = (std::env::var("MUXIN_TEST_SHARE_SERVER"), std::env::var("MUXIN_TEST_LINK_FILE")) else {
        panic!("set MUXIN_TEST_SHARE_SERVER, MUXIN_TEST_AGENT and MUXIN_TEST_LINK_FILE");
    };
    let machine = std::env::var("MUXIN_TEST_SHARE_MACHINE").is_ok_and(|v| v == "1");
    let desktop = match std::env::var("MUXIN_TEST_SHARE_DESKTOP").unwrap_or_default().as_str() {
        "view" => Desktop::View,
        "control" => Desktop::Control,
        _ => Desktop::None,
    };
    let (home, spec, l) = test_machine("browser").await;
    start_session(&l, &home, "shared", "Checkout fix").await;
    let share = if machine {
        start_session(&l, &home, "tests", "Tests").await;
        let web = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = web.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = web.accept().await {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let body = "<h1>Hello from the shared machine</h1>";
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        let ports = vec![Port { port, name: "Checkout dev".into(), ..Default::default() }];
        host::start(&server, target("", "Forge", "", "Forge"), options(true, desktop, ports, 600), Arc::new(Local(spec))).await.expect("share")
    } else {
        host::start(&server, target("shared", "Checkout fix", "claude", "Forge"), options(true, Desktop::None, Vec::new(), 600), Arc::new(Local(spec))).await.expect("share")
    };
    std::fs::write(&link_file, &share.link).unwrap();
    let (owner, _rx) = l.open_channel();
    l.request(json!({ "op": "attach", "name": "shared", "channel": owner }), Duration::from_secs(10)).await.unwrap();
    let started = std::time::Instant::now();
    let mut admitted: std::collections::HashMap<u64, std::time::Instant> = Default::default();
    let mut tick = 0u32;
    while started.elapsed() < Duration::from_secs(600) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tick += 1;
        let Some(view) = host::list().into_iter().find(|s| s.id == share.id) else { break };
        for g in &view.guests {
            if g.state == "waiting" && !admitted.contains_key(&g.id) {
                eprintln!("guest {} ({}) code {} — letting in", g.name, g.client, g.code);
                admitted.insert(g.id, std::time::Instant::now());
                host::admit(&share.id, g.id, true);
            }
            if let Some(at) = admitted.get(&g.id) {
                if !g.can_type && at.elapsed() > Duration::from_secs(15) {
                    eprintln!("letting {} type ({})", g.name, g.path);
                    host::set_typing(&share.id, g.id, true);
                }
            }
        }
        if tick % 5 == 0 {
            l.send(owner, link::DATA, format!("echo owner tick {tick}\r").as_bytes());
        }
    }
    host::stop(&share.id);
    for name in ["shared", "tests"] {
        let _ = l.request(json!({ "op": "stop", "name": name }), Duration::from_secs(10)).await;
    }
    let _ = std::fs::remove_dir_all(&home);
}
