//! The assistant's chat: one conversation with a model that sees and works
//! with everything Muxin does, through Muxin's own tools (crate::mcp).
//!
//! - `brain`: what answers (Claude Code, Codex or Grok Build, on the user's
//!   own plan).
//! - `bridge`: how a brain reaches the tools of the Muxin that holds the
//!   chat, rather than a second Muxin of its own.
//! - `tools`: which of Muxin's tools the brain is given in full, and how it
//!   reaches the rest.
//! - `risk` and `gate`: what a tool call can change, and asking the user
//!   before it does.
//! - `store`: chats on disk.
//! - `attach` and `mention`: what goes with a message, files and the things it points at.
//! - `prompt`: what the assistant is told about itself.
//! - `command`: what a message that starts with "/" does instead of being said.
//! - `term`: the chat in a terminal (`muxin chat`).
//! - `live`: a chat running by itself, for the app's page.
//!
//! `Chat` puts them together: it holds one conversation and runs its turns.

pub mod attach;
pub mod brain;
pub mod bridge;
pub mod command;
pub mod gate;
// Only the app's page runs a chat this way.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub mod live;
pub mod mention;
pub mod prompt;
pub mod risk;
pub mod store;
pub mod term;
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
    /// "claude", "codex" or "grok". None: the chat's own, else the user's setting,
    /// else the first one installed.
    pub brain: Option<String>,
    /// None: the user's setting (asking, unless they chose otherwise).
    pub mode: Option<Mode>,
    /// The model to answer with, by the brain's id for it (brain/catalog.rs),
    /// and how hard it's asked to think. None: the chat's own, else what was
    /// picked last for that brain, else the brain's own choice.
    pub model: Option<String>,
    pub effort: Option<String>,
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
    /// The files that go with the next message (`attach`).
    staged: Vec<attach::Attached>,
    /// How a brain that's another program reaches Muxin's tools. Muxin's own needs none.
    _bridge: Bridge,
}

/// The chats kept for `engine`: the user's, or the simulated machines' own.
pub fn store(engine: &Engine) -> Store {
    let dir = &engine.paths().dir;
    if engine.is_demo() { Store::new(&dir.join("demo")) } else { Store::new(dir) }
}

/// A chat from before: by its id, or `last` for the one used last. `Err`
/// says, for the user, that there's none.
pub fn find(store: &Store, which: &str) -> Result<Meta, String> {
    match which {
        "last" => store.list().into_iter().next().ok_or_else(|| "There's no chat to go on with yet.".to_string()),
        id => store.get(id).ok_or_else(|| format!("There's no chat {id}. `muxin chat --list` shows them.")),
    }
}

/// Said when a chat that's open here was deleted somewhere else.
const DELETED: &str = "This chat was deleted.";

/// Saves a chat's record, with the title it has on disk now: it may have
/// been renamed since it was opened here (in the app, from another terminal).
fn save(store: &Store, meta: &mut Meta) {
    if let Some(kept) = store.get(&meta.id) {
        meta.title = kept.title;
    }
    let _ = store.save(meta);
}

