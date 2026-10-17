//! Controller-side lifecycle state shared by the app and CLI. Lock files
//! stay in place: removing one could let two processes lock different files.

use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::MachineSpec;
use crate::fleet::direct;

pub struct Guard {
    _file: File,
    marker: PathBuf,
}

fn paths(dir: &Path, machine_id: &str) -> (PathBuf, PathBuf) {
    let key = direct::fingerprint(machine_id.as_bytes());
    let root = dir.join("agent-state");
    (
        root.join(format!("{key}.lock")),
        root.join(format!("{key}.uninstalled")),
    )
}

fn uninstalled(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Couldn't read the agent's uninstall state: {error}"
        )),
    }
}

fn refuse(spec: &MachineSpec, marker: &Path) -> Result<(), String> {
    if !spec.agent_install_requested && uninstalled(marker)? {
        Err(format!(
            "The agent on {} was uninstalled. Reinstall it with `muxin agent install {}`.",
            spec.name, spec.id
        ))
    } else {
        Ok(())
    }
}

impl Guard {
    async fn at(dir: &Path, machine_id: &str) -> Result<Self, String> {
        let (lock, marker) = paths(dir, machine_id);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(lock.parent().ok_or("No agent state folder")?).map_err(|e| e.to_string())?;
            let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(lock).map_err(|e| format!("Couldn't open the agent lifecycle lock: {e}"))?;
            let deadline = Instant::now() + Duration::from_secs(300);
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(Self { _file: file, marker }),
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                    Err(TryLockError::WouldBlock) => return Err("Another agent install or uninstall is still running. Try again when it finishes.".into()),
                    Err(TryLockError::Error(error)) => return Err(format!("Couldn't lock the agent lifecycle: {error}")),
                }
            }
        }).await.map_err(|e| format!("Couldn't wait for the agent lifecycle lock: {e}"))?
    }

    pub fn mark_uninstalled(&self) -> Result<(), String> {
        crate::config::write_atomic(&self.marker, b"uninstalled\n")
            .map_err(|e| format!("Couldn't save the agent's uninstall state: {e}"))
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.marker) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "Couldn't clear the agent's uninstall state: {error}"
            )),
        }
    }
}

pub async fn lock(machine_id: &str) -> Result<Guard, String> {
    let dir = direct::config_dir().ok_or("Muxin's config folder isn't set")?;
    Guard::at(dir, machine_id).await
}

/// Check the persistent marker before trusting the in-process hash cache.
pub fn refuse_if_uninstalled(spec: &MachineSpec) -> Result<(), String> {
    let dir = direct::config_dir().ok_or("Muxin's config folder isn't set")?;
    refuse(spec, &paths(dir, &spec.id).1)
}

/// Run a remote check or install only after rechecking the marker under the
/// OS lock. Cached checks need only `refuse_if_uninstalled`.
pub async fn ensure(
    spec: &MachineSpec,
    work: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let dir = direct::config_dir().ok_or("Muxin's config folder isn't set")?;
    ensure_at(dir, spec, work).await
}

