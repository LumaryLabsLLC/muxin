//! Schedules: a command or an agent session that runs on one of the user's
//! machines at set times. Each one lives on its machine, kept and run there
//! by Muxen's agent through the machine's own scheduler (agent/src/schedules.rs),
//! so it runs whether or not this app is open.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Engine;
use super::sessions::agent as agent_cli;

/// When a schedule runs, in the machine's own time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct When {
    /// "minutes" | "hours" | "day" | "weekdays" | "week"
    pub every: String,
    /// For "minutes" and "hours": every n of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// "HH:MM" ("hours": its minute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// For "week": "mon", "tue", ...
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
}

/// What a schedule runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct What {
    /// "command" | "agent"
    pub kind: String,
    /// A shell command (kind "command"), run in the user's login shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The folder it runs in ("~" by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The agent CLI (kind "agent"): claude, codex, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// What the agent is asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The session's command line and the text typed in once it's up
    /// (worked out by Muxen; the agent runs them as given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send: Option<String>,
}

/// One run of a schedule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    /// Unix seconds.
    pub started: i64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    /// The agent session it started (kind "agent").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// "s-" and six letters or digits.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub when: When,
    pub what: What,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Run>,
    pub running: bool,
    /// `when` in words ("Weekdays at 9:00"), in the machine's own time.
    pub when_text: String,
}

/// A machine's schedules, or why they couldn't be read.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MachineSchedules {
    pub machine_id: String,
    pub machine: String,
    pub schedules: Vec<Schedule>,
    /// What runs them there: "cron", "launchd" or "task-scheduler".
    pub scheduler: Option<String>,
    /// Why schedules can't run there (no cron, say).
    pub problem: Option<String>,
    /// It couldn't be asked (offline, say).
    pub error: Option<String>,
}

/// A schedule's runs and one run's output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleLog {
    pub runs: Vec<Run>,
    pub run: Option<Run>,
    pub output: String,
}

/// What a new or changed schedule is.
#[derive(Debug, Clone, Default)]
pub struct ScheduleInput {
    /// Set to change an existing one.
    pub id: Option<String>,
    pub name: String,
    pub when: When,
    /// A shell command; or `agent` and `prompt` for an agent session.
    pub command: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    /// The folder it runs in (default home).
    pub folder: Option<String>,
    pub enabled: bool,
}

/// "Every 15 minutes", "Every day at 9:00", "Weekdays at 18:30", "Mon, Thu at 7:00".
pub fn when_text(w: &When) -> String {
    let at = w.at.as_deref().map(|a| a.trim_start_matches('0').to_string()).map(|a| if a.starts_with(':') { format!("0{a}") } else { a }).unwrap_or_default();
    let n = w.n.unwrap_or(1);
    match w.every.as_str() {
        "minutes" if n == 1 => "Every minute".into(),
        "minutes" => format!("Every {n} minutes"),
        "hours" => {
            let minute = w.at.as_deref().and_then(|a| a.split(':').nth(1)).unwrap_or("00");
            if n == 1 { format!("Every hour at :{minute}") } else { format!("Every {n} hours at :{minute}") }
        }
        "day" => format!("Every day at {at}"),
        "weekdays" => format!("Weekdays at {at}"),
        "week" => {
            let days: Vec<String> = w.days.iter().map(|d| {
                let mut c = d.chars();
                c.next().map(|f| f.to_ascii_uppercase().to_string() + &c.take(2).collect::<String>()).unwrap_or_default()
            }).collect();
            format!("{} at {at}", days.join(", "))
        }
        other => other.to_string(),
    }
}

fn new_id() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 6];
    let _ = SystemRandom::new().fill(&mut bytes);
    format!("s-{}", bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect::<String>())
}

impl Engine {
    /// The schedules on each machine (or one), asked of them all at once.
    pub async fn schedules(self: &Arc<Self>, machine_id: Option<&str>) -> Vec<MachineSchedules> {
        let machines: Vec<(String, String)> = self
            .current()
            .machines
            .iter()
            .filter(|m| !m.host.is_empty() && m.status == "online")
            .filter(|m| machine_id.is_none_or(|id| m.id == id))
            .map(|m| (m.id.clone(), m.name.clone()))
            .collect();
        let asks = machines.into_iter().map(|(id, name)| {
            let me = self.clone();
            async move {
                let mut out = MachineSchedules { machine_id: id.clone(), machine: name, ..Default::default() };
                match Box::pin(me.schedule_ctl(&id, json!({ "op": "schedules" }))).await {
                    Ok(reply) => {
                        out.schedules = serde_json::from_value(reply["schedules"].clone()).unwrap_or_default();
                        for s in &mut out.schedules {
                            s.when_text = when_text(&s.when);
                        }
                        out.scheduler = reply["scheduler"].as_str().map(str::to_string);
                        out.problem = reply["problem"].as_str().map(str::to_string);
                    }
                    Err(e) => out.error = Some(e),
                }
                out
            }
        });
        futures::future::join_all(asks).await
    }

