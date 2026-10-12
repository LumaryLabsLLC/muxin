//! What's listening on this machine (the app's Servers): its TCP ports, with
//! the program or Docker container behind each, and for web servers the
//! status and title of their front page, so the app can offer to open them on
//! the user's computer (the link carries the connections; see link.rs `tcp`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::logs::quick;

/// Ports not asked for a web page: databases, caches, mail, ssh, ... (a stray
/// HTTP request fills their logs with warnings).
const NOT_WEB_PORTS: &[u16] = &[
    21, 22, 23, 25, 53, 110, 111, 135, 139, 143, 389, 445, 465, 514, 587, 631, 636, 873, 993, 995, 1433, 1521, 1883, 2049, 2181, 2375, 2376, 2379, 2380, 3306, 3389, 4369, 5353, 5432, 5672, 5900, 5984, 6379, 6380, 7000, 7001, 7946, 8883, 9042, 9092, 9160, 11211, 25672, 27017, 27018, 28015,
];
/// Programs (or images) not asked either.
const NOT_WEB_NAMES: &[&str] = &[
    "sshd", "postgres", "mysqld", "mariadbd", "redis", "valkey", "mongod", "memcached", "rabbitmq", "beam.smp", "epmd", "systemd-resolve", "dnsmasq", "cupsd", "rpcbind", "smbd", "nmbd", "avahi", "containerd", "kafka", "zookeeper", "etcd", "nats", "mosquitto", "clickhouse", "cassandra", "rapportd", "controlcenter", "sharingd",
];

struct Listening {
    port: u16,
    /// As it listens: 0.0.0.0, 127.0.0.1, ::, a LAN address.
    address: String,
    process: String,
    pid: Option<u32>,
}

/// `{"op": "ports"}`: the TCP ports something listens on, web servers first.
pub fn list() -> Value {
    let found = if cfg!(target_os = "macos") { lsof() } else { ss() };
    // One entry a port (IPv4 and IPv6 are usually both there), the one with a name.
    let mut by_port: HashMap<u16, Listening> = HashMap::new();
    for l in found {
        match by_port.get(&l.port) {
            Some(have) if !have.process.is_empty() || l.process.is_empty() => {}
            _ => {
                by_port.insert(l.port, l);
            }
        }
    }
    let docker = published();
    let mut ports: Vec<(Listening, Option<(String, String)>)> = by_port.into_values().map(|l| {
        let c = docker.get(&l.port).cloned();
        (l, c)
    }).collect();
    ports.sort_by_key(|(l, _)| l.port);
    ports.truncate(200);
    // Ask the likely web servers for their front page, side by side.
    let pages: Vec<Option<(u16, String)>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = ports
            .iter()
            .map(|(l, c)| {
                let ask = may_be_web(l, c.as_ref());
                let host = connect_host(&l.address);
                let port = l.port;
                scope.spawn(move || if ask { page(&host, port) } else { None })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap_or(None)).collect()
    });
    let mut out: Vec<Value> = ports
        .iter()
        .zip(pages)
        .map(|((l, c), page)| {
            json!({
                "port": l.port,
                "host": connect_host(&l.address),
                "address": l.address,
                "process": l.process,
                "pid": l.pid,
                "container": c.as_ref().map(|(name, _)| name),
                "image": c.as_ref().map(|(_, image)| image),
                "web": page.is_some(),
                "status": page.as_ref().map(|(s, _)| s),
                "title": page.as_ref().map(|(_, t)| t).filter(|t| !t.is_empty()),
            })
        })
        .collect();
    out.sort_by_key(|p| (p["web"] != true, p["port"].as_u64()));
    json!({ "ok": true, "ports": out })
}

/// `{"op": "listening"}`: just the port numbers something listens on (no
/// asking around: cheap enough to look every few seconds).
pub fn listening() -> Value {
    let found = if cfg!(target_os = "macos") { lsof() } else { ss() };
    let mut ports: Vec<u16> = found.into_iter().map(|l| l.port).collect();
    ports.sort_unstable();
    ports.dedup();
    json!({ "ok": true, "ports": ports })
}

/// Where to connect for a port listening on `address`: itself, or this
/// machine for one listening everywhere.
fn connect_host(address: &str) -> String {
    match address {
        "" | "*" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".into(),
        a => a.trim_start_matches('[').trim_end_matches(']').split('%').next().unwrap_or(a).to_string(),
    }
}

fn may_be_web(l: &Listening, container: Option<&(String, String)>) -> bool {
    let name = l.process.to_lowercase();
    let image = container.map(|(_, i)| i.to_lowercase()).unwrap_or_default();
    !NOT_WEB_PORTS.contains(&l.port) && !NOT_WEB_NAMES.iter().any(|n| name.contains(n) || image.contains(n))
}

/// Linux: `ss -ltnpH` ("LISTEN 0 4096 0.0.0.0:8080 0.0.0.0:* users:(("node",pid=812,fd=20))").
fn ss() -> Vec<Listening> {
    let ss = crate::root::tool("ss").unwrap_or_else(|| "ss".into());
    quick(&ss, &["-ltnpH"]).unwrap_or_default().lines().filter_map(parse_ss).collect()
}

