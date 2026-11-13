//! The gate every tool call from the chat passes before it runs. It works
//! like Claude Code's permissions, in two modes:
//!
//! - `Ask`: calls that only read run. Anything else waits for the user, who
//!   answers Yes, "Yes, and don't ask again" (kept as a rule, see risk.rs), or
//!   No, with a note for the assistant if they like.
//! - `Bypass`: nothing asks.
//!
//! The assistant can't talk its way past this: it's Muxin that decides, from
//! the call's name and arguments, whatever the conversation said. That matters
//! because the assistant reads text other people wrote (an issue, a session's
//! screen, a log), and such text can ask for things the user never did.
//!
//! A waiting call is a `Request` sent to whoever shows it (the terminal, the
//! app's card); `answer` lets the call go on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::risk::{self, Risk};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ask,
    Bypass,
}

impl Mode {
    /// `[settings.chat] mode`: "bypass", or anything else for asking.
    pub fn from_setting(text: &str) -> Mode {
        if text.trim().eq_ignore_ascii_case("bypass") { Mode::Bypass } else { Mode::Ask }
    }
}

/// What the user said to a waiting call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    /// Yes, and the same kind of call runs without asking from now on.
    Always,
    /// No, with what to tell the assistant (may be empty).
    No(String),
}

impl Answer {
    pub fn word(&self) -> &'static str {
        match self {
            Answer::Yes => "yes",
            Answer::Always => "always",
            Answer::No(_) => "no",
        }
    }
}

/// A call waiting for the user.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: u64,
    pub tool: String,
    pub args: Value,
    pub risk: Risk,
    /// What "don't ask again" would remember; `None` when that isn't offered.
    pub rule: Option<String>,
}

/// What the gate tells whoever shows the chat.
#[derive(Clone, Debug)]
pub enum Event {
    Asked(Request),
    Answered { id: u64, answer: Answer },
    /// The saved rules changed: all of them, to keep.
    Rules(Vec<String>),
}

/// Whether a call runs or waits, with nothing else involved.
pub fn decide(mode: Mode, rules: &[String], tool: &str, args: &Value) -> bool {
    mode == Mode::Bypass || risk::of(tool, args) == Risk::Read || risk::covered(rules, tool, args)
}

pub struct Gate {
    mode: Mutex<Mode>,
    rules: Mutex<Vec<String>>,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
    next: AtomicU64,
    events: mpsc::UnboundedSender<Event>,
}

impl Gate {
    pub fn new(mode: Mode, rules: Vec<String>) -> (Arc<Gate>, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        (Arc::new(Gate { mode: Mutex::new(mode), rules: Mutex::new(rules), waiting: Mutex::default(), next: AtomicU64::new(1), events }), rx)
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    /// Lets a call through, or waits for the user's answer to it. `Err` is
    /// what the assistant is told when it may not run.
    pub async fn check(&self, tool: &str, args: &Value) -> Result<(), String> {
        let rules = self.rules.lock().unwrap().clone();
        if decide(self.mode(), &rules, tool, args) {
            return Ok(());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        let request = Request { id, tool: tool.to_string(), args: args.clone(), risk: risk::of(tool, args), rule: risk::rule(tool, args) };
        if self.events.send(Event::Asked(request.clone())).is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err("Nobody is there to approve this, so it didn't run.".into());
        }
        // Dropped without an answer: the chat closed, or the turn was stopped.
        let answer = rx.await.unwrap_or_else(|_| Answer::No("The chat was stopped before the user answered.".into()));
        match answer {
            Answer::Yes => Ok(()),
            Answer::Always => {
                if let Some(rule) = request.rule {
                    let all = {
                        let mut rules = self.rules.lock().unwrap();
                        if !rules.contains(&rule) {
                            rules.push(rule);
                        }
                        rules.clone()
                    };
                    let _ = self.events.send(Event::Rules(all));
                }
                Ok(())
            }
            Answer::No(note) => {
                let note = note.trim();
                Err(if note.is_empty() {
                    "The user said no to this, so it didn't run. Don't try it again; ask what they'd like instead.".to_string()
                } else {
                    format!("The user said no to this, so it didn't run. They said: {note}")
                })
            }
        }
    }

    /// The user's answer to a waiting call. False when it isn't waiting (any more).
    pub fn answer(&self, id: u64, answer: Answer) -> bool {
        let Some(tx) = self.waiting.lock().unwrap().remove(&id) else { return false };
        let _ = self.events.send(Event::Answered { id, answer: answer.clone() });
        tx.send(answer).is_ok()
    }

    /// Says no to every waiting call (the turn was stopped).
    pub fn decline_all(&self, why: &str) {
        let ids: Vec<u64> = self.waiting.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.answer(id, Answer::No(why.to_string()));
        }
    }
}

