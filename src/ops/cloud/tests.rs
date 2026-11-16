//! Creating a cloud machine, against a stand-in for DigitalOcean's API: no
//! droplet is made anywhere, and the sign-in (the Keychain) is never read.
//!
//! Each test's Muxin "process" is an engine on a tokio runtime of its own:
//! shutting the runtime down is the process being quit, or crashing, with
//! whatever it was doing cut off there.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

use crate::cloud::digitalocean::stand_in::{self, StandIn};
use crate::cloud::{CloudCreate, pending};
use crate::config::{Inventory, Paths};
use crate::fleet::Engine;
use crate::fleet::model::Operation;

const SIZE: &str = "s-1vcpu-1gb";
const REGION: &str = "nyc3";
const HOURLY: f64 = 0.00893;
/// An address nothing answers at (kept for documentation, RFC 5737).
const ADDRESS: &str = "203.0.113.7";

// ---------------------------------------------------------------------------
// The stand-in

/// What the stand-in does with the next request for a droplet.
#[derive(Clone, Copy, PartialEq)]
enum OnCreate {
    /// Makes it and says so.
    Make,
    /// Makes it, and the answer never arrives (the connection drops).
    MakeAndHangUp,
    /// Makes nothing, and says why (a 422).
    Refuse,
    /// Makes nothing, and answers with an error of its own (a 500): from
    /// here, there's no telling that from one it made.
    Fail,
}

#[derive(Clone)]
struct Made {
    id: u64,
    name: String,
    region: String,
    size: String,
    status: String,
    ip: Option<String>,
    created_at: String,
}

struct Sea {
    droplets: Vec<Made>,
    next_id: u64,
    on_create: OnCreate,
    /// A new droplet starts by itself once it has been asked about this often.
    starts_after: Option<u32>,
    asked_about: u32,
    /// "POST /droplets", "DELETE /droplets/7", ...
    calls: Vec<String>,
}

/// A stand-in for DigitalOcean: its droplets exist only here.
#[derive(Clone)]
struct Ocean {
    api: String,
    sea: Arc<Mutex<Sea>>,
}

