//! Droplets Muxin has asked DigitalOcean for whose answer it hasn't written
//! into config.toml yet, in cloud-pending.json.
//!
//! A droplet is billed from the moment DigitalOcean makes it, and that can be
//! before Muxin hears of it: the answer can be lost on the way, or Muxin can
//! be quit (or crash) while it waits. So what's asked for is written down
//! before asking, and crossed off once the droplet is a machine in
//! config.toml, or it's sure that DigitalOcean made nothing. An entry that
//! stays (see ops::cloud's catch-up) is looked for by name later, by
//! whichever Muxin process checks the machines next, and is shown to the
//! user (`status.attention`) for as long as it's there.
//!
//! A droplet has nothing on it that says which request made it, so the one
//! made for an entry is told by its name, region and size, and by when it was
//! made: within `WINDOW_SECS` of the asking, by DigitalOcean's own clock (its
//! answers say what time it has; this computer's clock may be minutes off).

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
    /// When it was asked for, by this computer's clock.
    pub asked_at: DateTime<Utc>,
    /// How far DigitalOcean's clock was ahead of this computer's then, in
    /// seconds (see `Client::clock_ahead`); None when it hadn't said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_ahead_secs: Option<i64>,
    /// Why it couldn't be settled the last time it was looked for, in words
    /// for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// How long after the asking DigitalOcean may still make the droplet: its
/// answer is waited for 30 seconds, and a request it's slow with can go
/// through after that. Past this (and the slack), a droplet of that name
/// isn't this entry's, and none means none was made.
pub const WINDOW_SECS: i64 = 120;

/// How long after asking an entry is left to the Muxin process that asked: it
/// waits for the answer, and when that's lost it looks for the droplet until
/// the window has closed (ops::cloud `LOOKS`). Past this, any Muxin looks for
/// it, and it's shown to the user.
pub const SETTLE_SECS: i64 = 240;

/// How far the two clocks may still be apart once DigitalOcean has said what
/// time it has (its `Date` header counts in whole seconds, and the answer
/// took a moment to get here).
const SLACK_SECS: i64 = 30;

/// And when it never said: this computer's clock is all there is to go by.
const UNKNOWN_CLOCK_SLACK_SECS: i64 = 300;

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

/// Writes one down whatever is there already (tests: what another Muxin left).
#[cfg(test)]
pub fn add(dir: &Path, asked: &Asked) -> Result<(), String> {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    all.push(asked.clone());
    write(dir, &all)
}

/// Writes one down unless a droplet of that name is asked for already (two
/// of one name couldn't be told apart afterwards). False: it is, and nothing
/// was written.
pub fn claim(dir: &Path, asked: &Asked) -> Result<bool, String> {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    if all.iter().any(|a| a.droplet.eq_ignore_ascii_case(&asked.droplet)) {
        return Ok(false);
    }
    all.push(asked.clone());
    write(dir, &all).map(|()| true)
}

/// Crosses one off. The file goes with the last one.
pub fn remove(dir: &Path, asked: &Asked) {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    let before = all.len();
    all.retain(|a| !a.is(asked));
    if all.len() == before {
        return;
    }
    let done = if all.is_empty() { std::fs::remove_file(path(dir)).map_err(|e| e.to_string()) } else { write(dir, &all) };
    if let Err(e) = done {
        tracing::warn!("couldn't update {}: {e}", path(dir).display());
    }
}

/// Notes why one couldn't be settled (None: it has no trouble now).
pub fn set_problem(dir: &Path, asked: &Asked, problem: Option<String>) {
    let _one = FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read(dir);
    let Some(entry) = all.iter_mut().find(|a| a.is(asked)) else { return };
    if entry.problem == problem {
        return;
    }
    entry.problem = problem;
    if let Err(e) = write(dir, &all) {
        tracing::warn!("{e}");
    }
}