async fn ensure_at(
    dir: &Path,
    spec: &MachineSpec,
    work: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    refuse(spec, &paths(dir, &spec.id).1)?;
    let guard = Guard::at(dir, &spec.id).await?;
    refuse(spec, &guard.marker)?;
    if spec.agent_install_requested {
        super::agent::forget(&spec.id);
    }
    work.await?;
    if spec.agent_install_requested {
        guard.clear()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Inventory;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "muxin-lifecycle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec() -> MachineSpec {
        Inventory::parse("[[machine]]\nname = \"Box\"\nhost = \"box\"\n")
            .unwrap()
            .machines
            .remove(0)
    }

    #[tokio::test]
    async fn uninstall_is_persistent_and_blocks_cached_or_new_work() {
        let fixture = Fixture::new();
        let spec = spec();
        {
            let guard = Guard::at(&fixture.0, &spec.id).await.unwrap();
            guard.mark_uninstalled().unwrap();
        }
        for _ in 0..2 {
            let error = ensure_at(&fixture.0, &spec, async {
                panic!("uninstalled agents must not be checked or uploaded")
            })
            .await
            .unwrap_err();
            assert!(error.contains("was uninstalled"));
            assert!(error.contains("muxin agent install box"));
        }
        let mut other = spec.clone();
        other.id = "other".into();
        ensure_at(&fixture.0, &other, async { Ok(()) })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_explicit_reinstall_clears_the_marker_only_after_success() {
        let fixture = Fixture::new();
        let mut spec = spec();
        let marker = paths(&fixture.0, &spec.id).1;
        let guard = Guard::at(&fixture.0, &spec.id).await.unwrap();
        guard.mark_uninstalled().unwrap();
        drop(guard);
        spec.agent_install_requested = true;
        let error = ensure_at(&fixture.0, &spec, async {
            Err("health check failed".into())
        })
        .await
        .unwrap_err();
        assert_eq!(error, "health check failed");
        assert!(marker.exists());
        ensure_at(&fixture.0, &spec, async {
            assert!(marker.exists());
            Ok(())
        })
        .await
        .unwrap();
        assert!(!marker.exists());
        assert!(
            paths(&fixture.0, &spec.id).0.exists(),
            "the lock file must never be unlinked"
        );
    }

    #[tokio::test]
    async fn an_in_flight_install_finishes_before_uninstall_cleanup() {
        let fixture = Fixture::new();
        let dir = fixture.0.clone();
        let spec = spec();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let installed = finished.clone();
        let install_dir = dir.clone();
        let install_spec = spec.clone();
        let install = tokio::spawn(async move {
            ensure_at(&install_dir, &install_spec, async {
                started.send(()).unwrap();
                released.await.unwrap();
                installed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        });
        ready.await.unwrap();
        let uninstall_dir = dir.clone();
        let uninstall_id = spec.id.clone();
        let uninstall = tokio::spawn(async move {
            let guard = Guard::at(&uninstall_dir, &uninstall_id).await.unwrap();
            assert!(finished.load(Ordering::SeqCst), "cleanup raced the install");
            guard.mark_uninstalled().unwrap();
        });
        release.send(()).unwrap();
        install.await.unwrap();
        uninstall.await.unwrap();
        assert!(uninstalled(&paths(&dir, &spec.id).1).unwrap());
    }

    #[tokio::test]
    async fn process_exit_releases_the_lock_without_replacing_its_file() {
        const CHILD_DIR: &str = "MUXIN_LIFECYCLE_LOCK_HOLDER";
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let dir = PathBuf::from(dir);
            let _guard = Guard::at(&dir, "box").await.unwrap();
            println!("LIFECYCLE_LOCK_HELD");
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::park();
            }
        }
        let fixture = Fixture::new();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fleet::sessions::lifecycle_state::tests::process_exit_releases_the_lock_without_replacing_its_file", "--nocapture"])
            .env(CHILD_DIR, &fixture.0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut held = false;
        for _ in 0..20 {
            let mut line = String::new();
            if output.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line.contains("LIFECYCLE_LOCK_HELD") {
                held = true;
                break;
            }
        }
        assert!(held);
        let path = paths(&fixture.0, "box").0;
        let original = std::fs::metadata(&path).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let _guard = Guard::at(&fixture.0, "box").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(original.ino(), std::fs::metadata(path).unwrap().ino());
        }
        #[cfg(not(unix))]
        {
            let _ = original;
            assert!(path.exists());
        }
    }

    #[tokio::test]
    async fn processes_serialize_install_and_uninstall() {
        const CHILD_DIR: &str = "MUXIN_LIFECYCLE_TEST_CHILD";
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let dir = PathBuf::from(dir);
            let spec = spec();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(paths(&dir, &spec.id).0)
                .unwrap();
            assert!(matches!(file.try_lock(), Err(TryLockError::WouldBlock)));
            println!("LIFECYCLE_LOCK_BLOCKED");
            std::io::stdout().flush().unwrap();
            let error = ensure_at(&dir, &spec, async {
                panic!("the other process uninstalled this agent")
            })
            .await
            .unwrap_err();
            assert!(error.contains("was uninstalled"));
            return;
        }
        let fixture = Fixture::new();
        let spec = spec();
        let guard = Guard::at(&fixture.0, &spec.id).await.unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fleet::sessions::lifecycle_state::tests::processes_serialize_install_and_uninstall", "--nocapture"])
            .env(CHILD_DIR, &fixture.0).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = false;
        for _ in 0..20 {
            let mut line = String::new();
            if output.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line.contains("LIFECYCLE_LOCK_BLOCKED") {
                ready = true;
                break;
            }
        }
        assert!(ready, "child did not observe the OS lock");
        guard.mark_uninstalled().unwrap();
        drop(guard);
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
