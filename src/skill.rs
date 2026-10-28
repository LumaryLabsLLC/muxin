//! The agent skill for Muxin (skills/muxin): instructions that teach agents
//! (Claude Code, Codex, Hermes, ...) to drive the `muxin` command. It's built
//! into the binary, so `muxin skill install` writes the version that matches
//! this copy of Muxin.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::display_path;

/// The skill's files, by their path in its folder.
pub const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skills/muxin/SKILL.md")),
    ("references/sessions-and-boxes.md", include_str!("../skills/muxin/references/sessions-and-boxes.md")),
    ("references/github.md", include_str!("../skills/muxin/references/github.md")),
    ("references/workspaces.md", include_str!("../skills/muxin/references/workspaces.md")),
    ("references/runners.md", include_str!("../skills/muxin/references/runners.md")),
    ("references/machines.md", include_str!("../skills/muxin/references/machines.md")),
    ("references/cloud-and-vms.md", include_str!("../skills/muxin/references/cloud-and-vms.md")),
];

/// Its folder's name, as `name:` in SKILL.md says.
const NAME: &str = "muxin";

/// In SKILL.md's frontmatter: the folder is ours to replace, not a skill of
/// the user's own that happens to have the name.
const MARK: &str = "source: https://github.com/LumaryLabsLLC/muxin/tree/main/skills/muxin";

/// Where agents look for skills, under the home folder: the agent, the folder
/// that says it's on this computer, and its skills folder.
const AGENTS: &[(&str, &str, &str)] = &[
    ("claude", ".claude", ".claude/skills"),
    ("codex", ".codex", ".codex/skills"),
    ("opencode", ".config/opencode", ".config/opencode/skills"),
    ("gemini", ".gemini", ".gemini/skills"),
    ("pi", ".pi", ".pi/agent/skills"),
    ("hermes", ".hermes", ".hermes/skills"),
    // The folder several agents share: only when asked for, so an agent that
    // reads it and its own doesn't get the skill twice.
    ("agents", ".agents", ".agents/skills"),
];

#[derive(Debug, Serialize)]
pub struct Outcome {
    /// The agent, or "dir" for a folder given with --dir.
    pub agent: String,
    /// The skill's folder.
    pub path: String,
    /// "installed" | "updated" | "removed" | "skipped" (something else is
    /// there) | "failed"
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The skills folders to use: `dir`, the agents asked for, or by default the
/// agents found on this computer (installing) or the folders that have the
/// skill (removing).
pub fn targets(agents: &[String], dir: Option<PathBuf>, installing: bool) -> Result<Vec<(String, PathBuf)>, String> {
    if let Some(dir) = dir {
        return Ok(vec![("dir".into(), dir)]);
    }
    let home = dirs::home_dir().ok_or("Couldn't tell where your home folder is. Pass --dir.")?;
    let known = || AGENTS.iter().map(|(id, ..)| *id).collect::<Vec<_>>().join(", ");
    if !agents.is_empty() {
        return agents
            .iter()
            .map(|a| match AGENTS.iter().find(|(id, ..)| id.eq_ignore_ascii_case(a)) {
                Some((id, _, skills)) => Ok((id.to_string(), home.join(skills))),
                None => Err(format!("\"{a}\" isn't an agent Muxin knows ({}). Pass --dir for another one's skills folder.", known())),
            })
            .collect();
    }
    let found: Vec<_> = AGENTS
        .iter()
        .filter(|(id, marker, skills)| {
            if installing {
                *id != "agents" && home.join(marker).is_dir()
            } else {
                std::fs::symlink_metadata(home.join(skills).join(NAME)).is_ok()
            }
        })
        .map(|(id, _, skills)| (id.to_string(), home.join(skills)))
        .collect();
    if found.is_empty() && installing {
        return Err(format!("No agent found on this computer. Pass --agent ({}) or --dir.", known()));
    }
    Ok(found)
}

/// Writes the skill into a skills folder, replacing an older version of it.
pub fn install(agent: String, skills: &Path) -> Outcome {
    let dir = skills.join(NAME);
    let mut out = Outcome { agent, path: display_path(&dir), result: "installed", note: None };
    match state(&dir) {
        State::Absent => {}
        State::Ours => out.result = "updated",
        State::Link(to) => return out.skipped(format!("It's a link to {}; left as it is.", display_path(&to))),
        State::Other => return out.skipped("Something else is there (a skill of your own?); left as it is.".into()),
    }
    if let Err(e) = write(&dir, out.result == "updated") {
        out.result = "failed";
        out.note = Some(e);
    }
    out
}

fn write(dir: &Path, replacing: bool) -> Result<(), String> {
    // A fresh copy, so files an older version had and this one doesn't go.
    if replacing {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", display_path(dir)))?;
    }
    for (path, text) in FILES {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().unwrap_or(dir))
            .and_then(|()| std::fs::write(&file, text))
            .map_err(|e| format!("{}: {e}", display_path(&file)))?;
    }
    Ok(())
}

