//! Droplets Muxin has asked DigitalOcean for whose answer it hasn't written
//! into config.toml yet, in cloud-pending.json.
//!
//! A droplet is billed from the moment DigitalOcean makes it, and that can be
//! before Muxin hears of it: the answer can be lost on the way, or Muxin can
//! be quit (or crash) while it waits. So what's asked for is written down
//! before asking, and crossed off once the droplet is a machine in
//! config.toml, or DigitalOcean says it made nothing. An entry that stays
//! (see ops::cloud's catch-up) is looked for by name later, by whichever
//! Muxin process checks the machines next.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::digitalocean::Droplet;

/// One droplet asked for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Asked {
    /// The machine's name in Muxin.
    pub name: String,
    /// The droplet's name at DigitalOcean (a slug of the machine's).
    pub droplet: String,
    pub region: String,
    pub size: String,
    /// USD an hour, as the size cost when it was asked for.
    pub price_hourly: f64,
    /// The account its first boot makes, which ssh signs in to.
    pub user: String,
    pub asked_at: DateTime<Utc>,
}

/// How long after asking an entry is left alone: the Muxin process that asked
/// may still be waiting for DigitalOcean's answer (30 seconds at most), or
/// asking whether the droplet was made (30 more).
pub const SETTLE_SECS: i64 = 90;

/// How far before `asked_at` DigitalOcean may say it made the droplet: this
/// computer's clock and DigitalOcean's don't agree to the second.
const CLOCK_SLACK_MINUTES: i64 = 5;

/// One writer at a time in this process (other processes: last one wins, as
/// for the other files here; an entry lost that way is one in a great many).
static FILE: Mutex<()> = Mutex::new(());

pub fn path(dir: &Path) -> PathBuf {
    dir.join("cloud-pending.json")
}

pub fn read(dir: &Path) -> Vec<Asked> {
    std::fs::read_to_string(path(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Whether anything's written down, without reading it (for every machine check).
pub fn any(dir: &Path) -> bool {
    path(dir).exists()
}

pub fn add(dir: &Path, asked: &Asked) -> Result<(), String> {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    all.push(asked.clone());
    write(dir, &all)
}

/// Crosses one off. The file goes with the last one.
pub fn remove(dir: &Path, asked: &Asked) {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    let before = all.len();
    all.retain(|a| !(a.droplet == asked.droplet && a.asked_at == asked.asked_at));
    if all.len() == before {
        return;
    }
    let done = if all.is_empty() { std::fs::remove_file(path(dir)).map_err(|e| e.to_string()) } else { write(dir, &all) };
    if let Err(e) = done {
        tracing::warn!("couldn't update {}: {e}", path(dir).display());
    }
}

fn write(dir: &Path, all: &[Asked]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(all).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&path(dir), text.as_bytes()).map_err(|e| format!("Couldn't write {}: {e:#}", crate::config::display_path(&path(dir))))
}

impl Asked {
    /// Old enough that the process that asked isn't still waiting for the answer.
    pub fn settled(&self, now: DateTime<Utc>) -> bool {
        now - self.asked_at >= Duration::seconds(SETTLE_SECS)
    }

    /// The droplet DigitalOcean made for this, among the ones with its name
    /// (`named`): made since it was asked for, where and as big as asked, and
    /// not a machine already (`known` ids). The first one made, if several.
    pub fn made<'a>(&self, named: &'a [Droplet], known: &[u64]) -> Option<&'a Droplet> {
        let since = self.asked_at - Duration::minutes(CLOCK_SLACK_MINUTES);
        named
            .iter()
            .filter(|d| d.id != 0 && !known.contains(&d.id) && d.name.eq_ignore_ascii_case(&self.droplet))
            .filter(|d| d.region == self.region && d.size == self.size)
            .filter_map(|d| DateTime::parse_from_rfc3339(&d.created_at).ok().map(|t| (t.with_timezone(&Utc), d)))
            .filter(|(made_at, _)| *made_at >= since)
            .min_by_key(|(made_at, _)| *made_at)
            .map(|(_, d)| d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn asked(name: &str, at: &str) -> Asked {
        Asked { name: name.into(), droplet: crate::util::slug(name), region: "nyc3".into(), size: "s-1vcpu-1gb".into(), price_hourly: 0.00893, user: "me".into(), asked_at: t(at) }
    }

    fn droplet(id: u64, name: &str, made_at: &str) -> Droplet {
        Droplet { id, name: name.into(), status: "new".into(), ipv4: None, region: "nyc3".into(), size: "s-1vcpu-1gb".into(), price_hourly: 0.00893, created_at: made_at.into() }
    }

    #[test]
    fn keeps_what_was_asked_for_until_its_crossed_off() {
        let dir = std::env::temp_dir().join(format!("muxin-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!any(&dir) && read(&dir).is_empty());
        let (one, two) = (asked("Dev", "2026-11-16T10:00:00Z"), asked("Dev", "2026-11-16T10:05:00Z"));
        add(&dir, &one).unwrap();
        add(&dir, &two).unwrap();
        assert!(any(&dir));
        assert_eq!(read(&dir), [one.clone(), two.clone()]);
        // By name and time: the same name asked for twice is two entries.
        remove(&dir, &one);
        assert_eq!(read(&dir), [two.clone()]);
        remove(&dir, &one);
        assert_eq!(read(&dir), [two.clone()]);
        remove(&dir, &two);
        assert!(!any(&dir), "the file goes with the last entry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_droplet_made_for_it() {
        let a = asked("Dev Box", "2026-11-16T10:00:00Z");
        assert!(!a.settled(t("2026-11-16T10:01:00Z")) && a.settled(t("2026-11-16T10:01:30Z")));
        let named = [
            // One the user made by hand last week, under the same name.
            droplet(1, "dev-box", "2026-11-09T09:00:00Z"),
            // The one this asked for (DigitalOcean's clock a little behind), and one asked for again later.
            droplet(2, "Dev-Box", "2026-11-16T09:59:58Z"),
            droplet(3, "dev-box", "2026-11-16T10:04:00Z"),
            droplet(4, "dev-box-2", "2026-11-16T10:00:01Z"),
        ];
        assert_eq!(a.made(&named, &[]).map(|d| d.id), Some(2));
        // Already a machine in Muxin: the next one.
        assert_eq!(a.made(&named, &[2]).map(|d| d.id), Some(3));
        assert_eq!(a.made(&named[..1], &[]).map(|d| d.id), None);
        // Another size or region isn't what was asked for.
        let mut other = droplet(5, "dev-box", "2026-11-16T10:00:01Z");
        other.size = "s-2vcpu-4gb".into();
        assert!(a.made(&[other], &[]).is_none());
        let mut unread = droplet(6, "dev-box", "");
        unread.region = "nyc3".into();
        assert!(a.made(&[unread], &[]).is_none(), "no creation time, no match");
    }
}
