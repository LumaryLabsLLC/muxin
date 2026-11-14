//! What the user points at in a message: "restart the runner on @forge",
//! "what is @forge/claude-3f9a waiting for?".
//!
//! A mention is an `@` and a thing's own id, the one Muxin's tools take: a
//! machine's ("forge"), an agent session's ("forge/claude-3f9a"), a box's, a
//! runner's, a workspace's, a repository's ("acme/api"). The page offers
//! them by name as the user types; the terminal takes them typed.
//!
//! The message goes to the brain as the user wrote it, followed by a line
//! for each thing it names: what it is, where, and the id to use. So the
//! brain doesn't have to look a name up, or guess between two machines.

use std::ops::Range;

use crate::fleet::model::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Machine,
    Session,
    Box,
    Runner,
    Workspace,
    Repo,
}

impl Kind {
    /// What it's called, in a list and to the brain.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Machine => "machine",
            Kind::Session => "agent session",
            Kind::Box => "box",
            Kind::Runner => "runner",
            Kind::Workspace => "workspace",
            Kind::Repo => "repository",
        }
    }
}

/// Something that can be mentioned.
#[derive(Clone, Debug, PartialEq)]
pub struct Thing {
    /// What follows the `@`: its id.
    pub id: String,
    pub kind: Kind,
    /// What it goes by: a machine's name, a session's title.
    pub name: String,
    /// A few words for a list: where it is, how it's doing.
    pub about: String,
    /// What the brain is told it is.
    told: String,
}

/// Everything in `s` that can be mentioned: machines first, then what's on them.
pub fn things(s: &Snapshot) -> Vec<Thing> {
    let on = |machine_id: &str| s.machines.iter().find(|m| m.id == machine_id).map(|m| m.name.clone()).unwrap_or_else(|| machine_id.to_string());
    let words = |state: &str| state.replace('_', " ");
    let mut all = Vec::new();
    for m in &s.machines {
        let platform = if m.platform.is_empty() { String::new() } else { format!(", {}", m.platform) };
        all.push(Thing {
            id: m.id.clone(),
            kind: Kind::Machine,
            name: m.name.clone(),
            about: m.status.clone(),
            told: format!("the machine \"{}\" (machine_id \"{}\"{platform}, {})", m.name, m.id, m.status),
        });
    }
    // A box's own sessions go by the box.
    for x in s.sessions.iter().filter(|x| x.box_id.is_none()) {
        let agent = if x.agent_name.is_empty() { "A session" } else { &x.agent_name };
        let state = if x.status == "exited" { "exited".to_string() } else { words(&x.state) };
        let folder = if x.cwd.is_empty() { String::new() } else { format!(", in {}", x.cwd) };
        all.push(Thing {
            id: x.id.clone(),
            kind: Kind::Session,
            name: if x.title.is_empty() { x.id.clone() } else { x.title.clone() },
            about: format!("{agent} on {} · {state}", on(&x.machine_id)),
            told: format!("the agent session \"{}\" (session id \"{}\": {agent} on machine \"{}\"{folder}, {state})", x.title, x.id, x.machine_id),
        });
    }
    for b in &s.boxes {
        let task: String = b.task.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(140).collect();
        all.push(Thing {
            id: b.id.clone(),
            kind: Kind::Box,
            name: if b.name.is_empty() { b.id.clone() } else { b.name.clone() },
            about: format!("Box on {} · {}", on(&b.machine_id), b.state),
            told: format!("the box \"{}\" (box_id \"{}\" on machine \"{}\", repository {}, branch {}, {}; its task: {task})", b.name, b.id, b.machine_id, b.repo, b.branch, b.state),
        });
    }
    for r in &s.runners {
        let machine = r.machine_id.as_deref().unwrap_or("github");
        all.push(Thing {
            id: r.id.clone(),
            kind: Kind::Runner,
            name: r.name.clone(),
            about: format!("Runner on {} · {}", if r.machine_id.is_some() { on(machine) } else { "GitHub only".to_string() }, r.state),
            told: format!("the runner \"{}\" (runner_id \"{}\" on machine \"{machine}\", {})", r.name, r.id, r.state),
        });
    }
    for w in &s.workspaces {
        let repo = w.repo.as_ref().map(|r| format!(", repository {r}")).unwrap_or_default();
        all.push(Thing {
            id: w.id.clone(),
            kind: Kind::Workspace,
            name: if w.name.is_empty() { w.id.clone() } else { w.name.clone() },
            about: format!("Workspace on {}", on(&w.machine_id)),
            told: format!("the workspace \"{}\" (workspace id \"{}\" on machine \"{}\" at {}{repo})", w.name, w.id, w.machine_id, w.path),
        });
    }
    // Repositories, by where they're checked out.
    let mut repos: Vec<(&str, Vec<String>, Vec<String>)> = Vec::new();
    for c in &s.checkouts {
        let Some(repo) = c.repo.as_deref() else { continue };
        let (place, name) = (format!("machine \"{}\" at {} (branch {})", c.machine_id, c.path, c.branch), on(&c.machine_id));
        match repos.iter_mut().find(|(known, _, _)| *known == repo) {
            Some((_, places, names)) => {
                places.push(place);
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            None => repos.push((repo, vec![place], vec![name])),
        }
    }
    for (repo, places, names) in repos {
        all.push(Thing {
            id: repo.to_string(),
            kind: Kind::Repo,
            name: repo.to_string(),
            about: format!("Repository on {}", names.join(", ")),
            told: format!("the GitHub repository {repo}, checked out on {}", places.iter().take(4).cloned().collect::<Vec<_>>().join("; ")),
        });
    }
    // One name, one thing: the first to have it keeps it.
    let mut taken = std::collections::HashSet::new();
    all.retain(|thing| taken.insert(thing.id.to_lowercase()));
    all
}

/// What an id is made of. (A `.` or `:` at its end is the sentence's.)
fn of_an_id(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':')
}

