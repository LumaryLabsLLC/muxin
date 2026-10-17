//! Remove an agent over ssh without starting or updating it first.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::sessions::agent;
use super::workspaces::transport::{self, Limits};
use super::{Engine, direct, link};
use crate::api::{ApiError, ApiResult, MachineRequest};

const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize)]
pub struct AgentUninstall {
    pub machine_id: String,
    pub summary: String,
    pub removed: Vec<String>,
    pub failed: Vec<String>,
    pub demo: bool,
}

impl AgentUninstall {
    pub fn text(&self) -> String {
        let mut text = format!("{}\n", self.summary);
        for item in &self.removed {
            text.push_str(&format!("Removed: {item}\n"));
        }
        for item in &self.failed {
            text.push_str(&format!("Could not remove: {item}\n"));
        }
        text
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Mac,
    Linux,
    Windows,
}

fn uninstall_script(os: Os) -> String {
    match os {
        Os::Windows => include_str!("sessions/scripts/uninstall-agent.ps1").to_string(),
        Os::Mac | Os::Linux => include_str!("sessions/scripts/uninstall-agent.sh")
            .replace("__OS__", if os == Os::Mac { "darwin" } else { "linux" }),
    }
    .replace("\r\n", "\n")
}

fn read_cleanup(report: &mut AgentUninstall, output: &str) -> bool {
    let mut complete = false;
    for line in output.lines() {
        if let Some(item) = line.strip_prefix("removed\t") {
            report.removed.push(item.to_string());
        } else if let Some(item) = line.strip_prefix("failed\t") {
            report.failed.push(item.to_string());
        } else if line.trim() == "complete" {
            complete = true;
        }
    }
    if !complete {
        report
            .failed
            .push("The machine did not confirm that cleanup finished.".into());
    }
    complete
}

fn finish_cleanup(
    report: &mut AgentUninstall,
    output: Result<String, String>,
    forget: impl FnOnce() -> Result<bool, String>,
) {
    let complete = match output {
        Ok(output) => read_cleanup(report, &output),
        Err(error) => {
            report.failed.push(format!("Remote cleanup: {error}"));
            false
        }
    };
    if complete && report.failed.is_empty() {
        match forget() {
            Ok(true) => report
                .removed
                .push("Local pairing and the machine's trusted fingerprint".into()),
            Ok(false) => {}
            Err(error) => report.failed.push(format!("Local pairing: {error}")),
        }
    } else {
        report
            .failed
            .push("Local pairing was kept because remote cleanup did not finish.".into());
    }
}

#[derive(Debug, Serialize)]
pub struct AgentInstall {
    pub machine_id: String,
    pub summary: String,
    pub demo: bool,
}

/// Explicit installation is operation-local. The installer verifies the
/// remote build before clearing the persistent uninstall marker.
pub async fn agent_install(
    engine: &Arc<Engine>,
    request: MachineRequest,
) -> ApiResult<AgentInstall> {
    engine.refresh_from_disk();
    let machine = crate::api::find_machine(&engine.current(), &request.machine_id)?.clone();
    let (mut spec, probe) = engine
        .machine(&machine.id)
        .ok_or_else(|| ApiError::not_found(format!("No machine called {}.", machine.id)))?;
    let demo = engine.demo_world().is_some();
    if !demo {
        spec.agent_install_requested = true;
        agent::ensure(&spec, probe.as_ref())
            .await
            .map_err(|error| ApiError {
                code: "operation_failed",
                message: error,
                hint: None,
                details: None,
            })?;
        link::reset(&spec.id);
        direct::retry_now(&spec.id);
    }
    Ok(AgentInstall {
        machine_id: spec.id,
        summary: format!("The agent on {} is ready.", spec.name),
        demo,
    })
}

/// The API used by both the CLI and MCP. A partial cleanup is an error with
/// the full report, including the work that succeeded.
pub async fn agent_uninstall(
    engine: &Arc<Engine>,
    request: MachineRequest,
) -> ApiResult<AgentUninstall> {
    engine.refresh_from_disk();
    let machine = crate::api::find_machine(&engine.current(), &request.machine_id)?.clone();
    let report = engine
        .uninstall_agent(&machine.id)
        .await
        .map_err(ApiError::invalid)?;
    if report.failed.is_empty() {
        Ok(report)
    } else {
        Err(ApiError {
            code: "operation_failed",
            message: report.text().trim_end().to_string(),
            hint: Some("Fix the reported errors, then run the uninstall again.".into()),
            details: Some(
                serde_json::to_value(&report).map_err(|e| ApiError::invalid(e.to_string()))?,
            ),
        })
    }
}

// A line callback keeps the transport on ssh. The agent must not run the
// command that removes itself, or cleanup would lose its connection halfway.
async fn run_uninstall(
    spec: &crate::config::MachineSpec,
    windows: bool,
    script: &str,
) -> Result<String, String> {
    let marker = transport::new_marker();
    let ignore_line = |_: &str| {};
    let out = Box::pin(transport::run(
        spec,
        windows,
        &marker,
        transport::with_marker(&marker, windows, script),
        UNINSTALL_TIMEOUT,
        Limits::OUTPUT,
        &[],
        Some(&ignore_line),
    ))
    .await?;
    if out.timed_out || !out.started || out.status != Some(0) {
        Err(out.reason())
    } else {
        Ok(out.stdout)
    }
}

impl Engine {
    pub async fn uninstall_agent(
        self: &Arc<Self>,
        machine_id: &str,
    ) -> Result<AgentUninstall, String> {
        let (spec, probe) = self
            .machine(machine_id)
            .ok_or_else(|| format!("No machine called {machine_id}."))?;
        spec.refuse_if_disconnected()?;
        let mut report = AgentUninstall {
            machine_id: spec.id.clone(),
            summary: String::new(),
            removed: Vec::new(),
            failed: Vec::new(),
            demo: self.demo_world().is_some(),
        };
        if report.demo {
            report.summary = format!("Removed the demo agent from {}.", spec.name);
            report
                .removed
                .push("Demo agent, schedules and pairing".into());
            return Ok(report);
        }
        if !spec.is_checkable() {
            return Err(format!("{} has no host to connect to.", spec.name));
        }
        let os = if spec.is_local() {
            if cfg!(windows) {
                Some(Os::Windows)
            } else if cfg!(target_os = "macos") {
                Some(Os::Mac)
            } else if cfg!(target_os = "linux") {
                Some(Os::Linux)
            } else {
                None
            }
        } else {
            let platform = probe
                .as_ref()
                .and_then(|p| p.os.as_deref())
                .or(spec.platform.as_deref())
                .unwrap_or("");
            if platform.starts_with("darwin") || platform.starts_with("macos") {
                Some(Os::Mac)
            } else if platform.starts_with("linux") {
                Some(Os::Linux)
            } else if platform.starts_with("windows") {
                Some(Os::Windows)
            } else {
                None
            }
        }
        .ok_or_else(|| {
            format!(
                "Muxin doesn't know the system on {}. Run `muxin check` first.",
                spec.name
            )
        })?;
        // The app and CLI share this OS lock and persistent desired state.
        let guard = super::sessions::lifecycle_state::lock(&spec.id).await?;
        guard.mark_uninstalled()?;
        link::reset(&spec.id);
        agent::forget(&spec.id);
        let script = uninstall_script(os);
        let output = run_uninstall(&spec, os == Os::Windows, &script).await;
        finish_cleanup(&mut report, output, || direct::forget_pairing(&spec.id));
        link::reset(&spec.id);
        agent::forget(&spec.id);
        report.summary = if !report.failed.is_empty() {
            format!(
                "Could not finish removing the agent from {}. Automatic installation stays off until `muxin agent install {}` succeeds.",
                spec.name, spec.id
            )
        } else if report.removed.is_empty() {
            format!("No agent files were found on {}.", spec.name)
        } else {
            format!(
                "Removed the agent from {}. Reinstall it with `muxin agent install {}`.",
                spec.name, spec.id
            )
        };
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    #[test]
    fn pairing_is_kept_on_failed_partial_or_unconfirmed_remote_cleanup() {
        for output in [
            Err("ssh failed".into()),
            Ok("failed\tservice refused\ncomplete\n".into()),
            Ok("removed\tfiles\n".into()),
        ] {
            let mut report = AgentUninstall {
                machine_id: "box".into(),
                summary: String::new(),
                removed: Vec::new(),
                failed: Vec::new(),
                demo: false,
            };
            finish_cleanup(&mut report, output, || {
                panic!("pairing must survive failed cleanup")
            });
            assert!(
                report
                    .failed
                    .iter()
                    .any(|message| message.contains("Local pairing was kept"))
            );
        }
        let mut report = AgentUninstall {
            machine_id: "box".into(),
            summary: String::new(),
            removed: Vec::new(),
            failed: Vec::new(),
            demo: false,
        };
        finish_cleanup(&mut report, Ok("removed\tfiles\ncomplete\n".into()), || {
            Ok(true)
        });
        assert!(report.failed.is_empty());
        assert!(
            report
                .removed
                .iter()
                .any(|message| message.contains("Local pairing"))
        );
    }

    #[tokio::test]
    async fn mcp_uninstall_requires_a_machine_and_runs_in_demo_mode() {
        let engine = Engine::new(
            Paths::resolve(
                Some(std::env::temp_dir().join("muxin-uninstall-mcp-demo")),
                None,
            ),
            true,
        );
        let session = crate::mcp::Session::default();
        let missing =
            crate::mcp::call_tool(&engine, &session, "agent_uninstall", serde_json::json!({}))
                .await;
        assert_eq!(missing["isError"], true);
        assert_eq!(missing["structuredContent"]["error"]["code"], "invalid");
        let unknown = crate::mcp::call_tool(
            &engine,
            &session,
            "agent_uninstall",
            serde_json::json!({ "machine_id": "no-such-machine" }),
        )
        .await;
        assert_eq!(unknown["structuredContent"]["error"]["code"], "not_found");
        let removed = crate::mcp::call_tool(
            &engine,
            &session,
            "agent_uninstall",
            serde_json::json!({ "machine_id": "forge" }),
        )
        .await;
        assert_eq!(removed["isError"], false);
        assert_eq!(removed["structuredContent"]["demo"], true);
        assert_eq!(removed["structuredContent"]["machine_id"], "forge");
    }

    #[test]
    fn plans_only_muxens_files_and_registrations_for_each_system() {
        let mac = uninstall_script(Os::Mac);
        assert!(mac.contains("case 'darwin'"));
        assert!(mac.contains("$HOME/Library/LaunchAgents/$label.plist"));
        assert!(mac.contains("com.lumarylabs.muxen-agent"));
        assert!(mac.contains("com.lumarylabs.muxen.schedule.*.plist"));
        assert!(mac.contains("launchctl bootout"));
        let linux = uninstall_script(Os::Linux);
        assert!(linux.contains("case 'linux'"));
        assert!(linux.contains("$HOME/.config/systemd/user/$unit"));
        assert!(linux.contains("muxen-agent.service"));
        assert!(linux.contains("systemctl --user disable --now"));
        assert!(linux.contains(
            "# muxen schedules: managed by Muxen (edits between these lines are replaced)"
        ));
        assert!(linux.contains("# end of muxen schedules"));
        assert!(linux.contains("crontab -"));
        for script in [mac, linux] {
            assert!(script.contains("remove_path \"$HOME/.muxen\""));
            assert!(!script.contains("config.toml"));
            assert!(!script.contains("sudo"));
            assert!(!script.contains('\r'));
        }
        let windows = uninstall_script(Os::Windows);
        assert!(windows.contains("$env:USERPROFILE '.muxen'"));
        assert!(windows.contains("$_.TaskName -eq 'Muxen agent'"));
        assert!(windows.contains("$_.TaskPath -eq '\\Muxen\\'"));
        assert!(windows.contains("Muxen schedule s-*"));
        assert!(windows.contains("Unregister-ScheduledTask"));
        assert!(windows.contains("Remove-NetFirewallRule"));
        assert!(!windows.contains("config.toml"));
    }

    #[test]
    fn reports_successes_and_failures_without_claiming_incomplete_cleanup() {
        let mut report = AgentUninstall {
            machine_id: "box".into(),
            summary: "Cleanup".into(),
            removed: Vec::new(),
            failed: Vec::new(),
            demo: false,
        };
        read_cleanup(
            &mut report,
            "banner\nremoved\t~/.muxen\nfailed\tservice: access denied\ncomplete\n",
        );
        assert_eq!(report.removed, ["~/.muxen"]);
        assert_eq!(report.failed, ["service: access denied"]);
        assert!(
            report
                .text()
                .contains("Could not remove: service: access denied")
        );
        read_cleanup(&mut report, "removed\tlocal pairing\n");
        assert!(report.failed.last().unwrap().contains("did not confirm"));
    }

    #[tokio::test]
    async fn demo_uninstall_keeps_the_machine_and_does_not_touch_pairing_files() {
        let dir =
            std::env::temp_dir().join(format!("muxin-agent-uninstall-demo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("direct.json"), b"pairing must stay").unwrap();
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), true);
        let before = engine.current().machines.len();
        let report = agent_uninstall(
            &engine,
            MachineRequest {
                machine_id: "Forge".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.machine_id, "forge");
        assert!(report.demo && report.failed.is_empty());
        assert_eq!(engine.current().machines.len(), before);
        assert_eq!(
            std::fs::read(dir.join("direct.json")).unwrap(),
            b"pairing must stay"
        );
        let error = agent_uninstall(
            &engine,
            MachineRequest {
                machine_id: "no-such-machine".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "not_found");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    struct Machine {
        dir: PathBuf,
    }

    impl Machine {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("muxin-uninstall-{}-{sequence}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            let me = Self { dir };
            me.write(".muxen/bin/muxen-agent", b"agent");
            me.write("tools/pgrep", b"#!/bin/sh\nexit 1\n");
            me.write("tools/pkill", b"#!/bin/sh\nexit 1\n");
            me.write("tools/launchctl", b"#!/bin/sh\nexit 1\n");
            me.write("tools/systemctl", b"#!/bin/sh\nexit 0\n");
            me.write("tools/crontab", b"#!/bin/sh\nif [ \"$1\" = '-l' ]; then cat \"$MUXEN_TEST_CRONTAB\"; else cat > \"$MUXEN_TEST_CRONTAB\"; fi\n");
            me
        }

        fn write(&self, name: &str, data: &[u8]) {
            let path = self.dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn run(&self, os: Os) -> AgentUninstall {
            // Keep the real home out of the fixture, including its child shell.
            let script = uninstall_script(os).replace("$HOME", "$MUXEN_TEST_HOME");
            let mut child = Command::new("/bin/sh")
                .env("MUXEN_TEST_HOME", &self.dir)
                .env("MUXEN_TEST_CRONTAB", self.dir.join("crontab"))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.dir.join("tools").display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let result = child.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let mut report = AgentUninstall {
                machine_id: "test".into(),
                summary: "Cleanup".into(),
                removed: Vec::new(),
                failed: Vec::new(),
                demo: false,
            };
            read_cleanup(&mut report, &String::from_utf8_lossy(&result.stdout));
            report
        }
    }

    impl Drop for Machine {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn mac_cleanup_leaves_other_launch_agents_and_files_alone() {
        let machine = Machine::new();
        machine.write(
            "Library/LaunchAgents/com.lumarylabs.muxen-agent.plist",
            b"agent",
        );
        machine.write(
            "Library/LaunchAgents/com.lumarylabs.muxen.schedule.s-abc123.plist",
            b"schedule",
        );
        machine.write("Library/LaunchAgents/com.example.important.plist", b"keep");
        machine.write("config.toml", b"keep the machine");
        let report = machine.run(Os::Mac);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(!machine.dir.join(".muxen").exists());
        assert!(
            !machine
                .dir
                .join("Library/LaunchAgents/com.lumarylabs.muxen-agent.plist")
                .exists()
        );
        assert!(
            !machine
                .dir
                .join("Library/LaunchAgents/com.lumarylabs.muxen.schedule.s-abc123.plist")
                .exists()
        );
        assert_eq!(
            std::fs::read(
                machine
                    .dir
                    .join("Library/LaunchAgents/com.example.important.plist")
            )
            .unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read(machine.dir.join("config.toml")).unwrap(),
            b"keep the machine"
        );
    }

    #[test]
    fn linux_cleanup_removes_its_units_and_only_its_crontab_block() {
        let machine = Machine::new();
        machine.write(".config/systemd/user/muxen-agent.service", b"agent");
        machine.write(".config/systemd/user/important.service", b"keep");
        std::fs::create_dir_all(
            machine
                .dir
                .join(".config/systemd/user/default.target.wants"),
        )
        .unwrap();
        symlink(
            "../muxen-agent.service",
            machine
                .dir
                .join(".config/systemd/user/default.target.wants/muxen-agent.service"),
        )
        .unwrap();
        machine.write("crontab", b"# my jobs\n0 7 * * * backup\n# muxen schedules: managed by Muxen (edits between these lines are replaced)\n0 8 * * * agent schedule-run s-abc123\n# end of muxen schedules\n0 9 * * * report\n");
        let report = machine.run(Os::Linux);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(!machine.dir.join(".muxen").exists());
        assert!(
            !machine
                .dir
                .join(".config/systemd/user/muxen-agent.service")
                .exists()
        );
        assert!(
            std::fs::symlink_metadata(
                machine
                    .dir
                    .join(".config/systemd/user/default.target.wants/muxen-agent.service")
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(machine.dir.join(".config/systemd/user/important.service")).unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read_to_string(machine.dir.join("crontab")).unwrap(),
            "# my jobs\n0 7 * * * backup\n0 9 * * * report\n"
        );
    }

    #[test]
    fn cleanup_reports_a_failed_stop_and_still_removes_its_files() {
        let machine = Machine::new();
        machine.write("tools/systemctl", b"#!/bin/sh\ncase \"$*\" in *'disable --now'*) echo 'access denied' >&2; exit 1;; esac\n");
        machine.write(".config/systemd/user/muxen-agent.service", b"agent");
        machine.write("crontab", b"# no schedules\n");
        let report = machine.run(Os::Linux);
        assert!(
            report.failed.iter().any(|f| f.contains("access denied")),
            "{:?}",
            report.failed
        );
        assert!(report.removed.iter().any(|f| f.ends_with("/.muxen")));
        assert!(!machine.dir.join(".muxen").exists());
    }

    #[test]
    fn cron_warnings_are_never_written_into_the_replacement_crontab() {
        let machine = Machine::new();
        machine.write("tools/crontab", b"#!/bin/sh\nif [ \"$1\" = '-l' ]; then echo warning >&2; cat \"$MUXEN_TEST_CRONTAB\"; else cat > \"$MUXEN_TEST_CRONTAB\"; fi\n");
        machine.write("crontab", b"# keep\n# muxen schedules: managed by Muxen (edits between these lines are replaced)\njob\n# end of muxen schedules\n");
        assert!(machine.run(Os::Linux).failed.is_empty());
        assert_eq!(
            std::fs::read_to_string(machine.dir.join("crontab")).unwrap(),
            "# keep\n"
        );
    }

    #[test]
    fn a_missing_crontab_is_not_a_cleanup_failure() {
        let machine = Machine::new();
        machine.write("tools/crontab", b"#!/bin/sh\nif [ \"$1\" = '-l' ]; then echo 'no crontab for test-user' >&2; exit 1; else echo overwritten > \"$MUXEN_TEST_CRONTAB\"; fi\n");
        machine.write("crontab", b"unchanged\n");
        assert!(machine.run(Os::Linux).failed.is_empty());
        assert_eq!(
            std::fs::read_to_string(machine.dir.join("crontab")).unwrap(),
            "unchanged\n"
        );
    }

    #[test]
    fn a_crontab_read_error_is_reported_without_replacing_it() {
        let machine = Machine::new();
        machine.write("tools/crontab", b"#!/bin/sh\nif [ \"$1\" = '-l' ]; then echo 'crontab: permission denied' >&2; exit 1; else echo overwritten > \"$MUXEN_TEST_CRONTAB\"; fi\n");
        machine.write("crontab", b"unchanged\n");
        assert!(
            machine.run(Os::Linux).failed.iter().any(|f| {
                f.contains("Could not read crontab") && f.contains("permission denied")
            })
        );
        assert_eq!(
            std::fs::read_to_string(machine.dir.join("crontab")).unwrap(),
            "unchanged\n"
        );
    }
}
