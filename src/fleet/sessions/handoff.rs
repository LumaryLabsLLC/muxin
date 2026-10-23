//! Handing a session's work to another agent (Claude Code to Codex, say):
//! Muxin's agent on the machine writes a note in its folder from the
//! conversation so far and where the git work stands (agent/src/handoff.rs),
//! then the new agent starts there, asked to read it first, and the old one
//! ends. A box keeps its container and checkout and changes agents.
//!
//! The work can move to another machine too (agent/src/carry.rs): its commits
//! that aren't on origin, uncommitted changes and new files are packed, copied
//! across through Muxin, and put in a workspace there (a worktree of a
//! checkout of the repository, else a clone) that starts where the work did.
//! The folder it left stays as it was.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::Serialize;
use serde_json::{Value, json};

use super::{AgentKind, Control, SessionInfo, StartSession, agent, default_title};
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
    /// The workspace the work moved to, on another machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// What didn't go quite as planned in a move (changes that didn't apply cleanly, ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What the new agent is asked first.
pub fn prompt(from: &str) -> String {
    format!("You're taking over this work from {from}. First read {NOTE}: Muxin wrote it from {from}'s conversation, with where the code stands. Then carry on with what's left.")
}

impl Engine {
    /// Hands session `id`'s work (or box `id`'s) to agent `to`: writes the
    /// note, starts `to` in the same folder asked to read it, and ends the old
    /// session unless `keep`. A box's agent changes in the box. With
    /// `machine` (another one), the work moves there first.
    pub async fn handoff_session(self: &Arc<Self>, id: &str, to: &str, keep: bool, machine: Option<&str>) -> Result<HandedOff, String> {
        let kind = super::agent(to).filter(|k| !k.command.is_empty()).ok_or_else(|| {
            let ids: Vec<&str> = super::AGENTS.iter().filter(|a| !a.command.is_empty()).map(|a| a.id).collect();
            format!("Hand off to which agent? Use one of: {}.", ids.join(", "))
        })?;
        if self.demo_world().is_some() {
            return self.demo_handoff(id, kind, keep, machine).await;
        }
        let snap = self.current();
        let s = snap.sessions.iter().find(|s| s.id == id).cloned();
        let here = self.find_box(id).map(|b| b.machine_id).ok().or_else(|| s.as_ref().map(|s| s.machine_id.clone()));
        let target = match machine.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => Some(snap.machines.iter().find(|x| x.id == m || x.name.eq_ignore_ascii_case(m)).map(|x| x.id.clone()).ok_or_else(|| format!("No machine called {m}."))?),
            None => None,
        }
        .filter(|t| Some(t) != here.as_ref());
        let in_box = self.find_box(id).is_ok() || s.as_ref().is_some_and(|s| s.box_id.is_some());
        if target.is_some() && in_box {
            return Err("A box stays on its machine: hand it to another agent there, or start a new box on the other machine.".into());
        }
        if let Ok(b) = self.find_box(id) {
            return self.box_handoff(&b, kind.id).await;
        }
        let s = s.ok_or_else(|| format!("No session called {id}."))?;
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
        if let Some(t) = target {
            return self.move_session(&s, from, kind, &t, keep).await;
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
        Ok(HandedOff { session, note: note_path(&note), source: source(&note), kept: keep.then(|| s.id.clone()), box_id: None, workspace: None, warnings: Vec::new() })
    }

