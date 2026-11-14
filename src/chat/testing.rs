//! For tests: a small web server on this computer that stands in for a
//! service Muxin talks to (OpenAI's sign-in, its Responses API), answering
//! each request with what the test says and keeping what was asked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request the stand-in got.
#[derive(Clone, Debug)]
pub struct Asked {
    pub method: String,
    /// Without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names in lowercase.
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Asked {
    /// The body as a form (`a=1&b=2`).
    pub fn form(&self) -> HashMap<String, String> {
        reqwest::Url::parse(&format!("http://x/?{}", self.body)).map(|u| u.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_default()
    }
}

/// What the stand-in answers: a status, a content type, a body.
pub type Answer = (u16, &'static str, String);

pub fn json(status: u16, body: serde_json::Value) -> Answer {
    (status, "application/json", body.to_string())
}

/// A stream of events, as the Responses API sends them.
pub fn events(all: &[serde_json::Value]) -> Answer {
    let body: String = all.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect();
    (200, "text/event-stream", body)
}

pub struct Server {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    asked: Arc<Mutex<Vec<Asked>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    /// Starts one that answers each request with `answer(request)`.
    pub async fn start(answer: impl Fn(&Asked) -> Answer + Send + Sync + 'static) -> Server {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (kept, answer) = (asked.clone(), Arc::new(answer));
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (kept, answer) = (kept.clone(), answer.clone());
                tokio::spawn(async move {
                    let Some(request) = read(&mut sock).await else { return };
                    let (status, kind, body) = answer(&request);
                    kept.lock().unwrap().push(request);
                    let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Server { url, asked, task }
    }

    /// Every request so far, in the order they came.
    pub fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    /// The requests to `path`.
    pub fn asked_at(&self, path: &str) -> Vec<Asked> {
        self.asked().into_iter().filter(|a| a.path == path).collect()
    }
}

async fn read(sock: &mut tokio::net::TcpStream) -> Option<Asked> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: HashMap<String, String> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = bytes[head_end + 4..].to_vec();
    while body.len() < length {
        match sock.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    let url = reqwest::Url::parse(&format!("http://x{target}")).ok()?;
    Some(Asked {
        method,
        path: url.path().to_string(),
        query: url.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}
