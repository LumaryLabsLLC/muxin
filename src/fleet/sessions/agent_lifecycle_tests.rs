use super::*;
use crate::config::Inventory;
use crate::fleet::{direct, sessions::lifecycle_state};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const CHILD: &str = "MUXIN_AGENT_STATE_TEST_CHILD";

fn run_child(test: &str, mode: &str) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "muxin-agent-policy-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(dir.join("builds")).unwrap();
    std::fs::create_dir_all(dir.join("tools")).unwrap();
    std::fs::write(dir.join("builds/muxen-agent-darwin-aarch64"), b"test build").unwrap();
    let ssh = dir.join("tools/ssh");
    std::fs::write(
        &ssh,
        b"#!/bin/sh\necho attempted > \"$MUXIN_TEST_SSH_ATTEMPT\"\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, &dir)
        .env("MUXIN_AGENT_STATE_TEST_MODE", mode)
        .env("MUXEN_AGENTS_DIR", dir.join("builds"))
        .env("MUXIN_TEST_SSH_ATTEMPT", dir.join("ssh-attempt"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.join("tools").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spec() -> MachineSpec {
    Inventory::parse("[[machine]]\nname = \"Box\"\nhost = \"should-not-connect.invalid\"\n")
        .unwrap()
        .machines
        .remove(0)
}

async fn mark(dir: &Path, spec: &MachineSpec) {
    direct::init(dir.to_path_buf());
    lifecycle_state::lock(&spec.id)
        .await
        .unwrap()
        .mark_uninstalled()
        .unwrap();
}

#[tokio::test]
async fn verified_fast_path_does_not_touch_the_lifecycle_lock() {
    let Some(dir) = std::env::var_os(CHILD) else {
        run_child(
            "fleet::sessions::agent::lifecycle_tests::verified_fast_path_does_not_touch_the_lifecycle_lock",
            "cached",
        );
        return;
    };
    let dir = PathBuf::from(dir);
    direct::init(dir.clone());
    let spec = spec();
    verified()
        .lock()
        .unwrap()
        .insert(spec.id.clone(), build("darwin-aarch64").unwrap().1);
    let probe = ProbeResult {
        os: Some("darwin".into()),
        arch: Some("aarch64".into()),
        ..Default::default()
    };
    ensure(&spec, Some(&probe)).await.unwrap();
    assert!(
        !dir.join("agent-state").exists(),
        "the cache created a lock"
    );

    let lock = dir
        .join("agent-state")
        .join(format!("{}.lock", direct::fingerprint(spec.id.as_bytes())));
    std::fs::create_dir_all(&lock).unwrap();
    ensure(&spec, Some(&probe)).await.unwrap();
    assert!(lock.is_dir(), "the cache touched the unusable lock path");
    assert!(!dir.join("ssh-attempt").exists());
}

#[tokio::test]
async fn uninstall_blocks_warm_and_cold_processes_before_remote_work() {
    let Some(dir) = std::env::var_os(CHILD) else {
        for mode in ["warm", "cold"] {
            run_child(
                "fleet::sessions::agent::lifecycle_tests::uninstall_blocks_warm_and_cold_processes_before_remote_work",
                mode,
            );
        }
        return;
    };
    let dir = PathBuf::from(dir);
    let spec = spec();
    if std::env::var("MUXIN_AGENT_STATE_TEST_MODE").unwrap() == "warm" {
        verified()
            .lock()
            .unwrap()
            .insert(spec.id.clone(), build("darwin-aarch64").unwrap().1);
    }
    mark(&dir, &spec).await;
    let probe = ProbeResult {
        os: Some("darwin".into()),
        arch: Some("aarch64".into()),
        ..Default::default()
    };
    let error = ensure(&spec, Some(&probe)).await.unwrap_err();
    assert!(error.contains("was uninstalled"), "{error}");
    assert!(!dir.join("ssh-attempt").exists());
}

#[tokio::test]
async fn explicit_reinstall_invalidates_cached_verification_and_keeps_failed_state() {
    let Some(dir) = std::env::var_os(CHILD) else {
        run_child(
            "fleet::sessions::agent::lifecycle_tests::explicit_reinstall_invalidates_cached_verification_and_keeps_failed_state",
            "explicit",
        );
        return;
    };
    let dir = PathBuf::from(dir);
    let inventory = Inventory::parse("[[machine]]\nname = \"Box\"\nhost = \"box\"\n").unwrap();
    let mut spec = inventory.machines[0].clone();
    assert!(!spec.agent_install_requested);
    mark(&dir, &spec).await;
    verified()
        .lock()
        .unwrap()
        .insert(spec.id.clone(), "stale verification".into());
    spec.agent_install_requested = true;
    let error = lifecycle_state::ensure(&spec, async {
        assert!(!verified().lock().unwrap().contains_key(&spec.id));
        Err("fresh remote verification failed".into())
    })
    .await
    .unwrap_err();
    assert_eq!(error, "fresh remote verification failed");
    assert!(!inventory.machines[0].agent_install_requested);
    spec.agent_install_requested = false;
    assert!(
        lifecycle_state::ensure(&spec, async {
            panic!("failed reinstall must keep the tombstone")
        })
        .await
        .unwrap_err()
        .contains("was uninstalled")
    );
    spec.agent_install_requested = true;
    lifecycle_state::ensure(&spec, async { Ok(()) })
        .await
        .unwrap();
    spec.agent_install_requested = false;
    lifecycle_state::ensure(&spec, async { Ok(()) })
        .await
        .unwrap();
}
