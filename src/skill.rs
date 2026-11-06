//! The agent skill for Muxin (skills/muxin): instructions that teach agents
//! (Claude Code, Codex, Hermes, ...) to drive the `muxin` command.
//!
//! `muxin skill install` puts a stub in the agents' skills folders: the
//! skill's description, so agents load it for the same requests, and a few
//! lines that ask `muxin skill show` for the guide. The guide is built into
//! the binary, so it always matches the Muxin installed, and an update never
//! leaves an old copy behind. (Muxin before 0.10 copied the whole folder;
//! installing again replaces that copy with the stub.)

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::display_path;

/// The skill's files, by their path in its folder: the guide.
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

/// In the stub's metadata, after `MARK`: it's a stub, of this format.
const STUB: &str = "muxin-stub: \"1\"";

/// What the stub says, under the skill's frontmatter.
const STUB_BODY: &str = "<!-- Installed by \"muxin skill install\". Remove it with \"muxin skill remove\". -->

Run `muxin skill show` and follow the guide it prints. The guide comes from the Muxin installed here, so it matches the commands this version has. It names more topics, each with its own command, such as `muxin skill show sessions-and-boxes`.

If there's no `muxin` command, ask the user to turn it on in Muxin (Settings › General › Command line). If `muxin skill show` fails, this Muxin is older than the guide: start with `muxin status --json`, then `muxin --help`.
";

/// Where agents look for skills, under the home folder: the agent, the folder
/// that says it's on this computer, and its skills folder.
const AGENTS: &[(&str, &str, &str)] = &[
    ("claude", ".claude", ".claude/skills"),
    // Codex's personal skills are in the folder several agents share.
    ("codex", ".codex", ".agents/skills"),
    ("opencode", ".config/opencode", ".config/opencode/skills"),
    ("gemini", ".gemini", ".gemini/skills"),
    ("pi", ".pi", ".pi/agent/skills"),
    ("hermes", ".hermes", ".hermes/skills"),
    // These read the shared folder too: one copy there, not one each.
    ("copilot", ".copilot", ".agents/skills"),
    ("cursor", ".cursor", ".agents/skills"),
    ("qwen", ".qwen", ".agents/skills"),
    ("droid", ".factory", ".agents/skills"),
    ("kimi", ".kimi-code", ".agents/skills"),
    // That folder for any other agent that reads it: only when asked for, so
    // one that reads it and its own doesn't get the skill twice.
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
            .collect::<Result<_, String>>()
            .map(once);
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
    Ok(once(found))
}

/// Each folder once: `codex` and `agents` are the same one.
fn once(mut targets: Vec<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    targets.retain(|(_, dir)| seen.insert(dir.clone()));
    targets
}

/// Writes the stub into a skills folder, replacing an older copy of the skill.
pub fn install(agent: String, skills: &Path) -> Outcome {
    let dir = skills.join(NAME);
    let mut out = Outcome { agent, path: display_path(&dir), result: "installed", note: None };
    match state(&dir) {
        State::Absent => {}
        State::Ours => {
            out.result = "updated";
            if !is_stub(&dir) {
                out.note = Some("It was a full copy, which goes out of date. Now it asks muxin for the guide.".into());
            }
        }
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
    // Only the stub: the files of an older copy go.
    if replacing {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", display_path(dir)))?;
    }
    let file = dir.join("SKILL.md");
    std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&file, stub())).map_err(|e| format!("{}: {e}", display_path(&file)))
}

fn is_stub(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("SKILL.md")).is_ok_and(|s| s.contains(STUB))
}

/// SKILL.md's frontmatter with `STUB` in its metadata, and `STUB_BODY`.
pub fn stub() -> String {
    let (front, _) = split(FILES[0].1);
    let front = front.replacen(MARK, &format!("{MARK}\n  {STUB}"), 1);
    format!("---\n{front}\n---\n{STUB_BODY}")
}

/// A skill file's frontmatter and the rest.
fn split(text: &str) -> (&str, &str) {
    text.strip_prefix("---\n").and_then(|s| s.split_once("\n---\n")).unwrap_or(("", text))
}

/// The guide's topics, by name (`references/github.md` is `github`).
pub fn topics() -> Vec<&'static str> {
    FILES.iter().filter_map(|(path, _)| path.strip_prefix("references/")?.strip_suffix(".md")).collect()
}

/// What `muxin skill show` prints: the guide (SKILL.md without its
/// frontmatter), or one of its topics. Links to other files become the
/// command that shows them.
pub fn guide(topic: Option<&str>) -> Result<String, String> {
    let Some(topic) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(unlink(split(FILES[0].1).1.trim_start()));
    };
    let wanted = topic.trim_end_matches(".md").rsplit('/').next().unwrap_or(topic).to_ascii_lowercase();
    match FILES.iter().find(|(path, _)| path.strip_prefix("references/").and_then(|p| p.strip_suffix(".md")) == Some(wanted.as_str())) {
        Some((_, text)) => Ok(unlink(text)),
        None if wanted == "skill" || wanted == "muxin" => Ok(unlink(split(FILES[0].1).1.trim_start())),
        None => Err(format!("There's no topic \"{topic}\". Topics: {}.", topics().join(", "))),
    }
}

