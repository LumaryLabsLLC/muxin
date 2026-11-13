//! How a brain that is another program (Claude Code, Codex) reaches Muxin's
//! tools: not through a `muxin mcp` of its own, which would be a second
//! Muxin with its own links to the machines, its own list of operations and
//! nobody to ask the user, but through the Muxin that holds the chat.
//!
//! That Muxin listens on a socket only this user can open. The brain is told
//! its MCP server is `muxin mcp --chat <socket>`, which does nothing but pass
//! bytes between the brain and the socket. So each tool call arrives here,
//! passes the chat's gate (gate.rs), and runs in this process like any other.
//! What it's offered is the chat's short list of tools (tools.rs).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fleet::Engine;
use crate::mcp::Front;

/// A socket path longer than this doesn't fit the system's limit (104 bytes
/// on macOS, 108 on Linux).
#[cfg(unix)]
const MAX_PATH: usize = 100;

/// The listening end, in the Muxin that holds the chat. Dropping it closes it.
pub struct Bridge {
    path: PathBuf,
    /// A folder made just for the socket, to remove with it.
    own_dir: Option<PathBuf>,
    task: tokio::task::JoinHandle<()>,
}

impl Bridge {
    /// The socket to give the brain.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
        remove(&self.path, self.own_dir.as_deref());
        LIVE.lock().unwrap().retain(|(path, _)| path != &self.path);
    }
}

/// The sockets this process has open, for `close_all`.
static LIVE: std::sync::Mutex<Vec<(PathBuf, Option<PathBuf>)>> = std::sync::Mutex::new(Vec::new());

fn remove(path: &Path, own_dir: Option<&Path>) {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = own_dir {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Removes every socket this process opened: for a process about to exit
/// without unwinding (Ctrl-C), so it leaves none behind.
pub fn close_all() {
    for (path, own_dir) in LIVE.lock().unwrap().drain(..) {
        remove(&path, own_dir.as_deref());
    }
}

#[cfg(unix)]
impl Bridge {
    /// Starts listening, under `dir` (Muxin's folder) when the path fits.
    /// Call from inside the runtime.
    pub fn start(engine: Arc<Engine>, front: Arc<dyn Front>, dir: &Path) -> std::io::Result<Bridge> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!("chat-{}-{}.sock", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        // A folder nobody else can look into, so the socket is the user's from
        // the moment it exists.
        let private = |dir: &Path| -> std::io::Result<()> {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        };
        let run = dir.join("run");
        let (path, own_dir) = if run.join(&name).as_os_str().len() <= MAX_PATH {
            private(&run)?;
            (run.join(&name), None)
        } else {
            let own = PathBuf::from("/tmp").join(format!("muxin-{}", crate::util::random_token(12)));
            private(&own)?;
            (own.join("c.sock"), Some(own))
        };
        let _ = std::fs::remove_file(&path);
        let socket = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        socket.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(socket)?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let (engine, front) = (engine.clone(), front.clone());
                tokio::spawn(async move {
                    let (read, write) = stream.into_split();
                    let session = crate::mcp::Session::named("chat");
                    let _ = crate::mcp::serve_as(engine, tokio::io::BufReader::new(read), write, session, Some(front)).await;
                });
            }
        });
        LIVE.lock().unwrap().push((path.clone(), own_dir.clone()));
        Ok(Bridge { path, own_dir, task })
    }
}

/// `muxin mcp --chat <socket>`: passes this process's stdin to the socket and
/// what comes back to stdout, until either side closes.
#[cfg(unix)]
pub async fn proxy(socket: &Path) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (mut from_muxin, mut to_muxin) = stream.into_split();
    let up = async {
        let copied = tokio::io::copy(&mut tokio::io::stdin(), &mut to_muxin).await;
        let _ = to_muxin.shutdown().await;
        copied
    };
    let down = async {
        let mut out = tokio::io::stdout();
        let copied = tokio::io::copy(&mut from_muxin, &mut out).await;
        let _ = out.flush().await;
        copied
    };
    tokio::select! {
        r = up => r.map(|_| ()),
        r = down => r.map(|_| ()),
    }
}

// Windows has no such sockets under a folder; a named pipe does this there
// (as in crate::instance). Until then, a brain that is another program can't
// be used on Windows.
#[cfg(not(unix))]
impl Bridge {
    pub fn start(_engine: Arc<Engine>, _front: Arc<dyn Front>, _dir: &Path) -> std::io::Result<Bridge> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "The chat can't use Claude Code or Codex on Windows yet."))
    }
}