/// Whether a mention can start after `before`: at the start of the text,
/// after a space, or after an opening mark. Not inside a word ("a@b.c").
fn starts_after(before: Option<char>) -> bool {
    before.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\'' | ','))
}

/// The `@names` in `text`, in order: where each is (its `@` included), and
/// the name.
pub fn mentions(text: &str) -> Vec<(Range<usize>, &str)> {
    let mut found = Vec::new();
    let mut before = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c == '@' && starts_after(before) {
            let start = at + 1;
            let mut end = start;
            while let Some((i, c)) = chars.peek().copied().filter(|(_, c)| of_an_id(*c)) {
                end = i + c.len_utf8();
                chars.next();
            }
            if end > start {
                found.push((at..end, &text[start..end]));
            }
            before = text[..end].chars().next_back();
            continue;
        }
        before = Some(c);
    }
    found
}

/// The thing `name` is: by its id, whatever its letters' case. A mark that
/// ends the sentence isn't part of it.
pub fn find<'a>(all: &'a [Thing], name: &str) -> Option<&'a Thing> {
    let by_id = |name: &str| all.iter().find(|t| t.id.eq_ignore_ascii_case(name));
    by_id(name).or_else(|| by_id(name.trim_end_matches(['.', ':', '/'])))
}

/// What follows a message for the brain: a line for each thing it mentions.
/// Empty when it mentions nothing Muxin knows.
pub fn note(text: &str, all: &[Thing]) -> String {
    let mut told: Vec<&Thing> = Vec::new();
    for (_, name) in mentions(text) {
        if let Some(thing) = find(all, name)
            && !told.iter().any(|t| t.id == thing.id)
        {
            told.push(thing);
        }
    }
    if told.is_empty() {
        return String::new();
    }
    let mut note = String::from("\n\n[From Muxin: what this message points at.");
    for thing in told {
        note.push_str(&format!("\n@{} is {}.", thing.id, thing.told));
    }
    note.push(']');
    note
}

