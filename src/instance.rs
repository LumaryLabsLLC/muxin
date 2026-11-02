//! One running copy of the app per Muxin home. Opening it again brings that
//! copy forward (and hands it a share link) instead of starting another. A
//! Mac's Dock does this for the app, but not for `muxin` in a terminal, and
//! Linux and Windows have nothing that would.
//!
//! The first copy holds a lock for as long as it runs (the system lets go when
//! it ends, however it ends) and listens on a socket beside it, a named pipe on
//! Windows. Anything going wrong here only means another copy starts, as it
//! would without this.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// What a later launch asks of the running copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Show,
    /// A share link opened with Muxin.
    Join(String),
    Quit,
}

pub enum Claim {
    /// This is the first copy: it answers the later ones (see `Listener::serve`).
    First(Listener),
    /// Another copy runs. `tell` it what's wanted.
    Taken,
    /// Can't be told: this copy runs by itself.
    Unknown,
}

/// What came of telling the running copy.
#[derive(Debug, PartialEq, Eq)]
pub enum Told {
    Done,
    /// It's another version, and quits for this one to take over (see `claim_when_free`).
    Replacing,
    NoAnswer,
}

pub struct Listener {
    _lock: File,
    #[cfg(unix)]
    socket: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(windows)]
    pipe_name: String,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A share link's text fits well within this.
const LONGEST_REQUEST: u64 = 64 * 1024;

fn try_lock(dir: &Path) -> Result<File, Claim> {
    let _ = std::fs::create_dir_all(dir);
    let Ok(lock) = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock")) else {
        return Err(Claim::Unknown);
    };
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(Claim::Taken),
        Err(TryLockError::Error(_)) => Err(Claim::Unknown),
    }
}

/// Becomes the running copy, if none is. In the async runtime's context.
pub fn claim(dir: &Path) -> Claim {
    let lock = match try_lock(dir) {
        Ok(lock) => lock,
        Err(claim) => return claim,
    };
    match Listener::listen(dir, lock) {
        Ok(listener) => Claim::First(listener),
        Err(e) => {
            tracing::warn!("listening for later launches: {e}");
            Claim::Unknown
        }
    }
}

/// Whether a copy of the app runs for this home.
pub fn is_running(dir: &Path) -> bool {
    matches!(try_lock(dir), Err(Claim::Taken))
}

/// After `Told::Replacing`: the other copy has a moment to quit.
pub async fn claim_when_free(dir: &Path) -> Option<Listener> {
    for _ in 0..60 {
        match claim(dir) {
            Claim::First(listener) => return Some(listener),
            Claim::Unknown => return None,
            Claim::Taken => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    None
}

/// Tells the running copy. It may only just have started: it gets a few seconds.
pub async fn tell(dir: &Path, request: &Request) -> Told {
    for _ in 0..30 {
        if let Ok(stream) = connect(dir).await {
            return exchange(stream, request).await.unwrap_or(Told::NoAnswer);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Told::NoAnswer
}

async fn exchange(stream: impl AsyncRead + AsyncWrite + Unpin, request: &Request) -> std::io::Result<Told> {
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(format!("{VERSION} {}\n", encode(request)).as_bytes()).await?;
    stream.get_mut().flush().await?;
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut reply)).await??;
    Ok(if reply.trim() == "replacing" { Told::Replacing } else { Told::Done })
}

fn encode(request: &Request) -> String {
    match request {
        Request::Show => "show".into(),
        Request::Join(url) => format!("join {url}"),
        Request::Quit => "quit".into(),
    }
}

/// One line, "<version> <request>". None for anything else.
fn decode(line: &str) -> Option<(&str, Request)> {
    let (version, rest) = line.trim_end().split_once(' ')?;
    let request = match rest.split_once(' ') {
        None if rest == "show" => Request::Show,
        None if rest == "quit" => Request::Quit,
        Some(("join", url)) if url.starts_with("muxin://join") => Request::Join(url.to_string()),
        _ => return None,
    };
    Some((version, request))
}

async fn answer(stream: impl AsyncRead + AsyncWrite + Unpin, on_request: &(dyn Fn(Request) + Send + Sync)) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    let mut limited = (&mut stream).take(LONGEST_REQUEST);
    tokio::time::timeout(Duration::from_secs(5), limited.read_line(&mut line)).await??;
    let Some((version, request)) = decode(&line) else {
        return Ok(());
    };
    // Another version was installed: opening it should run it. It takes over.
    let replacing = version != VERSION && request != Request::Quit;
    stream.get_mut().write_all(if replacing { b"replacing\n" } else { b"ok\n" }).await?;
    stream.get_mut().flush().await?;
    on_request(if replacing { Request::Quit } else { request });
    Ok(())
}

impl Listener {
    /// Answers later launches for as long as the app runs.
    pub fn serve(self, on_request: impl Fn(Request) + Send + Sync + 'static) {
        let on_request: std::sync::Arc<dyn Fn(Request) + Send + Sync> = std::sync::Arc::new(on_request);
        tokio::spawn(async move {
            if let Err(e) = self.accept_all(on_request).await {
                tracing::warn!("answering later launches: {e}");
            }
        });
    }
}

#[cfg(unix)]
impl Listener {
    fn listen(dir: &Path, lock: File) -> std::io::Result<Listener> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("app.sock");
        // Holding the lock, so this one is left from a copy that's gone.
        let _ = std::fs::remove_file(&path);
        let socket = std::os::unix::net::UnixListener::bind(&path)?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        socket.set_nonblocking(true)?;
        Ok(Listener { _lock: lock, socket: tokio::net::UnixListener::from_std(socket)? })
    }

    async fn accept_all(self, on_request: std::sync::Arc<dyn Fn(Request) + Send + Sync>) -> std::io::Result<()> {
        // (The lock goes with this, when the app ends.)
        loop {
            let (stream, _) = self.socket.accept().await?;
            let on_request = on_request.clone();
            tokio::spawn(async move {
                let _ = answer(stream, &*on_request).await;
            });
        }
    }
}