    /// The demo's handoff: the new agent's session starts (in a workspace on
    /// the other machine for a move) and the old one ends, with no note to write.
    async fn demo_handoff(self: &Arc<Self>, id: &str, kind: &'static AgentKind, keep: bool, machine: Option<&str>) -> Result<HandedOff, String> {
        let snap = self.current();
        let s = snap.sessions.iter().find(|s| s.id == id).cloned().ok_or_else(|| format!("No session called {id}."))?;
        if s.box_id.is_some() {
            return Err("The demo's boxes are only for show.".into());
        }
        let from = super::agent(&s.agent).filter(|k| !k.command.is_empty()).ok_or_else(|| format!("{} is a shell: only an agent's work can be handed off.", s.title))?;
        let target = match machine.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => snap.machines.iter().find(|x| x.id == m || x.name.eq_ignore_ascii_case(m)).map(|x| x.id.clone()).ok_or_else(|| format!("No machine called {m}."))?,
            None => s.machine_id.clone(),
        };
        let moved = target != s.machine_id;
        let workspace = moved.then(|| format!("ws-{}", super::super::workspaces::random_id(6)));
        let cwd = match &workspace {
            Some(ws) => format!("~/muxen/workspaces/{ws}"),
            None => s.cwd.clone(),
        };
        let title = if s.title == default_title(from, &s.cwd) { default_title(kind, &s.cwd) } else { s.title.clone() };
        let session = self.start_session(StartSession { machine_id: target, agent: kind.id.into(), cwd: cwd.clone(), title: Some(title), ..Default::default() }).await?;
        if !keep {
            self.control_session(&s.id, Control::Stop).await?;
        }
        Ok(HandedOff { session, note: format!("{cwd}/{NOTE}"), source: "transcript".into(), kept: keep.then(|| s.id.clone()), box_id: None, workspace, warnings: Vec::new() })
    }

    /// Moves session `s`'s work to machine `target` and hands it to `kind`
    /// there, as an operation (its steps show in Activity).
    async fn move_session(self: &Arc<Self>, s: &SessionInfo, from: &'static AgentKind, kind: &'static AgentKind, target: &str, keep: bool) -> Result<HandedOff, String> {
        let m = self.current().machines.iter().find(|m| m.id == target).cloned().ok_or_else(|| format!("No machine called {target}."))?;
        if m.platform.starts_with("windows") {
            return Err(format!("Work can't move to a Windows machine ({}) yet.", m.name));
        }
        if m.is_down() {
            return Err(format!("{} isn't reachable right now.", m.name));
        }
        // The agent's CLI there (looked up first when Muxin hasn't yet), before anything's copied.
        if self.sessions_of(&m.id).is_none_or(|f| f.agents_checked_at.is_none()) {
            let _ = self.refresh_tools(&m.id).await;
        }
        if self.sessions_of(&m.id).and_then(|f| f.agents_checked_at.map(|_| f.agents)).is_some_and(|a| !a.iter().any(|x| x == kind.id)) {
            return Err(format!("{} isn't installed on {} (or isn't on its PATH there).", kind.name, m.name));
        }
        let (src, src_probe) = self.machine(&s.machine_id).ok_or_else(|| format!("No machine called {}.", s.machine_id))?;
        let (dst, dst_probe) = self.machine(&m.id).ok_or_else(|| format!("No machine called {}.", m.id))?;
        let op = self.begin_op("handoff", format!("Move {} from {} to {} ({})", s.title, src.name, dst.name, kind.name), Some(dst.id.clone()), None);
        let pack_id = format!("mv-{}", super::super::workspaces::random_id(8));
        let mut made: Option<String> = None;
        let result = async {
            agent::ensure(&src, src_probe.as_ref()).await?;
            agent::ensure(&dst, dst_probe.as_ref()).await?;
            self.op_log(op, format!("▸ Writing the note from {}'s conversation", from.name));
            let terminal = self.control_session(&s.id, Control::Read { lines: 400 }).await.unwrap_or_default();
            let note = agent::ctl(
                &src,
                &json!({
                    "op": "handoff",
                    "agent": from.id,
                    "cwd": s.cwd,
                    "since": s.created_at.map(|t| t.timestamp()).unwrap_or(0),
                    "from": from.name,
                    "to": kind.name,
                    "machine": src.name,
                    "base": s.git.as_ref().and_then(|g| g.base.clone()),
                    "terminal": terminal,
                    "write": false,
                }),
            )
            .await?;

            self.op_log(op, format!("▸ Packing the work in {}", s.cwd));
            let pack = agent::ctl(&src, &json!({ "op": "carry_pack", "pack_id": pack_id, "cwd": s.cwd })).await?;
            let (unpushed, uncommitted) = (pack["unpushed"].as_u64().unwrap_or(0), pack["uncommitted"].as_u64().unwrap_or(0));
            self.op_log(
                op,
                format!(
                    "✓ {} not on origin, {} uncommitted ({})",
                    plural(unpushed, "commit"),
                    plural(uncommitted, "file"),
                    crate::util::bytes(pack["bytes"].as_u64().unwrap_or(0))
                ),
            );
            let origin = pack["origin"].as_str().map(String::from);
            let base = pack["base"].as_str().map(String::from);
            let gh_host = super::super::workspaces::github_web_host(&self.settings().github.api_url);
            let github = s.git.as_ref().and_then(|g| g.repo.clone()).or_else(|| origin.as_deref().and_then(|o| crate::fleet::boxes::github_name(o, &gh_host)));
            let branch = branch_there(pack["branch"].as_str(), s.git.as_ref().and_then(|g| g.base.as_deref()), origin.is_some(), &s.name);

            // A worktree of a checkout of the repository there (looked at now: one may
            // have been cloned since), else a clone, else a new repository.
            let _ = self.refresh_checkouts(&dst.id).await;
            // The user's own checkouts, not folders Muxin made (workspaces, boxes).
            let made_by_muxin = |p: &str| p.contains("/muxen/workspaces/") || p.contains("/muxen/boxes/");
            let checkout = github
                .as_ref()
                .and_then(|g| self.checkouts_of(&dst.id).into_iter().find(|c| !made_by_muxin(&c.path) && c.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(g))));
            let mut req = super::super::workspaces::CreateWorkspace {
                machine: Some(dst.id.clone()),
                name: Some(s.title.clone()),
                owner: Some("handoff".into()),
                ..Default::default()
            };
            match (&checkout, &origin) {
                (Some(c), _) => {
                    self.op_log(op, format!("▸ Making a worktree of {} on {}, on {branch}", c.path, dst.name));
                    req.worktree_of = Some(c.path.clone());
                    req.git_ref = base.clone();
                    req.branch = Some(branch.clone());
                }
                (None, Some(o)) => {
                    let repo = github.clone().unwrap_or_else(|| o.clone());
                    self.op_log(op, format!("▸ Cloning {repo} on {}, on {branch}", dst.name));
                    req.repo = Some(repo);
                    req.git_ref = base.clone();
                    req.branch = Some(branch.clone());
                }
                (None, None) => self.op_log(op, format!("▸ Making a new repository for it on {}", dst.name)),
            }
            let ws = self.create_workspace(req).await?;
            made = Some(ws.id.clone());

            self.op_log(op, format!("▸ Copying the work to {}", dst.name));
            let cancel = AtomicBool::new(false);
            self.run_copy(op, &src, "~/.muxen/carry", std::slice::from_ref(&pack_id), &dst, "~/.muxen/carry", &cancel, false).await?;
            let _ = agent::ctl(&src, &json!({ "op": "carry_drop", "pack_id": pack_id })).await;
            let unpacked = agent::ctl(
                &dst,
                &json!({ "op": "carry_unpack", "pack_id": pack_id, "dir": ws.path, "branch": branch, "base": base, "note": note["text"] }),
            )
            .await?;
            let warnings: Vec<String> = unpacked["warnings"].as_array().into_iter().flatten().filter_map(|w| w.as_str().map(String::from)).collect();
            for w in &warnings {
                self.op_log(op, format!("! {w}"));
            }
            self.op_log(op, format!("✓ The work is in {} on {}", ws.path, unpacked["branch"].as_str().unwrap_or(&branch)));

            let sub = pack["sub"].as_str().unwrap_or("").trim_matches('/');
            let cwd = if sub.is_empty() { ws.path.clone() } else { format!("{}/{sub}", ws.path.trim_end_matches('/')) };
            let title = if s.title == default_title(from, &s.cwd) { default_title(kind, &s.cwd) } else { s.title.clone() };
            self.op_log(op, format!("▸ Starting {} on {}", kind.name, dst.name));
            let session = self
                .start_session(StartSession {
                    machine_id: dst.id.clone(),
                    agent: kind.id.into(),
                    cwd,
                    title: Some(title),
                    prompt: Some(prompt(&format!("{} on {}", from.name, src.name))),
                    cols: 120,
                    rows: 40,
                    ..Default::default()
                })
                .await?;
            if !keep {
                self.control_session(&s.id, Control::Stop).await?;
            }
            self.op_log(op, format!("✓ {} took over on {}", kind.name, dst.name));
            Ok(HandedOff {
                session,
                note: format!("{}/{NOTE}", ws.path.trim_end_matches('/')),
                source: source(&note),
                kept: keep.then(|| s.id.clone()),
                box_id: None,
                workspace: Some(ws.id),
                warnings,
            })
        }
        .await;
        if let Err(e) = &result {
            self.op_log(op, format!("error: {e}"));
            // Nothing half-made stays: the packs, and the workspace there.
            let _ = agent::ctl(&src, &json!({ "op": "carry_drop", "pack_id": pack_id })).await;
            let _ = agent::ctl(&dst, &json!({ "op": "carry_drop", "pack_id": pack_id })).await;
            if let Some(ws) = made {
                let _ = self.destroy_workspace(&ws, false).await;
            }
        }
        self.op_finish(op, result.as_ref().map(|_| ()).map_err(Clone::clone));
        result
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
        Ok(HandedOff { session, note: note_path(&note), source: source(&note), kept: None, box_id: Some(b.id.clone()), workspace: None, warnings: Vec::new() })
    }
}

