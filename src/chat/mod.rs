//! The assistant's chat: one conversation with a model that sees and works
//! with everything Muxin does, through Muxin's own tools (crate::mcp).
//!
//! - `brain`: what answers (Claude Code or Codex, on the user's own plan, or
//!   Muxin's own brain on their ChatGPT plan).
//! - `account` and `jwt`: signing in with ChatGPT.
//! - `bridge`: how such a brain reaches the tools of the Muxin that holds
//!   the chat, rather than a second Muxin of its own.
//! - `tools`: which of Muxin's tools the brain is given in full, and how it
//!   reaches the rest.
//! - `risk` and `gate`: what a tool call can change, and asking the user
//!   before it does.
//! - `store`: chats on disk.
//! - `prompt`: what the assistant is told about itself.
//! - `term`: the chat in a terminal (`muxin chat`).
//! - `live`: a chat running by itself, for the app's page.
//!
//! `Chat` puts them together: it holds one conversation and runs its turns.

pub mod account;
pub mod brain;
pub mod bridge;
pub mod gate;
pub mod jwt;
// Only the app's page runs a chat this way.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub mod live;
pub mod prompt;
pub mod risk;
pub mod store;
pub mod term;
#[cfg(test)]
pub mod testing;
pub mod tools;

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::fleet::Engine;
use bridge::Bridge;
use gate::{Gate, Mode};
use store::{Item, Meta, Store};

/// Which chat to open, and how.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// A chat's id to go on with, or "last" for the one used last. None: a new chat.
    pub chat: Option<String>,
    /// "claude" or "codex". None: the chat's own, else the user's setting,
    /// else the first one installed.
    pub brain: Option<String>,
    /// None: the user's setting (asking, unless they chose otherwise).
    pub mode: Option<Mode>,
}

/// What happens during a turn, for whoever shows the chat. (A call's `id`
/// pairs its result with it, which the terminal doesn't need: it prints them
/// in order.)
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Update {
    /// A piece of the assistant's text, as it comes.
    Delta(String),
    /// A whole stretch of its text (after its pieces, when there were any).
    Text(String),
    Tool { id: String, name: String, args: Value },
    Result { id: String, text: String, is_error: bool },
    /// A call waits for the user: answer with `Chat::gate().answer(id, ..)`.
    Ask(gate::Request),
    Usage { input: u64, output: u64, cached: u64, cost: Option<f64> },
    /// Something about the user's plan and its limits, in words.
    Limit(String),
    /// The model that's answering.
    Model(String),
}

/// One open conversation.
pub struct Chat {
    pub meta: Meta,
    store: Store,
    engine: Arc<Engine>,
    gate: Arc<Gate>,
    gate_events: mpsc::UnboundedReceiver<gate::Event>,
    brain: Box<dyn brain::Brain>,
    /// The last thing said about the plan's limits, so it's said once.
    limit: Option<String>,
    /// How a brain that's another program reaches Muxin's tools. Muxin's own needs none.
    _bridge: Option<Bridge>,
}

/// The chats kept for `engine`: the user's, or the simulated machines' own.
pub fn store(engine: &Engine) -> Store {
    let dir = &engine.paths().dir;
    if engine.is_demo() { Store::new(&dir.join("demo")) } else { Store::new(dir) }
}