#[cfg(unix)]
async fn connect(dir: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(dir.join("app.sock")).await
}

/// Windows' pipes are one namespace for the whole computer: the name stands for
/// this home (which has the user's name in it).
#[cfg(windows)]
fn pipe_name(dir: &Path) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, dir.to_string_lossy().as_bytes());
    let hex: String = digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\muxin-{hex}")
}

#[cfg(windows)]
impl Listener {
    fn listen(dir: &Path, lock: File) -> std::io::Result<Listener> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let pipe_name = pipe_name(dir);
        let pipe = ServerOptions::new().first_pipe_instance(true).create(&pipe_name)?;
        Ok(Listener { _lock: lock, pipe, pipe_name })
    }

    async fn accept_all(self, on_request: std::sync::Arc<dyn Fn(Request) + Send + Sync>) -> std::io::Result<()> {
        use tokio::net::windows::named_pipe::ServerOptions;
        // (The lock goes with this, when the app ends.)
        let Listener { _lock, mut pipe, pipe_name } = self;
        loop {
            pipe.connect().await?;
            let stream = std::mem::replace(&mut pipe, ServerOptions::new().create(&pipe_name)?);
            let on_request = on_request.clone();
            tokio::spawn(async move {
                let _ = answer(stream, &*on_request).await;
            });
        }
    }
}

#[cfg(windows)]
async fn connect(dir: &Path) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(pipe_name(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn home(name: &str) -> std::path::PathBuf {
        // Short enough for a socket's address, wherever the temp folder is.
        let dir = std::env::temp_dir().join(format!("mx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn serve(listener: Listener) -> Arc<Mutex<Vec<Request>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        listener.serve(move |r| recorded.lock().unwrap().push(r));
        seen
    }

    async fn settled(seen: &Mutex<Vec<Request>>, want: usize) -> Vec<Request> {
        for _ in 0..100 {
            if seen.lock().unwrap().len() >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_later_launch_reaches_the_first() {
        let dir = home("later");
        let Claim::First(listener) = claim(&dir) else { panic!("the first copy should get it") };
        let seen = serve(listener);
        assert!(matches!(claim(&dir), Claim::Taken));
        assert!(is_running(&dir));
        assert_eq!(tell(&dir, &Request::Show).await, Told::Done);
        assert_eq!(tell(&dir, &Request::Join("muxin://join?server=x#y".into())).await, Told::Done);
        assert_eq!(settled(&seen, 2).await, vec![Request::Show, Request::Join("muxin://join?server=x#y".into())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn another_version_takes_over() {
        let dir = home("version");
        let Claim::First(listener) = claim(&dir) else { panic!("the first copy should get it") };
        let seen = serve(listener);
        let told = exchange(connect(&dir).await.unwrap(), &Request::Show).await.unwrap();
        assert_eq!(told, Told::Done, "the same version is only shown");
        let mut stream = BufReader::new(connect(&dir).await.unwrap());
        stream.get_mut().write_all(b"0.0.0-older show\n").await.unwrap();
        let mut reply = String::new();
        stream.read_line(&mut reply).await.unwrap();
        assert_eq!(reply.trim(), "replacing");
        assert_eq!(settled(&seen, 2).await, vec![Request::Show, Request::Quit]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_socket_left_behind_is_replaced() {
        let dir = home("stale");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        drop(std::os::unix::net::UnixListener::bind(dir.join("app.sock")).unwrap());
        let Claim::First(listener) = claim(&dir) else { panic!("nobody holds the lock") };
        let seen = serve(listener);
        assert_eq!(tell(&dir, &Request::Show).await, Told::Done);
        assert_eq!(settled(&seen, 1).await, vec![Request::Show]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn homes_keep_to_themselves() {
        let (a, b) = (home("a"), home("b"));
        let (Claim::First(_a), Claim::First(_b)) = (claim(&a), claim(&b)) else { panic!("each home has its own copy") };
        assert!(!is_running(&home("c")));
        for dir in [a, b, home("c")] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn only_requests_it_knows() {
        assert_eq!(decode("0.9.0 show\n"), Some(("0.9.0", Request::Show)));
        assert_eq!(decode("0.9.0 quit"), Some(("0.9.0", Request::Quit)));
        assert_eq!(decode("0.9.0 join muxin://join?a#b"), Some(("0.9.0", Request::Join("muxin://join?a#b".into()))));
        assert_eq!(decode("0.9.0 join https://example.com"), None);
        assert_eq!(decode("0.9.0 rm -rf"), None);
        assert_eq!(decode("show"), None);
    }
}