impl Ocean {
    fn start() -> Ocean {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let api = format!("http://{}/v2", listener.local_addr().unwrap());
        let sea = Arc::new(Mutex::new(Sea { droplets: Vec::new(), next_id: 7001, on_create: OnCreate::Make, starts_after: None, asked_about: 0, calls: Vec::new() }));
        let ocean = Ocean { api, sea };
        let serving = ocean.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let ocean = serving.clone();
                std::thread::spawn(move || ocean.serve(stream));
            }
        });
        ocean
    }

    fn on_create(&self, what: OnCreate) {
        self.sea.lock().unwrap().on_create = what;
    }

    fn starts_after(&self, asks: u32) {
        self.sea.lock().unwrap().starts_after = Some(asks);
    }

    /// A droplet that's there without Muxin having heard of it.
    fn has(&self, name: &str, created_at: chrono::DateTime<Utc>) -> u64 {
        let mut sea = self.sea.lock().unwrap();
        let id = sea.next_id;
        sea.next_id += 1;
        sea.droplets.push(Made { id, name: name.into(), region: REGION.into(), size: SIZE.into(), status: "new".into(), ip: None, created_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) });
        id
    }

    /// The droplet has booted and has its address.
    fn boots(&self, id: u64) {
        let mut sea = self.sea.lock().unwrap();
        let d = sea.droplets.iter_mut().find(|d| d.id == id).expect("a droplet to boot");
        d.status = "active".into();
        d.ip = Some(ADDRESS.into());
    }

    fn droplets(&self) -> Vec<Made> {
        self.sea.lock().unwrap().droplets.clone()
    }

    fn calls(&self) -> Vec<String> {
        self.sea.lock().unwrap().calls.clone()
    }

    fn serve(&self, mut stream: std::net::TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head_end, length) = loop {
            let Ok(n) = stream.read(&mut chunk) else { return };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                break (i + 4, length);
            }
        };
        while buf.len() < head_end + length {
            let Ok(n) = stream.read(&mut chunk) else { return };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut first = head.lines().next().unwrap_or("").split_whitespace();
        let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
        let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
        let Some((status, answer)) = self.answer(&method, &target, &body) else { return };
        let text = if answer.is_null() { String::new() } else { answer.to_string() };
        let _ = stream.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len()).as_bytes());
    }

    /// The status line and body for a request; None hangs up without a word.
    fn answer(&self, method: &str, target: &str, body: &Value) -> Option<(&'static str, Value)> {
        let (path, query) = target.strip_prefix("/v2").unwrap_or(target).split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target.strip_prefix("/v2").unwrap_or(target).to_string(), String::new()));
        let mut sea = self.sea.lock().unwrap();
        sea.calls.push(format!("{method} {path}{}", query.split('&').find(|q| q.starts_with("name=")).map(|q| format!("?{q}")).unwrap_or_default()));
        let not_found = ("404 Not Found", json!({ "id": "not_found", "message": "The resource you were accessing could not be found." }));
        let droplet = |d: &Made| {
            json!({
                "id": d.id, "name": d.name, "status": d.status, "created_at": d.created_at,
                "region": { "slug": d.region }, "size_slug": d.size, "size": { "price_hourly": HOURLY },
                "networks": { "v4": d.ip.iter().map(|ip| json!({ "ip_address": ip, "type": "public" })).collect::<Vec<_>>() },
            })
        };
        Some(match (method, path.as_str()) {
            ("GET", "/account") => ("200 OK", json!({ "account": { "email": "you@example.com", "droplet_limit": 10, "status": "active" } })),
            ("GET", "/regions") => ("200 OK", json!({ "regions": [{ "slug": REGION, "name": "New York 3", "available": true, "sizes": [SIZE] }] })),
            ("GET", "/sizes") => ("200 OK", json!({ "sizes": [{ "slug": SIZE, "available": true, "vcpus": 1, "memory": 1024, "disk": 25, "price_hourly": HOURLY, "price_monthly": 6.0, "regions": [REGION] }] })),
            ("GET", "/account/keys") => ("200 OK", json!({ "ssh_keys": [] })),
            ("POST", "/account/keys") => ("201 Created", json!({ "ssh_key": { "id": 1, "public_key": body["public_key"] } })),
            ("GET", "/customers/my/balance") => ("200 OK", json!({ "month_to_date_usage": "0.01", "month_to_date_balance": "0.01", "account_balance": "0.00" })),
            ("POST", "/droplets") => {
                if sea.on_create == OnCreate::Refuse {
                    return Some(("422 Unprocessable Entity", json!({ "id": "unprocessable_entity", "message": "You specified an invalid size for Droplet creation." })));
                }
                if sea.on_create == OnCreate::Fail {
                    return Some(("500 Internal Server Error", json!({ "id": "server_error", "message": "Server Error" })));
                }
                let id = sea.next_id;
                sea.next_id += 1;
                let made = Made {
                    id,
                    name: body["name"].as_str().unwrap_or_default().into(),
                    region: body["region"].as_str().unwrap_or_default().into(),
                    size: body["size"].as_str().unwrap_or_default().into(),
                    status: "new".into(),
                    ip: None,
                    created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                };
                sea.droplets.push(made.clone());
                if sea.on_create == OnCreate::MakeAndHangUp {
                    return None;
                }
                ("202 Accepted", json!({ "droplet": droplet(&made) }))
            }
            ("GET", "/droplets") => {
                let name = query.split('&').find_map(|q| q.strip_prefix("name=")).unwrap_or("").to_string();
                ("200 OK", json!({ "droplets": sea.droplets.iter().filter(|d| d.name.eq_ignore_ascii_case(&name)).map(droplet).collect::<Vec<_>>() }))
            }
            ("GET", p) if p.starts_with("/droplets/") => {
                let id: u64 = p["/droplets/".len()..].parse().unwrap_or(0);
                sea.asked_about += 1;
                let boots = sea.starts_after.is_some_and(|n| sea.asked_about >= n);
                match sea.droplets.iter_mut().find(|d| d.id == id) {
                    Some(d) => {
                        if boots && d.status == "new" {
                            d.status = "active".into();
                            d.ip = Some(ADDRESS.into());
                        }
                        ("200 OK", json!({ "droplet": droplet(d) }))
                    }
                    None => not_found,
                }
            }
            ("DELETE", p) if p.starts_with("/droplets/") => {
                let id: u64 = p["/droplets/".len()..].parse().unwrap_or(0);
                let before = sea.droplets.len();
                sea.droplets.retain(|d| d.id != id);
                if sea.droplets.len() == before { not_found } else { ("204 No Content", Value::Null) }
            }
            _ => not_found,
        })
    }
}

