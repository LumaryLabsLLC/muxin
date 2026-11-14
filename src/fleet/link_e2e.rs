//! Direct links against a real agent (in a home of its own, on a port of its
//! own): terminals on streams of their own (lanes) with old and new agents,
//! an old app with a new agent, and a machine that sleeps and wakes (the
//! agent told so by a file, not by the computer). And the focused terminal on a slow
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

/// The machines whose agents said they woke, for every test here (the app
/// takes wake notices one way, set once a process).
fn woke() -> tokio::sync::broadcast::Receiver<String> {
    static HEARD: std::sync::OnceLock<tokio::sync::broadcast::Sender<String>> = std::sync::OnceLock::new();
    HEARD
        .get_or_init(|| {
            let (tx, _) = tokio::sync::broadcast::channel(16);
            let heard = tx.clone();
            direct::take_notices(move |id| {
                let _ = heard.send(id.to_string());
            });
            tx
        })
        .subscribe()
}

/// A plain way to `agent` that counts what the app sends it: returns the
/// port to reach it at, and the count.
async fn counted_link(agent: SocketAddr) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    back.connect(agent).await.unwrap();
    let port = front.local_addr().unwrap().port();
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app: Arc<Mutex<Option<SocketAddr>>> = Default::default();
    let (f, b, a, n) = (front.clone(), back.clone(), app.clone(), sent.clone());
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((len, from)) = f.recv_from(&mut buf).await {
            *a.lock().unwrap() = Some(from);
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = b.send(&buf[..len]).await;
        }
    });
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok(len) = back.recv(&mut buf).await {
            let to = *app.lock().unwrap();
            if let Some(to) = to {
                let _ = front.send_to(&buf[..len], to).await;
            }
        }
    });
    (port, sent)
}

/// A test agent listening in a home of its own.
struct TestAgent {
    home: std::path::PathBuf,
    id: String,
    paired: direct::Paired,
    listener: std::process::Child,
}

/// A test that fails on the way leaves no agent listening behind it.
impl Drop for TestAgent {
    fn drop(&mut self) {
        let _ = self.listener.kill();
        let _ = self.listener.wait();
    }
}

impl TestAgent {
    /// The port the agent for `tag` listens on.
    fn port(tag: &str) -> u16 {
        43000 + (std::process::id() % 1000) as u16 * 4 + tag.len() as u16 % 4
    }

    /// Starts `MUXIN_TEST_AGENT` and links to it directly, through a slow
    /// link (bytes a second, delay each way, queue) when asked.
    async fn start(tag: &str, slow: Option<(f64, Duration, Duration)>) -> (TestAgent, Arc<Link>) {
        Self::start_with(tag, slow, &[]).await
    }

