//! Direct links against a real agent (in a home of its own, on a port of its
//! own): terminals on streams of their own (lanes) with old and new agents,
//! and an old app with a new agent. And the focused terminal on a slow
//! network, measured: the agent reached through a slow link made here (UDP
//! with a bandwidth cap, a delay each way and a queue that drops past its
//! length), one session printing as fast as it can and another echoing
//! what's typed. How long a key takes to come back is the number.
//!
//! ```text
//! MUXIN_TEST_AGENT=target/release/muxen-agent cargo test --no-default-features \
//!     fleet::link_e2e -- --ignored --nocapture --test-threads=1
//! ```
//!
//! For the bench, `MUXIN_BENCH_KBPS` (default 2000), `MUXIN_BENCH_DELAY_MS`
//! (one way, default 25), `MUXIN_BENCH_QUEUE_MS` (default 150) shape the
//! link and `MUXIN_BENCH_PROBES` (default 12) says how many keys to time.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::direct;
use super::link::{self, DATA, Link};
use crate::config::MachineSpec;

/// A slow link to `agent`: returns the port to reach it at.
async fn slow_link(agent: SocketAddr, bytes_per_sec: f64, delay: Duration, queue: Duration) -> u16 {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    back.connect(agent).await.unwrap();
    let port = front.local_addr().unwrap().port();
    let app: Arc<Mutex<Option<SocketAddr>>> = Default::default();
    // One way of the link: packets wait their turn on the wire (dropped when
    // the queue is longer than `queue`), then `delay`.
    fn way(bytes_per_sec: f64, delay: Duration, queue: Duration) -> (mpsc::UnboundedSender<Vec<u8>>, mpsc::UnboundedReceiver<(Instant, Vec<u8>)>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut free_at = Instant::now();
            while let Some(p) = rx.recv().await {
                let now = Instant::now();
                let start = free_at.max(now);
                if start - now > queue {
                    continue;
                }
                free_at = start + Duration::from_secs_f64(p.len() as f64 / bytes_per_sec);
                let _ = out_tx.send((free_at + delay, p));
            }
        });
        (tx, out_rx)
    }
    let (up, mut up_out) = way(bytes_per_sec, delay, queue);
    let (down, mut down_out) = way(bytes_per_sec, delay, queue);
    let (f, a) = (front.clone(), app.clone());
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            *a.lock().unwrap() = Some(from);
            let _ = up.send(buf[..n].to_vec());
        }
    });
    let b = back.clone();
    tokio::spawn(async move {
        while let Some((at, p)) = up_out.recv().await {
            tokio::time::sleep_until(at).await;
            let _ = b.send(&p).await;
        }
    });
    let b = back.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok(n) = b.recv(&mut buf).await {
            let _ = down.send(buf[..n].to_vec());
        }
    });
    tokio::spawn(async move {
        while let Some((at, p)) = down_out.recv().await {
            tokio::time::sleep_until(at).await;
            let to = *app.lock().unwrap();
            if let Some(to) = to {
                let _ = front.send_to(&p, to).await;
            }
        }
    });
    port
}