impl Chat {
    /// Opens a chat: a new one, or one from before. Nothing runs until the
    /// first `send`. Call from inside the runtime.
    pub fn open(engine: &Arc<Engine>, options: Options) -> Result<Chat, String> {
        let settings = engine.settings().chat;
        let store = store(engine);
        let existing = options.chat.as_deref().map(|which| find(&store, which)).transpose()?;
        // Before anything is made: a brain that isn't one makes no chat.
        if let Some(id) = &options.brain
            && brain::kind(id).is_none()
        {
            return Err(format!("\"{id}\" isn't a brain Muxin knows. Use {}.", brain::ids()));
        }
        // What was asked for, else the chat's own, else the user's setting. One
        // of those that Muxin no longer has (a chat ChatGPT answered, a setting
        // from then) is passed over: the first there is takes it up.
        let known = |id: &str| Some(id.to_string()).filter(|id| brain::kind(id).is_some());
        let wanted = options.brain.clone().or_else(|| existing.as_ref().and_then(|m| known(&m.brain))).or_else(|| known(&settings.brain));
        let brain_id = match wanted {
            Some(id) => id,
            None => brain::available().first().map(|k| k.id.to_string()).ok_or_else(brain::none_here)?,
        };
        let kind = brain::kind(&brain_id).ok_or_else(|| format!("\"{brain_id}\" isn't a brain Muxin knows. Use {}.", brain::ids()))?;
        if brain::find(kind.program).is_none() {
            return Err(format!("{} isn't on this computer. Get it at {}.", kind.name, kind.get));
        }
        let mut meta = match existing {
            Some(meta) => meta,
            None => store.create(&brain_id).map_err(|e| format!("Couldn't make the chat: {e}"))?,
        };
        let other_brain = meta.brain != brain_id;
        if other_brain {
            // Another brain takes over: it starts without the other's memory
            // of the conversation, and with a model of its own.
            meta.brain = brain_id.clone();
            meta.model = None;
            meta.effort = None;
        }
        let some = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let (picked_model, picked_effort) = settings.pick(&brain_id);
        let model = some(options.model).or_else(|| meta.model.clone()).or(picked_model);
        // An effort belongs to a model: one picked for another model isn't carried over.
        let effort = some(options.effort).or_else(|| meta.effort.clone()).or(picked_effort);
        if other_brain || (&meta.model, &meta.effort) != (&model, &effort) {
            meta.model = model.clone();
            meta.effort = effort.clone();
            let _ = store.save(&meta);
        }
        let mode = options.mode.unwrap_or_else(|| Mode::from_setting(&settings.mode));
        let (gate, gate_events) = Gate::new(mode, settings.allow);
        let work = store.work(&meta.id).map_err(|e| format!("Couldn't make the chat's folder: {e}"))?;
        // It reaches Muxin's tools through a bridge to this Muxin: the short
        // list, or all of them for a program that finds tools by itself.
        let front: Arc<dyn crate::mcp::Front> = if kind.finds_tools { tools::Whole::new(gate.clone()) } else { tools::Short::new(gate.clone()) };
        let bridge = Bridge::start(engine.clone(), front, &engine.paths().dir).map_err(|e| format!("Couldn't open the chat's way to Muxin's tools: {e}"))?;
        let setup = brain::Setup {
            exe: std::env::current_exe().map_err(|e| format!("Couldn't find this Muxin's own program: {e}"))?,
            bridge: bridge.path().to_path_buf(),
            work,
            prompt: prompt::system(),
            resume: meta.sessions.get(&brain_id).cloned(),
            model,
            effort,
        };
        let brain = brain::start(&brain_id, setup)?;
        Ok(Chat { meta, store, engine: engine.clone(), gate, gate_events, brain, limit: None, staged: Vec::new(), _bridge: bridge })
    }

    /// Answers with this model, at this effort, from the next turn on (None:
    /// the brain's own choice). The conversation goes on as it was.
    pub fn choose(&mut self, model: Option<String>, effort: Option<String>) {
        let some = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let (model, effort) = (some(model), some(effort));
        self.meta.model = model.clone();
        self.meta.effort = effort.clone();
        save(&self.store, &mut self.meta);
        self.brain.choose(model, effort);
    }