    /// The same, with more in the agent's environment.
    async fn start_with(tag: &str, slow: Option<(f64, Duration, Duration)>, env: &[(&str, &std::path::Path)]) -> (TestAgent, Arc<Link>) {
        let agent = std::env::var("MUXIN_TEST_AGENT").expect("set MUXIN_TEST_AGENT");
        let home = std::env::temp_dir().join(format!("muxin-e2e-{tag}-{}", std::process::id()));
        let dir = home.join(".muxen/direct");
        std::fs::create_dir_all(&dir).unwrap();
        // This app's key, one for the whole run (`direct::init` takes one folder).
        let app_dir = std::env::temp_dir().join(format!("muxin-e2e-app-{}", std::process::id()));
        std::fs::create_dir_all(&app_dir).unwrap();
        direct::init(app_dir);
        let app = direct::app_fingerprint().unwrap();
        let port = Self::port(tag);
        std::fs::write(dir.join("port"), port.to_string()).unwrap();
        std::fs::write(dir.join("trusted"), format!("{app}\n")).unwrap();
        // In its home, so what it starts can be found and stopped after.
        std::fs::create_dir_all(home.join(".muxen/bin")).unwrap();
        let agent_copy = home.join(".muxen/bin/muxen-agent");
        std::fs::copy(&agent, &agent_copy).unwrap();
        let listener = std::process::Command::new(&agent_copy).arg("listen").env("HOME", &home).envs(env.iter().copied()).stderr(std::process::Stdio::null()).spawn().unwrap();
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

/// A machine that sleeps says so before it does, is left alone while it
/// sleeps (reached all the same, it says it's asleep), and tells the app
/// when it has woken, with no link left to say it on. The agent is told it
/// sleeps and wakes by a file (`MUXEN_TEST_POWER`), not by this computer.
///
/// Run with a new agent. With one from before it could say any of this
/// (`MUXIN_TEST_AGENT` set to it), the link goes on as it always did.
#[test]
#[ignore]
fn a_machine_that_sleeps_says_so_and_says_when_it_wakes() {
    use super::asleep;
    runtime().block_on(async {
        let power = std::env::temp_dir().join(format!("muxin-e2e-power-{}", std::process::id()));
        std::fs::write(&power, "awake").unwrap();
        // This app takes wake notices, as the window does.
        let mut woke = woke();
        let (machine, l) = TestAgent::start_with("sleeper", None, &[("MUXEN_TEST_POWER", &power)]).await;
        let id = machine.id.clone();
        let spec: MachineSpec = toml::from_str(&format!("name = \"{id}\"\nid = \"{id}\"\nhost = \"{id}.invalid\"")).unwrap();
        let mut events = l.subscribe();
        async fn event(events: &mut tokio::sync::broadcast::Receiver<serde_json::Value>, name: &str, within: Duration) -> Option<serde_json::Value> {
            tokio::time::timeout(within, async {
                loop {
                    match events.recv().await {
                        Ok(e) if e["event"] == name => return Some(e),
                        Ok(_) => {}
                        Err(_) => return None,
                    }
                }
            })
            .await
            .ok()
            .flatten()
        }

        if !l.says_sleep() {
            // An agent from before: it says nothing, and the link stays up.
            eprintln!("agent {}: from before it could say its machine sleeps", l.version());
            assert_eq!(l.asleep(), None);
            std::fs::write(&power, "asleep").unwrap();
            assert!(event(&mut events, "sleep", Duration::from_secs(2)).await.is_none());
            let pong = l.request(json!({ "op": "ping", "wake_notice": 53312 }), Duration::from_secs(5)).await.unwrap();
            assert_eq!((pong["ok"].as_bool(), pong.get("asleep"), pong.get("wake_notice")), (Some(true), None, None));
            assert!(l.alive());
            let _ = std::fs::remove_file(&power);
            machine.stop(&l).await;
            return;
        }
        eprintln!("agent {}: says when its machine sleeps", l.version());
        // It knows where this app's notice goes (the ping sent as the link came up).
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Going to sleep: said on the link, with where the wake notice will go.
        std::fs::write(&power, "asleep").unwrap();
        let said = event(&mut events, "sleep", Duration::from_secs(5)).await.expect("it says it's going to sleep");
        let notice = said["wake_notice"].as_str().expect("where its wake notice goes").to_string();
        assert!(direct::notice_arrives(&notice), "{notice}");
        assert!(!direct::notice_arrives("192.0.2.7:1"), "an address this computer doesn't have");
        // As the engine does: let go of it, and leave it alone.
        const OWNER: usize = 9001;
        asleep::sync(OWNER, [&id].into_iter());
        link::reset(&id);
        assert!(link::current_or_direct(&spec).await.is_none() && link::get(&spec).await.is_none());
        assert_eq!(spec.refuse_if_disconnected().unwrap_err(), format!("{id} is asleep. Wake it first."));

        // Looked at on purpose (a check of it by name): reached in its
        // sleep, it says it's asleep, and no link is kept.
        {
            let _looking = asleep::look(&id);
            let since = std::time::Instant::now();
            assert!(link::current_or_direct(&spec).await.is_none());
            let told = asleep::told_since(&id, since).expect("its agent said it's asleep");
            assert_eq!(told.notice.as_deref(), Some(notice.as_str()));
            assert!(link::current(&id).is_none());
            // Asked to wake, it says where it stands (the file doesn't wake).
            let reached = link::reach(&spec).await.expect("reached to wake it");
            assert!(reached.asleep().is_some());
            let r = reached.request(json!({ "op": "wake_self" }), Duration::from_secs(10)).await.unwrap();
            assert_eq!((r["ok"].as_bool(), r["awake"].as_bool()), (Some(true), Some(false)), "{r}");
            reached.hang_up();
        }
        assert!(tokio::time::timeout(Duration::from_secs(2), woke.recv()).await.is_err(), "nothing is said while it sleeps");

        // It wakes: this app hears, with nothing linked to it.
        std::fs::write(&power, "awake").unwrap();
        let who = tokio::time::timeout(Duration::from_secs(15), woke.recv()).await.expect("its wake notice").unwrap();
        assert_eq!(who, id);
        asleep::sync(OWNER, std::iter::empty());
        direct::retry_now(&id);
        let again = link::current_or_direct(&spec).await.expect("linked again");
        assert!(again.asleep().is_none() && again.says_sleep());
        // Told once: nothing more comes.
        assert!(tokio::time::timeout(Duration::from_secs(3), woke.recv()).await.is_err());
        let _ = std::fs::remove_file(&power);
        machine.stop(&again).await;
    });
}

/// The whole of it, with the engine: a machine that goes to sleep shows as
/// asleep within a moment, nothing is sent to it while it sleeps (counted on
/// the way: no keep-alive, no check, no try at linking again, for longer
/// than any of them waits), and it's online again when it says it woke.
/// Run with a new agent.
#[test]
#[ignore]
fn nothing_is_sent_to_a_machine_while_it_sleeps() {
    use super::Engine;
    use std::sync::atomic::Ordering;
    runtime().block_on(async {
        let power = std::env::temp_dir().join(format!("muxin-e2e-power2-{}", std::process::id()));
        std::fs::write(&power, "awake").unwrap();
        let mut woke = woke();
        let id = "quiet";
        // The engine reaches the machine one way only, which counts what's sent to it.
        let (port, sent) = counted_link(SocketAddr::from(([127, 0, 0, 1], TestAgent::port(id)))).await;
        // The engine first: it follows every new link.
        let dir = std::env::temp_dir().join(format!("muxin-e2e-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!("[[machine]]\nname = \"{id}\"\nhost = \"{id}.invalid\"\ndirect_address = \"127.0.0.1:{port}\"\n\n[settings]\ncheck_interval_minutes = 1\n"),
        )
        .unwrap();
        let engine = Engine::new(crate::config::Paths::resolve(Some(dir.clone()), None), false);
        let (machine, l) = TestAgent::start_with(id, None, &[("MUXEN_TEST_POWER", &power)]).await;
        if !l.says_sleep() {
            eprintln!("agent {}: from before it could say its machine sleeps; nothing to see here", l.version());
            machine.stop(&l).await;
            return;
        }
        // The engine learns the machine's own addresses from its first link
        // and links afresh once (the test paired it by 127.0.0.1): let that
        // pass. Then the test's link, which went straight there, goes too.
        for _ in 0..30 {
            if direct::paired(id).is_some_and(|p| p.lan != machine.paired.lan) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        link::reset(id);
        let status = |engine: &Engine| engine.current().machines.iter().find(|m| m.id == id).map(|m| m.status.clone()).unwrap_or_default();
        let until = |engine: Arc<Engine>, want: &'static str, within: Duration| async move {
            let deadline = Instant::now() + within;
            while status(&engine) != want && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            status(&engine)
        };
        engine.run_check(Some(vec![id.to_string()])).await;
        assert_eq!(status(&engine), "online", "{:?}", engine.machine(id).and_then(|(_, p)| p).and_then(|p| p.error));
        assert!(link::current(id).is_some_and(|l| l.transport() == "direct"));
        // The link's keep-alives are counted, as they'd wake a sleeping Mac.
        let before = sent.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(sent.load(Ordering::Relaxed) > before, "an idle link still sends");

        // It goes to sleep, and says so.
        std::fs::write(&power, "asleep").unwrap();
        assert_eq!(until(engine.clone(), "asleep", Duration::from_secs(5)).await, "asleep");
        let s = engine.current();
        let m = s.machines.iter().find(|m| m.id == id).unwrap();
        assert_eq!(m.unreachable.as_ref().map(|u| (u.kind.as_str(), u.detail.as_str())), Some(("asleep", "Muxin lets it sleep, and reconnects when it wakes.")));
        assert!(link::current(id).is_none());
        // The goodbye goes while it still hears; then nothing. What would
        // have reached for it: the link's keep-alive (4 s), a look at its
        // sessions (8 s), a check after the link was lost, the look for a
        // machine that's offline (30 s), a check of every machine, a request.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let quiet = sent.load(Ordering::Relaxed);
        engine.run_check(None).await;
        assert!(engine.refresh_sessions(id).await.is_err());
        assert!(engine.connection(id).await.is_ok_and(|c| c.via == "none"));
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(sent.load(Ordering::Relaxed), quiet, "something was sent to a machine that's asleep");
        assert_eq!(status(&engine), "asleep");

        // It wakes and says so: the app (as the window does) looks at it.
        std::fs::write(&power, "awake").unwrap();
        let who = tokio::time::timeout(Duration::from_secs(15), woke.recv()).await.expect("its wake notice").unwrap();
        assert_eq!(who, id);
        engine.machine_woke(id).await;
        assert_eq!(status(&engine), "online");
        let again = link::current(id).expect("linked again");
        assert!(sent.load(Ordering::Relaxed) > quiet);
        let _ = std::fs::remove_file(&power);
        machine.stop(&again).await;
        let _ = std::fs::remove_dir_all(&dir);
    });
}