/// The branch the work goes on in the other machine's checkout: its own, but
/// not the default branch of a clone ("main" is there already, as origin has
/// it), so one named for the session then.
fn branch_there(branch: Option<&str>, base: Option<&str>, has_origin: bool, session: &str) -> String {
    let default = base.and_then(|b| b.split_once('/')).map(|(_, b)| b);
    match branch.filter(|b| !b.is_empty()) {
        Some(b) if has_origin && (Some(b) == default || (default.is_none() && matches!(b, "main" | "master"))) => format!("muxin/{session}"),
        Some(b) => b.to_string(),
        None if has_origin => format!("muxin/{session}"),
        None => "main".into(),
    }
}

fn plural(n: u64, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn note_path(reply: &Value) -> String {
    reply["path"].as_str().unwrap_or(NOTE).to_string()
}

fn source(reply: &Value) -> String {
    reply["source"].as_str().unwrap_or("none").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_work_keeps_its_branch_but_not_the_default_one() {
        assert_eq!(branch_there(Some("fix-login"), Some("origin/main"), true, "claude-3f9a"), "fix-login");
        assert_eq!(branch_there(Some("main"), Some("origin/main"), true, "claude-3f9a"), "muxin/claude-3f9a");
        assert_eq!(branch_there(Some("master"), None, true, "codex-1"), "muxin/codex-1");
        assert_eq!(branch_there(None, Some("origin/main"), true, "codex-1"), "muxin/codex-1");
        // Without a remote there's no default branch to clash with.
        assert_eq!(branch_there(Some("main"), None, false, "codex-1"), "main");
        assert_eq!(branch_there(None, None, false, "codex-1"), "main");
    }
}