fn write(dir: &Path, all: &[Asked]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(all).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&path(dir), text.as_bytes()).map_err(|e| format!("Couldn't write {}: {e:#}", crate::config::display_path(&path(dir))))
}

impl Asked {
    /// The same asking (whatever was noted on it since).
    fn is(&self, other: &Asked) -> bool {
        self.droplet == other.droplet && self.asked_at == other.asked_at
    }

    /// Old enough that the process that asked isn't still working on it.
    pub fn settled(&self, now: DateTime<Utc>) -> bool {
        now - self.asked_at >= Duration::seconds(SETTLE_SECS)
    }

    /// When DigitalOcean may have made the droplet for this, by its own
    /// clock: from, until. `ahead_now` is how far its clock is ahead of this
    /// computer's as its latest answer had it, for an entry that was written
    /// before it had said.
    fn window(&self, ahead_now: Option<i64>) -> (DateTime<Utc>, DateTime<Utc>) {
        let (ahead, slack) = match self.clock_ahead_secs.or(ahead_now) {
            Some(ahead) => (ahead, SLACK_SECS),
            None => (0, UNKNOWN_CLOCK_SLACK_SECS),
        };
        let asked = self.asked_at + Duration::seconds(ahead);
        (asked - Duration::seconds(slack), asked + Duration::seconds(WINDOW_SECS + slack))
    }