    /// Attaches files to the next message: each is checked, and copied into
    /// the chat's folder. `Err` says, for the user, which one can't be
    /// attached and why; none of them is then.
    pub fn attach(&mut self, paths: &[std::path::PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        if self.staged.len() + paths.len() > attach::MOST_FILES {
            return Err(format!("That's {} files. A message takes {}.", self.staged.len() + paths.len(), attach::MOST_FILES));
        }
        if self.store.get(&self.meta.id).is_none() {
            return Err(DELETED.into());
        }
        let home = self.store.home(&self.meta.id).ok_or("This chat has no folder to keep files in.")?;
        self.staged.extend(attach::take(&home, paths)?);
        Ok(())
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
        let Chat { meta, store, engine, gate, gate_events, brain, limit, staged, .. } = self;
        // Its record as it is now: the chat may have been renamed since it was
        // opened here, or deleted.
        match store.get(&meta.id) {
            Some(kept) => meta.title = kept.title,
            None => return Err(DELETED.into()),
        }
        let files = std::mem::take(staged);
        // A call that came in while no turn was running isn't this turn's to ask about.
        while let Ok(event) = gate_events.try_recv() {
            if let gate::Event::Asked(request) = &event {
                gate.answer(request.id, gate::Answer::No("It was called while no turn was running.".into()));
            }
        }
        while gate_events.try_recv().is_ok() {}
        let _ = store.append(&meta.id, &Item::User { at: store::now(), text: text.to_string(), files: files.iter().map(|f| f.name.clone()).collect() });
        if meta.title.is_empty() {
            meta.title = store::title_from(text);
        }
        meta.updated = store::now();
        let _ = store.save(meta);

        // What the message points at (@forge) is said after it, by id. Then
        // text files, in the message's own words; pictures and PDFs go the brain's own way.
        let pointed = mention::note(text, &mention::things(&engine.current()));
        let message = attach::with_text(&format!("{text}{pointed}"), &files);
        if let Err(why) = brain.attach(files.into_iter().filter(|f| f.kind != attach::Kind::Text).collect()) {
            let _ = store.append(&meta.id, &Item::Error { at: store::now(), text: why.clone() });
            return Err(why);
        }

        let (tx, mut said) = mpsc::unbounded_channel();
        let outcome = {
            let turn = brain.turn(&message, &tx);
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
        // With the title it has now: it can be renamed while a turn runs.
        save(store, meta);
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
                save(store, meta);
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
        Chat { meta, store, engine, gate, gate_events, brain, limit: None, staged: Vec::new(), _bridge: bridge }
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

    /// A brain that keeps what it was told, and takes pictures but not PDFs.
    struct Listening {
        heard: Arc<std::sync::Mutex<Vec<String>>>,
        given: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl brain::Brain for Listening {
        fn attach(&mut self, files: Vec<attach::Attached>) -> Result<(), String> {
            if let Some(pdf) = files.iter().find(|f| f.kind == attach::Kind::Pdf) {
                return Err(format!("It can't be given {}.", pdf.name));
            }
            self.given.lock().unwrap().extend(files.into_iter().map(|f| f.name));
            Ok(())
        }

        async fn turn(&mut self, text: &str, _events: &mpsc::UnboundedSender<brain::Event>) -> Result<(), String> {
            self.heard.lock().unwrap().push(text.to_string());
            Ok(())
        }

        async fn stop(&mut self) {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_files_go_with_one_message_text_in_its_words_and_the_rest_the_brains_way() {
        let mut chat = chat("attach", vec![]);
        let (heard, given) = (Arc::new(std::sync::Mutex::new(Vec::new())), Arc::new(std::sync::Mutex::new(Vec::new())));
        chat.brain = Box::new(Listening { heard: heard.clone(), given: given.clone() });
        let dir = std::env::temp_dir().join(format!("muxin-chat-attach-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("build.log"), "error: nope\n").unwrap();
        std::fs::write(dir.join("shot.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join("paper.pdf"), b"%PDF").unwrap();
        std::fs::write(dir.join("x.bin"), [0u8, 1]).unwrap();

        chat.attach(&[dir.join("build.log"), dir.join("shot.png")]).unwrap();
        chat.send("Why does this fail?", &mut |_| {}).await.unwrap();
        // The log is in the message's own words; the picture went the brain's way.
        assert_eq!(heard.lock().unwrap()[0], "Why does this fail?\n\nAttached file: build.log\n```\nerror: nope\n```");
        assert_eq!(*given.lock().unwrap(), ["shot.png"]);
        // What was said keeps their names, and the chat its own copies.
        assert!(matches!(&chat.thread()[0], Item::User { text, files, .. } if text == "Why does this fail?" && files == &["build.log", "shot.png"]));
        let home = chat.store.home(&chat.meta.id).unwrap();
        assert!(home.join("files/build.log").exists() && home.join("files/shot.png").exists());

        // They went with that message, not the next one.
        chat.send("And now?", &mut |_| {}).await.unwrap();
        assert_eq!(heard.lock().unwrap()[1], "And now?");

        // What a message points at is said after it, by the id Muxin's tools take, and before a file's text.
        chat.attach(&[dir.join("build.log")]).unwrap();
        chat.send("Is @forge free? Not @nobody.", &mut |_| {}).await.unwrap();
        let said = heard.lock().unwrap().last().cloned().unwrap();
        assert!(said.starts_with("Is @forge free? Not @nobody.\n\n[From Muxin: what this message points at.\n@forge is the machine \"Forge\" (machine_id \"forge\""), "{said}");
        assert!(said.ends_with(".]\n\nAttached file: build.log\n```\nerror: nope\n```") && !said.contains("@nobody is"), "{said}");
        // The chat keeps what the user wrote, as they wrote it.
        assert!(chat.thread().iter().any(|item| matches!(item, Item::User { text, .. } if text == "Is @forge free? Not @nobody.")));
        heard.lock().unwrap().pop();
        assert_eq!(given.lock().unwrap().len(), 1);

        // One this brain can't take: the message isn't sent, and the chat says why.
        chat.attach(&[dir.join("paper.pdf")]).unwrap();
        assert_eq!(chat.send("Read this", &mut |_| {}).await.unwrap_err(), "It can't be given paper.pdf.");
        assert_eq!(heard.lock().unwrap().len(), 2);
        assert!(matches!(chat.thread().last(), Some(Item::Error { text, .. }) if text == "It can't be given paper.pdf."));
        // And it isn't still waiting to go with the message after.
        chat.send("Never mind", &mut |_| {}).await.unwrap();
        assert_eq!(heard.lock().unwrap()[2], "Never mind");

        // A file no brain could read is refused when it's attached.
        assert!(chat.attach(&[dir.join("x.bin")]).unwrap_err().starts_with("x.bin isn't text"));
        assert!(chat.attach(&vec![dir.join("build.log"); 11]).unwrap_err().starts_with("That's 11 files."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_from_a_brain_muxin_no_longer_has_is_taken_up_by_the_first_there_is() {
        let dir = std::env::temp_dir().join(format!("muxin-chat-gone-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(Paths::resolve(Some(dir), None), true);
        let store = store(&engine);
        // A chat ChatGPT answered, from when Muxin could answer on a ChatGPT plan.
        let old = store.create("chatgpt").unwrap();
        let opened = Chat::open(&engine, Options { chat: Some(old.id.clone()), ..Default::default() });
        match brain::available().first() {
            Some(first) => {
                assert_eq!(opened.map(|chat| chat.meta.brain.clone()), Ok(first.id.to_string()));
                assert_eq!(store.get(&old.id).unwrap().brain, first.id);
            }
            // Nothing on this computer to answer with: that's what's said.
            None => assert_eq!(opened.err(), Some(brain::none_here())),
        }
        // Asked for by name, it's said not to be one.
        let asked = Chat::open(&engine, Options { brain: Some("chatgpt".into()), ..Default::default() });
        assert!(asked.err().is_some_and(|why| why.contains("isn't a brain Muxin knows. Use claude, codex or grok.")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_renamed_or_deleted_elsewhere_stays_that_way() {
        let mut chat = chat("elsewhere", vec![vec![Step::Say("One.")], vec![Step::Say("Two.")]]);
        chat.send("restart the runners", &mut |_| {}).await.unwrap();
        assert_eq!(chat.meta.title, "restart the runners");
        let store = store(&chat.engine);

        // Renamed while it's open here (in the app, from another terminal): the next turn keeps the name.
        store.rename(&chat.meta.id, "Runner restarts").unwrap();
        chat.send("and again", &mut |_| {}).await.unwrap();
        assert_eq!((chat.meta.title.as_str(), store.get(&chat.meta.id).unwrap().title.as_str()), ("Runner restarts", "Runner restarts"));
        // So does picking another model.
        store.rename(&chat.meta.id, "Restarts").unwrap();
        chat.choose(Some("opus".into()), None);
        let kept = store.get(&chat.meta.id).unwrap();
        assert_eq!((kept.title.as_str(), kept.model.as_deref()), ("Restarts", Some("opus")));

        // Deleted while it's open here: said, and nothing makes it again.
        store.delete(&chat.meta.id).unwrap();
        assert_eq!(chat.send("still there?", &mut |_| {}).await.unwrap_err(), "This chat was deleted.");
        chat.choose(None, None);
        assert_eq!(chat.attach(&[std::env::temp_dir()]).unwrap_err(), "This chat was deleted.");
        assert!(store.get(&chat.meta.id).is_none() && store.list().is_empty());
        assert!(!store.home(&chat.meta.id).unwrap().exists());
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
        assert!(err(Options { brain: Some("gpt".into()), ..Default::default() }).contains("claude, codex or grok"));
        // Nothing was made along the way.
        assert!(store(&engine).list().is_empty());
    }
}