impl Chat {
    /// Opens a chat: a new one, or one from before. Nothing runs until the
    /// first `send`. Call from inside the runtime.
    pub fn open(engine: &Arc<Engine>, options: Options) -> Result<Chat, String> {
        let settings = engine.settings().chat;
        let store = store(engine);
        let existing = match options.chat.as_deref() {
            None => None,
            Some("last") => Some(store.list().into_iter().next().ok_or("There's no chat to go on with yet.")?),
            Some(id) => Some(store.get(id).ok_or_else(|| format!("There's no chat {id}. `muxin chat --list` shows them."))?),
        };
        let wanted = options.brain.clone().or_else(|| existing.as_ref().map(|m| m.brain.clone())).or_else(|| Some(settings.brain.clone()).filter(|b| !b.trim().is_empty()));
        // Before anything is made: a brain that isn't here makes no chat.
        if let Some(id) = &wanted
            && brain::kind(id).is_none()
        {
            return Err(format!("\"{id}\" isn't a brain Muxin knows. Use {}.", brain::ids()));
        }
        let vault = account::Vault::of(&engine.paths().dir);
        let signed_in = account::current(&vault);
        let brain_id = match wanted {
            Some(id) => id,
            None => brain::available(signed_in.as_ref().is_some_and(|a| a.plan)).first().map(|k| k.id.to_string()).ok_or_else(brain::none_here)?,
        };
        let kind = brain::kind(&brain_id).ok_or_else(|| format!("\"{brain_id}\" isn't a brain Muxin knows. Use {}.", brain::ids()))?;
        match (kind.program, &signed_in) {
            (Some(program), _) if brain::find(program).is_none() => return Err(format!("{} isn't on this computer. Get it at {}.", kind.name, kind.get)),
            (None, None) => return Err("Sign in with ChatGPT first: `muxin chat --sign-in`.".into()),
            (None, Some(account)) if !account.plan => return Err("You're signed in with ChatGPT, but didn't allow Muxin to use your plan. Sign in again and allow it: `muxin chat --sign-in`.".into()),
            _ => {}
        }
        let mut meta = match existing {
            Some(meta) => meta,
            None => store.create(&brain_id).map_err(|e| format!("Couldn't make the chat: {e}"))?,
        };
        if meta.brain != brain_id {
            // Another brain takes over: it starts without the other's memory of the conversation.
            meta.brain = brain_id.clone();
            let _ = store.save(&meta);
        }
        let mode = options.mode.unwrap_or_else(|| Mode::from_setting(&settings.mode));
        let (gate, gate_events) = Gate::new(mode, settings.allow);
        let work = store.work(&meta.id).map_err(|e| format!("Couldn't make the chat's folder: {e}"))?;
        let (brain, bridge): (Box<dyn brain::Brain>, Option<Bridge>) = match kind.program {
            // Another program: it reaches Muxin's tools through a bridge to this Muxin.
            Some(_) => {
                let bridge = Bridge::start(engine.clone(), tools::Short::new(gate.clone()), &engine.paths().dir).map_err(|e| format!("Couldn't open the chat's way to Muxin's tools: {e}"))?;
                let setup = brain::Setup {
                    exe: std::env::current_exe().map_err(|e| format!("Couldn't find this Muxin's own program: {e}"))?,
                    bridge: bridge.path().to_path_buf(),
                    work,
                    prompt: prompt::system(),
                    resume: meta.sessions.get(&brain_id).cloned(),
                };
                (brain::start(&brain_id, setup)?, Some(bridge))
            }
            // Muxin's own: it calls the tools right here, and keeps the conversation in the chat's folder.
            None => {
                let folder = work.parent().unwrap_or(&work).to_path_buf();
                let model = Some(settings.model.clone()).filter(|m| !m.trim().is_empty());
                let own = brain::responses::ChatGpt::new(engine.clone(), gate.clone(), vault, account::Endpoints::default(), folder, prompt::system(), model);
                (Box::new(own), None)
            }
        };
        Ok(Chat { meta, store, engine: engine.clone(), gate, gate_events, brain, limit: None, _bridge: bridge })
    }

    /// The chat's gate: its mode, and where answers to waiting calls go.
    pub fn gate(&self) -> Arc<Gate> {
        self.gate.clone()
    }

    /// What was said and done so far.
    #[allow(dead_code)] // For the app's page; the terminal only shows what's new.
    pub fn thread(&self) -> Vec<Item> {
        self.store.thread(&self.meta.id)
    }

