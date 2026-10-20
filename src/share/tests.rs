//! A share end to end: a share server, an agent with a session, a host and
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

use super::guest::{self, Update};
use super::host::{self, Options, Source, Target};
use crate::config::MachineSpec;
use crate::fleet::link::{self, Link};

struct Local(MachineSpec);

#[async_trait::async_trait]
impl Source for Local {
    async fn link(&self) -> Result<Arc<Link>, String> {
        link::get(&self.0).await.ok_or_else(|| "no link".to_string())
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

/// Everything the guest's terminal got, until `needle` shows up (or not).
async fn output_until(rx: &mut mpsc::UnboundedReceiver<Update>, needle: &str, within: Duration) -> (String, bool) {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(u)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let Update::Output(b) = u {
            text.push_str(&String::from_utf8_lossy(&b));
            if text.contains(needle) {
                return (text, true);
            }
        }
    }
    (text, false)
}

async fn screen(l: &Link) -> String {
    let r = l.request(json!({ "op": "read", "name": "shared", "lines": 50 }), Duration::from_secs(10)).await.unwrap();
    r["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn a_guest_watches_then_types_when_let() {
    let (Ok(server), Ok(agent)) = (std::env::var("MUXIN_TEST_SHARE_SERVER"), std::env::var("MUXIN_TEST_AGENT")) else {
        panic!("set MUXIN_TEST_SHARE_SERVER and MUXIN_TEST_AGENT");
    };
    // A home of its own: the agent and its sessions live there.
    let home = std::env::temp_dir().join(format!("muxin-share-test-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".muxen/bin")).unwrap();
    std::fs::copy(&agent, home.join(".muxen/bin/muxen-agent")).unwrap();
    unsafe { std::env::set_var("HOME", &home) };
    let spec: MachineSpec = toml::from_str("name = \"Here\"\nid = \"here\"\nhost = \"local\"").unwrap();
    let l = link::get(&spec).await.expect("link to the test agent");
    let started = l.request(json!({ "op": "start", "name": "shared", "agent": "shell", "cwd": home.display().to_string(), "cols": 100, "rows": 30 }), Duration::from_secs(15)).await.unwrap();
    assert_eq!(started["ok"], true, "{started}");

    let target = Target {
        session_id: "here/shared".into(),
        machine_id: "here".into(),
        name: "shared".into(),
        title: "Checkout fix".into(),
        agent: "shell".into(),
        machine_name: "Here".into(),
        by: "Adib".into(),
    };
    let share = host::start(&server, target, Options { approve: true, can_type: false, ttl: Duration::from_secs(120) }, Arc::new(Local(spec.clone()))).await.expect("share");
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
    next(&mut rx, "admitted", |u| matches!(u, Update::Admitted { can_type: false })).await;
    match next(&mut rx, "the size", |u| matches!(u, Update::Size(..))).await {
        Update::Size(c, r) => assert_eq!((c, r), (100, 30)),
        _ => unreachable!(),
    }

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
    let (_, seen) = output_until(&mut rx, "FROM_OWNER", Duration::from_secs(10)).await;
    assert!(seen, "the guest sees the session");
    joined.send_keys(b"echo FROM_SAM_EARLY\r");
    // Bypassing the app's own check: the host drops it too.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!screen(&l).await.contains("FROM_SAM_EARLY"));

    // Let Sam type.
    host::set_typing(&share.id, sam.id, true);
    next(&mut rx, "typing allowed", |u| matches!(u, Update::Typing(true))).await;
    joined.send_keys(b"echo FROM_SAM\r");
    let (_, typed) = output_until(&mut rx, "FROM_SAM", Duration::from_secs(10)).await;
    assert!(typed, "Sam's keys reach the session once let");
    assert!(screen(&l).await.contains("FROM_SAM"));

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

/// Hosts a share for a browser to join by hand (or by a browser driver):
/// writes the link to `MUXIN_TEST_LINK_FILE`, lets guests in, lets them type
/// 15 seconds later, and keeps the session busy. Stops after 10 minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn hosts_for_a_browser() {
    let (Ok(server), Ok(agent), Ok(link_file)) = (std::env::var("MUXIN_TEST_SHARE_SERVER"), std::env::var("MUXIN_TEST_AGENT"), std::env::var("MUXIN_TEST_LINK_FILE")) else {
        panic!("set MUXIN_TEST_SHARE_SERVER, MUXIN_TEST_AGENT and MUXIN_TEST_LINK_FILE");
    };
    let home = std::env::temp_dir().join(format!("muxin-share-browser-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".muxen/bin")).unwrap();
    std::fs::copy(&agent, home.join(".muxen/bin/muxen-agent")).unwrap();
    unsafe { std::env::set_var("HOME", &home) };
    let spec: MachineSpec = toml::from_str("name = \"Here\"\nid = \"here\"\nhost = \"local\"").unwrap();
    let l = link::get(&spec).await.expect("link to the test agent");
    l.request(json!({ "op": "start", "name": "shared", "agent": "shell", "cwd": home.display().to_string(), "cols": 100, "rows": 30 }), Duration::from_secs(15)).await.unwrap();
    let target = Target {
        session_id: "here/shared".into(),
        machine_id: "here".into(),
        name: "shared".into(),
        title: "Checkout fix".into(),
        agent: "claude".into(),
        machine_name: "Forge".into(),
        by: "Adib".into(),
    };
    let share = host::start(&server, target, Options { approve: true, can_type: false, ttl: Duration::from_secs(600) }, Arc::new(Local(spec))).await.expect("share");
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
    let _ = l.request(json!({ "op": "stop", "name": "shared" }), Duration::from_secs(10)).await;
    let _ = std::fs::remove_dir_all(&home);
}
