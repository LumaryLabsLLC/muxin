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
    /// How far its clock is ahead of this computer's, in seconds: what its
    /// `Date` header says, and when it says it made a droplet.
    clock_ahead: i64,
    /// Its clock moves on this many seconds with every list of droplets it
    /// gives: minutes pass there while a test's looks take milliseconds.
    clock_runs: i64,
    /// What it answers with in place of the list of droplets ("429 Too Many Requests").
    on_list: Option<&'static str>,
    /// And in place of one droplet.
    on_droplet: Option<&'static str>,
    /// How many more times it won't delete a droplet (it's "still making it").
    busy_deletes: u32,
    /// Gives every droplet, whatever name is asked for.
    ignores_name: bool,
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
        let sea = Arc::new(Mutex::new(Sea {
            droplets: Vec::new(),
            next_id: 7001,
            on_create: OnCreate::Make,
            starts_after: None,
            asked_about: 0,
            calls: Vec::new(),
            clock_ahead: 0,
            clock_runs: 0,
            on_list: None,
            on_droplet: None,
            busy_deletes: 0,
            ignores_name: false,
        }));
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

    /// Its clock is this many seconds ahead of this computer's (negative: behind).
    fn clock(&self, ahead: i64) {
        self.sea.lock().unwrap().clock_ahead = ahead;
    }

    fn clock_runs(&self, seconds_a_list: i64) {
        self.sea.lock().unwrap().clock_runs = seconds_a_list;
    }

    /// The time there.
    fn now(&self) -> chrono::DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(self.sea.lock().unwrap().clock_ahead)
    }

    fn on_list(&self, status: Option<&'static str>) {
        self.sea.lock().unwrap().on_list = status;
    }

    fn on_droplet(&self, status: Option<&'static str>) {
        self.sea.lock().unwrap().on_droplet = status;
    }

    fn busy_deletes(&self, times: u32) {
        self.sea.lock().unwrap().busy_deletes = times;
    }

    fn ignores_name(&self) {
        self.sea.lock().unwrap().ignores_name = true;
    }

    /// Deleted there (in its panel, with doctl), without Muxin.
    fn deletes(&self, id: u64) {
        self.sea.lock().unwrap().droplets.retain(|d| d.id != id);
    }

    /// Everything there was made this much longer ago.
    fn older(&self, seconds: i64) {
        for d in self.sea.lock().unwrap().droplets.iter_mut() {
            let made = chrono::DateTime::parse_from_rfc3339(&d.created_at).unwrap().with_timezone(&Utc) - chrono::Duration::seconds(seconds);
            d.created_at = made.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        }
    }

    fn asked(&self, call: &str) -> usize {
        self.calls().iter().filter(|c| c.as_str() == call).count()
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
        // What time it has, as every answer of DigitalOcean's says.
        let date = self.now().format("%a, %d %b %Y %H:%M:%S GMT");
        let _ = stream.write_all(format!("HTTP/1.1 {status}\r\ndate: {date}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len()).as_bytes());
    }

    /// The status line and body for a request; None hangs up without a word.
    fn answer(&self, method: &str, target: &str, body: &Value) -> Option<(&'static str, Value)> {
        let (path, query) = target.strip_prefix("/v2").unwrap_or(target).split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target.strip_prefix("/v2").unwrap_or(target).to_string(), String::new()));
        let mut sea = self.sea.lock().unwrap();
        sea.calls.push(format!("{method} {path}{}", query.split('&').find(|q| q.starts_with("name=")).map(|q| format!("?{q}")).unwrap_or_default()));
        let not_found = ("404 Not Found", json!({ "id": "not_found", "message": "The resource you were accessing could not be found." }));
        let refusal = |status: &'static str| {
            let message = match &status[..3] {
                "401" => "Unable to authenticate you",
                "429" => "API Rate limit exceeded.",
                _ => "Server Error",
            };
            (status, json!({ "id": "error", "message": message }))
        };
        let now_there = Utc::now() + chrono::Duration::seconds(sea.clock_ahead);
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
                    created_at: now_there.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                };
                sea.droplets.push(made.clone());
                if sea.on_create == OnCreate::MakeAndHangUp {
                    return None;
                }
                ("202 Accepted", json!({ "droplet": droplet(&made) }))
            }
            ("GET", "/droplets") => {
                if let Some(status) = sea.on_list {
                    return Some(refusal(status));
                }
                sea.clock_ahead += sea.clock_runs;
                let param = |key: &str| query.split('&').find_map(|q| q.strip_prefix(key)).map(str::to_string);
                let name = param("name=").unwrap_or_default();
                let page = param("page=").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
                let per_page = param("per_page=").and_then(|p| p.parse::<usize>().ok()).unwrap_or(20).max(1);
                let all: Vec<&Made> = sea.droplets.iter().filter(|d| sea.ignores_name || d.name.eq_ignore_ascii_case(&name)).collect();
                let shown: Vec<Value> = all.iter().skip((page - 1) * per_page).take(per_page).map(|d| droplet(d)).collect();
                // As DigitalOcean pages: a link to the next page while there's one.
                let links = if page * per_page < all.len() { json!({ "pages": { "next": format!("{}/droplets?page={}&per_page={per_page}", self.api, page + 1) } }) } else { json!({}) };
                ("200 OK", json!({ "droplets": shown, "links": links, "meta": { "total": all.len() } }))
            }
            ("GET", p) if p.starts_with("/droplets/") => {
                if let Some(status) = sea.on_droplet {
                    return Some(refusal(status));
                }
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
                if sea.busy_deletes > 0 {
                    sea.busy_deletes -= 1;
                    return Some(("422 Unprocessable Entity", json!({ "id": "unprocessable_entity", "message": "Droplet already has a pending event." })));
                }
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
    // The other waits (looking again, asking again) pause for milliseconds.
    stand_in::set(&dir, StandIn { api: ocean.api.clone(), start_wait, pause: Some(Duration::from_millis(10)) });
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

fn asked(name: &str, asked_at: chrono::DateTime<Utc>) -> pending::Asked {
    pending::Asked {
        name: name.into(),
        droplet: crate::util::slug(name),
        region: REGION.into(),
        size: SIZE.into(),
        price_hourly: HOURLY,
        user: "dev".into(),
        asked_at,
        clock_ahead_secs: Some(0),
        problem: None,
    }
}

/// Time passes: what was asked for, and what DigitalOcean made, are this
/// much older (past the time the Muxin that asked looks for it itself).
fn later(dir: &Path, ocean: &Ocean, seconds: i64) {
    for a in pending::read(dir) {
        pending::remove(dir, &a);
        pending::add(dir, &pending::Asked { asked_at: a.asked_at - chrono::Duration::seconds(seconds), ..a }).unwrap();
    }
    ocean.older(seconds);
}

fn attention(muxin: &Process, id: &str) -> Option<crate::fleet::model::Attention> {
    muxin.engine.republish();
    muxin.engine.current().attention.iter().find(|a| a.id == id).cloned()
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
    // The check waits for that: a command (`muxin check`) exits when its check returns.
    ocean.boots(ocean.droplets()[0].id);
    muxin.run(muxin.engine.run_check(None));
    assert_eq!(muxin.machines()[0].host, format!("{}@{ADDRESS}", me()), "{}", muxin.config());
    // And it was checked at its new address (nothing answers there).
    let snap = muxin.engine.current();
    let m = snap.machines.iter().find(|m| m.id == "slow").unwrap();
    assert!(m.probe.is_some() && m.status == "offline", "{} {:?}", m.status, m.probe.is_some());
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
/// Muxin looks until it's sure: for as long as DigitalOcean could still make
/// it (a couple of minutes, by its clock), and then says nothing was made.
/// Meanwhile the same droplet isn't asked for again.
#[test]
fn a_create_that_failed_at_digitalocean_is_looked_for_until_its_sure() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::Fail);
    // A minute passes there with every look.
    ocean.clock_runs(60);
    let dir = folder("failed", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    assert_eq!(failed.error.as_deref(), Some("DigitalOcean: Server Error. DigitalOcean made no droplet for it, so nothing is billed. Try again."));
    assert!(failed.log.iter().any(|l| l == "▸ It isn't listed. DigitalOcean can still make it for a couple of minutes: looking again until then"), "{:?}", failed.log);
    // Not at the first "it isn't there": while it could still be made, that says nothing.
    assert_eq!(ocean.asked("GET /droplets?name=dev-box"), 3, "{:?}", ocean.calls());
    assert!(muxin.machines().is_empty());
    assert!(!pending::any(&dir), "sure now: crossed off");
    // And the name can be created.
    ocean.on_create(OnCreate::Make);
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
    let asked = |name: &str, ago: i64| asked(name, now - chrono::Duration::seconds(ago));
    // Made, and never heard of. An older droplet of the same name isn't it.
    let old = ocean.has("build-box", now - chrono::Duration::days(3));
    let made = ocean.has("build-box", now - chrono::Duration::seconds(298));
    pending::add(&dir, &asked("Build Box", 300)).unwrap();
    // Never made: the request didn't get there.
    pending::add(&dir, &asked("Never", 600)).unwrap();
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
        Some("Muxin has asked DigitalOcean for \"just now\" and hasn't heard back. It's finding out whether the droplet was made: try again in a few minutes.")
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

// ---------------------------------------------------------------------------
// Which droplet is the one that was asked for

/// DigitalOcean says when it made a droplet by its own clock. This
/// computer's being minutes off (here: ten minutes fast, then ten slow)
/// doesn't make Muxin miss the droplet it asked for.
#[test]
fn this_computers_clock_being_wrong_doesnt_lose_the_droplet() {
    for (name, ahead) in [("fast-clock", -600), ("slow-clock", 600)] {
        let ocean = Ocean::start();
        ocean.clock(ahead);
        ocean.on_create(OnCreate::MakeAndHangUp);
        let dir = folder(name, &ocean, None);
        let muxin = Process::start(&dir);
        let op = muxin.create("Dev Box");
        let o = muxin.logs(op, "Waiting for it to start");
        assert_eq!(o.status, "running", "{name}: {:?} {:?}", o.log, o.error);
        assert!(o.log.iter().any(|l| l == "✓ It did"), "{name}: {:?}", o.log);
        let machines = muxin.machines();
        assert_eq!(machines.len(), 1, "{name}");
        let cloud = machines[0].cloud.as_ref().unwrap();
        assert_eq!(cloud.id, ocean.droplets()[0].id);
        // Billed from when DigitalOcean made it, by its clock.
        let made = chrono::DateTime::parse_from_rfc3339(&cloud.created_at).unwrap().with_timezone(&Utc);
        assert!((made - ocean.now()).num_seconds().abs() < 30 && (made - Utc::now()).num_seconds().abs() > 500, "{name}: {}", cloud.created_at);
        assert!(!pending::any(&dir));
        muxin.dies();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A droplet of the same name that DigitalOcean made at another time isn't
/// the one Muxin asked for: it's somebody's (made by hand, with doctl, from
/// another Muxin), and Muxin doesn't take it for its own.
#[test]
fn a_droplet_of_that_name_made_at_another_time_isnt_taken() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::Fail);
    ocean.clock_runs(60);
    // Made by hand four minutes before Muxin asked, where and as big as Muxin asks.
    let by_hand = ocean.has("dev-box", Utc::now() - chrono::Duration::minutes(4));
    let dir = folder("stranger", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    assert_eq!(failed.error.as_deref(), Some("DigitalOcean: Server Error. DigitalOcean made no droplet for it, so nothing is billed. Try again."));
    assert!(muxin.machines().is_empty(), "{}", muxin.config());
    assert!(!pending::any(&dir));

    // An entry that stayed for days (nobody ran Muxin) doesn't match what's made later either.
    ocean.clock_runs(0);
    pending::add(&dir, &asked("Dev Box", Utc::now() - chrono::Duration::days(3))).unwrap();
    let later = ocean.has("dev-box", Utc::now() - chrono::Duration::hours(1));
    muxin.run(muxin.engine.cloud_catch_up());
    assert!(muxin.machines().is_empty(), "{}", muxin.config());
    assert!(!pending::any(&dir), "nothing was made for it, and nothing will be");
    // Theirs, untouched.
    assert_eq!(ocean.droplets().iter().map(|d| d.id).collect::<Vec<_>>(), [by_hand, later]);
    assert!(!ocean.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", ocean.calls());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two droplets that both fit what was asked for can't be told apart, so
/// Muxin doesn't guess: it says so, keeps what it asked for, and shows it
/// until one of the two is out of the way.
#[test]
fn two_droplets_that_fit_are_shown_not_guessed_between() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::MakeAndHangUp);
    // Somebody made one of that name in the same minute.
    let theirs = ocean.has("dev-box", Utc::now());
    let dir = folder("several", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    let ours = ocean.droplets().iter().map(|d| d.id).find(|id| *id != theirs).expect("the droplet Muxin asked for");
    let cant_tell = format!(
        "DigitalOcean has 2 droplets called dev-box that were made at that time ({theirs}, {ours}), so Muxin can't tell which one it asked for. Rename or delete the one that isn't Muxin's in DigitalOcean's panel, and Muxin adds the other to your machines at the next machine check."
    );
    assert!(failed.error.as_deref().is_some_and(|e| e.ends_with(&cant_tell)), "{:?}", failed.error);
    assert!(muxin.machines().is_empty(), "{}", muxin.config());
    assert_eq!(pending::read(&dir).iter().map(|a| a.problem.clone()).collect::<Vec<_>>(), [Some(cant_tell.clone())]);
    // The droplet's name is taken meanwhile, however the machine's is written.
    let again = muxin.ends(muxin.create("dev-box"));
    assert!(again.error.as_deref().is_some_and(|e| e.starts_with("Muxin has asked DigitalOcean for \"dev-box\" and hasn't heard back.")), "{:?}", again.error);
    assert_eq!(ocean.droplets().len(), 2, "nothing new was asked for");
    // While the Muxin that asked may still be looking, it isn't under what needs attention.
    assert!(attention(&muxin, "cloud-asked-dev-box").is_none());

    // Later, and by whichever Muxin checks the machines: still two, still shown.
    later(&dir, &ocean, 300);
    muxin.run(muxin.engine.cloud_catch_up());
    assert!(muxin.machines().is_empty());
    let shown = attention(&muxin, "cloud-asked-dev-box").expect("what was asked for and isn't settled is shown");
    assert_eq!((shown.level.as_str(), shown.title.as_str()), ("warning", "Muxin asked DigitalOcean for Dev Box 5m ago and didn't hear back"));
    assert_eq!(shown.detail, format!("If the droplet was made, it costs ${HOURLY:.3} an hour, and it isn't in your machines yet. {cant_tell}"));

    // The user deletes theirs at DigitalOcean: the other is Muxin's.
    ocean.deletes(theirs);
    muxin.run(muxin.engine.cloud_catch_up());
    let machines = muxin.machines();
    assert_eq!(machines.iter().map(|m| (m.name.as_str(), m.cloud.as_ref().map(|c| c.id))).collect::<Vec<_>>(), [("Dev Box", Some(ours))]);
    assert!(!pending::any(&dir));
    assert!(attention(&muxin, "cloud-asked-dev-box").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// When DigitalOcean can't be asked what it made (it's limiting requests,
/// it has an error of its own), what was asked for stays written down, says
/// why, and is shown, until an ask goes through.
#[test]
fn a_lookup_that_fails_keeps_what_was_asked_for_and_says_why() {
    let ocean = Ocean::start();
    ocean.on_create(OnCreate::MakeAndHangUp);
    ocean.on_list(Some("429 Too Many Requests"));
    let dir = folder("lookup-fails", &ocean, None);
    let muxin = Process::start(&dir);
    let failed = muxin.ends(muxin.create("Dev Box"));
    let error = failed.error.clone().unwrap_or_default();
    let couldnt = "Muxin couldn't find out whether DigitalOcean made the droplet all the same (DigitalOcean is rate-limiting requests; try again in a minute).";
    assert!(error.starts_with("Couldn't reach DigitalOcean: "), "{error}");
    assert!(error.ends_with(&format!("{couldnt} Muxin asks again at every machine check, and adds it to your machines if it's there; until then it's listed under what needs attention.")), "{error}");
    // It was made, and is billed: not a machine yet, and not forgotten.
    assert_eq!(ocean.droplets().len(), 1);
    assert!(muxin.machines().is_empty());
    assert_eq!(pending::read(&dir).iter().map(|a| a.problem.as_deref().map(str::to_string)).collect::<Vec<_>>(), [Some(couldnt.to_string())]);

    // Another error, a few minutes on: one failed ask isn't "there's none".
    later(&dir, &ocean, 300);
    ocean.on_list(Some("500 Internal Server Error"));
    muxin.run(muxin.engine.cloud_catch_up());
    assert_eq!(pending::read(&dir).len(), 1);
    let shown = attention(&muxin, "cloud-asked-dev-box").expect("shown while it isn't settled");
    assert!(shown.detail.ends_with("Muxin couldn't ask DigitalOcean whether it made it: DigitalOcean: Server Error."), "{}", shown.detail);

    // DigitalOcean answers again: there it is.
    ocean.on_list(None);
    muxin.run(muxin.engine.cloud_catch_up());
    assert_eq!(muxin.machines().iter().map(|m| m.cloud.as_ref().map(|c| c.id)).collect::<Vec<_>>(), [Some(ocean.droplets()[0].id)]);
    assert!(!pending::any(&dir) && attention(&muxin, "cloud-asked-dev-box").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The list of droplets comes in pages. A droplet on a later page is still
/// found (here DigitalOcean gives every droplet, whatever name is asked for).
#[test]
fn the_list_of_droplets_is_read_to_its_end() {
    let ocean = Ocean::start();
    ocean.ignores_name();
    for n in 0..450 {
        ocean.has(&format!("other-{n}"), Utc::now());
    }
    let wanted = ocean.has("dev-box", Utc::now());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let client = crate::cloud::digitalocean::Client::for_stand_in(&ocean.api);
    let found = rt.block_on(client.droplets_named("dev-box")).unwrap();
    assert_eq!(found.iter().map(|d| d.id).collect::<Vec<_>>(), [wanted]);
    assert_eq!(ocean.asked("GET /droplets?name=dev-box"), 3, "451 droplets, 200 a page");
    assert_eq!(client.clock_ahead(), Some(0));
}

// ---------------------------------------------------------------------------
// Waiting for the address

/// A blip while waiting for a droplet to start is waited out. Errors that
/// last aren't taken for "not yet": the droplet deleted at DigitalOcean, or a
/// sign-in that ran out, end the wait with what's true.
#[test]
fn errors_while_a_droplet_starts_say_whats_true() {
    // A blip (DigitalOcean limits requests for a moment): the create goes on.
    let ocean = Ocean::start();
    let dir = folder("blip", &ocean, Some((Duration::from_millis(10), 3000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    ocean.on_droplet(Some("429 Too Many Requests"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(muxin.op(op).status, "running");
    ocean.on_droplet(Some("500 Internal Server Error"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(muxin.op(op).status, "running");
    ocean.on_droplet(None);
    ocean.boots(ocean.droplets()[0].id);
    let o = muxin.logs(op, "Started at");
    assert!(o.log.iter().any(|l| l == &format!("✓ Started at {ADDRESS}")), "{:?} {:?}", o.log, o.error);
    muxin.dies();
    let _ = std::fs::remove_dir_all(&dir);

    // Deleted at DigitalOcean while it starts.
    let ocean = Ocean::start();
    let dir = folder("gone", &ocean, Some((Duration::from_millis(10), 3000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    let droplet = ocean.droplets()[0].id;
    ocean.deletes(droplet);
    let failed = muxin.ends(op);
    assert_eq!(
        failed.error,
        Some(format!("DigitalOcean doesn't have Dev Box's droplet ({droplet}) anymore: it was deleted there while it was starting, so it isn't billed. It's still in your machines: destroy it from its page to take it out."))
    );
    let snap = muxin.engine.current();
    let m = snap.machines.iter().find(|m| m.id == "dev-box").expect("still listed");
    assert!(m.droplet_gone() && m.awaits_address());
    assert_eq!(m.diagnostics.iter().map(|d| (d.level.as_str(), d.code.as_str())).collect::<Vec<_>>(), [("warning", "cloud-gone")]);
    let shown = attention(&muxin, "cloud-gone-dev-box").expect("shown");
    assert_eq!(shown.title, "DigitalOcean doesn't have Dev Box's droplet");
    assert!(attention(&muxin, "cloud-starting-dev-box").is_none(), "it isn't starting, and isn't billed");
    // A machine check doesn't ask about it again and again.
    let before = ocean.calls().len();
    muxin.run(muxin.engine.cloud_catch_up());
    assert_eq!(ocean.calls().len(), before, "{:?}", ocean.calls());
    // Another Muxin knows it too (it's kept with the machines' state).
    let other = Process::start(&dir);
    assert!(other.engine.current().machines.iter().any(|m| m.id == "dev-box" && m.droplet_gone()));
    other.dies();
    // Destroying it takes it out of Muxin.
    assert_eq!(muxin.ends(muxin.engine.cloud_destroy("dev-box").unwrap()).status, "succeeded");
    assert!(muxin.machines().is_empty());
    let _ = std::fs::remove_dir_all(&dir);

    // The sign-in runs out while it starts: it exists and is billed, and no waiting brings its address.
    let ocean = Ocean::start();
    let dir = folder("signed-out", &ocean, Some((Duration::from_millis(10), 3000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    ocean.on_droplet(Some("401 Unauthorized"));
    let failed = muxin.ends(op);
    assert_eq!(
        failed.error.as_deref(),
        Some("DigitalOcean didn't accept the sign-in. Connect again in Settings › Cloud. Dev Box was created and stays in your machines, and DigitalOcean bills for it: once you're signed in again, Muxin adds its address at a machine check (what it was to come with isn't installed then).")
    );
    assert!(muxin.machines()[0].awaits_address());
    // What needs attention says why it has no address, at once.
    muxin.run(muxin.engine.cloud_catch_up());
    let shown = attention(&muxin, "cloud-starting-dev-box").expect("shown");
    assert!(shown.detail.contains("Muxin can't ask DigitalOcean whether it has started: DigitalOcean didn't accept the sign-in. Connect again in Settings › Cloud."), "{}", shown.detail);
    let snap = muxin.engine.current();
    let m = snap.machines.iter().find(|m| m.id == "dev-box").unwrap();
    assert_eq!(m.diagnostics.iter().map(|d| (d.level.as_str(), d.code.as_str())).collect::<Vec<_>>(), [("warning", "cloud-starting")]);
    // Signed in again, and started: the address comes, and the trouble goes.
    ocean.on_droplet(None);
    ocean.boots(ocean.droplets()[0].id);
    muxin.run(muxin.engine.cloud_catch_up());
    assert_eq!(muxin.machines()[0].host, format!("{}@{ADDRESS}", me()));
    assert!(attention(&muxin, "cloud-starting-dev-box").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Checking a machine that hasn't started by its name asks DigitalOcean
/// about it, and says where it stands, rather than "it has no host".
#[test]
fn checking_a_starting_machine_by_name_asks_digitalocean() {
    use crate::api::{self, CheckRequest};
    let ocean = Ocean::start();
    let dir = folder("check-by-name", &ocean, None);
    let first = Process::start(&dir);
    let op = first.create("Dev Box");
    first.logs(op, "Waiting for it to start");
    first.dies();
    let droplet = ocean.droplets()[0].id;

    let muxin = Process::start(&dir);
    let check = || muxin.run(api::check(&muxin.engine, CheckRequest { machine_id: Some("Dev Box".into()) }));
    let before = ocean.asked(&format!("GET /droplets/{droplet}"));
    let stands = check().expect("where it stands");
    assert_eq!(ocean.asked(&format!("GET /droplets/{droplet}")), before + 1);
    assert_eq!(stands.len(), 1);
    assert_eq!((stands[0].id.as_str(), stands[0].status.as_str(), stands[0].host.as_str()), ("dev-box", "manual", ""));
    assert_eq!(stands[0].cloud.as_ref().and_then(|c| c.power.as_deref()), Some("starting"));

    // DigitalOcean can't be asked: that's said, in its words.
    ocean.on_droplet(Some("401 Unauthorized"));
    let e = check().unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("operation_failed", "Dev Box has no address yet, and Muxin couldn't ask DigitalOcean whether it has started: DigitalOcean didn't accept the sign-in. Connect again in Settings › Cloud."));

    // Started: it gets its address, and is checked at it (nothing answers there).
    ocean.on_droplet(None);
    ocean.boots(droplet);
    let checked = check().expect("checked");
    assert_eq!((checked[0].host.clone(), checked[0].status.as_str()), (format!("{}@{ADDRESS}", me()), "offline"));
    assert!(checked[0].checked_at.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Destroying, and taking out of Muxin

/// DigitalOcean won't delete a droplet it's still making. Muxin asks again
/// until it will; if it never will, the machine stays listed and says so.
#[test]
fn a_droplet_digitalocean_is_still_making_is_deleted_once_it_can_be() {
    let ocean = Ocean::start();
    let dir = folder("busy-delete", &ocean, Some((Duration::from_millis(20), 3000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    let droplet = ocean.droplets()[0].id;

    // It never will (as far as the two minutes of asking go): nothing is taken out of Muxin.
    ocean.busy_deletes(1000);
    let refused = muxin.ends(muxin.engine.cloud_destroy("dev-box").unwrap());
    assert_eq!(
        refused.error.as_deref(),
        Some("DigitalOcean can't delete Dev Box yet, because it's still working on it (DigitalOcean: Droplet already has a pending event). It's still in your machines, and billed: destroy it again in a few minutes.")
    );
    assert_eq!(ocean.asked(&format!("DELETE /droplets/{droplet}")), super::DELETE_WAIT.1 as usize);
    assert_eq!((ocean.droplets().len(), muxin.machines().len()), (1, 1));
    assert_eq!(muxin.op(op).status, "running", "the create goes on: its droplet is still there");

    // It will after a few asks.
    ocean.busy_deletes(3);
    let destroyed = muxin.ends(muxin.engine.cloud_destroy("dev-box").unwrap());
    assert_eq!(destroyed.status, "succeeded", "{:?} {:?}", destroyed.log, destroyed.error);
    assert!(destroyed.log.iter().any(|l| l == "▸ DigitalOcean is still working on it and can't delete it yet; asking again for up to two minutes"), "{:?}", destroyed.log);
    assert!(ocean.droplets().is_empty() && muxin.machines().is_empty());
    assert_eq!(muxin.ends(op).error.as_deref(), Some("Dev Box was destroyed (or taken out of config.toml) while it was being created."));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A create that's setting its machine up sees another Muxin destroy it (the
/// app, while `muxin cloud create` waits), and stops: the address it was
/// trying may be somebody else's machine by then.
#[test]
fn another_muxins_destroy_stops_the_setup() {
    let ocean = Ocean::start();
    ocean.starts_after(1);
    let dir = folder("setup-destroyed", &ocean, Some((Duration::from_millis(10), 500)));
    let command = Process::start(&dir);
    let op = command.create("Dev Box");
    let o = command.logs(op, "Setting it up");
    assert_eq!(o.status, "running", "{:?} {:?}", o.log, o.error);

    let app = Process::start(&dir);
    let destroyed = app.ends(app.engine.cloud_destroy("dev-box").unwrap());
    assert_eq!(destroyed.status, "succeeded", "{:?}", destroyed.log);

    // Within one try of its address (ssh gives up on it after a few seconds).
    let stopped = command.ends(op);
    assert_eq!(stopped.error.as_deref(), Some("Dev Box was destroyed (or taken out of config.toml) while it was being set up."));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Taking a cloud machine out of Muxin without destroying it would leave a
/// billed droplet that nothing here lists, so a command or an agent is told
/// to destroy it instead (as the app only offers).
#[test]
fn a_cloud_machine_isnt_taken_out_of_muxin_without_its_droplet() {
    use crate::api::{self, MachineRequest, Surface};
    let ocean = Ocean::start();
    let dir = folder("remove", &ocean, None);
    let muxin = Process::start(&dir);
    let op = muxin.create("Dev Box");
    muxin.logs(op, "Waiting for it to start");
    let remove = |surface| muxin.run(api::remove_machine(&muxin.engine, MachineRequest { machine_id: "dev-box".into() }, surface)).unwrap_err();
    let e = remove(Surface::Mcp);
    assert_eq!((e.code, e.message.as_str()), ("invalid", "Dev Box is a cloud machine Muxin created: taken out of Muxin, its droplet would stay at DigitalOcean, billed and listed nowhere here."));
    assert_eq!(e.hint.as_deref(), Some("Destroy it with cloud_destroy(machine_id: \"dev-box\"), which deletes the droplet too. That also takes out one whose droplet is already gone."));
    assert_eq!(remove(Surface::Cli).hint.as_deref(), Some("muxin cloud destroy dev-box"));
    assert_eq!((muxin.machines().len(), ocean.droplets().len()), (1, 1));
    muxin.dies();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The idle watch

/// How long a cloud machine has been idle is kept for its droplet. A machine
/// destroyed and made again under the same name is another droplet: it
/// doesn't inherit hours of idleness (and with them, a power-off or a
/// destroy minutes after it comes up).
#[test]
fn a_machine_made_again_under_a_name_starts_its_idle_clock_anew() {
    let now = Utc::now();
    let long_ago = now - chrono::Duration::hours(5);
    let mut idle = super::IdleState::default();
    assert_eq!(*idle.clock("dev", 7001, long_ago), long_ago);
    assert_eq!(*idle.clock("dev", 7001, now), long_ago, "the same droplet keeps its clock");
    idle.acted.insert("dev".into());
    // Destroyed elsewhere and made again before the watch looked: another droplet under the id.
    assert_eq!(*idle.clock("dev", 7002, now), now);
    assert!(!idle.acted.contains("dev"), "and it hasn't been acted on");
    // A machine that's gone takes what's kept for it along.
    idle.clock("old", 1, long_ago);
    idle.acted.insert("old".into());
    idle.keep_only(&["dev"].into_iter().collect());
    assert_eq!(idle.last_active.keys().collect::<Vec<_>>(), ["dev"]);
    assert!(idle.droplets.len() == 1 && idle.acted.is_empty());

    // Destroying a machine forgets its clock at once.
    let ocean = Ocean::start();
    let dir = folder("idle", &ocean, Some((Duration::from_millis(20), 3000)));
    let muxin = Process::start(&dir);
    let op = muxin.create("Idle Box");
    muxin.logs(op, "Waiting for it to start");
    let droplet = ocean.droplets()[0].id;
    super::IDLE.lock().unwrap().get_or_insert_with(Default::default).clock("idle-box", droplet, long_ago);
    assert_eq!(super::idle_since("idle-box"), Some(long_ago));
    assert_eq!(muxin.ends(muxin.engine.cloud_destroy("idle-box").unwrap()).status, "succeeded");
    assert_eq!(super::idle_since("idle-box"), None);
    // And one that's made under a name whose old clock is still around doesn't get it.
    super::IDLE.lock().unwrap().get_or_insert_with(Default::default).clock("idle-box", droplet, long_ago);
    let op = muxin.create("Idle Box");
    muxin.logs(op, "Waiting for it to start");
    assert_eq!(super::idle_since("idle-box"), None);
    muxin.dies();
    let _ = std::fs::remove_dir_all(&dir);
}