    /// Saves a schedule on a machine (new, or changed when `id` is set).
    pub async fn schedule_save(self: &Arc<Self>, machine_id: &str, input: ScheduleInput) -> Result<Schedule, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let name = input.name.trim().to_string();
        if name.is_empty() {
            return Err("Give the schedule a name.".into());
        }
        let cwd = input.folder.clone().filter(|f| !f.trim().is_empty()).unwrap_or_else(|| "~".into());
        let what = match (input.command.as_deref().map(str::trim).filter(|c| !c.is_empty()), input.agent.as_deref().filter(|a| !a.is_empty())) {
            (Some(c), None) => What { kind: "command".into(), command: Some(c.to_string()), cwd: Some(cwd), ..Default::default() },
            (None, Some(a)) => {
                let kind = super::sessions::agent(a).ok_or_else(|| format!("Unknown agent \"{a}\". Use one of: {}.", super::sessions::AGENTS.iter().map(|k| k.id).collect::<Vec<_>>().join(", ")))?;
                let prompt = input.prompt.clone().map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
                let windows = matches!(crate::ops::remote::shell_for(&spec, probe.as_ref()), crate::probe::Shell::Powershell);
                let (run, send) = super::sessions::launch_line(kind, prompt.as_deref(), windows);
                What { kind: "agent".into(), cwd: Some(cwd), agent: Some(kind.id.into()), prompt, title: Some(name.clone()), run: Some(run), send, ..Default::default() }
            }
            (Some(_), Some(_)) => return Err("A schedule runs a command or starts an agent, not both.".into()),
            (None, None) => return Err("Say what it runs: a command, or an agent with a prompt.".into()),
        };
        let schedule = Schedule { id: input.id.clone().unwrap_or_else(new_id), name, enabled: input.enabled, when: input.when, what, ..Default::default() };
        let reply = Box::pin(self.schedule_ctl(&spec.id, json!({ "op": "schedule_save", "schedule": schedule }))).await?;
        let mut saved: Schedule = serde_json::from_value(reply["schedule"].clone()).unwrap_or(schedule);
        saved.when_text = when_text(&saved.when);
        Ok(saved)
    }

    pub async fn schedule_delete(self: &Arc<Self>, machine_id: &str, id: &str) -> Result<(), String> {
        Box::pin(self.schedule_ctl(machine_id, json!({ "op": "schedule_delete", "schedule_id": id }))).await.map(|_| ())
    }

    /// Runs one now (in the background there).
    pub async fn schedule_run(self: &Arc<Self>, machine_id: &str, id: &str) -> Result<(), String> {
        Box::pin(self.schedule_ctl(machine_id, json!({ "op": "schedule_run", "schedule_id": id }))).await.map(|_| ())
    }

    /// A schedule's runs, and one run's output (the newest, or the one that `started` then).
    pub async fn schedule_log(self: &Arc<Self>, machine_id: &str, id: &str, started: Option<i64>) -> Result<ScheduleLog, String> {
        let reply = Box::pin(self.schedule_ctl(machine_id, json!({ "op": "schedule_log", "schedule_id": id, "started": started }))).await?;
        serde_json::from_value(reply).map_err(|e| format!("The machine answered in a way Muxen can't read: {e}"))
    }

    async fn schedule_ctl(self: &Arc<Self>, machine_id: &str, request: Value) -> Result<Value, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(json!({ "ok": true, "schedules": [], "scheduler": "cron", "runs": [], "output": "" }));
        }
        agent_cli::ensure(&spec, probe.as_ref()).await?;
        agent_cli::ctl(&spec, &request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_when_in_words() {
        let w = |every: &str, n: Option<u32>, at: Option<&str>, days: &[&str]| when_text(&When { every: every.into(), n, at: at.map(Into::into), days: days.iter().map(|d| d.to_string()).collect() });
        assert_eq!(w("minutes", Some(15), None, &[]), "Every 15 minutes");
        assert_eq!(w("hours", Some(1), Some("00:30"), &[]), "Every hour at :30");
        assert_eq!(w("day", None, Some("09:00"), &[]), "Every day at 9:00");
        assert_eq!(w("day", None, Some("00:15"), &[]), "Every day at 0:15");
        assert_eq!(w("weekdays", None, Some("18:30"), &[]), "Weekdays at 18:30");
        assert_eq!(w("week", None, Some("07:00"), &["mon", "thu"]), "Mon, Thu at 7:00");
    }

    #[test]
    fn ids_look_right() {
        let id = new_id();
        assert!(id.starts_with("s-") && id.len() == 8);
    }
}