    /// The droplets DigitalOcean may have made for this, among the ones with
    /// its name (`named`): made when it was asked for (see `window`), where
    /// and as big as asked, and not machines already (`known` ids). The
    /// earliest first. One is the droplet; several can't be told apart.
    pub fn made<'a>(&self, named: &'a [Droplet], known: &[u64], ahead_now: Option<i64>) -> Vec<&'a Droplet> {
        let (from, until) = self.window(ahead_now);
        let mut fit: Vec<(DateTime<Utc>, &Droplet)> = named
            .iter()
            .filter(|d| d.id != 0 && !known.contains(&d.id) && d.name.eq_ignore_ascii_case(&self.droplet))
            .filter(|d| d.region == self.region && d.size == self.size)
            .filter_map(|d| DateTime::parse_from_rfc3339(&d.created_at).ok().map(|t| (t.with_timezone(&Utc), d)))
            .filter(|(made_at, _)| *made_at >= from && *made_at <= until)
            .collect();
        fit.sort_by_key(|(made_at, d)| (*made_at, d.id));
        fit.into_iter().map(|(_, d)| d).collect()
    }

    /// Whether DigitalOcean makes nothing for this anymore: its clock (this
    /// computer's `now`, and how far its own is ahead) is past the window.
    /// From then, a list with no droplet for it means none was made.
    pub fn closed(&self, now: DateTime<Utc>, ahead_now: Option<i64>) -> bool {
        // The time there: by what it says now, else by what it said when this was asked.
        now + Duration::seconds(ahead_now.or(self.clock_ahead_secs).unwrap_or(0)) > self.window(ahead_now).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Asked for with the two clocks agreeing.
    fn asked(name: &str, at: &str) -> Asked {
        Asked {
            name: name.into(),
            droplet: crate::util::slug(name),
            region: "nyc3".into(),
            size: "s-1vcpu-1gb".into(),
            price_hourly: 0.00893,
            user: "me".into(),
            asked_at: t(at),
            clock_ahead_secs: Some(0),
            problem: None,
        }
    }

    fn droplet(id: u64, name: &str, made_at: &str) -> Droplet {
        Droplet { id, name: name.into(), status: "new".into(), ipv4: None, region: "nyc3".into(), size: "s-1vcpu-1gb".into(), price_hourly: 0.00893, created_at: made_at.into() }
    }

    fn ids(found: Vec<&Droplet>) -> Vec<u64> {
        found.iter().map(|d| d.id).collect()
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxin-pending-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keeps_what_was_asked_for_until_its_crossed_off() {
        let dir = folder("kept");
        assert!(!any(&dir) && read(&dir).is_empty());
        let (one, two) = (asked("Dev", "2026-11-16T10:00:00Z"), asked("Dev", "2026-11-16T10:05:00Z"));
        add(&dir, &one).unwrap();
        add(&dir, &two).unwrap();
        assert!(any(&dir));
        assert_eq!(read(&dir), [one.clone(), two.clone()]);
        // Why one can't be settled is kept with it, and it's still the same asking.
        set_problem(&dir, &two, Some("Not signed in.".into()));
        assert_eq!(read(&dir)[1].problem.as_deref(), Some("Not signed in."));
        set_problem(&dir, &two, None);
        assert_eq!(read(&dir), [one.clone(), two.clone()]);
        // By name and time: the same name asked for twice is two entries.
        remove(&dir, &one);
        assert_eq!(read(&dir), [two.clone()]);
        remove(&dir, &one);
        assert_eq!(read(&dir), [two.clone()]);
        set_problem(&dir, &two, Some("Not signed in.".into()));
        remove(&dir, &two);
        assert!(!any(&dir), "the file goes with the last entry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A droplet's name is all that tells it from another, so one name is
    /// asked for once at a time ("Dev Box" and "dev-box" are one name there).
    #[test]
    fn a_name_is_asked_for_once_at_a_time() {
        let dir = folder("claim");
        let first = asked("Dev Box", "2026-11-16T10:00:00Z");
        assert!(claim(&dir, &first).unwrap());
        assert!(!claim(&dir, &asked("dev-box", "2026-11-16T10:00:01Z")).unwrap());
        assert!(!claim(&dir, &asked("DEV BOX", "2026-11-16T10:00:02Z")).unwrap());
        assert!(claim(&dir, &asked("Dev Box 2", "2026-11-16T10:00:03Z")).unwrap());
        assert_eq!(read(&dir).iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Dev Box", "Dev Box 2"]);
        remove(&dir, &first);
        assert!(claim(&dir, &asked("dev-box", "2026-11-16T10:00:04Z")).unwrap(), "free again once it's crossed off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An entry written by a Muxin that didn't note the clocks reads too.
    #[test]
    fn reads_an_entry_without_the_newer_keys() {
        let dir = folder("older");
        std::fs::create_dir_all(&dir).unwrap();
        let text = r#"[{"name":"Dev","droplet":"dev","region":"nyc3","size":"s-1vcpu-1gb","priceHourly":0.00893,"user":"me","askedAt":"2026-11-16T10:00:00Z"}]"#;
        std::fs::write(path(&dir), text).unwrap();
        let all = read(&dir);
        assert_eq!((all.len(), all[0].clock_ahead_secs, all[0].problem.clone()), (1, None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_droplet_made_for_it() {
        let a = asked("Dev Box", "2026-11-16T10:00:00Z");
        assert!(!a.settled(t("2026-11-16T10:03:59Z")) && a.settled(t("2026-11-16T10:04:00Z")));
        let named = [
            // One the user made by hand last week, under the same name.
            droplet(1, "dev-box", "2026-11-09T09:00:00Z"),
            // The one this asked for (DigitalOcean's clock read a little behind).
            droplet(2, "Dev-Box", "2026-11-16T09:59:58Z"),
            // Another name.
            droplet(4, "dev-box-2", "2026-11-16T10:00:01Z"),
        ];
        assert_eq!(ids(a.made(&named, &[], None)), [2]);
        // Already a machine in Muxin: not looked at again.
        assert!(a.made(&named, &[2], None).is_empty());
        assert!(a.made(&named[..1], &[], None).is_empty());
        // Another size or region isn't what was asked for.
        let mut other = droplet(5, "dev-box", "2026-11-16T10:00:01Z");
        other.size = "s-2vcpu-4gb".into();
        assert!(a.made(&[other], &[], None).is_empty());
        assert!(a.made(&[droplet(6, "dev-box", "")], &[], None).is_empty(), "no creation time, no match");
    }

    /// The window is closed on both sides: a droplet of that name made
    /// before the asking, or well after it, is somebody else's.
    #[test]
    fn a_droplet_made_at_another_time_isnt_it() {
        let a = asked("Dev Box", "2026-11-16T10:00:00Z");
        let made = |at: &str| ids(a.made(&[droplet(9, "dev-box", at)], &[], None));
        assert_eq!(made("2026-11-16T09:55:00Z"), [0u64; 0], "five minutes before it was asked for");
        assert_eq!(made("2026-11-16T09:59:29Z"), [0u64; 0]);
        assert_eq!(made("2026-11-16T09:59:30Z"), [9], "the clocks' slack");
        assert_eq!(made("2026-11-16T10:02:30Z"), [9], "a slow request, and the slack");
        assert_eq!(made("2026-11-16T10:02:31Z"), [0u64; 0]);
        assert_eq!(made("2026-11-16T10:04:00Z"), [0u64; 0], "made by hand a few minutes later");
        assert_eq!(made("2026-11-19T10:00:00Z"), [0u64; 0], "days later, with the entry still there");

        // Two in the window can't be told apart: both are given, the earliest first.
        let two = [droplet(8, "dev-box", "2026-11-16T10:00:20Z"), droplet(7, "dev-box", "2026-11-16T10:00:02Z")];
        assert_eq!(ids(a.made(&two, &[], None)), [7, 8]);

        // Nothing there means nothing was made only once the window has closed, by DigitalOcean's clock.
        assert!(!a.closed(t("2026-11-16T10:02:30Z"), Some(0)));
        assert!(a.closed(t("2026-11-16T10:02:31Z"), Some(0)));
    }

    /// DigitalOcean says when it made a droplet by its own clock. This
    /// computer's may be minutes off, either way: what counts is how far
    /// apart they were when it was asked for.
    #[test]
    fn this_computers_clock_being_wrong_doesnt_lose_the_droplet() {
        // This computer is ten minutes fast: it asked at 10:10 by its clock, 10:00 by DigitalOcean's.
        let fast = Asked { clock_ahead_secs: Some(-600), ..asked("Dev Box", "2026-11-16T10:10:00Z") };
        let named = [droplet(2, "dev-box", "2026-11-16T10:00:03Z")];
        assert_eq!(ids(fast.made(&named, &[], Some(-600))), [2]);
        // One made at "its" 10:10 isn't it.
        assert!(fast.made(&[droplet(3, "dev-box", "2026-11-16T10:10:03Z")], &[], Some(-600)).is_empty());
        assert!(!fast.closed(t("2026-11-16T10:12:00Z"), Some(-600)), "10:02 there");
        assert!(fast.closed(t("2026-11-16T10:12:31Z"), Some(-600)));

        // Ten minutes slow.
        let slow = Asked { clock_ahead_secs: Some(600), ..asked("Dev Box", "2026-11-16T09:50:00Z") };
        assert_eq!(ids(slow.made(&named, &[], Some(600))), [2]);

        // An entry written before DigitalOcean had said what time it has goes by what it says now.
        let unknown = Asked { clock_ahead_secs: None, ..asked("Dev Box", "2026-11-16T10:10:00Z") };
        assert_eq!(ids(unknown.made(&named, &[], Some(-600))), [2]);
        // And when it never says, by this computer's clock, with more slack (five minutes either way).
        let here = Asked { clock_ahead_secs: None, ..asked("Dev Box", "2026-11-16T10:00:00Z") };
        assert_eq!(ids(here.made(&[droplet(4, "dev-box", "2026-11-16T09:56:00Z")], &[], None)), [4]);
        assert!(here.made(&[droplet(4, "dev-box", "2026-11-16T09:54:00Z")], &[], None).is_empty());
        assert!(!here.closed(t("2026-11-16T10:07:00Z"), None) && here.closed(t("2026-11-16T10:07:01Z"), None));
    }
}
