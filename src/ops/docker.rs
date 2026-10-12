//! Docker work that takes a while (pulling, running, Compose up/down):
//! operations on the Activity page, their output streamed as they go.

use std::sync::Arc;
use std::time::Duration;

use super::remote::{self, sh_quote};
use crate::fleet::Engine;
use crate::fleet::services::{ComposeProject, DockerRun};
use crate::probe::Shell;

/// Where Docker and Homebrew put their tools (a plain `sh -s` over ssh lacks them on a Mac).
const PATH: &str = "export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin:/home/linuxbrew/.linuxbrew/bin:/snap/bin\"\n";

impl Engine {
    /// `docker run -d` with what the user filled in. The operation's id.
    pub fn docker_run(self: &Arc<Self>, machine_id: &str, run: DockerRun) -> Result<u64, String> {
        let image = run.image.trim();
        if image.is_empty() || image.starts_with('-') {
            return Err("Which image?".into());
        }
        let mut args = vec!["docker run -d".to_string()];
        if !run.name.trim().is_empty() {
            args.push(format!("--name {}", sh_quote(run.name.trim())));
        }
        for p in run.ports.iter().filter(|p| !p.trim().is_empty()) {
            args.push(format!("-p {}", sh_quote(p.trim())));
        }
        for e in run.env.iter().filter(|e| !e.trim().is_empty()) {
            args.push(format!("-e {}", sh_quote(e.trim())));
        }
        for v in run.volumes.iter().filter(|v| !v.trim().is_empty()) {
            args.push(format!("-v {}", sh_quote(v.trim())));
        }
        if !run.restart.is_empty() && run.restart != "no" {
            args.push(format!("--restart {}", sh_quote(&run.restart)));
        }
        args.push(sh_quote(image));
        // Its own command, as typed (the user's machine, the user's words).
        if !run.command.trim().is_empty() {
            args.push(run.command.trim().to_string());
        }
        let script = format!("{PATH}set -e\n{}\n", args.join(" "));
        self.docker_op(machine_id, &format!("Run {image}"), script)
    }

    /// `docker pull`.
    pub fn docker_pull(self: &Arc<Self>, machine_id: &str, image: &str) -> Result<u64, String> {
        let image = image.trim();
        if image.is_empty() || image.starts_with('-') {
            return Err("Which image?".into());
        }
        let script = format!("{PATH}set -e\ndocker pull {}\n", sh_quote(image));
        self.docker_op(machine_id, &format!("Pull {image}"), script)
    }

    /// up (detached), down, restart or pull for a Compose project.
    pub fn compose_action(self: &Arc<Self>, machine_id: &str, project: &ComposeProject, action: &str) -> Result<u64, String> {
        let verb = match action {
            "up" => "up -d",
            "down" => "down",
            "restart" => "restart",
            "pull" => "pull",
            other => return Err(format!("Can't {other} a Compose project.")),
        };
        let files: Vec<String> = project.files.split(',').map(str::trim).filter(|f| !f.is_empty()).map(|f| format!("-f {}", sh_quote(f))).collect();
        if files.is_empty() {
            return Err(format!("Muxen doesn't know where {}'s compose file is.", project.name));
        }
        let script = format!("{PATH}set -e\ndocker compose -p {} {} {verb}\n", sh_quote(&project.name), files.join(" "));
        let title = format!("{} {}", match action {
            "up" => "Up",
            "down" => "Down",
            "restart" => "Restart",
            _ => "Pull",
        }, project.name);
        self.docker_op(machine_id, &title, script)
    }

    fn docker_op(self: &Arc<Self>, machine_id: &str, title: &str, script: String) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Err("The demo doesn't run Docker.".into());
        }
        let op = self.begin_op("docker", format!("{title} on {}", spec.name), Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = remote::run(&me, op, &spec, Shell::Sh, script, &[], Duration::from_secs(30 * 60)).await;
            if let Err(e) = &result {
                me.op_log(op, format!("error: {e}"));
            }
            me.op_finish(op, result);
        });
        Ok(op)
    }
}