/// The mention being typed where the cursor is: where its `@` is, and what
/// follows it so far.
pub fn typing(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let upto = text.get(..cursor)?;
    let at = upto.rfind('@')?;
    let so_far = &upto[at + 1..];
    (so_far.chars().all(of_an_id) && starts_after(upto[..at].chars().next_back())).then_some((at, so_far))
}

/// Where the id that's at `from` ends: what picking another thing replaces.
pub fn end(text: &str, from: usize) -> usize {
    let rest = text.get(from..).unwrap_or_default();
    from + rest.find(|c: char| !of_an_id(c)).unwrap_or(rest.len())
}

/// The things `so_far` could become, best first: an id or a name that
/// starts with it, then a word in one that does, then anything it's in.
pub fn matching<'a>(all: &'a [Thing], so_far: &str) -> Vec<&'a Thing> {
    let wanted = so_far.to_lowercase();
    if wanted.is_empty() {
        return all.iter().collect();
    }
    let rank = |thing: &Thing| -> Option<u8> {
        let (id, name) = (thing.id.to_lowercase(), thing.name.to_lowercase());
        if id.starts_with(&wanted) || name.starts_with(&wanted) {
            Some(0)
        } else if id.split(['/', '-', '_', '.']).chain(name.split_whitespace()).any(|word| word.starts_with(&wanted)) {
            Some(1)
        } else if id.contains(&wanted) || name.contains(&wanted) || thing.kind.word().starts_with(&wanted) || thing.about.to_lowercase().contains(&wanted) {
            Some(2)
        } else {
            None
        }
    };
    let mut ranked: Vec<(u8, &Thing)> = all.iter().filter_map(|thing| rank(thing).map(|r| (r, thing))).collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, thing)| thing).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thing(id: &str, kind: Kind, name: &str) -> Thing {
        Thing { id: id.into(), kind, name: name.into(), about: format!("{} here", kind.word()), told: format!("the {} \"{name}\"", kind.word()) }
    }

    fn some() -> Vec<Thing> {
        vec![
            thing("forge", Kind::Machine, "Forge"),
            thing("mac-studio", Kind::Machine, "Mac Studio"),
            thing("forge/claude-3f9a", Kind::Session, "Checkout fix"),
            thing("box-7k2m9q", Kind::Box, "Flaky test"),
            thing("forge/forge-01", Kind::Runner, "forge-01"),
            thing("acme/api", Kind::Repo, "acme/api"),
        ]
    }

    #[test]
    fn mentions_are_found_where_a_word_starts_and_not_inside_one() {
        let text = "Restart @forge/forge-01 on @forge, then ask (@box-7k2m9q). Mail me@example.com about @ and @@.";
        let found: Vec<&str> = mentions(text).into_iter().map(|(_, name)| name).collect();
        assert_eq!(found, ["forge/forge-01", "forge", "box-7k2m9q"]);
        let (range, name) = mentions(text).remove(0);
        assert_eq!((&text[range], name), ("@forge/forge-01", "forge/forge-01"));
        assert!(mentions("no one here").is_empty());
        // Other alphabets are ids' letters too.
        assert_eq!(mentions("ask @büro now")[0].1, "büro");
    }

    #[test]
    fn a_mention_is_the_thing_with_that_id_whatever_ends_the_sentence() {
        let all = some();
        assert_eq!(find(&all, "forge").map(|t| t.kind), Some(Kind::Machine));
        assert_eq!(find(&all, "Forge/Claude-3F9A").map(|t| t.name.as_str()), Some("Checkout fix"));
        assert_eq!(find(&all, "acme/api.").map(|t| t.kind), Some(Kind::Repo));
        assert_eq!(find(&all, "forge:").map(|t| t.kind), Some(Kind::Machine));
        // Not by a part of an id, and not by a name: two machines can share a word.
        assert!(find(&all, "forg").is_none() && find(&all, "studio").is_none() && find(&all, "").is_none());
    }

    #[test]
    fn the_brain_is_told_what_each_mention_is_once() {
        let all = some();
        let note = note("Is @forge free? Move the work of @forge/claude-3f9a to @forge. And @nobody?", &all);
        assert_eq!(note, "\n\n[From Muxin: what this message points at.\n@forge is the machine \"Forge\".\n@forge/claude-3f9a is the agent session \"Checkout fix\".]");
        // Nothing Muxin knows is mentioned: nothing is added.
        assert_eq!(super::note("write to me@example.com or @nobody", &all), "");
    }

    #[test]
    fn what_is_being_typed_is_offered_by_id_name_or_kind() {
        let all = some();
        // The mention at the cursor, as far as it's typed.
        assert_eq!(typing("restart @fo", 11), Some((8, "fo")));
        assert_eq!(typing("restart @", 9), Some((8, "")));
        assert_eq!(typing("restart @forge now", 14), Some((8, "forge")));
        // Past its end, in a word that isn't one, or with nothing typed: none.
        assert_eq!(typing("restart @forge now", 18), None);
        assert_eq!(typing("me@example", 10), None);
        assert_eq!(typing("restart forge", 7), None);
        assert_eq!(typing("héllo @x", 99), None);
        // Picking one replaces the whole of what's there, not only up to the cursor.
        assert_eq!(end("restart @forge now", 11), 14);
        assert_eq!(end("restart @fo", 11), 11);

        let ids = |so_far: &str| matching(&all, so_far).into_iter().map(|t| t.id.as_str()).collect::<Vec<_>>();
        // Nothing typed: everything, machines first.
        assert_eq!(ids("").len(), all.len());
        // What starts with it comes before what only has it somewhere.
        assert_eq!(ids("fo"), ["forge", "forge/claude-3f9a", "forge/forge-01"]);
        assert_eq!(ids("check"), ["forge/claude-3f9a"]);
        assert_eq!(ids("studio"), ["mac-studio"]);
        assert_eq!(ids("api"), ["acme/api"]);
        // By what kind of thing it is.
        assert_eq!(ids("box"), ["box-7k2m9q"]);
        assert_eq!(ids("runner"), ["forge/forge-01"]);
        assert!(ids("zzz").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn everything_on_the_machines_can_be_mentioned() {
        let dir = std::env::temp_dir().join(format!("muxin-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = crate::fleet::Engine::new(crate::config::Paths::resolve(Some(dir), None), true);
        engine.check_once(false).await;
        let online: Vec<String> = engine.current().machines.iter().filter(|m| m.status == "online").map(|m| m.id.clone()).collect();
        for id in &online {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(15), engine.refresh_sessions(id)).await;
        }
        let snapshot = engine.current();
        let all = things(&snapshot);
        // Every machine, by its id, and first.
        assert!(!snapshot.machines.is_empty());
        assert_eq!(all.iter().take_while(|t| t.kind == Kind::Machine).count(), snapshot.machines.len());
        let forge = find(&all, "forge").expect("the simulated machines have a Forge");
        assert_eq!((forge.kind, forge.name.as_str()), (Kind::Machine, "Forge"));
        assert!(forge.told.contains("machine_id \"forge\""), "{}", forge.told);
        // What's on them, each under the id Muxin's tools take.
        for x in snapshot.sessions.iter().filter(|x| x.box_id.is_none()) {
            assert_eq!(find(&all, &x.id).map(|t| t.kind), Some(Kind::Session), "{}", x.id);
        }
        for r in &snapshot.runners {
            assert!(find(&all, &r.id).is_some(), "{}", r.id);
        }
        for b in &snapshot.boxes {
            assert_eq!(find(&all, &b.id).map(|t| t.kind), Some(Kind::Box));
        }
        // No two things answer to one name.
        let mut ids: Vec<String> = all.iter().map(|t| t.id.to_lowercase()).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before);
        // And a message that names one tells the brain which.
        assert!(note("is @forge busy?", &all).contains("@forge is the machine \"Forge\" (machine_id \"forge\""));
    }
}
