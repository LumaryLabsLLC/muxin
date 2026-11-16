//! Machines that are asleep: Muxin lets them sleep.
//!
//! A sleeping Mac wakes for a packet sent to it (a dark wake: awake for a
//! moment, its display off), answers, and looks online; an app that keeps
//! its link up, or keeps checking, never lets it rest. The machine's agent
//! says when it's going to sleep and when it has woken (agent/src/sleep.rs),
//! so while a machine is asleep nothing here reaches for it: no link, no
//! request, no check by itself. What's left is a check of that machine by
//! name, Wake, and the agent's own notice that it woke (direct.rs).
//!
//! This is the list of them in this process. It follows the machines' last
//! checks (`ProbeResult::asleep`, which state.json shares between Muxin's
//! processes); what an agent just said is kept beside it for the check that
//! asked.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::config::MachineSpec;

/// What a machine's agent said when it said it's asleep.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Told {
    /// Where its notice goes when it wakes ("192.168.1.20:53312": this app,
    /// as the machine sees it). None: it won't send one (over ssh, or this
    /// app didn't ask).
    pub notice: Option<String>,
    /// A laptop with its lid closed.
    pub lid_closed: bool,
    /// This app asked for a notice and the agent couldn't get one through
    /// (on a Mac: it isn't allowed on the local network there).
    pub notice_failed: bool,
}

impl Told {
    /// From a link's hello, a ping's reply or its `sleep` event.
    pub fn from(message: &serde_json::Value) -> Told {
        Told {
            notice: message["wake_notice"].as_str().map(str::to_string),
            lid_closed: message["lid_closed"].as_bool() == Some(true),
            notice_failed: message["wake_notice"].is_null() && message["wake_notice_error"].is_string(),
        }
    }
}

#[derive(Default)]
struct State {
    /// By who says so (an engine: there's one a process, but for tests).
    asleep: HashMap<usize, HashSet<String>>,
    /// Machines being looked at on purpose, and by how many.
    looking: HashMap<String, usize>,
    told: HashMap<String, (Instant, Told)>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

type OnTold = Box<dyn Fn(&str, &Told) + Send + Sync>;

fn on_told() -> &'static Mutex<Option<OnTold>> {
    static HOOK: OnceLock<Mutex<Option<OnTold>>> = OnceLock::new();
    HOOK.get_or_init(Default::default)
}

/// Called when a machine's agent says it's asleep to something that reached
/// for it (the engine records it).
pub fn set_on_told(f: impl Fn(&str, &Told) + Send + Sync + 'static) {
    *on_told().lock().unwrap() = Some(Box::new(f));
}

/// Whether to leave the machine alone: it's asleep, and nobody is looking at
/// it on purpose.
pub fn is(machine_id: &str) -> bool {
    let st = state().lock().unwrap();
    st.asleep.values().any(|ids| ids.contains(machine_id)) && !st.looking.contains_key(machine_id)
}

/// How every refusal of a machine that's asleep ends.
pub const WAKE_FIRST: &str = "is asleep. Wake it first.";

/// Err for a machine that's asleep: what the user sees instead of Muxin
/// waking it by reaching for it.
pub fn refuse(spec: &MachineSpec) -> Result<(), String> {
    if is(&spec.id) {
        return Err(format!("{} {WAKE_FIRST}", spec.name));
    }
    Ok(())
}

/// The machines asleep now, as `owner`'s last checks of them say.
pub fn sync<'a>(owner: usize, asleep: impl Iterator<Item = &'a String>) {
    let ids: HashSet<String> = asleep.cloned().collect();
    let mut st = state().lock().unwrap();
    if ids.is_empty() {
        st.asleep.remove(&owner);
    } else if st.asleep.get(&owner) != Some(&ids) {
        st.asleep.insert(owner, ids);
    }
}

/// A machine's agent said it's asleep.
pub fn told(machine_id: &str, what: Told) {
    state().lock().unwrap().told.insert(machine_id.to_string(), (Instant::now(), what.clone()));
    if let Some(hook) = on_told().lock().unwrap().as_ref() {
        hook(machine_id, &what);
    }
}

/// What its agent said, when it said it's asleep since `since` (a check
/// asks, about its own try at the machine).
pub fn told_since(machine_id: &str, since: Instant) -> Option<Told> {
    state().lock().unwrap().told.get(machine_id).filter(|(at, _)| *at >= since).map(|(_, what)| what.clone())
}

/// While this is held, the machine may be reached though it's asleep: a
/// check of it by name, or waking it.
pub struct Looking(String);

pub fn look(machine_id: &str) -> Looking {
    *state().lock().unwrap().looking.entry(machine_id.to_string()).or_insert(0) += 1;
    Looking(machine_id.to_string())
}

impl Drop for Looking {
    fn drop(&mut self) {
        let mut st = state().lock().unwrap();
        if let Some(n) = st.looking.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                st.looking.remove(&self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asleep_machine_is_left_alone_unless_looked_at() {
        let spec: MachineSpec = toml::from_str("name = \"Book\"\nid = \"asleep-test-book\"\nhost = \"book.invalid\"").unwrap();
        let id = spec.id.clone();
        assert!(!is(&id) && refuse(&spec).is_ok());
        sync(1, [&id].into_iter());
        assert!(is(&id));
        assert_eq!(refuse(&spec).unwrap_err(), "Book is asleep. Wake it first.");
        {
            let _first = look(&id);
            let _second = look(&id);
            assert!(!is(&id) && refuse(&spec).is_ok());
            drop(_first);
            assert!(!is(&id));
        }
        assert!(is(&id));
        // Awake again, as its next check says.
        sync(1, std::iter::empty());
        assert!(!is(&id));
    }

    #[test]
    fn remembers_what_an_agent_just_said() {
        let before = Instant::now();
        assert_eq!(told_since("asleep-test-told", before), None);
        let said = Told::from(&serde_json::json!({ "event": "sleep", "wake_notice": "192.168.1.20:53312", "lid_closed": true }));
        assert_eq!(said, Told { notice: Some("192.168.1.20:53312".into()), lid_closed: true, notice_failed: false });
        // The agent tried the way to this app and nothing came back.
        let blocked = Told::from(&serde_json::json!({ "event": "sleep", "wake_notice_error": "no answer" }));
        assert_eq!(blocked, Told { notice: None, lid_closed: false, notice_failed: true });
        told("asleep-test-told", said.clone());
        assert_eq!(told_since("asleep-test-told", before), Some(said));
        assert_eq!(told_since("asleep-test-told", Instant::now() + std::time::Duration::from_secs(1)), None);
        assert_eq!(Told::from(&serde_json::json!({ "event": "hello", "asleep": true })), Told::default());
    }
}
