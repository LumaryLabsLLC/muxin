//! A machine's servers, on this computer: what's listening there (the agent's
//! ports.rs), and forwarding a port to localhost here. A forward is a
//! listener on 127.0.0.1; each connection to it is a channel on the
//! machine's link, and the agent there connects to the port (link.rs `tcp`),
//! so it works over ssh or direct alike, with no extra ssh connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::Engine;
use super::link;

/// A port something listens on, on a machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ListeningPort {
    pub port: u16,
    /// Where the agent connects for it (127.0.0.1 for one listening everywhere).
    #[serde(default)]
    pub host: String,
    /// As it listens: 0.0.0.0, 127.0.0.1, ::, a LAN address.
    #[serde(default)]
    pub address: String,
    /// The program (when this user may see it).
    #[serde(default)]
    pub process: String,
    #[serde(default)]
    pub pid: Option<u32>,
    /// The Docker container it's published from, and its image.
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// It answered `GET /` like a web server: its status and page title.
    #[serde(default)]
    pub web: bool,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub title: Option<String>,
}

impl ListeningPort {
    /// A site to open in a browser: it answers with a page (not an API's 404).
    pub fn is_site(&self) -> bool {
        self.web && (self.title.is_some() || self.status.is_some_and(|s| (200..400).contains(&s)))
    }

    /// Only reachable on the machine itself.
    pub fn local_only(&self) -> bool {
        matches!(self.address.as_str(), "127.0.0.1" | "::1") || self.address.starts_with("127.")
    }

    /// What it is, in a few words: its page's title, else the program or container.
    pub fn name(&self) -> String {
        self.title.clone().or_else(|| self.container.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| if self.process.is_empty() { "Unknown program".into() } else { self.process.clone() })
    }
}

/// A port of a machine's, open on this computer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forward {
    pub machine_id: String,
    /// The port there, and where the agent connects for it.
    pub port: u16,
    pub host: String,
    /// localhost:`local_port` here.
    pub local_port: u16,
}

impl Forward {
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.local_port)
    }
}

/// Forwards running in this process, by machine and port, with their listener's task.
type Running = Mutex<HashMap<(String, u16), (Forward, tokio::task::AbortHandle)>>;

fn forwards() -> &'static Running {
    static FORWARDS: OnceLock<Running> = OnceLock::new();
    FORWARDS.get_or_init(Default::default)
}

impl Engine {
    /// What's listening on a machine, web servers first.
    pub async fn ports(self: &Arc<Self>, machine_id: &str) -> Result<Vec<ListeningPort>, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "ports" })).await?;
        serde_json::from_value(reply["ports"].clone()).map_err(|_| format!("{} listed its ports in a way Muxen can't read.", spec.name))
    }

    /// Just the ports something listens on there (a quick look, no asking).
    pub async fn listening(self: &Arc<Self>, machine_id: &str) -> Result<Vec<u16>, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo().iter().map(|p| p.port).collect());
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "listening" })).await?;
        serde_json::from_value(reply["ports"].clone()).map_err(|_| format!("{} listed its ports in a way Muxen can't read.", spec.name))
    }

    /// The ports forwarded to this computer.
    pub fn forwards(&self) -> Vec<Forward> {
        let mut out: Vec<Forward> = forwards().lock().unwrap().values().map(|(f, _)| f.clone()).collect();
        out.sort_by(|a, b| (&a.machine_id, a.port).cmp(&(&b.machine_id, b.port)));
        out
    }

    /// Opens a machine's port on this computer, at localhost:`local` (the
    /// same number when free, else the next free one). Already open: as it is.
    pub async fn forward(self: &Arc<Self>, machine_id: &str, port: u16, host: Option<&str>, local: Option<u16>) -> Result<Forward, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if let Some((f, _)) = forwards().lock().unwrap().get(&(spec.id.clone(), port)) {
            return Ok(f.clone());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't open ports.".into());
        }
        if spec.is_local() {
            return Err(format!("{} is this computer: it's at localhost:{port} already.", spec.name));
        }
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        // The same port here when it's free; else the next ones up.
        let start = local.unwrap_or(port);
        let mut listener = None;
        for p in (0..40u16).filter_map(|i| start.checked_add(i)) {
            if let Ok(l) = TcpListener::bind(("127.0.0.1", p)).await {
                listener = Some((l, p));
                break;
            }
        }
        let (listener, local_port) = listener.ok_or_else(|| format!("No free port here from {start} up."))?;
        let forward = Forward { machine_id: spec.id.clone(), port, host: host.filter(|h| !h.is_empty()).unwrap_or("127.0.0.1").to_string(), local_port };
        let me = self.clone();
        let f = forward.clone();
        let task = self.runtime().spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else { break };
                let (me, f) = (me.clone(), f.clone());
                tokio::spawn(async move {
                    if let Err(e) = pipe(&me, &f, socket).await {
                        tracing::debug!("forward {}:{}: {e}", f.machine_id, f.port);
                    }
                });
            }
        });
        forwards().lock().unwrap().insert((spec.id.clone(), port), (forward.clone(), task.abort_handle()));
        Ok(forward)
    }

    /// Stops forwarding a port (connections already open end too).
    pub fn stop_forward(&self, machine_id: &str, port: u16) -> bool {
        match forwards().lock().unwrap().remove(&(machine_id.to_string(), port)) {
            Some((_, task)) => {
                task.abort();
                true
            }
            None => false,
        }
    }
}