    /// Runs one turn: says `text` to the brain and reports what happens to
    /// `on` until it's done. `Err` says, for the user, why it couldn't.
    ///
    /// To stop a turn, drop this future and call `stop`.
    pub async fn send(&mut self, text: &str, on: &mut (dyn FnMut(Update) + Send)) -> Result<(), String> {
        let Chat { meta, store, engine, gate, gate_events, brain, limit, .. } = self;
        // A call that came in while no turn was running isn't this turn's to ask about.
        while let Ok(event) = gate_events.try_recv() {
            if let gate::Event::Asked(request) = &event {
                gate.answer(request.id, gate::Answer::No("It was called while no turn was running.".into()));
            }
        }
        while gate_events.try_recv().is_ok() {}
        let _ = store.append(&meta.id, &Item::User { at: store::now(), text: text.to_string() });
        if meta.title.is_empty() {
            meta.title = store::title_from(text);
        }
        meta.updated = store::now();
        let _ = store.save(meta);

        let (tx, mut said) = mpsc::unbounded_channel();
        let outcome = {
            let turn = brain.turn(text, &tx);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    // What the brain said first: a tool it called is shown before the question about it.
                    Some(event) = said.recv() => brain_event(event, meta, store, limit, on),
                    Some(event) = gate_events.recv() => gate_event(event, meta, store, engine, on),
                    outcome = &mut turn => break outcome,
                }
            }
        };
        // What was said just before it ended.
        while let Ok(event) = said.try_recv() {
            brain_event(event, meta, store, limit, on);
        }
        while let Ok(event) = gate_events.try_recv() {
            gate_event(event, meta, store, engine, on);
        }
        if let Err(why) = &outcome {
            let _ = store.append(&meta.id, &Item::Error { at: store::now(), text: why.clone() });
        }
        meta.updated = store::now();
        let _ = store.save(meta);
        outcome
    }

    /// Ends a turn that was dropped halfway: what waited for an answer gets a
    /// no, and the brain stops. The conversation can go on afterwards.
    pub async fn stop(&mut self) {
        self.gate.decline_all("The user stopped the turn.");
        self.brain.stop().await;
        while let Ok(event) = self.gate_events.try_recv() {
            gate_event(event, &self.meta, &self.store, &self.engine, &mut |_| {});
        }
        let _ = self.store.append(&self.meta.id, &Item::Error { at: store::now(), text: "Stopped.".into() });
    }
}

fn brain_event(event: brain::Event, meta: &mut Meta, store: &Store, limit: &mut Option<String>, on: &mut (dyn FnMut(Update) + Send)) {
    let at = store::now();
    match event {
        brain::Event::Delta(text) => on(Update::Delta(text)),
        brain::Event::Text(text) => {
            if !text.trim().is_empty() {
                let _ = store.append(&meta.id, &Item::Text { at, text: text.clone() });
            }
            on(Update::Text(text));
        }
        brain::Event::Tool { id, name, args } => {
            // Shown and kept as the tool it's really of, not the `use_tool` that carried it.
            let (name, args) = tools::shown(&name, args);
            let _ = store.append(&meta.id, &Item::Tool { at, id: id.clone(), name: name.clone(), args: args.clone() });
            on(Update::Tool { id, name, args });
        }
        brain::Event::Result { id, text, is_error } => {
            let _ = store.append(&meta.id, &Item::Result { at, id: id.clone(), text: text.clone(), is_error });
            on(Update::Result { id, text, is_error });
        }
        brain::Event::Usage { input, output, cached, cost } => {
            let _ = store.append(&meta.id, &Item::Usage { at, input, output, cached, cost });
            on(Update::Usage { input, output, cached, cost });
        }
        brain::Event::Limit(text) => {
            if limit.as_deref() != Some(text.as_str()) {
                *limit = Some(text.clone());
                on(Update::Limit(text));
            }
        }
        brain::Event::Model(name) => on(Update::Model(name)),
        brain::Event::Session(id) => {
            if meta.sessions.get(&meta.brain) != Some(&id) {
                meta.sessions.insert(meta.brain.clone(), id);
                let _ = store.save(meta);
            }
        }
    }
}