#[async_trait::async_trait]
impl crate::mcp::Guard for Gate {
    async fn allow(&self, tool: &str, args: &Value) -> Result<(), String> {
        self.check(tool, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn asking_lets_reads_through_and_bypass_lets_everything() {
        assert!(decide(Mode::Ask, &[], "status", &json!({})));
        assert!(!decide(Mode::Ask, &[], "session_start", &json!({})));
        assert!(!decide(Mode::Ask, &[], "cloud_destroy", &json!({})));
        assert!(decide(Mode::Bypass, &[], "cloud_destroy", &json!({})));
        let rules = vec!["session_start".to_string()];
        assert!(decide(Mode::Ask, &rules, "session_start", &json!({})));
        assert_eq!(Mode::from_setting("Bypass"), Mode::Bypass);
        assert_eq!(Mode::from_setting(""), Mode::Ask);
    }

    #[tokio::test]
    async fn a_change_waits_for_a_yes() {
        let (gate, mut events) = Gate::new(Mode::Ask, vec![]);
        let call = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.check("runner_action", &json!({"runner_id": "a/b", "action": "restart"})).await })
        };
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        assert_eq!((req.tool.as_str(), req.risk, req.rule.as_deref()), ("runner_action", Risk::Change, Some("runner_action:restart")));
        assert!(!call.is_finished());
        assert!(gate.answer(req.id, Answer::Yes));
        assert_eq!(call.await.unwrap(), Ok(()));
        // Answered once: a second answer has nothing to answer.
        assert!(!gate.answer(req.id, Answer::Yes));
        assert!(matches!(events.recv().await, Some(Event::Answered { answer: Answer::Yes, .. })));
    }

    #[tokio::test]
    async fn dont_ask_again_is_kept_and_then_nothing_asks() {
        let (gate, mut events) = Gate::new(Mode::Ask, vec![]);
        let args = json!({"runner_id": "a/b", "action": "restart"});
        let call = {
            let (gate, args) = (gate.clone(), args.clone());
            tokio::spawn(async move { gate.check("runner_action", &args).await })
        };
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        gate.answer(req.id, Answer::Always);
        assert_eq!(call.await.unwrap(), Ok(()));
        assert!(matches!(events.recv().await, Some(Event::Answered { .. })));
        let Some(Event::Rules(rules)) = events.recv().await else { panic!("rules") };
        assert_eq!(rules, ["runner_action:restart"]);
        // The same kind of call runs now; another action of the tool still asks.
        assert_eq!(gate.check("runner_action", &args).await, Ok(()));
        let other = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.check("runner_action", &json!({"action": "stop"})).await })
        };
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        gate.answer(req.id, Answer::No("leave it running".into()));
        let told = other.await.unwrap().unwrap_err();
        assert!(told.contains("said no") && told.contains("leave it running"), "{told}");
    }

    #[tokio::test]
    async fn danger_has_no_dont_ask_again() {
        let (gate, mut events) = Gate::new(Mode::Ask, vec!["cloud_destroy".to_string()]);
        let call = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.check("cloud_destroy", &json!({"machine_id": "x"})).await })
        };
        // A rule someone wrote by hand doesn't cover it either.
        let Some(Event::Asked(req)) = events.recv().await else { panic!("asked") };
        assert_eq!((req.risk, req.rule), (Risk::Danger, None));
        // "Always" on a call with no rule is a plain yes: nothing is saved.
        gate.answer(req.id, Answer::Always);
        assert_eq!(call.await.unwrap(), Ok(()));
        assert!(matches!(events.recv().await, Some(Event::Answered { .. })));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn stopping_says_no_to_what_waits_and_a_closed_chat_runs_nothing() {
        let (gate, mut events) = Gate::new(Mode::Ask, vec![]);
        let call = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.check("session_stop", &json!({})).await })
        };
        assert!(matches!(events.recv().await, Some(Event::Asked(_))));
        gate.decline_all("The user stopped the turn.");
        assert!(call.await.unwrap().unwrap_err().contains("stopped the turn"));

        // Nobody listening: the call is refused rather than left waiting.
        drop(events);
        assert!(gate.check("session_stop", &json!({})).await.unwrap_err().contains("Nobody is there"));
        // Reads never needed anyone.
        assert_eq!(gate.check("status", &json!({})).await, Ok(()));
    }
}
