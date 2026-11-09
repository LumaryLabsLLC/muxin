//! The sidebar's Agents list: every agent session on every machine in one
//! place, the ones that need you first, to switch between them and see how
//! each is doing at a glance.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use super::{Mark, SessionInfo};

/// Whether a session is an AI agent's. Shells aren't (nor sessions whose
/// program isn't known): they say nothing about needing you or being done.
pub fn is_agent(s: &SessionInfo) -> bool {
    !s.agent.is_empty() && s.agent != "shell"
}

/// The agent sessions in the order the list shows them, each with how deep a
/// helper it is (helpers under the session that started them, see `tree`):
/// - a session with its helpers ranks by the most urgent of them: needs you,
///   then exited with an error, working, done (until it's opened), idle;
/// - running before exited; sessions on machines that are down last (what
///   they last said is old news);
/// - then the newest first, so rows don't move while they work.
///
/// `query` (lowercase; empty for all) keeps the sessions whose title, agent
/// or machine has it. `machine_name` and `down` say a machine's name, and
/// whether it's offline or disconnected, by its id.
pub fn agents<'a>(sessions: &'a [SessionInfo], query: &str, machine_name: impl Fn(&str) -> String, down: impl Fn(&str) -> bool) -> Vec<(usize, &'a SessionInfo)> {
    let found = |s: &SessionInfo| {
        query.is_empty() || s.title.to_lowercase().contains(query) || s.agent_name.to_lowercase().contains(query) || machine_name(&s.machine_id).to_lowercase().contains(query)
    };
    let mut list: Vec<&SessionInfo> = sessions.iter().filter(|s| is_agent(s) && found(s)).collect();
    let ids: HashSet<&str> = list.iter().map(|&s| s.id.as_str()).collect();
    let mut kids: HashMap<&str, Vec<&SessionInfo>> = HashMap::new();
    for &s in &list {
        if let Some(p) = s.parent.as_deref().filter(|p| ids.contains(p) && *p != s.id) {
            kids.entry(p).or_default().push(s);
        }
    }
    // What a session shows, and the most urgent of it and its helpers'.
    let own = |s: &SessionInfo| if down(&s.machine_id) { Mark::Idle } else { s.mark() };
    fn family<'a>(s: &'a SessionInfo, kids: &HashMap<&str, Vec<&'a SessionInfo>>, own: &dyn Fn(&SessionInfo) -> Mark, seen: &mut HashSet<&'a str>) -> Mark {
        if !seen.insert(s.id.as_str()) {
            return Mark::Idle;
        }
        let below = kids.get(s.id.as_str()).map(|k| k.iter().map(|h| family(h, kids, own, seen)).max().unwrap_or(Mark::Idle)).unwrap_or(Mark::Idle);
        own(s).max(below)
    }
    let rank: HashMap<&str, Mark> = list.iter().map(|&s| (s.id.as_str(), family(s, &kids, &own, &mut HashSet::new()))).collect();
    list.sort_by(|a, b| {
        let key = |s: &SessionInfo| (down(&s.machine_id), Reverse(rank[s.id.as_str()]), s.status == "exited", Reverse(s.created_at));
        key(a).cmp(&key(b)).then_with(|| a.id.cmp(&b.id))
    });
    super::tree(&list)
}

/// How a row of the list joins the tree's lines (see `lines`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lines {
    /// How deep a helper it is (0: a session of its own).
    pub depth: usize,
    /// No more helpers of its parent below it: the parent's line ends here.
    pub last: bool,
    /// Its own helpers are right below it: a line goes down from its icon.
    pub kids: bool,
    /// Levels whose line passes by on its left (an ancestor has more helpers below).
    pub rails: Vec<usize>,
}

