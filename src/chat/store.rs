//! Chats on disk. Each is a folder, `<Muxin's folder>/chats/<id>/`:
//!
//! - `chat.json`: what it is (title, which brain answers, when), written
//!   whole to a temporary name and renamed into place.
//! - `thread.jsonl`: what was said and done, one JSON object per line, only
//!   ever added to. Muxin's own record, the same whichever brain answered, so
//!   the app can show a chat without asking that brain for it.
//! - `work/`: the folder the brain's CLI runs in. Empty, and its own, so the
//!   CLI picks up no project's settings, and its usage counts under this chat.
//! - `files/`: the chat's own copies of what was attached in it.
//!
//! Deleting a chat deletes that folder. A program that answered in it
//! (Claude Code, Codex, Grok Build) has its own record of the conversation,
//! in its own place; that's not Muxin's to delete.
//!
//! No sign-in tokens or keys are kept here. A thread can hold what tools
//! returned (a log, a file's text), so the folder is the user's alone.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::risk::Risk;

/// A tool result is kept up to this long (a screenshot's or a long log's
/// text is cut; the brain got all of it).
const KEEP_RESULT: usize = 4000;
/// A title the user gave is kept up to this many characters.
const MOST_TITLE: usize = 80;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// The first thing asked, shortened, until the user names it.
    #[serde(default)]
    pub title: String,
    /// Which brain answers: "claude", "codex", "grok".
    pub brain: String,
    pub created: i64,
    pub updated: i64,
    /// Each brain's own name for this conversation (Claude Code's session
    /// id, Codex's thread id), to continue it there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, String>,
    /// The model it answers with, by the brain's id for it, and how hard
    /// it's asked to think. None: the brain's, or the model's, own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// One line of a thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// What the user said, and the files that went with it, by their names
    /// (the chat's copies are in its `files` folder).
    User {
        at: i64,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    /// What the assistant said.
    Text { at: i64, text: String },
    /// A tool the assistant called.
    Tool { at: i64, id: String, name: String, args: Value },
    /// What that tool answered.
    Result {
        at: i64,
        id: String,
        text: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A call that waited for the user, and the answer ("yes", "always", "no").
    Ask { at: i64, id: u64, tool: String, args: Value, risk: Risk },
    Answer {
        at: i64,
        id: u64,
        answer: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    /// What a turn used: tokens, and its cost when the brain says.
    Usage {
        at: i64,
        #[serde(default)]
        input: u64,
        #[serde(default)]
        output: u64,
        #[serde(default)]
        cached: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
    },
    /// A turn that failed, in words.
    Error { at: i64, text: String },
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// "c-" and lowercase letters or digits: also a safe folder name.
pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("c-").is_some_and(|r| !r.is_empty() && r.len() <= 24 && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// A thread's title from what was first asked: its first line, shortened.
pub fn title_from(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut title: String = line.chars().take(60).collect();
    if title.chars().count() < line.chars().count() {
        title.push('…');
    }
    title
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The chats under Muxin's folder (`Paths::dir`).
    pub fn new(dir: &Path) -> Store {
        Store { root: dir.join("chats") }
    }

    fn folder(&self, id: &str) -> Option<PathBuf> {
        valid_id(id).then(|| self.root.join(id))
    }

    /// Makes a chat's folder, under a `chats` only the user can look into.
    fn make(&self, folder: &Path) -> std::io::Result<()> {
        if !self.root.is_dir() {
            std::fs::create_dir_all(&self.root)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        std::fs::create_dir_all(folder)
    }

    /// The folder a chat's brain runs in.
    /// A chat's own folder (its record, its thread, the files attached in it).
    pub fn home(&self, id: &str) -> Option<PathBuf> {
        self.folder(id)
    }

    pub fn work(&self, id: &str) -> std::io::Result<PathBuf> {
        let dir = self.folder(id).ok_or_else(|| std::io::Error::other("not a chat id"))?.join("work");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn create(&self, brain: &str) -> std::io::Result<Meta> {
        let now = now();
        for _ in 0..8 {
            let id = format!("c-{}", crate::fleet::workspaces::random_id(6));
            let Some(folder) = self.folder(&id) else { continue };
            if folder.exists() {
                continue;
            }
            self.make(&folder)?;
            let meta = Meta { id, title: String::new(), brain: brain.to_string(), created: now, updated: now, sessions: BTreeMap::new(), model: None, effort: None };
            self.save(&meta)?;
            return Ok(meta);
        }
        Err(std::io::Error::other("couldn't make a chat id"))
    }

    pub fn get(&self, id: &str) -> Option<Meta> {
        let text = std::fs::read_to_string(self.folder(id)?.join("chat.json")).ok()?;
        serde_json::from_str::<Meta>(&text).ok().filter(|m| m.id == id)
    }

    /// Every chat, the one used last first.
    pub fn list(&self) -> Vec<Meta> {
        let mut all: Vec<Meta> = std::fs::read_dir(&self.root)
            .map(|d| d.flatten().filter_map(|e| e.file_name().to_str().and_then(|id| self.get(id))).collect())
            .unwrap_or_default();
        all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
        all
    }

    /// Writes a chat's record. Not for a chat that was deleted since: one
    /// still open somewhere doesn't make it again.
    pub fn save(&self, meta: &Meta) -> std::io::Result<()> {
        let folder = self.folder(&meta.id).ok_or_else(|| std::io::Error::other("not a chat id"))?;
        if !folder.is_dir() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "the chat was deleted"));
        }
        let path = folder.join("chat.json");
        let tmp = folder.join(format!("chat.json.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(meta).unwrap_or_default())?;
        std::fs::rename(&tmp, &path)
    }

    /// Adds a line to a chat's thread.
    pub fn append(&self, id: &str, item: &Item) -> std::io::Result<()> {
        let folder = self.folder(id).ok_or_else(|| std::io::Error::other("not a chat id"))?;
        let item = match item {
            Item::Result { at, id, text, is_error } if text.len() > KEEP_RESULT => {
                let mut cut = KEEP_RESULT;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                &Item::Result { at: *at, id: id.clone(), text: format!("{}\n… ({} more bytes)", &text[..cut], text.len() - cut), is_error: *is_error }
            }
            other => other,
        };
        let mut line = serde_json::to_vec(item).unwrap_or_default();
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(folder.join("thread.jsonl"))?;
        file.write_all(&line)
    }

    /// A chat's thread, oldest first. A line that doesn't read (written by a
    /// newer Muxin, or cut short by a crash) is left out.
    #[allow(dead_code)] // For the app's page; the terminal only shows what's new.
    pub fn thread(&self, id: &str) -> Vec<Item> {
        let Some(folder) = self.folder(id) else { return Vec::new() };
        let text = std::fs::read_to_string(folder.join("thread.jsonl")).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// Names a chat, and gives back its record. No name (or only spaces):
    /// back to the first thing asked in it.
    pub fn rename(&self, id: &str, title: &str) -> std::io::Result<Meta> {
        let mut meta = self.get(id).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "there's no such chat"))?;
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        meta.title = if title.is_empty() {
            let asked = self.thread(id).into_iter().find_map(|item| match item {
                Item::User { text, .. } => Some(text),
                _ => None,
            });
            asked.map(|text| title_from(&text)).unwrap_or_default()
        } else if title.chars().count() > MOST_TITLE {
            title.chars().take(MOST_TITLE).chain(['…']).collect()
        } else {
            title
        };
        self.save(&meta)?;
        Ok(meta)
    }

    /// Deletes a chat: what was said in it, and the files attached in it.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        match self.folder(id) {
            Some(folder) => std::fs::remove_dir_all(folder),
            None => Err(std::io::Error::other("not a chat id")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("muxin-chat-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(&dir)
    }

    #[test]
    fn a_chat_is_made_listed_and_read_back() {
        let s = store("made");
        let mut a = s.create("claude").unwrap();
        assert!(valid_id(&a.id), "{}", a.id);
        assert_eq!(s.get(&a.id), Some(a.clone()));
        let mut b = s.create("codex").unwrap();
        // The one used last comes first.
        a.updated += 10;
        a.title = "Restart the runners".into();
        a.sessions.insert("claude".into(), "b59117ac".into());
        s.save(&a).unwrap();
        b.updated += 5;
        s.save(&b).unwrap();
        let ids: Vec<String> = s.list().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, [a.id.clone(), b.id.clone()]);
        assert_eq!(s.get(&a.id).unwrap().sessions["claude"], "b59117ac");
        assert!(s.work(&a.id).unwrap().ends_with(format!("chats/{}/work", a.id)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&s.root).unwrap().permissions().mode() & 0o777, 0o700);
        }
        s.delete(&b.id).unwrap();
        assert_eq!(s.list().len(), 1);
        // A chat that's gone isn't made again by something that still had it open.
        assert_eq!(s.save(&b).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert!(s.get(&b.id).is_none() && !s.root.join(&b.id).exists());
    }

    #[test]
    fn a_chat_is_named_and_unnamed_back_to_what_was_first_asked() {
        let s = store("named");
        let mut meta = s.create("codex").unwrap();
        meta.title = "restart beelink-01".into();
        s.save(&meta).unwrap();
        s.append(&meta.id, &Item::Text { at: 1, text: "A note before anything was asked.".into() }).unwrap();
        s.append(&meta.id, &Item::User { at: 2, text: "restart beelink-01\nand say why it stopped".into(), files: vec![] }).unwrap();

        // On one line, and kept on disk. When it was used last doesn't change.
        let named = s.rename(&meta.id, "  Runner \n restarts ").unwrap();
        assert_eq!((named.title.as_str(), named.updated), ("Runner restarts", meta.updated));
        assert_eq!(s.get(&meta.id).unwrap().title, "Runner restarts");
        // A very long one is cut.
        let long = s.rename(&meta.id, &"a".repeat(200)).unwrap().title;
        assert_eq!((long.chars().count(), long.ends_with('…')), (MOST_TITLE + 1, true));
        // No name: the first thing asked again.
        assert_eq!(s.rename(&meta.id, "   ").unwrap().title, "restart beelink-01");
        // A chat nothing was asked in has none.
        let empty = s.create("codex").unwrap();
        assert_eq!(s.rename(&empty.id, "").unwrap().title, "");
        assert_eq!(s.rename("c-gone", "x").unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert!(s.rename("../x", "x").is_err());
    }

    #[test]
    fn a_thread_keeps_its_lines_in_order_and_skips_what_it_cant_read() {
        let s = store("thread");
        let id = s.create("claude").unwrap().id;
        let items = [
            Item::User { at: 1, text: "restart beelink-01".into(), files: vec![] },
            Item::Tool { at: 2, id: "t1".into(), name: "runner_action".into(), args: json!({"runner_id": "beelink/beelink-01", "action": "restart"}) },
            Item::Ask { at: 2, id: 1, tool: "runner_action".into(), args: json!({"action": "restart"}), risk: Risk::Change },
            Item::Answer { at: 3, id: 1, answer: "yes".into(), note: String::new() },
            Item::Result { at: 4, id: "t1".into(), text: "restarted".into(), is_error: false },
            Item::Text { at: 5, text: "Done: beelink-01 is back and idle.".into() },
            Item::Usage { at: 5, input: 1200, output: 40, cached: 1000, cost: Some(0.01) },
        ];
        for item in &items {
            s.append(&id, item).unwrap();
        }
        // A line from a newer Muxin, and one cut short.
        let path = s.root.join(&id).join("thread.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"kind\":\"something_new\",\"at\":6}\n{\"kind\":\"text\",\"at\":7,\"te").unwrap();
        assert_eq!(s.thread(&id), items);
        assert!(std::fs::read_to_string(&path).unwrap().lines().next().unwrap().starts_with("{\"kind\":\"user\""));
    }

    #[test]
    fn a_long_result_is_cut_where_a_character_ends() {
        let s = store("long");
        let id = s.create("claude").unwrap().id;
        let text = "é".repeat(KEEP_RESULT);
        s.append(&id, &Item::Result { at: 1, id: "t".into(), text: text.clone(), is_error: false }).unwrap();
        let [Item::Result { text: kept, .. }] = &s.thread(&id)[..] else { panic!("one result") };
        assert!(kept.len() < text.len() && kept.contains("more bytes)"), "{}", kept.len());
    }

    #[test]
    fn ids_cant_leave_the_folder() {
        let s = store("ids");
        for bad in ["", "c-", "../x", "c-../x", "c-A", "c-a/b", "x-abc"] {
            assert!(!valid_id(bad), "{bad}");
            assert!(s.get(bad).is_none());
            assert!(s.append(bad, &Item::Text { at: 1, text: "x".into() }).is_err());
        }
    }

    #[test]
    fn a_title_is_the_first_line_shortened() {
        assert_eq!(title_from("\n  restart beelink-01\nand tell me why"), "restart beelink-01");
        let long = "a".repeat(100);
        assert_eq!(title_from(&long).chars().count(), 61);
        assert_eq!(title_from(""), "");
    }
}