fn env_num(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A terminal attached to `name`: over a stream of its own when `own` (and
/// the link has them), else on a channel of the machine's link.
async fn attach(l: &Arc<Link>, name: &str, own: bool) -> (Arc<Link>, u32, mpsc::UnboundedReceiver<(u8, Vec<u8>)>) {
    let on = if own { l.lane().unwrap_or_else(|| l.clone()) } else { l.clone() };
    let (channel, frames) = on.open_channel();
    let r = on.request(json!({ "op": "attach", "name": name, "channel": channel, "cols": 100, "rows": 30 }), Duration::from_secs(30)).await.unwrap();
    assert_eq!(r["ok"], true, "{r}");
    (on, channel, frames)
}

/// Types `probes` keys into an echoing terminal and times each one's echo.
async fn echoes(on: &Link, channel: u32, frames: &mut mpsc::UnboundedReceiver<(u8, Vec<u8>)>, probes: usize) -> Vec<Duration> {
    // Whatever it had to say first (its screen).
    while tokio::time::timeout(Duration::from_millis(300), frames.recv()).await.is_ok_and(|f| f.is_some()) {}
    let mut times = Vec::new();
    for i in 0..probes {
        let key = b'A' + (i % 26) as u8;
        let sent = Instant::now();
        on.send(channel, DATA, &[key]);
        // Past this, counted as this long (and the rest skipped).
        let deadline = sent + Duration::from_secs(30);
        let came = loop {
            match tokio::time::timeout_at(deadline, frames.recv()).await {
                Ok(Some((DATA, b))) if b.contains(&key) => break true,
                Ok(Some(_)) => {}
                Ok(None) => panic!("the terminal closed"),
                Err(_) => break false,
            }
        };
        times.push(sent.elapsed());
        if !came {
            eprintln!("  key {i}: no echo within 30 s");
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    times
}

fn summary(times: &mut [Duration]) -> String {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.;
    let at = |q: f64| ms(times[((times.len() - 1) as f64 * q).round() as usize]);
    format!("min {:.0} ms, median {:.0} ms, p90 {:.0} ms, max {:.0} ms", ms(times[0]), at(0.5), at(0.9), ms(times[times.len() - 1]))
}

/// One runtime for all of them: the app's endpoints for direct connections
/// are made once a process, on the runtime that first uses them.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("a runtime"))
}

/// A test agent listening in a home of its own.
struct TestAgent {
    home: std::path::PathBuf,
    id: String,
    paired: direct::Paired,
    listener: std::process::Child,
}

impl TestAgent {
    /// Starts `MUXIN_TEST_AGENT` and links to it directly, through a slow
    /// link (bytes a second, delay each way, queue) when asked.
    async fn start(tag: &str, slow: Option<(f64, Duration, Duration)>) -> (TestAgent, Arc<Link>) {
        let agent = std::env::var("MUXIN_TEST_AGENT").expect("set MUXIN_TEST_AGENT");
        let home = std::env::temp_dir().join(format!("muxin-e2e-{tag}-{}", std::process::id()));
        let dir = home.join(".muxen/direct");
        std::fs::create_dir_all(&dir).unwrap();
        // This app's key, one for the whole run (`direct::init` takes one folder).
        let app_dir = std::env::temp_dir().join(format!("muxin-e2e-app-{}", std::process::id()));
        std::fs::create_dir_all(&app_dir).unwrap();
        direct::init(app_dir);
        let app = direct::app_fingerprint().unwrap();
        let port = 43000 + (std::process::id() % 1000) as u16 * 4 + tag.len() as u16 % 4;
        std::fs::write(dir.join("port"), port.to_string()).unwrap();
        std::fs::write(dir.join("trusted"), format!("{app}\n")).unwrap();
        // In its home, so what it starts can be found and stopped after.
        std::fs::create_dir_all(home.join(".muxen/bin")).unwrap();
        let agent_copy = home.join(".muxen/bin/muxen-agent");
        std::fs::copy(&agent, &agent_copy).unwrap();
        let listener = std::process::Command::new(&agent_copy).arg("listen").env("HOME", &home).stderr(std::process::Stdio::null()).spawn().unwrap();
        let mut agent_fp = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(cert) = std::fs::read(dir.join("cert.der")) {
                agent_fp = Some(direct::fingerprint(&cert));
                break;
            }
        }
        let port = match slow {
            Some((rate, delay, queue)) => slow_link(SocketAddr::from(([127, 0, 0, 1], port)), rate, delay, queue).await,
            None => port,
        };
        let paired = direct::Paired { fingerprint: agent_fp.expect("the agent's certificate"), host: "127.0.0.1".into(), port, lan: vec!["127.0.0.1".into()], tailnet: Vec::new() };
        direct::save(tag, Some(paired.clone())).unwrap();
        let spec: MachineSpec = toml::from_str(&format!("name = \"{tag}\"\nid = \"{tag}\"\nhost = \"{tag}.invalid\"")).unwrap();
        let mut l = None;
        for _ in 0..20 {
            if let Some(found) = link::current_or_direct(&spec).await {
                l = Some(found);
                break;
            }
            direct::retry_now(tag);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let l = l.expect("a direct link");
        (TestAgent { home, id: tag.to_string(), paired, listener }, l)
    }

    /// Stops it and everything it started, and removes its home.
    async fn stop(mut self, l: &Link) {
        for s in l.request(json!({ "op": "list" }), Duration::from_secs(5)).await.ok().and_then(|r| r["sessions"].as_array().cloned()).unwrap_or_default() {
            let _ = l.request(json!({ "op": "stop", "name": s["name"] }), Duration::from_secs(10)).await;
        }
        link::reset(&self.id);
        let _ = self.listener.kill();
        let _ = self.listener.wait();
        let _ = std::process::Command::new("pkill").args(["-f", &self.home.display().to_string()]).status();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

async fn start_session(l: &Link, home: &std::path::Path, name: &str, run: &str) {
    let r = l.request(json!({ "op": "start", "name": name, "title": name, "agent": "shell", "cwd": home.display().to_string(), "cols": 100, "rows": 30, "run": run }), Duration::from_secs(15)).await.unwrap();
    assert_eq!(r["ok"], true, "{r}");
}

#[test]
#[ignore]
fn link_bench() {
    runtime().block_on(bench());
}

async fn bench() {
    let rate = env_num("MUXIN_BENCH_KBPS", 2000.) * 1000. / 8.;
    let delay = Duration::from_millis(env_num("MUXIN_BENCH_DELAY_MS", 25.) as u64);
    let queue = Duration::from_millis(env_num("MUXIN_BENCH_QUEUE_MS", 150.) as u64);
    let (machine, l) = TestAgent::start("bench", Some((rate, delay, queue))).await;
    eprintln!("agent {}, link {:.0} kB/s, {} ms each way, {} ms queue", l.version(), rate / 1000., delay.as_millis(), queue.as_millis());
    start_session(&l, &machine.home, "quiet", "stty raw -echo; exec cat").await;
    start_session(&l, &machine.home, "busy", "exec yes 'the quick brown fox jumps over the lazy dog, 0123456789, the quick brown fox jumps over'").await;
    let probes = env_num("MUXIN_BENCH_PROBES", 12.) as usize;

    let (on, channel, mut frames) = attach(&l, "quiet", true).await;
    let mut alone = echoes(&on, channel, &mut frames, probes).await;
    eprintln!("alone:                     {}", summary(&mut alone));

    // Own streams first: what's queued on a shared one can't be called back.
    for (label, own) in [("own streams (after)", true), ("one stream (before)", false)] {
        let (q_on, q_channel, mut q_frames) = attach(&l, "quiet", own).await;
        let (b_on, b_channel, mut b_frames) = attach(&l, "busy", own).await;
        let got = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = got.clone();
        let drain = tokio::spawn(async move {
            while let Some((_, b)) = b_frames.recv().await {
                counted.fetch_add(b.len(), std::sync::atomic::Ordering::Relaxed);
            }
        });
        if own {
            q_on.set_priority(link::FOCUSED);
            b_on.set_priority(link::BACKGROUND);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let started = Instant::now();
        let before = got.load(std::sync::atomic::Ordering::Relaxed);
        let mut times = echoes(&q_on, q_channel, &mut q_frames, probes).await;
        let busy_rate = (got.load(std::sync::atomic::Ordering::Relaxed) - before) as f64 / started.elapsed().as_secs_f64() / 1000.;
        eprintln!("busy beside, {label}: {} (busy got {busy_rate:.0} kB/s)", summary(&mut times));
        b_on.close_channel(b_channel);
        drain.abort();
        if own {
            // Let go of, as a hidden terminal that keeps printing is: what
            // the agent still had queued for it goes too.
            drop(b_on);
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut times = echoes(&q_on, q_channel, &mut q_frames, probes).await;
            eprintln!("busy one let go:           {}", summary(&mut times));
        }
        q_on.close_channel(q_channel);
        // Let what's queued for the busy one drain before the next round.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    machine.stop(&l).await;
}

/// Terminals get streams of their own with any agent that connects
/// directly: run with an agent from before lanes (0.13) and a new one. The
/// older one answers `priority` with an error, which changes nothing.
#[test]
#[ignore]
fn lanes_work_with_any_agent() {
    runtime().block_on(async {
        let (machine, l) = TestAgent::start("lanes", None).await;
        start_session(&l, &machine.home, "echo", "stty raw -echo; exec cat").await;
        let mut lanes = Vec::new();
        for (i, priority) in [link::FOCUSED, link::BACKGROUND].into_iter().enumerate() {
            let lane = l.lane().expect("a lane on a direct link");
            assert_eq!((lane.transport(), lane.version()), ("direct", l.version()));
            lane.set_priority(priority);
            let (channel, mut frames) = lane.open_channel();
            // Watch-only for the second: both attached to the one session.
            let r = lane.request(json!({ "op": "attach", "name": "echo", "channel": channel, "cols": 80, "rows": 24, "watch_only": i == 1 }), Duration::from_secs(15)).await.unwrap();
            assert_eq!(r["ok"], true, "{r}");
            while tokio::time::timeout(Duration::from_millis(300), frames.recv()).await.is_ok_and(|f| f.is_some()) {}
            lanes.push((lane, channel, frames));
        }
        // Typed on the first, seen on both.
        lanes[0].0.send(lanes[0].1, DATA, b"lanes!");
        for (_, _, frames) in &mut lanes {
            let mut seen = Vec::new();
            while !String::from_utf8_lossy(&seen).contains("lanes!") {
                match tokio::time::timeout(Duration::from_secs(10), frames.recv()).await {
                    Ok(Some((DATA, b))) => seen.extend(b),
                    Ok(Some(_)) => {}
                    other => panic!("no echo: {:?}", other.map(|f| f.map(|(k, _)| k))),
                }
            }
        }
        // A lane let go ends its link at the agent; the machine's link goes on.
        let (lane, channel, mut frames) = lanes.remove(1);
        lane.close_channel(channel);
        drop(lane);
        assert!(tokio::time::timeout(Duration::from_secs(5), frames.recv()).await.is_ok_and(|f| f.is_none()));
        assert_eq!(l.request(json!({ "op": "ping" }), Duration::from_secs(5)).await.unwrap()["ok"], true);
        assert!(lanes[0].0.alive() && lanes[0].0.is_lane() && !l.is_lane());
        machine.stop(&l).await;
    });
}

/// An app from before lanes (one stream, never a `priority`) is served by
/// a new agent as before: run with the new agent.
#[test]
#[ignore]
fn an_old_app_is_served_as_before() {
    runtime().block_on(async {
        let (machine, l) = TestAgent::start("old-app", None).await;
        start_session(&l, &machine.home, "echo", "stty raw -echo; exec cat").await;
        // What a 0.13 app does: a connection and one stream, spoken to by hand.
        let conn = super::direct::connect("old-app-raw", &machine.paired).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let frame = |channel: u32, kind: u8, payload: &[u8]| {
            let mut f = channel.to_be_bytes().to_vec();
            f.push(kind);
            f.extend((payload.len() as u32).to_be_bytes());
            f.extend(payload);
            f
        };
        async fn next(recv: &mut quinn::RecvStream) -> (u32, u8, Vec<u8>) {
            let mut head = [0u8; 9];
            recv.read_exact(&mut head).await.unwrap();
            let mut payload = vec![0u8; u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize];
            recv.read_exact(&mut payload).await.unwrap();
            (u32::from_be_bytes([head[0], head[1], head[2], head[3]]), head[4], payload)
        }
        send.write_all(&frame(link::CONTROL, link::JSON, br#"{"id":1,"op":"ping"}"#)).await.unwrap();
        let attach = json!({ "id": 2, "op": "attach", "name": "echo", "channel": 1, "cols": 80, "rows": 24 });
        send.write_all(&frame(link::CONTROL, link::JSON, attach.to_string().as_bytes())).await.unwrap();
        let mut replies = Vec::new();
        let mut typed = false;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !String::from_utf8_lossy(&seen).contains("old app") {
            let (channel, kind, payload) = tokio::time::timeout_at(deadline, next(&mut recv)).await.expect("its replies and its echo");
            if channel == link::CONTROL {
                let msg: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                replies.push(msg.clone());
                if msg["id"] == 2 && !typed {
                    assert_eq!(msg["ok"], true, "{msg}");
                    send.write_all(&frame(1, DATA, b"old app")).await.unwrap();
                    typed = true;
                }
            } else if channel == 1 && kind == DATA {
                seen.extend(payload);
            }
        }
        assert_eq!(replies[0]["event"], "hello");
        assert!(replies.iter().any(|r| r["id"] == 1 && r["ok"] == true), "{replies:?}");
        conn.close(0u32.into(), b"bye");
        machine.stop(&l).await;
    });
}