/// Takes the skill out of a skills folder.
pub fn remove(agent: String, skills: &Path) -> Outcome {
    let dir = skills.join(NAME);
    let mut out = Outcome { agent, path: display_path(&dir), result: "removed", note: None };
    match state(&dir) {
        State::Absent => out.note = Some("It wasn't there.".into()),
        State::Ours => {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                out.result = "failed";
                out.note = Some(e.to_string());
            }
        }
        State::Link(to) => return out.skipped(format!("It's a link to {}; left as it is.", display_path(&to))),
        State::Other => return out.skipped("It isn't Muxin's skill; left as it is.".into()),
    }
    out
}

impl Outcome {
    fn skipped(mut self, note: String) -> Self {
        self.result = "skipped";
        self.note = Some(note);
        self
    }
}

enum State {
    Absent,
    /// A copy `install` wrote.
    Ours,
    /// A link, such as one to a checkout's skills/muxin: the user's to change.
    Link(PathBuf),
    Other,
}

fn state(dir: &Path) -> State {
    match std::fs::symlink_metadata(dir) {
        Err(_) => State::Absent,
        Ok(m) if m.file_type().is_symlink() => State::Link(std::fs::read_link(dir).unwrap_or_default()),
        Ok(m) if m.is_dir() && std::fs::read_to_string(dir.join("SKILL.md")).is_ok_and(|s| s.contains(MARK)) => State::Ours,
        Ok(_) => State::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_every_file_in_the_folder() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills").join(NAME);
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                } else if path.is_dir() {
                    stack.push(path);
                } else {
                    on_disk.push(path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut bundled: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        bundled.sort();
        assert_eq!(on_disk, bundled, "skills/muxin and skill::FILES differ");
    }

    #[test]
    fn frontmatter_is_valid() {
        let skill = FILES[0].1;
        let front = skill.strip_prefix("---\n").and_then(|s| s.split_once("\n---\n")).map(|(f, _)| f).expect("SKILL.md starts with frontmatter");
        assert!(front.lines().any(|l| l == format!("name: {NAME}")));
        let description = front.lines().find_map(|l| l.strip_prefix("description: ")).expect("a description");
        assert!(!description.is_empty() && description.chars().count() <= 1024, "a description of at most 1024 characters");
        assert!(front.contains(MARK));
    }

    #[test]
    fn links_point_at_bundled_files() {
        for (path, text) in FILES {
            let base = Path::new(path).parent().unwrap();
            for link in text.split("](").skip(1).filter_map(|s| s.split_once(')')).map(|(l, _)| l) {
                if link.starts_with("http") {
                    continue;
                }
                let file = link.split('#').next().unwrap();
                let target = base.join(file).to_string_lossy().replace('\\', "/");
                assert!(FILES.iter().any(|(p, _)| *p == target), "{path} links to {link}, which isn't bundled");
            }
        }
    }

    #[test]
    fn installs_updates_and_removes() {
        let skills = std::env::temp_dir().join(format!("muxin-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&skills);
        let dir = skills.join(NAME);

        assert_eq!(install("dir".into(), &skills).result, "installed");
        for (path, text) in FILES {
            assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), *text);
        }
        std::fs::write(dir.join("references").join("old.md"), "from an older version").unwrap();
        assert_eq!(install("dir".into(), &skills).result, "updated");
        assert!(!dir.join("references").join("old.md").exists(), "an update leaves no stale files");

        assert_eq!(remove("dir".into(), &skills).result, "removed");
        assert!(!dir.exists());
        assert!(remove("dir".into(), &skills).note.is_some(), "removing it again says it wasn't there");

        // A skill of the user's own with the same name stays.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: muxin\ndescription: mine\n---\n").unwrap();
        assert_eq!(install("dir".into(), &skills).result, "skipped");
        assert_eq!(remove("dir".into(), &skills).result, "skipped");
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "---\nname: muxin\ndescription: mine\n---\n");

        let _ = std::fs::remove_dir_all(&skills);
    }

    #[test]
    fn knows_the_agents_asked_for() {
        let t = targets(&["Claude".into(), "hermes".into()], None, true).unwrap();
        assert_eq!(t.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), ["claude", "hermes"]);
        assert!(t[1].1.ends_with(".hermes/skills"));
        assert!(targets(&["nope".into()], None, true).is_err());
        let dir = PathBuf::from("/tmp/skills");
        assert_eq!(targets(&[], Some(dir.clone()), true).unwrap(), vec![("dir".to_string(), dir)]);
    }
}