/// The tree's lines for each row of `agents`.
pub fn lines(list: &[(usize, &SessionInfo)]) -> Vec<Lines> {
    // more[d]: the last row seen at depth d has siblings below it.
    let mut more: Vec<bool> = Vec::new();
    let mut out = Vec::with_capacity(list.len());
    for (i, (depth, _)) in list.iter().enumerate() {
        let depth = *depth;
        let last = list[i + 1..].iter().take_while(|(d, _)| *d >= depth).all(|(d, _)| *d > depth);
        let kids = list.get(i + 1).is_some_and(|(d, _)| *d > depth);
        // The line at level l runs from a row at depth l down to its helpers;
        // it passes this row when the helper at depth l + 1 above it isn't the last.
        let rails = (0..depth.saturating_sub(1)).filter(|l| more.get(l + 1).copied().unwrap_or(false)).collect();
        more.truncate(depth);
        more.resize(depth + 1, false);
        more[depth] = !last;
        out.push(Lines { depth, last, kids, rails });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn session(id: &str, agent: &str, state: &str, created: i64) -> SessionInfo {
        let (machine, name) = id.split_once('/').unwrap();
        SessionInfo {
            id: id.into(),
            machine_id: machine.into(),
            name: name.into(),
            agent: agent.into(),
            agent_name: super::super::agent(agent).map(|a| a.name.to_string()).unwrap_or_default(),
            title: name.into(),
            status: "running".into(),
            state: state.into(),
            // Done shows until it's opened.
            attention: (state == "done").then(|| "waiting".into()),
            created_at: Some(Utc.timestamp_opt(created, 0).unwrap()),
            ..Default::default()
        }
    }

    fn ids(list: &[(usize, &SessionInfo)]) -> Vec<String> {
        list.iter().map(|(d, s)| format!("{}{}", "  ".repeat(*d), s.id)).collect()
    }

    fn named(id: &str) -> String {
        match id {
            "mac" => "Personal".into(),
            "beelink" => "Beelink".into(),
            _ => id.into(),
        }
    }

    #[test]
    fn only_agents_are_listed_not_shells() {
        let sessions = vec![session("mac/claude-a", "claude", "idle", 1), session("mac/shell-b", "shell", "working", 2), session("mac/x-c", "", "idle", 3), session("beelink/codex-d", "codex", "idle", 4)];
        assert_eq!(ids(&agents(&sessions, "", named, |_| false)), ["beelink/codex-d", "mac/claude-a"]);
        assert!(is_agent(&sessions[0]) && !is_agent(&sessions[1]) && !is_agent(&sessions[2]));
        assert!(agents(&[], "", named, |_| false).is_empty());
    }

    #[test]
    fn needs_you_first_then_working_done_idle_and_exited_last() {
        let mut exited = session("mac/claude-e", "claude", "idle", 9);
        exited.status = "exited".into();
        exited.exit_code = Some(0);
        let mut failed = session("mac/claude-f", "claude", "idle", 1);
        failed.status = "exited".into();
        failed.exit_code = Some(1);
        failed.attention = Some("exited".into());
        let sessions = vec![
            exited,
            session("mac/claude-idle", "claude", "idle", 8),
            session("mac/codex-done", "codex", "done", 7),
            session("beelink/codex-work", "codex", "working", 6),
            session("beelink/claude-ask", "claude", "needs_you", 5),
            failed,
        ];
        assert_eq!(
            ids(&agents(&sessions, "", named, |_| false)),
            ["beelink/claude-ask", "mac/claude-f", "beelink/codex-work", "mac/codex-done", "mac/claude-idle", "mac/claude-e"]
        );
    }

    #[test]
    fn a_rang_bell_needs_you_and_an_opened_done_is_idle() {
        let mut bell = session("mac/claude-bell", "claude", "idle", 1);
        bell.attention = Some("bell".into());
        let mut seen = session("mac/codex-seen", "codex", "done", 3);
        seen.attention = None;
        let sessions = vec![seen, session("mac/claude-idle", "claude", "idle", 2), bell];
        assert_eq!(ids(&agents(&sessions, "", named, |_| false)), ["mac/claude-bell", "mac/codex-seen", "mac/claude-idle"]);
    }

    #[test]
    fn newest_first_within_a_state_whatever_they_print() {
        let mut a = session("mac/claude-old", "claude", "working", 1);
        a.last_output_at = Some(Utc.timestamp_opt(100, 0).unwrap());
        let mut b = session("beelink/codex-new", "codex", "working", 5);
        b.last_output_at = Some(Utc.timestamp_opt(10, 0).unwrap());
        let sessions = vec![a, b, session("mac/claude-mid", "claude", "working", 3)];
        assert_eq!(ids(&agents(&sessions, "", named, |_| false)), ["beelink/codex-new", "mac/claude-mid", "mac/claude-old"]);
        // The same sessions in another order: the same list.
        let mut again = sessions.clone();
        again.reverse();
        assert_eq!(ids(&agents(&again, "", named, |_| false)), ids(&agents(&sessions, "", named, |_| false)));
    }

    #[test]
    fn helpers_stay_under_their_session_which_ranks_by_the_family() {
        let mut helper = session("beelink/codex-h", "codex", "needs_you", 3);
        helper.parent = Some("mac/claude-p".into());
        let mut shell_helper = session("mac/shell-s", "shell", "working", 4);
        shell_helper.parent = Some("mac/claude-p".into());
        let sessions = vec![session("mac/claude-work", "claude", "working", 9), session("mac/claude-p", "claude", "idle", 1), helper, shell_helper];
        // The idle parent goes first: its helper needs you. Shells stay out, helpers or not.
        assert_eq!(ids(&agents(&sessions, "", named, |_| false)), ["mac/claude-p", "  beelink/codex-h", "mac/claude-work"]);
    }

    #[test]
    fn sessions_on_a_machine_that_is_down_go_last_and_rank_as_idle() {
        let sessions = vec![session("beelink/claude-ask", "claude", "needs_you", 5), session("mac/codex-idle", "codex", "idle", 1)];
        assert_eq!(ids(&agents(&sessions, "", named, |m| m == "beelink")), ["mac/codex-idle", "beelink/claude-ask"]);
        // A helper on a machine that's down doesn't lift its parent.
        let mut helper = session("beelink/codex-h", "codex", "needs_you", 3);
        helper.parent = Some("mac/claude-p".into());
        let sessions = vec![session("mac/claude-work", "claude", "working", 9), session("mac/claude-p", "claude", "idle", 1), helper];
        assert_eq!(ids(&agents(&sessions, "", named, |m| m == "beelink")), ["mac/claude-work", "mac/claude-p", "  beelink/codex-h"]);
    }

    #[test]
    fn a_search_keeps_titles_agents_and_machines_that_match() {
        let mut fix = session("mac/claude-a", "claude", "idle", 1);
        fix.title = "Fix the login page".into();
        let sessions = vec![fix, session("beelink/codex-b", "codex", "idle", 2), session("beelink/shell-c", "shell", "idle", 3)];
        assert_eq!(ids(&agents(&sessions, "login", named, |_| false)), ["mac/claude-a"]);
        assert_eq!(ids(&agents(&sessions, "codex", named, |_| false)), ["beelink/codex-b"]);
        assert_eq!(ids(&agents(&sessions, "beelink", named, |_| false)), ["beelink/codex-b"]);
        assert_eq!(ids(&agents(&sessions, "personal", named, |_| false)), ["mac/claude-a"]);
        assert!(agents(&sessions, "nothing", named, |_| false).is_empty());
        // A helper found without its parent stands on its own.
        let mut helper = session("mac/codex-h", "codex", "idle", 4);
        helper.parent = Some("mac/claude-a".into());
        helper.title = "Look things up".into();
        let sessions = vec![sessions[0].clone(), helper];
        assert_eq!(ids(&agents(&sessions, "look", named, |_| false)), ["mac/codex-h"]);
    }

    #[test]
    fn helpers_of_each_other_are_listed_once() {
        let mut a = session("mac/claude-a", "claude", "idle", 1);
        a.parent = Some("mac/codex-b".into());
        let mut b = session("mac/codex-b", "codex", "working", 2);
        b.parent = Some("mac/claude-a".into());
        let sessions = vec![a, b];
        let list = agents(&sessions, "", named, |_| false);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn tree_lines_join_helpers_to_their_session() {
        let mk = |id: &str, parent: Option<&str>, created: i64| {
            let mut s = session(id, "claude", "idle", created);
            s.parent = parent.map(str::to_string);
            s
        };
        // a ─ b ─ d
        //   └ c
        // e
        let sessions = vec![mk("m/claude-a", None, 9), mk("m/claude-b", Some("m/claude-a"), 8), mk("m/claude-c", Some("m/claude-a"), 7), mk("m/claude-d", Some("m/claude-b"), 6), mk("m/claude-e", None, 1)];
        let list = agents(&sessions, "", |_| String::new(), |_| false);
        assert_eq!(ids(&list), ["m/claude-a", "  m/claude-b", "    m/claude-d", "  m/claude-c", "m/claude-e"]);
        let l = lines(&list);
        let row = |depth, last, kids, rails: &[usize]| Lines { depth, last, kids, rails: rails.to_vec() };
        assert_eq!(l, [row(0, false, true, &[]), row(1, false, true, &[]), row(2, true, false, &[0]), row(1, true, false, &[]), row(0, true, false, &[])]);
        // Once b is its parent's last helper, a's line stops at b.
        let infos = vec![SessionInfo::default(); 3];
        let rows: Vec<(usize, &SessionInfo)> = [0, 1, 2].into_iter().zip(&infos).collect();
        assert_eq!(lines(&rows), [row(0, true, true, &[]), row(1, true, true, &[]), row(2, true, false, &[])]);
    }
}
