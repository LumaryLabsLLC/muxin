//! A chat that runs by itself, for the app. The app's page can't hold a
//! `Chat` while a turn runs (a turn can take minutes, and the page has to go
//! on drawing), so the chat lives in a task of its own: the page sends it
//! what the user typed and hears back what happens, as it happens.
//!
//! Answers to a call that waits for the user don't go through the task:
//! they go straight to the chat's gate (`Live::gate`), which is what the
//! waiting call listens to.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::gate::Gate;
use super::store::{Item, Meta};
use super::{Chat, Options, Update};
use crate::fleet::Engine;

enum Command {
    Send(String),
    Stop,
}

/// What a running chat tells the page.
#[derive(Clone, Debug)]
pub enum Event {
    /// A turn began, with this message.
    Started(String),
    /// Something happened in it.
    Update(Update),
    /// The turn is over. `Err` says why it didn't finish ("Stopped." when
    /// the user stopped it).
    Ended(Result<(), String>),
    /// What the chat is changed (its title, after the first message).
    Meta(Meta),
}

/// The page's end of a running chat. Dropping it ends the chat's task, and
/// with it the brain's process and the bridge.
pub struct Live {
    commands: mpsc::UnboundedSender<Command>,
    gate: Arc<Gate>,
}

/// An open chat: the way to talk to it, what it is, what was said before,
/// and what it reports from now on.
pub type Opened = (Live, Meta, Vec<Item>, mpsc::UnboundedReceiver<Event>);

impl Live {
    /// Opens a chat (see `Chat::open`) and starts its task on the engine's runtime.
    pub fn open(engine: &Arc<Engine>, options: Options) -> Result<Opened, String> {
        let _in_runtime = engine.runtime().enter();
        let chat = Chat::open(engine, options)?;
        let (meta, thread) = (chat.meta.clone(), chat.thread());
        let (live, events) = Live::run(chat, engine.runtime());
        Ok((live, meta, thread, events))
    }

    pub(crate) fn run(chat: Chat, runtime: &tokio::runtime::Handle) -> (Live, mpsc::UnboundedReceiver<Event>) {
        let gate = chat.gate();
        let (commands, inbox) = mpsc::unbounded_channel();
        let (events, heard) = mpsc::unbounded_channel();
        runtime.spawn(run(chat, inbox, events));
        (Live { commands, gate }, heard)
    }

    /// Says something to the assistant. While a turn runs, it waits its turn.
    pub fn send(&self, text: impl Into<String>) {
        let _ = self.commands.send(Command::Send(text.into()));
    }

    /// Stops the turn that's running, and drops what was waiting behind it.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }

    /// The chat's gate: its mode, and where the user's answers go.
    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }
}

async fn run(mut chat: Chat, mut inbox: mpsc::UnboundedReceiver<Command>, events: mpsc::UnboundedSender<Event>) {
    let mut waiting: VecDeque<String> = VecDeque::new();
    loop {
        let text = match waiting.pop_front() {
            Some(text) => text,
            None => match inbox.recv().await {
                Some(Command::Send(text)) => text,
                // Nothing is running.
                Some(Command::Stop) => continue,
                None => return,
            },
        };
        let _ = events.send(Event::Started(text.clone()));
        let outcome = {
            let out = events.clone();
            let mut on = move |update: Update| {
                let _ = out.send(Event::Update(update));
            };
            tokio::select! {
                outcome = chat.send(&text, &mut on) => Some(outcome),
                gone = stop_asked(&mut inbox, &mut waiting) => {
                    if gone {
                        // The page closed: the chat goes with this task.
                        return;
                    }
                    None
                }
            }
        };
        let outcome = match outcome {
            Some(outcome) => outcome,
            None => {
                waiting.clear();
                chat.stop().await;
                Err("Stopped.".to_string())
            }
        };
        let _ = events.send(Event::Meta(chat.meta.clone()));
        let _ = events.send(Event::Ended(outcome));
    }
}