fn parse_ss(line: &str) -> Option<Listening> {
    let f: Vec<&str> = line.split_whitespace().collect();
    let local = f.get(3)?;
    let (address, port) = local.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let rest = f.get(5..).map(|r| r.join(" ")).unwrap_or_default();
    let process = rest.split_once("((\"").and_then(|(_, r)| r.split_once('"')).map(|(n, _)| n.to_string()).unwrap_or_default();
    let pid = rest.split_once("pid=").and_then(|(_, r)| r.split(|c: char| !c.is_ascii_digit()).next()).and_then(|p| p.parse().ok());
    Some(Listening { port, address: address.trim_start_matches('[').trim_end_matches(']').split('%').next().unwrap_or(address).to_string(), process, pid })
}

/// A Mac: `lsof -nP -iTCP -sTCP:LISTEN -Fpcn` (the user's own programs, Docker
/// Desktop's ports included).
fn lsof() -> Vec<Listening> {
    let lsof = crate::root::tool("lsof").unwrap_or_else(|| "/usr/sbin/lsof".into());
    parse_lsof(&quick(&lsof, &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]).unwrap_or_default())
}

fn parse_lsof(text: &str) -> Vec<Listening> {
    let (mut pid, mut process) = (None, String::new());
    let mut out = Vec::new();
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().ok(),
            "c" => process = value.to_string(),
            "n" => {
                if let Some((address, port)) = value.rsplit_once(':')
                    && let Ok(port) = port.parse()
                {
                    out.push(Listening { port, address: address.trim_start_matches('[').trim_end_matches(']').to_string(), process: process.clone(), pid });
                }
            }
            _ => {}
        }
    }
    out
}

/// Docker's published ports: the host port to the container and its image.
fn published() -> HashMap<u16, (String, String)> {
    let mut out = HashMap::new();
    let Some(docker) = crate::root::tool("docker") else { return out };
    for line in quick(&docker, &["ps", "--format", "{{.Names}}\t{{.Image}}\t{{.Ports}}"]).unwrap_or_default().lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [name, image, ports] = f[..] else { continue };
        // "0.0.0.0:8080->80/tcp, [::]:8080->80/tcp"
        for p in ports.split(',') {
            if let Some((host, _)) = p.trim().split_once("->")
                && let Some(port) = host.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
            {
                out.insert(port, (name.to_string(), image.to_string()));
            }
        }
    }
    out
}

/// A web server's answer to `GET /`: its status and its page's title. None
/// when it doesn't speak HTTP (or says nothing in time).
fn page(host: &str, port: u16) -> Option<(u16, String)> {
    let ip: IpAddr = host.parse().ok()?;
    let mut s = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_millis(300)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(300)));
    s.write_all(format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\nUser-Agent: Muxen\r\nAccept: text/html\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let started = Instant::now();
    while buf.len() < 64 * 1024 && started.elapsed() < Duration::from_millis(900) {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        // Enough to know: the title's in.
        if find_ci(&buf, b"</title>").is_some() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let status = text.strip_prefix("HTTP/")?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, title(&text)))
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle))
}

/// A page's `<title>`, plain.
fn title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else { return String::new() };
    let Some(start) = lower[open..].find('>').map(|i| open + i + 1) else { return String::new() };
    let end = lower[start..].find("</title>").map(|i| start + i).unwrap_or(html.len().min(start + 200));
    let raw = html.get(start..end).unwrap_or("");
    let text = raw.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ss_and_lsof() {
        let l = parse_ss("LISTEN 0      4096       0.0.0.0:8080      0.0.0.0:*    users:((\"docker-proxy\",pid=1234,fd=7))").unwrap();
        assert_eq!((l.port, l.address.as_str(), l.process.as_str(), l.pid), (8080, "0.0.0.0", "docker-proxy", Some(1234)));
        let l = parse_ss("LISTEN 0 511 [::1]:5173 [::]:*").unwrap();
        assert_eq!((l.port, l.address.as_str(), l.process.as_str()), (5173, "::1", ""));
        let l = parse_ss("LISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:*").unwrap();
        assert_eq!(l.address, "127.0.0.53");
        let found = parse_lsof("p812\ncnode\nn*:3000\nn[::1]:3001\np90\ncpostgres\nn127.0.0.1:5432\n");
        assert_eq!(found.len(), 3);
        assert_eq!((found[0].port, found[0].address.as_str(), found[0].process.as_str()), (3000, "*", "node"));
        assert_eq!((found[1].port, found[1].address.as_str()), (3001, "::1"));
        assert_eq!((found[2].process.as_str(), found[2].pid), ("postgres", Some(90)));
    }

    #[test]
    fn knows_where_to_connect_and_what_to_ask() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("*"), "127.0.0.1");
        assert_eq!(connect_host("::1"), "::1");
        assert_eq!(connect_host("192.168.1.5"), "192.168.1.5");
        let l = |port, process: &str| Listening { port, address: "0.0.0.0".into(), process: process.into(), pid: None };
        assert!(may_be_web(&l(8080, "node"), None));
        assert!(!may_be_web(&l(5432, "docker-proxy"), None));
        assert!(!may_be_web(&l(15432, "postgres"), None));
        assert!(!may_be_web(&l(16379, "docker-proxy"), Some(&("cache".into(), "redis:8-alpine".into()))));
    }

    #[test]
    fn reads_a_title() {
        assert_eq!(title("HTTP/1.1 200 OK\r\n\r\n<html><head><TITLE>\n  Shop &amp; Co\n</TITLE>"), "Shop & Co");
        assert_eq!(title("<p>no title</p>"), "");
    }

    #[test]
    fn asks_a_web_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<title>Vite App</title>");
        });
        assert_eq!(page("127.0.0.1", port), Some((200, "Vite App".to_string())));
    }
}
