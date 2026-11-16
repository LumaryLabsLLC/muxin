//! A machine that really sleeps, watched by an engine that stays open the
//! way the window does (its background work running), from a config folder
//! of its own. For checking by hand what the other tests can't: that a Mac's
//! own notices are what the agent takes them for, that nothing Muxin sends
//! wakes it, and that it's back when it wakes.
//!
//! It connects to a real machine, puts this build's agent on it and pairs
//! with it: only with its owner's say. Disconnect the machine in any other
//! Muxin first (an older one keeps waking it, and puts its own agent back).
//! Every run is a new app to the machine (a test run's certificate is its
//! own): `unpair` before `stop`, or it stays in the machine's trusted list.
//! A Mac must not ask about the agent under Privacy & Security › Local
//! Network: it only answers (`notice` says whether this copy waits to hear
//! the machine wake, and on which connection).
//!
//! ```text
//! MUXEN_AGENTS_DIR=dist/agents MUXIN_SLEEP_TEST_HOST=<ssh host> MUXIN_SLEEP_TEST_DIR=<folder> \
//!     cargo test --no-default-features fleet::sleep_e2e -- --ignored --nocapture
//! ```
//!
//! What it sees goes to `<folder>/watch.log`, a line a change (the machine's
//! status, how it's linked), and Muxin's own log to `<folder>/muxin.log`
//! (`MUXIN_SLEEP_TEST_LOG` says what goes in it; `muxin=debug` unless set). It
//! does what's written to `<folder>/do`, a line each:
//!
//! - `check` (that machine, by name), `check-all` (every machine)
//! - `wake`, `connection`, `sessions`, `notice` (does this copy wait to hear it wake?)
//! - `run <command>` (on the machine, in its login shell)
//! - `unpair` (this copy's direct connection), `stop`
//!
//! On the machine, `~/.muxen/direct/listen.log` has a line for each sleep and
//! each wake, with how often it was reached in between; `pmset -g log` has
//! the Mac's side of it. Read them once it's awake: reading wakes it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::Engine;