/// One connection to a forwarded port: a channel on the machine's link, the
/// agent joined to the port there, bytes both ways until either side ends.
async fn pipe(engine: &Arc<Engine>, f: &Forward, socket: TcpStream) -> Result<(), String> {
    let (spec, probe) = engine.machine(&f.machine_id).ok_or("The machine is gone.")?;
    super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
    let link = link::get(&spec).await.ok_or_else(|| format!("Can't reach the agent on {}.", spec.name))?;
    let (channel, mut frames) = link.open_channel();
    let request = json!({ "op": "tcp", "channel": channel, "port": f.port, "host": f.host });
    match link.request(request, Duration::from_secs(10)).await {
        Ok(r) if r["ok"].as_bool() == Some(true) => {}
        Ok(r) => {
            link.close_channel(channel);
            return Err(r["error"].as_str().unwrap_or("The machine couldn't connect to it.").to_string());
        }
        Err(e) => {
            link.close_channel(channel);
            return Err(e);
        }
    }
    let _ = socket.set_nodelay(true);
    let (mut rd, mut wr) = socket.into_split();
    let up = {
        let link = link.clone();
        async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => link.send(channel, link::DATA, &buf[..n]),
                }
            }
        }
    };
    let down = async move {
        while let Some((kind, bytes)) = frames.recv().await {
            match kind {
                link::DATA => {
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                link::CLOSE => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    link.close_channel(channel);
    Ok(())
}

fn demo() -> Vec<ListeningPort> {
    let p = |port: u16, process: &str, title: Option<&str>, web: bool| ListeningPort {
        port,
        host: "127.0.0.1".into(),
        address: "0.0.0.0".into(),
        process: process.into(),
        web,
        status: web.then_some(200),
        title: title.map(str::to_string),
        ..Default::default()
    };
    vec![p(3000, "node", Some("Beacon dashboard"), true), p(5173, "node", Some("Vite + React"), true), p(8080, "nginx", Some("Welcome to nginx!"), true), p(5432, "postgres", None, false)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_sites_from_other_ports() {
        let site = ListeningPort { port: 5173, web: true, status: Some(200), title: Some("Vite App".into()), address: "::1".into(), ..Default::default() };
        assert!(site.is_site() && site.local_only());
        assert_eq!(site.name(), "Vite App");
        let api = ListeningPort { port: 6463, web: true, status: Some(404), process: "Discord Helper".into(), address: "0.0.0.0".into(), ..Default::default() };
        assert!(!api.is_site() && !api.local_only());
        assert_eq!(api.name(), "Discord Helper");
        let db = ListeningPort { port: 5432, container: Some("shop-db-1".into()), ..Default::default() };
        assert_eq!(db.name(), "shop-db-1");
        let f = Forward { machine_id: "beelink".into(), port: 8080, host: "127.0.0.1".into(), local_port: 8081 };
        assert_eq!(f.url(), "http://localhost:8081");
    }
}