/// `[sessions and boxes](references/sessions-and-boxes.md#x)` as
/// "sessions and boxes (`muxin skill show sessions-and-boxes`)". Web links stay.
fn unlink(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let (before, after) = rest.split_at(at);
        let open = before.rfind('[').filter(|&i| !before[i..].contains('\n'));
        let close = after[2..].find(')').map(|i| i + 2).filter(|&i| !after[..i].contains('\n'));
        let (Some(open), Some(close)) = (open, close) else {
            out.push_str(&rest[..at + 2]);
            rest = &rest[at + 2..];
            continue;
        };
        let target = &after[2..close];
        let file = target.split('#').next().unwrap_or("");
        if target.contains("://") || !file.ends_with(".md") {
            out.push_str(&rest[..at + close + 1]);
        } else {
            let stem = file.rsplit('/').next().unwrap_or(file).trim_end_matches(".md");
            let command = if stem == "SKILL" { "muxin skill show".to_string() } else { format!("muxin skill show {stem}") };
            out.push_str(&before[..open]);
            out.push_str(&format!("{} (`{command}`)", &before[open + 1..]));
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
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
    /// The stub, or a copy an older `install` wrote.
    Ours,
    /// A link, such as one to a checkout's skills/muxin: the user's to change.
    Link(PathBuf),
    Other,
}

fn state(dir: &Path) -> State {
    match std::fs::symlink_metadata(dir) {
        Err(_) => State::Absent,
        Ok(m) if m.file_type().is_symlink() => State::Link(std::fs::read_link(dir).unwrap_or_default()),
        // The stub, or a full copy an older Muxin wrote.
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
        let (front, _) = split(FILES[0].1);
        assert!(!front.is_empty(), "SKILL.md starts with frontmatter");
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
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), stub());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "only the stub");
        let again = install("dir".into(), &skills);
        assert_eq!((again.result, again.note), ("updated", None));

        assert_eq!(remove("dir".into(), &skills).result, "removed");
        assert!(!dir.exists());
        assert!(remove("dir".into(), &skills).note.is_some(), "removing it again says it wasn't there");

        // A full copy, as Muxin before the stub wrote it, becomes the stub.
        for (path, text) in FILES {
            std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.join(path), text).unwrap();
        }
        let replaced = install("dir".into(), &skills);
        assert_eq!(replaced.result, "updated");
        assert!(replaced.note.is_some_and(|n| n.contains("full copy")));
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), stub());
        assert!(!dir.join("references").exists(), "the old copy's files go");
        assert_eq!(remove("dir".into(), &skills).result, "removed");

        // A skill of the user's own with the same name stays.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: muxin\ndescription: mine\n---\n").unwrap();
        assert_eq!(install("dir".into(), &skills).result, "skipped");
        assert_eq!(remove("dir".into(), &skills).result, "skipped");
        assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "---\nname: muxin\ndescription: mine\n---\n");

        let _ = std::fs::remove_dir_all(&skills);
    }

    #[test]
    fn the_stub_keeps_the_description_and_asks_for_the_guide() {
        let stub = stub();
        let (front, body) = split(&stub);
        let (full, _) = split(FILES[0].1);
        for line in full.lines() {
            assert!(front.lines().any(|l| l == line), "the stub's frontmatter lacks `{line}`");
        }
        assert!(front.contains(&format!("  {MARK}\n  {STUB}")), "marked as a stub under metadata:\n{front}");
        assert!(body.contains("`muxin skill show`"));
        assert!(stub.lines().count() < 20, "a stub, not the guide");
    }

    #[test]
    fn shows_the_guide_and_its_topics() {
        let main = guide(None).unwrap();
        assert!(main.starts_with("# Muxin"), "no frontmatter: {}", &main[..40]);
        assert!(!main.contains("](references/"), "links to files it can't open");
        assert!(main.contains("Sessions and boxes (`muxin skill show sessions-and-boxes`)"));
        assert!(!main.contains("[sessions and boxes]"));
        assert_eq!(topics(), ["sessions-and-boxes", "github", "workspaces", "runners", "machines", "cloud-and-vms"]);
        for t in topics() {
            let text = guide(Some(t)).unwrap();
            assert!(!text.contains(".md)") && !text.contains(".md#"), "{t} still links a file");
        }
        assert!(guide(Some("github")).unwrap().contains("sessions and boxes (`muxin skill show sessions-and-boxes`)"));
        assert_eq!(guide(Some("references/github.md")), guide(Some("GitHub")));
        assert!(guide(Some("nope")).unwrap_err().contains("sessions-and-boxes"));
        assert_eq!(unlink("see [the docs](https://muxin.dev/docs) and [x](y)"), "see [the docs](https://muxin.dev/docs) and [x](y)");
    }

    #[test]
    fn knows_the_agents_asked_for() {
        let t = targets(&["Claude".into(), "hermes".into()], None, true).unwrap();
        assert_eq!(t.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), ["claude", "hermes"]);
        assert!(t[1].1.ends_with(".hermes/skills"));
        assert!(targets(&["nope".into()], None, true).is_err());
        // Codex's folder is the shared one, and a folder is used once.
        let t = targets(&["codex".into(), "agents".into(), "qwen".into(), "kimi".into()], None, true).unwrap();
        assert_eq!(t.len(), 1);
        assert!(t[0].1.ends_with(".agents/skills"));
        let dir = PathBuf::from("/tmp/skills");
        assert_eq!(targets(&[], Some(dir.clone()), true).unwrap(), vec![("dir".to_string(), dir)]);
    }
}