#[cfg(not(unix))]
pub async fn proxy(_socket: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "The chat can't use Claude Code or Codex on Windows yet."))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::chat::gate::{Answer, Event, Gate, Mode};
    use crate::config::Paths;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Speaks MCP to the bridge the way a brain's client would.
    struct Client {
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        write: tokio::net::unix::OwnedWriteHalf,
    }

    impl Client {
        async fn connect(path: &Path) -> Client {
            let (read, write) = tokio::net::UnixStream::connect(path).await.unwrap().into_split();
            let mut c = Client { lines: BufReader::new(read).lines(), write };
            c.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code"}}})).await;
            assert_eq!(c.recv().await["result"]["serverInfo"]["name"], "muxin");
            c
        }

        async fn send(&mut self, msg: Value) {
            let mut bytes = serde_json::to_vec(&msg).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }

        async fn recv(&mut self) -> Value {
            let line = tokio::time::timeout(std::time::Duration::from_secs(30), self.lines.next_line()).await.expect("a reply in time");
            serde_json::from_str(&line.unwrap().unwrap()).unwrap()
        }

        async fn call(&mut self, id: u64, tool: &str, args: Value) {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}})).await;
        }
    }

    fn demo(name: &str) -> (Arc<Engine>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("muxin-bridge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Engine::new(Paths::resolve(Some(dir.clone()), None), true), dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_runs_and_a_change_waits_for_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let (engine, dir) = demo("gate");
        let (gate, mut events) = Gate::new(Mode::Ask, vec![]);
        let bridge = Bridge::start(engine, crate::chat::tools::Short::new(gate.clone()), &dir).unwrap();
        assert_eq!(std::fs::metadata(bridge.path()).unwrap().permissions().mode() & 0o777, 0o600);
        let mut c = Client::connect(bridge.path()).await;

        // Looking needs nobody.
        c.call(1, "status", json!({})).await;
        let status = c.recv().await;
        assert_eq!(status["result"]["isError"], false);
        assert!(status["result"]["structuredContent"]["counts"]["machines"].as_u64().unwrap() > 0);
        assert!(events.try_recv().is_err());

        // Saving a snippet waits; a no comes back to the brain in words, and nothing was saved.
        let snippet = json!({"name": "Uptime", "command": "uptime"});
        c.call(2, "snippet_save", snippet.clone()).await;
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        assert_eq!((req.tool.as_str(), req.args["name"].as_str()), ("snippet_save", Some("Uptime")));
        gate.answer(req.id, Answer::No("not that one".into()));
        let refused = c.recv().await;
        assert_eq!(refused["id"], 2);
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(refused["result"]["structuredContent"]["error"]["code"], "declined");
        assert!(refused["result"]["structuredContent"]["error"]["message"].as_str().unwrap().contains("not that one"));

        assert!(matches!(events.recv().await, Some(Event::Answered { .. })));
        c.call(3, "snippet_list", json!({})).await;
        assert!(!c.recv().await["result"]["content"][0]["text"].as_str().unwrap().contains("Uptime"));

        // A yes lets it run, here, in this process.
        c.call(4, "snippet_save", snippet).await;
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        gate.answer(req.id, Answer::Yes);
        let done = c.recv().await;
        assert_eq!(done["result"]["isError"], false, "{done}");
        c.call(5, "snippet_list", json!({})).await;
        assert!(c.recv().await["result"]["content"][0]["text"].as_str().unwrap().contains("Uptime"));

        let path = bridge.path().to_path_buf();
        drop(bridge);
        assert!(!path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bypass_asks_nothing() {
        let (engine, dir) = demo("bypass");
        let (gate, mut events) = Gate::new(Mode::Bypass, vec![]);
        let bridge = Bridge::start(engine, crate::chat::tools::Short::new(gate), &dir).unwrap();
        let mut c = Client::connect(bridge.path()).await;
        c.call(1, "snippet_save", json!({"name": "Uptime", "command": "uptime"})).await;
        assert_eq!(c.recv().await["result"]["isError"], false);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_too_long_for_a_socket_moves_to_tmp() {
        let (engine, _) = demo("long");
        let (gate, _events) = Gate::new(Mode::Ask, vec![]);
        let deep = std::env::temp_dir().join("x".repeat(120));
        let bridge = Bridge::start(engine, crate::chat::tools::Short::new(gate), &deep).unwrap();
        assert!(bridge.path().starts_with("/tmp/"), "{}", bridge.path().display());
        let own = bridge.path().parent().unwrap().to_path_buf();
        Client::connect(bridge.path()).await;
        drop(bridge);
        assert!(!own.exists());
    }
}