fn gate_event(event: gate::Event, meta: &Meta, store: &Store, engine: &Arc<Engine>, on: &mut (dyn FnMut(Update) + Send)) {
    let at = store::now();
    match event {
        gate::Event::Asked(request) => {
            let _ = store.append(&meta.id, &Item::Ask { at, id: request.id, tool: request.tool.clone(), args: request.args.clone(), risk: request.risk });
            on(Update::Ask(request));
        }
        gate::Event::Answered { id, answer } => {
            let note = match &answer {
                gate::Answer::No(note) => note.clone(),
                _ => String::new(),
            };
            let _ = store.append(&meta.id, &Item::Answer { at, id, answer: answer.word().to_string(), note });
        }
        gate::Event::Rules(rules) => {
            // "Don't ask again" holds for every chat from now on.
            let mut settings = engine.settings();
            settings.chat.allow = rules;
            if let Err(e) = engine.update_settings(settings) {
                tracing::warn!("chat: couldn't keep \"don't ask again\": {e}");
            }
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::config::Paths;
    use serde_json::json;

    /// A brain that says what it's told to, calling tools through the gate
    /// as a real one's would arrive.
    struct Scripted {
        gate: Arc<Gate>,
        turns: Vec<Vec<Step>>,
    }

    pub(crate) enum Step {
        Say(&'static str),
        Call(&'static str, Value),
        Fail(&'static str),
    }

    #[async_trait::async_trait]
    impl brain::Brain for Scripted {
        async fn turn(&mut self, _text: &str, events: &mpsc::UnboundedSender<brain::Event>) -> Result<(), String> {
            let _ = events.send(brain::Event::Session("thread-1".into()));
            for (n, step) in self.turns.remove(0).into_iter().enumerate() {
                match step {
                    Step::Say(text) => {
                        let _ = events.send(brain::Event::Delta(text.into()));
                        let _ = events.send(brain::Event::Text(text.into()));
                    }
                    Step::Call(tool, args) => {
                        let id = format!("t{n}");
                        let _ = events.send(brain::Event::Tool { id: id.clone(), name: tool.into(), args: args.clone() });
                        let (text, is_error) = match self.gate.check(tool, &args).await {
                            Ok(()) => ("ran".to_string(), false),
                            Err(why) => (why, true),
                        };
                        let _ = events.send(brain::Event::Result { id, text, is_error });
                    }
                    Step::Fail(why) => return Err(why.into()),
                }
            }
            let _ = events.send(brain::Event::Usage { input: 100, output: 10, cached: 80, cost: Some(0.01) });
            Ok(())
        }

        async fn stop(&mut self) {}
    }

    /// A chat on simulated machines whose brain follows a script: a list of steps for each turn.
    pub(crate) fn chat(name: &str, turns: Vec<Vec<Step>>) -> Chat {
        let dir = std::env::temp_dir().join(format!("muxin-chat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(Paths::resolve(Some(dir.clone()), None), true);
        let store = store(&engine);
        let meta = store.create("claude").unwrap();
        let (gate, gate_events) = Gate::new(Mode::Ask, vec![]);
        let bridge = Bridge::start(engine.clone(), tools::Short::new(gate.clone()), &dir).unwrap();
        let brain = Box::new(Scripted { gate: gate.clone(), turns });
        Chat { meta, store, engine, gate, gate_events, brain, limit: None, _bridge: Some(bridge) }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_is_shown_as_it_goes_and_kept() {
        let mut chat = chat("turn", vec![vec![Step::Call("status", json!({})), Step::Say("Nine machines, two need you.")]]);
        let mut seen = Vec::new();
        chat.send("what needs me?\nbe brief", &mut |u| seen.push(u)).await.unwrap();
        let kinds: Vec<&str> = seen
            .iter()
            .map(|u| match u {
                Update::Delta(_) => "delta",
                Update::Text(_) => "text",
                Update::Tool { .. } => "tool",
                Update::Result { .. } => "result",
                Update::Ask(_) => "ask",
                Update::Usage { .. } => "usage",
                Update::Limit(_) => "limit",
                Update::Model(_) => "model",
            })
            .collect();
        // A read asked nobody.
        assert_eq!(kinds, ["tool", "result", "delta", "text", "usage"]);
        assert_eq!(chat.meta.title, "what needs me?");
        assert_eq!(chat.meta.sessions["claude"], "thread-1");
        let thread = chat.thread();
        assert!(matches!(&thread[0], Item::User { text, .. } if text == "what needs me?\nbe brief"));
        assert!(matches!(&thread[1], Item::Tool { name, .. } if name == "status"));
        assert!(matches!(&thread[3], Item::Text { text, .. } if text == "Nine machines, two need you."));
        assert!(matches!(thread.last(), Some(Item::Usage { cost: Some(_), .. })));
        // It's on disk, for the next time the chat is opened.
        assert_eq!(store(&chat.engine).get(&chat.meta.id).unwrap().sessions["claude"], "thread-1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_asks_and_the_answer_is_kept_with_the_thread() {
        let restart = json!({"runner_id": "mac-studio/mac-studio-01", "action": "restart"});
        let mut chat = chat("ask", vec![vec![Step::Call("runner_action", restart.clone()), Step::Say("Left it alone.")], vec![Step::Call("runner_action", restart.clone())], vec![Step::Call("runner_action", restart)]]);
        let gate = chat.gate();

        // No, with a word why: the brain is told, and nothing ran.
        let mut results = Vec::new();
        chat.send("restart mac-studio-01", &mut |u| match u {
            Update::Ask(req) => {
                assert_eq!((req.tool.as_str(), req.rule.as_deref()), ("runner_action", Some("runner_action:restart")));
                gate.answer(req.id, gate::Answer::No("it's mid-job".into()));
            }
            Update::Result { text, is_error, .. } => results.push((text, is_error)),
            _ => {}
        })
        .await
        .unwrap();
        assert!(results[0].1 && results[0].0.contains("it's mid-job"), "{results:?}");
        let thread = chat.thread();
        assert!(thread.iter().any(|i| matches!(i, Item::Ask { tool, risk: risk::Risk::Change, .. } if tool == "runner_action")));
        assert!(thread.iter().any(|i| matches!(i, Item::Answer { answer, note, .. } if answer == "no" && note == "it's mid-job")));

        // "Don't ask again" is saved to the settings, and the next one runs unasked.
        let mut asked = 0;
        chat.send("do it now", &mut |u| {
            if let Update::Ask(req) = u {
                asked += 1;
                gate.answer(req.id, gate::Answer::Always);
            }
        })
        .await
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(chat.engine.settings().chat.allow, ["runner_action:restart"]);
        let mut asked = 0;
        let mut ran = false;
        chat.send("and again", &mut |u| match u {
            Update::Ask(_) => asked += 1,
            Update::Result { is_error, .. } => ran = !is_error,
            _ => {}
        })
        .await
        .unwrap();
        assert_eq!((asked, ran), (0, true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_from_outside_a_turn_gets_a_no_when_the_next_turn_starts() {
        let mut chat = chat("stray", vec![vec![Step::Say("Hello.")]]);
        let gate = chat.gate();
        let stray = tokio::spawn(async move { gate.check("session_stop", &json!({"session": "beelink/codex-1"})).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!stray.is_finished());
        let mut asked = 0;
        chat.send("hi", &mut |u| asked += matches!(u, Update::Ask(_)) as u32).await.unwrap();
        assert_eq!(asked, 0);
        assert!(stray.await.unwrap().unwrap_err().contains("no turn was running"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_turn_is_said_and_kept() {
        let mut chat = chat("fail", vec![vec![Step::Say("Looking…"), Step::Fail("Claude Code ended before it answered.")]]);
        let err = chat.send("hello", &mut |_| {}).await.unwrap_err();
        assert_eq!(err, "Claude Code ended before it answered.");
        assert!(matches!(chat.thread().last(), Some(Item::Error { text, .. }) if text == &err));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn opening_a_chat_that_isnt_there_says_so() {
        let dir = std::env::temp_dir().join(format!("muxin-chat-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(Paths::resolve(Some(dir), None), true);
        let err = |options| Chat::open(&engine, options).err().unwrap();
        assert!(err(Options { chat: Some("last".into()), ..Default::default() }).contains("no chat to go on with"));
        assert!(err(Options { chat: Some("c-nope12".into()), ..Default::default() }).contains("no chat c-nope12"));
        assert!(err(Options { brain: Some("gpt".into()), ..Default::default() }).contains("claude, codex or chatgpt"));
        // Nothing was made along the way.
        assert!(store(&engine).list().is_empty());
    }
}