// ---------------------------------------------------------------------------
// A Muxin process

/// One Muxin (the app, `muxin cloud create`, the MCP server) on a config folder.
struct Process {
    rt: Option<tokio::runtime::Runtime>,
    engine: Arc<Engine>,
}

impl Process {
    fn start(dir: &Path) -> Process {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let engine = rt.block_on(async { Engine::new(Paths::resolve(Some(dir.to_path_buf()), None), false) });
        ssh_key();
        Process { rt: Some(rt), engine }
    }

    fn run<T>(&self, f: impl std::future::Future<Output = T>) -> T {
        self.rt.as_ref().unwrap().block_on(f)
    }

    /// Quit, Ctrl-C, a crash: whatever it was doing stops where it was.
    fn dies(mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    fn create(&self, name: &str) -> u64 {
        self.engine.cloud_create(CloudCreate { name: name.into(), region: None, size: None, install: Some(Vec::new()) }).unwrap()
    }

    fn op(&self, id: u64) -> Operation {
        self.engine.current().operations.iter().find(|o| o.id == id).cloned().expect("the operation")
    }

    /// Waits until the operation has logged a line with `text` (or ended).
    fn logs(&self, id: u64, text: &str) -> Operation {
        wait(&format!("the operation to log \"{text}\""), || Some(self.op(id)).filter(|o| o.log.iter().any(|l| l.contains(text)) || o.status != "running"))
    }

    fn ends(&self, id: u64) -> Operation {
        wait("the operation to end", || Some(self.op(id)).filter(|o| o.status != "running"))
    }

    fn config(&self) -> String {
        std::fs::read_to_string(&self.engine.paths().config).unwrap_or_default()
    }

    fn machines(&self) -> Vec<crate::config::MachineSpec> {
        Inventory::parse(&self.config()).unwrap().machines
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn wait<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(v) = ready() {
            return v;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "waited 30 seconds for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A config folder of the test's own, with no machines, and the stand-in as its DigitalOcean.
fn folder(name: &str, ocean: &Ocean, start_wait: Option<(Duration, u32)>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("muxin-cloud-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "# A test's machines.\n").unwrap();
    stand_in::set(&dir, StandIn { api: ocean.api.clone(), start_wait });
    dir
}

/// The key new machines get, in the test run's own home (engines put HOME there).
fn ssh_key() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let home = dirs::home_dir().unwrap();
        assert!(home.to_string_lossy().contains("mxt-"), "tests keep to a home of their own, not {}", home.display());
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_ed25519"), "not a key\n").unwrap();
        std::fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMuxinTestsOnly muxin-tests\n").unwrap();
    });
}

fn me() -> String {
    super::local_user()
}

// ---------------------------------------------------------------------------

/// The gap this closes: Muxin ends between DigitalOcean making the droplet
/// and the droplet having an address (up to six minutes). The droplet is
/// billed all along, so the next Muxin has to list it, cost it, and be able
/// to destroy it.
#[test]
fn a_create_cut_off_before_the_droplet_starts_stays_in_muxin() {
    let ocean = Ocean::start();
    // As released: six minutes of waiting, an ask every four seconds.
    let dir = folder("cut-off", &ocean, None);
    let first = Process::start(&dir);
    let op = first.create("Dev Box");
    let o = first.logs(op, "Waiting for it to start");
    assert_eq!(o.status, "running", "{:?}", o.log);
    assert!(o.log.iter().any(|l| l == "✓ Created; it's in your machines, and DigitalOcean bills for it from now"), "{:?}", o.log);

    // It's in config.toml already, with no address, in keys Muxin has always read.
    let droplet = ocean.droplets()[0].clone();
    assert_eq!((droplet.name.as_str(), droplet.status.as_str(), droplet.ip.as_deref()), ("dev-box", "new", None));
    let config = first.config();
    let row = format!(
        "[[machine]]\nname = \"Dev Box\"\nhost = \"\"\nuser = \"{}\"\ncloud = {{ provider = \"digitalocean\", id = {}, region = \"{REGION}\", size = \"{SIZE}\", price_hourly = {HOURLY}, created_at = \"{}\" }}\n",
        me(),
        droplet.id,
        droplet.created_at
    );
    assert!(config.ends_with(&row), "{config}");
    assert!(pending::read(&dir).is_empty(), "written into config.toml: nothing's left to look for");

    // Quit, Ctrl-C on `muxin cloud create`, a crash.
    first.dies();

    // Another Muxin, later: it's one of the machines, not started yet.
    let second = Process::start(&dir);
    let snap = second.engine.current();
    let m = snap.machines.iter().find(|m| m.id == "dev-box").expect("the cut-off machine is listed");
    assert_eq!((m.name.as_str(), m.status.as_str(), m.host.as_str()), ("Dev Box", "manual", ""));
    assert!(m.awaits_address());
    let cloud = m.cloud.as_ref().unwrap();
    assert_eq!((cloud.id, cloud.power.as_deref(), cloud.price_hourly, cloud.price_monthly), (droplet.id, Some("starting"), HOURLY, 6.0));
    assert!(cloud.cost_so_far >= 0.01, "billed from the moment it was made: {}", cloud.cost_so_far);
    assert_eq!(m.diagnostics.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(), ["cloud-starting"]);

    // Costed.
    let usage = second.run(second.engine.cloud_usage());
    assert_eq!(usage.hourly_now, HOURLY);
    assert_eq!(usage.machines.len(), 1);
    let line = &usage.machines[0];
    assert_eq!((line.machine_id.as_deref(), line.name.as_str(), line.power.as_deref(), line.price_hourly), (Some("dev-box"), "Dev Box", Some("starting"), HOURLY));
    assert!(line.this_month >= 0.01 && usage.this_month >= 0.01);

    // Its name is taken, so the same droplet isn't asked for again.
    assert_eq!(second.engine.cloud_create(CloudCreate { name: "dev box".into(), region: None, size: None, install: None }).unwrap_err(), "You already have a machine called \"dev box\".");
    // Nothing to power yet.
    assert_eq!(
        second.engine.cloud_power("dev-box", false).unwrap_err(),
        "Dev Box hasn't started yet, so there's nothing to power off. Muxin adds its address once it has; destroy it if you don't want it."
    );

    // And destroyed from Muxin: the droplet goes, and the charges with it.
    let destroyed = second.ends(second.engine.cloud_destroy("dev-box").unwrap());
    assert_eq!(destroyed.status, "succeeded", "{:?}", destroyed.log);
    assert!(ocean.droplets().is_empty());
    assert!(ocean.calls().contains(&format!("DELETE /droplets/{}", droplet.id)), "{:?}", ocean.calls());
    assert!(second.machines().is_empty(), "{}", second.config());
    assert!(second.engine.current().machines.is_empty());
    // What it cost this month still counts.
    let usage = second.run(second.engine.cloud_usage());
    assert_eq!(usage.hourly_now, 0.);
    assert_eq!(usage.machines.iter().map(|l| (l.name.as_str(), l.power.as_deref())).collect::<Vec<_>>(), [("Dev Box", Some("destroyed"))]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A machine whose create was cut off gets its address from whichever Muxin
/// checks the machines once the droplet has started.
#[test]
fn a_cut_off_machine_gets_its_address_once_it_has_started() {
    let ocean = Ocean::start();
    let dir = folder("address", &ocean, None);
    let first = Process::start(&dir);
    let op = first.create("Dev Box");
    first.logs(op, "Waiting for it to start");
    first.dies();
    let droplet = ocean.droplets()[0].id;

    let second = Process::start(&dir);
    // Not started yet: asked, and nothing changes.
    second.run(second.engine.cloud_catch_up());
    assert!(second.machines()[0].awaits_address());
    assert!(ocean.calls().contains(&format!("GET /droplets/{droplet}")));

    ocean.boots(droplet);
    second.run(second.engine.cloud_catch_up());
    let machines = second.machines();
    let m = &machines[0];
    // As cloud machines have always been written: the account in the host, no `user`.
    assert_eq!((m.host.clone(), m.user.clone(), m.cloud.as_ref().map(|c| c.id)), (format!("{}@{ADDRESS}", me()), None, Some(droplet)));
    assert!(!second.config().contains("user ="), "{}", second.config());
    let snap = second.engine.current();
    let listed = snap.machines.iter().find(|m| m.id == "dev-box").unwrap();
    assert!(!listed.awaits_address() && listed.status != "manual", "{}", listed.status);
    assert!(listed.diagnostics.iter().all(|d| d.code != "cloud-starting"));
    // Nothing more to catch up with: DigitalOcean isn't asked again.
    let before = ocean.calls().len();
    second.run(second.engine.cloud_catch_up());
    assert_eq!(ocean.calls().len(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The usual way: the droplet starts while the create waits, and the create
/// itself fills in the address.
#[test]
fn a_create_fills_in_the_address_when_the_droplet_starts() {
    let ocean = Ocean::start();
    ocean.starts_after(3);
    let dir = folder("starts", &ocean, Some((Duration::from_millis(20), 500)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    let o = muxin.logs(op, "Started at");
    assert!(o.log.iter().any(|l| l == &format!("✓ Started at {ADDRESS}")), "{:?}", o.log);
    let machines = muxin.machines();
    assert_eq!((machines.len(), machines[0].host.clone(), machines[0].user.clone()), (1, format!("{}@{ADDRESS}", me()), None));
    assert_eq!(machines[0].cloud.as_ref().map(|c| (c.id, c.price_hourly)), Some((ocean.droplets()[0].id, HOURLY)));
    // Its first boot goes on for minutes at a real machine; there's none here.
    muxin.dies();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A droplet that takes longer than the create waits isn't left to
/// DigitalOcean's panel anymore: it stays one of the machines.
#[test]
fn a_droplet_that_doesnt_start_in_time_stays_in_muxin() {
    let ocean = Ocean::start();
    let dir = folder("slow", &ocean, Some((Duration::from_millis(10), 3)));
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Slow"));
    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed.error.as_deref(),
        Some("It didn't start within 6 minutes. It stays in your machines as Slow, and DigitalOcean bills for it: Muxin adds its address at a machine check once it has started (what it was to come with isn't installed then). Destroy it if you don't want it.")
    );
    assert_eq!(ocean.droplets().len(), 1, "it's still there, and billed");
    let machines = muxin.machines();
    assert!(machines.len() == 1 && machines[0].awaits_address(), "{}", muxin.config());
    let usage = muxin.run(muxin.engine.cloud_usage());
    assert_eq!((usage.machines.len(), usage.hourly_now), (1, HOURLY));

    // It starts after all: the next machine check gives it its address.
    ocean.boots(ocean.droplets()[0].id);
    muxin.run(muxin.engine.run_check(None));
    wait("its address", || Some(muxin.machines()).filter(|m| !m[0].awaits_address()));
    assert_eq!(muxin.machines()[0].host, format!("{}@{ADDRESS}", me()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// DigitalOcean makes the droplet and its answer never arrives: Muxin asks
/// what's there by name rather than report a failure and leave it billed.
#[test]
fn a_droplet_whose_answer_was_lost_is_found_by_name() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::MakeAndHangUp);
    let dir = folder("lost-answer", &ocean, None);
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    let o = muxin.logs(op, "Waiting for it to start");
    assert_eq!(o.status, "running", "{:?} {:?}", o.log, o.error);
    assert!(o.log.iter().any(|l| l.starts_with("! Couldn't reach DigitalOcean")), "{:?}", o.log);
    assert!(o.log.iter().any(|l| l == "✓ It did"), "{:?}", o.log);
    assert_eq!(ocean.droplets().len(), 1, "asked for once");
    assert!(ocean.calls().contains(&"GET /droplets?name=dev-box".to_string()), "{:?}", ocean.calls());
    let machines = muxin.machines();
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].cloud.as_ref().map(|c| c.id), Some(ocean.droplets()[0].id));
    assert!(pending::read(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Turned down: there's no droplet, so nothing is listed and nothing is looked for.
#[test]
fn a_create_digitalocean_turns_down_leaves_nothing() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::Refuse);
    let dir = folder("refused", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    assert_eq!((failed.status.as_str(), failed.error.as_deref()), ("failed", Some("DigitalOcean: You specified an invalid size for Droplet creation.")));
    assert!(ocean.droplets().is_empty());
    assert!(muxin.machines().is_empty(), "{}", muxin.config());
    assert!(!pending::any(&dir));
    assert!(!ocean.calls().iter().any(|c| c.starts_with("GET /droplets")), "{:?}", ocean.calls());
    // The name is free to try again with.
    ocean.on_create(OnCreate::Make);
    let op = muxin.create("Dev Box");
    assert_eq!(muxin.logs(op, "Waiting for it to start").status, "running");
    assert_eq!(muxin.machines().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An error of DigitalOcean's own doesn't say whether the droplet was made.
/// Muxin looks, finds none, and looks once more later before it lets go:
/// meanwhile the same droplet isn't asked for again.
#[test]
fn a_create_that_failed_at_digitalocean_is_looked_for_before_its_let_go() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::Fail);
    let dir = folder("failed", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    assert_eq!(
        failed.error.as_deref(),
        Some("DigitalOcean: Server Error. DigitalOcean doesn't list the droplet, so it probably wasn't made. Muxin looks once more at the next machine check, and adds it to your machines if it's there after all.")
    );
    assert!(ocean.calls().contains(&"GET /droplets?name=dev-box".to_string()), "{:?}", ocean.calls());
    assert!(muxin.machines().is_empty());
    let asked = pending::read(&dir);
    assert_eq!(asked.iter().map(|a| (a.name.as_str(), a.droplet.as_str())).collect::<Vec<_>>(), [("Dev Box", "dev-box")]);
    ocean.on_create(OnCreate::Make);
    assert!(muxin.ends(muxin.create("Dev Box")).error.unwrap().starts_with("Muxin asked DigitalOcean for \"Dev Box\" a moment ago"));
    assert!(ocean.droplets().is_empty());

    // A couple of minutes on (here: the entry made older), still nothing there: it's let go.
    pending::remove(&dir, &asked[0]);
    pending::add(&dir, &pending::Asked { asked_at: asked[0].asked_at - chrono::Duration::seconds(pending::SETTLE_SECS), ..asked[0].clone() }).unwrap();
    muxin.run(muxin.engine.cloud_catch_up());
    assert!(!pending::any(&dir) && muxin.machines().is_empty());
    // And the name can be created.
    let op = muxin.create("Dev Box");
    assert_eq!(muxin.logs(op, "Waiting for it to start").status, "running");
    assert_eq!((ocean.droplets().len(), muxin.machines().len()), (1, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Muxin ends in the second between asking and DigitalOcean's answer: all
/// that's left is what it wrote down before asking. The next Muxin finds the
/// droplet by name, or finds there's none.
#[test]
fn a_droplet_asked_for_as_muxin_ended_is_found_later() {
    let ocean = Ocean::start();
    let dir = folder("asked", &ocean, None);
    let now = Utc::now();
    let asked = |name: &str, ago: i64| pending::Asked {
        name: name.into(),
        droplet: crate::util::slug(name),
        region: REGION.into(),
        size: SIZE.into(),
        price_hourly: HOURLY,
        user: "dev".into(),
        asked_at: now - chrono::Duration::seconds(ago),
    };
    // Made, and never heard of. An older droplet of the same name isn't it.
    let old = ocean.has("build-box", now - chrono::Duration::days(3));
    let made = ocean.has("build-box", now - chrono::Duration::seconds(118));
    pending::add(&dir, &asked("Build Box", 120)).unwrap();
    // Never made: the request didn't get there.
    pending::add(&dir, &asked("Never", 300)).unwrap();
    // Asked for a moment ago by a Muxin that may still be waiting for the answer.
    let young = asked("Just Now", 5);
    ocean.has("just-now", now - chrono::Duration::seconds(4));
    pending::add(&dir, &young).unwrap();

    let muxin = Process::start(&dir);
    // Not while it isn't known whether "Just Now" was made.
    muxin.run(muxin.engine.cloud_catch_up());
    let machines = muxin.machines();
    assert_eq!(machines.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["Build Box"], "{}", muxin.config());
    let m = &machines[0];
    assert!(m.awaits_address());
    assert_eq!(m.user.as_deref(), Some("dev"));
    let cloud = m.cloud.as_ref().unwrap();
    assert_eq!((cloud.id, cloud.region.as_str(), cloud.size.as_str(), cloud.price_hourly), (made, REGION, SIZE, HOURLY));
    assert_ne!(cloud.id, old);
    // Billed from when DigitalOcean made it.
    assert_eq!(cloud.created_at, ocean.droplets().iter().find(|d| d.id == made).unwrap().created_at);
    assert_eq!(pending::read(&dir), [young.clone()], "found, and found not made, are crossed off; the young one waits");
    assert_eq!(muxin.run(muxin.engine.cloud_usage()).hourly_now, HOURLY);

    // The same droplet isn't asked for again while that's open.
    let refused = muxin.ends(muxin.create("just now"));
    assert_eq!(
        refused.error.as_deref(),
        Some("Muxin asked DigitalOcean for \"just now\" a moment ago and didn't hear back. It's finding out whether the droplet was made: try again in a minute or two.")
    );
    assert_eq!(ocean.droplets().len(), 3, "nothing new was made");

    // Started: its address goes in under the account it was made with.
    ocean.boots(made);
    muxin.run(muxin.engine.cloud_catch_up());
    assert_eq!(muxin.machines()[0].host, format!("dev@{ADDRESS}"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// It can be destroyed while it's being created (by this Muxin or another):
/// the create stops, rather than waiting on an address that isn't its own anymore.
#[test]
fn destroying_it_while_its_created_stops_the_create() {
    let ocean = Ocean::start();
    let dir = folder("destroyed", &ocean, Some((Duration::from_millis(20), 2000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    let destroyed = muxin.ends(muxin.engine.cloud_destroy("dev-box").unwrap());
    assert_eq!(destroyed.status, "succeeded", "{:?}", destroyed.log);
    let failed = muxin.ends(op);
    assert_eq!(failed.error.as_deref(), Some("Dev Box was destroyed (or taken out of config.toml) while it was being created."));
    assert!(ocean.droplets().is_empty() && muxin.machines().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