fn say(log: &Mutex<std::fs::File>, text: impl AsRef<str>) {
    let line = format!("{}  {}\n", chrono::Local::now().format("%H:%M:%S"), text.as_ref());
    eprint!("{line}");
    let mut f = log.lock().unwrap();
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

/// The machine as a line: its status and why, and how it's linked.
fn describe(engine: &Engine, id: &str) -> String {
    let s = engine.current();
    let Some(m) = s.machines.iter().find(|m| m.id == id) else { return "not a machine any more".into() };
    let why = m.unreachable.as_ref().map(|u| format!(" ({}: {} {})", u.kind, u.title, u.detail)).unwrap_or_default();
    let link = match super::link::current(id) {
        Some(l) => format!("{} over {}, agent {}", l.transport(), l.network().unwrap_or("?"), l.version()),
        None => "none".into(),
    };
    format!("{}{}{why} · link: {link}", m.status, if m.checking { ", checking" } else { "" })
}

#[test]
#[ignore]
fn watch_a_machine_that_sleeps() {
    let (Ok(host), Ok(dir)) = (std::env::var("MUXIN_SLEEP_TEST_HOST"), std::env::var("MUXIN_SLEEP_TEST_DIR")) else {
        eprintln!("set MUXIN_SLEEP_TEST_HOST (an ssh host) and MUXIN_SLEEP_TEST_DIR (a folder of its own)");
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let open = |name: &str| std::fs::OpenOptions::new().create(true).append(true).open(dir.join(name)).unwrap();
    let log = Arc::new(Mutex::new(open("watch.log")));
    // Muxin's own words for what it does: every link, check and notice.
    let filter = std::env::var("MUXIN_SLEEP_TEST_LOG").unwrap_or_else(|_| "muxin=debug".into());
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(filter)).with_ansi(false).with_writer(Mutex::new(open("muxin.log"))).try_init();
    if !dir.join("config.toml").exists() {
        std::fs::write(dir.join("config.toml"), format!("[[machine]]\nname = \"Sleep test\"\nhost = \"{host}\"\n")).unwrap();
    }
    // ssh signs in as it does for the app: with the user's ssh agent.
    crate::probe::find_ssh_agent();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    runtime.block_on(async {
        let engine = Engine::new(crate::config::Paths::resolve(Some(dir.clone()), None), false);
        engine.start();
        let id = engine.current().machines.first().map(|m| m.id.clone()).expect("a machine in its config.toml");
        say(&log, format!("watching {id} at {host}"));
        engine.run_check(Some(vec![id.clone()])).await;
        say(&log, describe(&engine, &id));
        // Directly, as machines are by default: the agent goes on first.
        if engine.current().machines.iter().any(|m| m.id == id && m.status == "online") && super::direct::paired(&id).is_none() {
            match engine.set_direct(&id, true).await {
                Ok(c) => say(&log, format!("paired: {} over {}", c.via, c.network.unwrap_or_default())),
                Err(e) => say(&log, format!("couldn't pair: {e}")),
            }
        }
        let mut last = String::new();
        loop {
            let now = describe(&engine, &id);
            if now != last {
                say(&log, &now);
                last = now;
            }
            let asked = std::fs::read_to_string(dir.join("do")).unwrap_or_default();
            if !asked.is_empty() {
                let _ = std::fs::remove_file(dir.join("do"));
            }
            for line in asked.lines().map(str::trim).filter(|l| !l.is_empty()) {
                say(&log, format!("> {line}"));
                match line.split_once(' ').unwrap_or((line, "")) {
                    ("check", _) => engine.run_check(Some(vec![id.clone()])).await,
                    ("check-all", _) => engine.run_check(None).await,
                    ("wake", _) => match engine.wake(&id).await {
                        Ok(w) => say(&log, format!("wake: awake {} · {}", w.awake, w.message)),
                        Err(e) => say(&log, format!("wake: {e}")),
                    },
                    ("connection", _) => match engine.connection(&id).await {
                        Ok(c) => say(&log, format!("connection: {} over {} · {:?} ms · {}", c.via, c.network.unwrap_or_default(), c.latency_ms, c.note.unwrap_or_default())),
                        Err(e) => say(&log, format!("connection: {e}")),
                    },
                    ("sessions", _) => match engine.refresh_sessions(&id).await {
                        Ok(found) => say(&log, format!("sessions: {} there, agent {:?}", found.sessions.len(), found.agent_version)),
                        Err(e) => say(&log, format!("sessions: {e}")),
                    },
                    ("run", command) => {
                        let req = crate::api::RunRequest { machine_ids: vec![id.clone()], command: command.to_string(), timeout_secs: Some(120), wait: Some(true) };
                        match crate::api::run_on_machines(&engine, req).await {
                            Ok(out) => {
                                for o in out {
                                    say(&log, format!("run: exit {:?} {}", o.exit_code, o.error.unwrap_or_default()));
                                    for l in o.operation.map(|op| op.log_tail).unwrap_or_default() {
                                        say(&log, format!("    {l}"));
                                    }
                                }
                            }
                            Err(e) => say(&log, format!("run: {} ({})", e.message, e.code)),
                        }
                    }
                    // Whether this copy waits to hear the machine wake, and on what.
                    ("notice", _) => match (super::quiet::held(&id), super::link::current(&id)) {
                        (Some(at), _) => say(&log, format!("notice: it says so on the connection from {at}")),
                        (None, Some(l)) => say(&log, format!("notice: none (its agent offers one: {}; this copy wants one: {})", l.offers_wake_notice(), super::quiet::wanted())),
                        (None, None) => say(&log, "notice: none, and no link"),
                    },
                    ("unpair", _) => match engine.set_direct(&id, false).await {
                        Ok(c) => say(&log, format!("unpaired: now {}", c.via)),
                        Err(e) => say(&log, format!("unpair: {e}")),
                    },
                    ("stop", _) => {
                        say(&log, "stopped");
                        return;
                    }
                    _ => say(&log, "?"),
                }
                last.clear();
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}
