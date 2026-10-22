//! Handing a session's work to another agent (Claude Code to Codex, say):
//! Muxin's agent on the machine writes a note in its folder from the
//! conversation so far and where the git work stands (agent/src/handoff.rs),
//! then the new agent starts there, asked to read it first, and the old one
//! ends. A box keeps its container and checkout and changes agents.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::{Control, SessionInfo, StartSession, agent, default_title};
use crate::fleet::Engine;
use crate::fleet::boxes::BoxInfo;

/// Where the note goes, in the session's folder (kept out of git).
pub const NOTE: &str = ".muxin/handoff.md";

/// A handoff, done.
#[derive(Debug, Clone, Serialize)]
pub struct HandedOff {
    /// The new agent's session.
    pub session: SessionInfo,
    /// The note, on the machine.
    pub note: String,
    /// What the note was made from: "transcript" (the old agent's own),
    /// "terminal" (what its terminal showed) or "none" (only the git state).
    pub source: String,
    /// The old session, when it was kept running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
    /// The box, when it was a box's agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub box_id: Option<String>,
}

/// What the new agent is asked first.
pub fn prompt(from: &str) -> String {
    format!("You're taking over this work from {from}. First read {NOTE}: Muxin wrote it from {from}'s conversation, with where the code stands. Then carry on with what's left.")
}

impl Engine {
    /// Hands session `id`'s work (or box `id`'s) to agent `to`: writes the
    /// note, starts `to` in the same folder asked to read it, and ends the old
    /// session unless `keep`. A box's agent changes in the box.
    pub async fn handoff_session(self: &Arc<Self>, id: &str, to: &str, keep: bool) -> Result<HandedOff, String> {
        if self.demo_world().is_some() {
            return Err("The demo's sessions are only for show.".into());
        }
        let kind = super::agent(to).filter(|k| !k.command.is_empty()).ok_or_else(|| {
            let ids: Vec<&str> = super::AGENTS.iter().filter(|a| !a.command.is_empty()).map(|a| a.id).collect();
            format!("Hand off to which agent? Use one of: {}.", ids.join(", "))
        })?;
        if let Ok(b) = self.find_box(id) {
            return self.box_handoff(&b, kind.id).await;
        }
        let s = self.current().sessions.iter().find(|s| s.id == id).cloned().ok_or_else(|| format!("No session called {id}."))?;
        if let Some(b) = s.box_id.as_deref() {
            if b != s.name {
                return Err(format!("{} is a shell in box {b}: hand off the box's agent instead.", s.title));
            }
            let b = self.find_box(b)?;
            return self.box_handoff(&b, kind.id).await;
        }
        let from = super::agent(&s.agent).filter(|k| !k.command.is_empty()).ok_or_else(|| format!("{} is a shell: only an agent's work can be handed off.", s.title))?;
        if !s.persistent {
            return Err(format!("{} runs only in the Muxin window, so its conversation can't be read from here.", s.title));
        }

        // The note, from the transcript (or the terminal, for agents whose transcript Muxin can't read).
        let (spec, probe) = self.machine(&s.machine_id).ok_or_else(|| format!("No machine called {}.", s.machine_id))?;
        agent::ensure(&spec, probe.as_ref()).await?;
        let terminal = self.control_session(&s.id, Control::Read { lines: 400 }).await.unwrap_or_default();
        let note = agent::ctl(
            &spec,
            &json!({
                "op": "handoff",
                "agent": from.id,
                "cwd": s.cwd,
                "since": s.created_at.map(|t| t.timestamp()).unwrap_or(0),
                "from": from.name,
                "to": kind.name,
                "base": s.git.as_ref().and_then(|g| g.base.clone()),
                "terminal": terminal,
            }),
        )
        .await?;

        // The new agent first, so a failure leaves the old one as it was.
        let title = if s.title == default_title(from, &s.cwd) { default_title(kind, &s.cwd) } else { s.title.clone() };
        let started = self
            .start_session(StartSession {
                machine_id: s.machine_id.clone(),
                agent: kind.id.into(),
                cwd: s.cwd.clone(),
                title: Some(title.clone()),
                prompt: Some(prompt(from.name)),
                cols: 120,
                rows: 40,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("The note is in {}, but {e}", s.cwd.trim_end_matches('/').to_string() + "/" + NOTE))?;
        let mut session = started;
        if !keep {
            self.control_session(&s.id, Control::Stop).await?;
            // It took a "… 2" title while the old one was still there.
            if session.title != title
                && let Ok(renamed) = self.rename_session(&session.id, &title).await
            {
                session = renamed;
            }
        }
        Ok(HandedOff { session, note: note_path(&note), source: source(&note), kept: keep.then(|| s.id.clone()), box_id: None })
    }

    /// A box changes agents: the note in its checkout, its agent's session
    /// ends, and the new agent starts in the box on it.
    async fn box_handoff(self: &Arc<Self>, b: &BoxInfo, to: &str) -> Result<HandedOff, String> {
        if !crate::fleet::boxes::BOX_AGENTS.contains(&to) {
            return Err(format!("A box can't run {to}."));
        }
        let from = super::agent(&b.agent).map(|k| k.name).unwrap_or(&b.agent);
        let to_name = super::agent(to).map(|k| k.name).unwrap_or(to);
        let (spec, probe) = self.machine(&b.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", b.machine_id))?;
        agent::ensure(&spec, probe.as_ref()).await?;
        let terminal = match &b.session_id {
            Some(sid) => self.control_session(sid, Control::Read { lines: 400 }).await.unwrap_or_default(),
            None => String::new(),
        };
        // The box's home: next to its checkout (its agents' transcripts that aren't the machine's).
        let home = std::path::Path::new(&b.path).parent().map(|p| p.join("home").to_string_lossy().to_string());
        let note = agent::ctl(
            &spec,
            &json!({
                "op": "handoff",
                "agent": b.agent,
                "cwd": b.path,
                "since": b.created_at.map(|t| t.timestamp()).unwrap_or(0),
                "homes": home.into_iter().collect::<Vec<_>>(),
                "from": from,
                "to": to_name,
                "base": b.git.as_ref().and_then(|g| g.base.clone()),
                "terminal": terminal,
            }),
        )
        .await?;
        let (start, typed) = crate::fleet::boxes::launcher(to).ok_or_else(|| format!("A box can't run {to}."))?;
        agent::ctl(&spec, &json!({ "op": "box_switch", "box_id": b.id, "agent": to, "start": start, "type_prompt": typed })).await?;
        agent::ctl(&spec, &json!({ "op": "box_resume", "box_id": b.id, "prompt": prompt(from) })).await?;
        let found = self.refresh_sessions(&spec.id).await?;
        let session = found.sessions.into_iter().find(|s| s.name == b.id).ok_or_else(|| format!("{to_name} didn't start in {}.", b.id))?;
        Ok(HandedOff { session, note: note_path(&note), source: source(&note), kept: None, box_id: Some(b.id.clone()) })
    }
}

fn note_path(reply: &Value) -> String {
    reply["path"].as_str().unwrap_or(NOTE).to_string()
}

fn source(reply: &Value) -> String {
    reply["source"].as_str().unwrap_or("none").to_string()
}