/// Waits until the user asks to stop; what they send meanwhile waits its
/// turn. True when the page is gone instead.
async fn stop_asked(inbox: &mut mpsc::UnboundedReceiver<Command>, waiting: &mut VecDeque<String>) -> bool {
    loop {
        match inbox.recv().await {
            Some(Command::Stop) => return false,
            Some(Command::Send(text)) => waiting.push_back(text),
            None => return true,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::chat::gate::Answer;
    use crate::chat::tests::{Step, chat};
    use serde_json::json;

    async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(std::time::Duration::from_secs(20), events.recv()).await.expect("an event in time").expect("the chat is running")
    }

    /// Everything up to and including the end of a turn.
    async fn turn(events: &mut mpsc::UnboundedReceiver<Event>) -> (Vec<Event>, Result<(), String>) {
        let mut seen = Vec::new();
        loop {
            match next(events).await {
                Event::Ended(outcome) => return (seen, outcome),
                other => seen.push(other),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turns_run_one_after_another_and_report_as_they_go() {
        let chat = chat("live-turns", vec![vec![Step::Call("status", json!({})), Step::Say("Nine machines.")], vec![Step::Say("Two need you.")]]);
        let (live, mut events) = Live::run(chat, &tokio::runtime::Handle::current());
        // The second is sent before the first is done: it waits its turn.
        live.send("how many machines?");
        live.send("which need me?");

        let (seen, outcome) = turn(&mut events).await;
        assert_eq!(outcome, Ok(()));
        assert!(matches!(&seen[0], Event::Started(text) if text == "how many machines?"));
        assert!(seen.iter().any(|e| matches!(e, Event::Update(Update::Tool { name, .. }) if name == "status")));
        assert!(seen.iter().any(|e| matches!(e, Event::Update(Update::Text(text)) if text == "Nine machines.")));
        // The chat got its title from the first thing asked.
        assert!(matches!(seen.last(), Some(Event::Meta(meta)) if meta.title == "how many machines?"));

        let (seen, outcome) = turn(&mut events).await;
        assert_eq!(outcome, Ok(()));
        assert!(matches!(&seen[0], Event::Started(text) if text == "which need me?"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_goes_to_the_gate_and_the_turn_goes_on() {
        let chat = chat("live-ask", vec![vec![Step::Call("session_stop", json!({"session": "forge/codex-1"})), Step::Say("Ended it.")]]);
        let (live, mut events) = Live::run(chat, &tokio::runtime::Handle::current());
        live.send("end that session");
        let id = loop {
            if let Event::Update(Update::Ask(request)) = next(&mut events).await {
                assert_eq!((request.tool.as_str(), request.rule.as_deref()), ("session_stop", None));
                break request.id;
            }
        };
        assert!(live.gate().answer(id, Answer::Yes));
        let (seen, outcome) = turn(&mut events).await;
        assert_eq!(outcome, Ok(()));
        assert!(seen.iter().any(|e| matches!(e, Event::Update(Update::Result { is_error: false, .. }))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_ends_the_turn_says_no_to_what_waits_and_drops_what_was_queued() {
        let chat = chat("live-stop", vec![vec![Step::Call("session_stop", json!({})), Step::Say("never said")], vec![Step::Say("Still here.")]]);
        let (live, mut events) = Live::run(chat, &tokio::runtime::Handle::current());
        live.send("end it");
        live.send("queued behind it");
        loop {
            if let Event::Update(Update::Ask(_)) = next(&mut events).await {
                break;
            }
        }
        // The call waits for an answer that never comes: the user stops instead.
        live.stop();
        let (_, outcome) = turn(&mut events).await;
        assert_eq!(outcome, Err("Stopped.".into()));
        // The chat goes on with what's sent next, not with what was queued.
        live.send("are you there?");
        let (seen, outcome) = turn(&mut events).await;
        assert_eq!(outcome, Ok(()));
        assert!(matches!(&seen[0], Event::Started(text) if text == "are you there?"));
        // A stop with nothing running does nothing.
        live.stop();
        drop(live);
        assert!(events.recv().await.is_none());
    }
}
