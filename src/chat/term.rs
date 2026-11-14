//! The chat in a terminal: `muxin chat`. One question and its answer, or a
//! conversation typed line by line.
//!
//! The assistant's words go to stdout and nothing else does, so its answer
//! can be piped. What it does along the way (tools, what a turn used) and the
//! questions it needs answered go to stderr.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::gate::{Answer, Mode, Request};
use super::risk::{self, Risk};
use super::{Chat, Options, Update, brain, command, store};
use crate::fleet::Engine;

/// What `muxin chat` was asked to do.
pub struct Ask {
    /// What to say. Empty: a conversation, typed line by line.
    pub prompt: String,
    pub options: Options,
    /// Show the chats instead.
    pub list: bool,
    /// Turn the chat on (or off) instead.
    pub switch: Option<bool>,
    /// Delete this chat instead, by its id.
    pub delete: Option<String>,
    /// Name a chat instead: which (its id, or `last`), and the title.
    pub rename: Option<(String, String)>,
    /// Show the models there are to answer with instead (of `options.brain`, when given).
    pub models: bool,
    /// Files that go with the first message.
    pub attach: Vec<std::path::PathBuf>,
}

/// What a turn would cost through the API, in words after its token counts.
pub(crate) fn priced(cost: f64) -> String {
    // A cent rounds to nothing below half of one.
    if cost < 0.005 { " · under $0.01 at API prices".to_string() } else { format!(" · ${cost:.2} at API prices") }
}

/// Dim, when stderr is a terminal.
fn dim(text: &str) -> String {
    if std::io::stderr().is_terminal() { format!("\x1b[2m{text}\x1b[0m") } else { text.to_string() }
}

fn bold(text: &str) -> String {
    if std::io::stderr().is_terminal() { format!("\x1b[1m{text}\x1b[0m") } else { text.to_string() }
}

/// A turn is running: Ctrl-C stops it. Otherwise Ctrl-C ends the chat.
static IN_TURN: AtomicBool = AtomicBool::new(false);

/// Takes over Ctrl-C for the chat; what it returns is told when a turn should stop.
fn watch_ctrl_c() -> Arc<tokio::sync::Notify> {
    let stop = Arc::new(tokio::sync::Notify::new());
    let told = stop.clone();
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if IN_TURN.load(Ordering::SeqCst) {
                told.notify_one();
            } else {
                // Waiting for a line to be typed: nothing to unwind to.
                super::bridge::close_all();
                eprintln!();
                std::process::exit(130);
            }
        }
    });
    stop
}

/// Runs `muxin chat`; the exit code.
pub async fn run(engine: Arc<Engine>, ask: Ask) -> u8 {
    if let Some(on) = ask.switch {
        return switch(&engine, on);
    }
    if ask.list {
        return list(&engine);
    }
    if let Some(id) = &ask.delete {
        return delete(&engine, id);
    }
    if let Some((which, title)) = &ask.rename {
        return rename(&engine, which, title);
    }
    if ask.models {
        return models(ask.options.brain.as_deref());
    }
    // Off until the user turns it on. (Chats from before can still be listed, named and deleted.)
    if !is_on(&engine) {
        eprintln!("The chat is off. Turn it on in the app (Settings › Chat), or here: muxin chat --on");
        return 1;
    }
    let stop = watch_ctrl_c();
    let mut chat = match Chat::open(&engine, ask.options) {
        Ok(chat) => chat,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let name = brain::kind(&chat.meta.brain).map(|k| k.name).unwrap_or("?");
    let mode = match chat.gate().mode() {
        Mode::Ask => "asks before it changes anything",
        Mode::Bypass => "bypasses permissions: it doesn't ask before it changes things",
    };
    eprintln!("{}", dim(&format!("Chat {} · {name} · {mode}", chat.meta.id)));

    // What's attached goes with the first message said.
    if let Err(why) = chat.attach(&ask.attach) {
        eprintln!("error: {why}");
        return 2;
    }
    if !ask.prompt.trim().is_empty() {
        return match turn(&mut chat, &ask.prompt, &stop).await {
            Ok(()) => 0,
            Err(_) => 1,
        };
    }
    if !std::io::stdin().is_terminal() {
        // Piped in: all of it is one message.
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text);
        if text.trim().is_empty() {
            eprintln!("error: nothing to ask. Give it as an argument: muxin chat \"what needs me?\"");
            return 2;
        }
        return match turn(&mut chat, &text, &stop).await {
            Ok(()) => 0,
            Err(_) => 1,
        };
    }
    eprintln!("{}", dim("Type a message. @forge points at a machine, a session or a box by its id. /help lists the commands."));
    loop {
        eprint!("{} ", bold("›"));
        let _ = std::io::stderr().flush();
        let Some(line) = read_line() else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A command is done, not said.
        let Some((command, rest)) = command::parse(line).filter(|(command, _)| command.term) else {
            let _ = turn(&mut chat, line, &stop).await;
            continue;
        };
        let said = |text: &str| eprintln!("{}", dim(text));
        match (command.name, rest) {
            ("quit", _) => break,
            ("help", _) => {
                for command in command::COMMANDS.iter().filter(|c| c.term) {
                    said(&format!("  {:<18} {}", format!("/{} {}", command.name, command.takes), command.about));
                }
            }
            ("bypass", _) => {
                chat.gate().set_mode(Mode::Bypass);
                said("Bypass permissions: it won't ask before it changes things, in this chat.");
            }
            ("ask", _) => {
                chat.gate().set_mode(Mode::Ask);
                said("Ask permissions: it asks before it changes anything.");
            }
            ("attach", "") => said("Which file? /attach <file>"),
            ("attach", named) => match chat.attach(&[command::path(named)]) {
                Ok(()) => said(&format!("{named} goes with your next message.")),
                Err(why) => said(&format!("  ✗ {why}")),
            },
            ("model", said_model) => said(&model(&mut chat, said_model)),
            ("effort", said_effort) => said(&effort(&mut chat, said_effort)),
            ("rename", "") => said("What should it be called? /rename <title>"),
            ("rename", title) => match super::store(&engine).rename(&chat.meta.id, title) {
                Ok(meta) => said(&format!("This chat is now: {}", titled(&meta))),
                Err(why) => said(&format!("  ✗ Couldn't rename it: {why}")),
            },
            ("chats", _) => {
                list(&engine);
                said("Go on with one: muxin chat --chat <id>");
            }
            // Another chat, answered by the same program in the same mode.
            ("new", _) => match Chat::open(&engine, Options { brain: Some(chat.meta.brain.clone()), mode: Some(chat.gate().mode()), ..Default::default() }) {
                Ok(fresh) => {
                    said(&format!("A new chat: {}. The one before is {}.", fresh.meta.id, chat.meta.id));
                    chat = fresh;
                }
                Err(why) => said(&format!("  ✗ {why}")),
            },
            _ => {}
        }
    }
    eprintln!("{}", dim(&format!("Go on with it later: muxin chat --chat {}", chat.meta.id)));
    0
}

/// One line typed at the terminal; None when there's no more (Ctrl-D).
fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Runs one turn, printing it as it goes. Ctrl-C stops the turn, not the chat.
async fn turn(chat: &mut Chat, text: &str, stop: &tokio::sync::Notify) -> Result<(), String> {
    let gate = chat.gate();
    let mut screen = Screen::default();
    IN_TURN.store(true, Ordering::SeqCst);
    let outcome = {
        let mut on = |update: Update| screen.show(update, &gate);
        tokio::select! {
            outcome = chat.send(text, &mut on) => Some(outcome),
            _ = stop.notified() => None,
        }
    };
    IN_TURN.store(false, Ordering::SeqCst);
    screen.end_line();
    match outcome {
        Some(Ok(())) => Ok(()),
        Some(Err(why)) => {
            eprintln!("error: {why}");
            Err(why)
        }
        None => {
            chat.stop().await;
            eprintln!("{}", dim("Stopped."));
            Err("Stopped.".into())
        }
    }
}

/// What's on the terminal so far in this turn.
#[derive(Default)]
struct Screen {
    /// Text was printed piece by piece and its line isn't ended yet.
    open: bool,
    /// The stretch of text being said came in pieces: its whole needn't be printed again.
    pieces: bool,
}

impl Screen {
    fn end_line(&mut self) {
        if self.open {
            println!();
            self.open = false;
        }
    }

    fn show(&mut self, update: Update, gate: &super::gate::Gate) {
        match update {
            Update::Delta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.open = !text.ends_with('\n');
                self.pieces = true;
            }
            Update::Text(text) => {
                if !self.pieces && !text.trim().is_empty() {
                    println!("{}", text.trim_end());
                    self.open = false;
                }
                self.end_line();
                self.pieces = false;
            }
            Update::Tool { name, args, .. } => {
                self.end_line();
                eprintln!("{}", dim(&format!("  · {}", brief(&name, &args))));
            }
            Update::Result { text, is_error, .. } => {
                // A no the user gave here was already said, where it was asked.
                if is_error && let Some(why) = failure(&text) {
                    eprintln!("{}", dim(&format!("    ✗ {}", shorten(&why, 200))));
                }
            }
            Update::Ask(request) => {
                self.end_line();
                let answer = ask(&request);
                gate.answer(request.id, answer);
            }
            Update::Usage { input, output, cached, cost } => {
                self.end_line();
                let cached = if cached > 0 { format!(" ({} read from cache)", count(cached)) } else { String::new() };
                let cost = cost.map(priced).unwrap_or_default();
                eprintln!("{}", dim(&format!("  {} tokens in{cached}, {} out{cost}", count(input), count(output))));
            }
            Update::Limit(text) => {
                self.end_line();
                eprintln!("{}", dim(&format!("  {text}")));
            }
            Update::Model(name) => {
                self.end_line();
                eprintln!("{}", dim(&format!("  Answering with {name}")));
            }
        }
    }
}

/// Asks the person at the terminal about a waiting call.
fn ask(request: &Request) -> Answer {
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", dim(&format!("  ✗ {} needs a yes, and nobody is at a terminal to give it. --bypass runs without asking.", request.tool)));
        return Answer::No("Nobody was at the terminal to approve it.".into());
    }
    let what = match request.risk {
        Risk::Danger => "The assistant wants to run this (it can't simply be undone, so it asks every time):",
        _ => "The assistant wants to run:",
    };
    eprintln!("\n{}", bold(what));
    for line in risk::describe(&request.tool, &request.args) {
        eprintln!("  {line}");
    }
    let always = request.rule.as_ref().map(|rule| format!("   [a] yes, and don't ask again for {rule}")).unwrap_or_default();
    loop {
        eprint!("{} ", bold(&format!("[y] yes{always}   [n] no (add why, if you like) ›")));
        let _ = std::io::stderr().flush();
        let Some(line) = read_line() else { return Answer::No(String::new()) };
        if let Some(answer) = parse_answer(&line, request.rule.is_some()) {
            return answer;
        }
    }
}

/// What a typed answer means; None for one to ask again about.
fn parse_answer(line: &str, may_always: bool) -> Option<Answer> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match word.to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Answer::Yes),
        "a" | "always" if may_always => Some(Answer::Always),
        "n" | "no" => Some(Answer::No(rest.trim().to_string())),
        _ => None,
    }
}

/// Why a tool failed, on one line: Muxin's own error message (and its hint)
/// when that's what came back, else the first line of what did. None for a
/// call the user declined.
pub(crate) fn failure(text: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(error) = value.get("error")
    {
        if error["code"] == "declined" {
            return None;
        }
        if let Some(message) = error["message"].as_str() {
            return Some(match error["hint"].as_str() {
                Some(hint) => format!("{message} ({hint})"),
                None => message.to_string(),
            });
        }
    }
    Some(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("it failed").trim().to_string())
}

/// A tool call on one line: its name and the arguments that say what it's about.
pub(crate) fn brief(name: &str, args: &serde_json::Value) -> String {
    let mut parts = vec![name.to_string()];
    if let Some(map) = args.as_object() {
        for key in ["action", "power", "machine_id", "runner_id", "session", "box_id", "workspace_id", "repo", "agent", "name", "path", "command"] {
            if let Some(v) = map.get(key).and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
                parts.push(shorten(v.lines().next().unwrap_or(""), 60));
            }
        }
    }
    parts.join(" ")
}

fn shorten(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

/// 950, 12.3k, 1.2M.
pub(crate) fn count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `--models`: each brain there is here, and the models it can answer with,
/// with the efforts each takes (the model's own marked `*`).
fn models(only: Option<&str>) -> u8 {
    use brain::catalog;
    let brains: Vec<&brain::Kind> = brain::available().into_iter().filter(|k| only.is_none_or(|id| id == k.id)).collect();
    if brains.is_empty() {
        eprintln!("error: {}", match only {
            Some(id) if brain::kind(id).is_none() => format!("\"{id}\" isn't a brain Muxin knows. Use {}.", brain::ids()),
            Some(id) => format!("{} isn't here to answer with.", brain::kind(id).map(|k| k.name).unwrap_or(id)),
            None => brain::none_here(),
        });
        return 1;
    }
    for (n, kind) in brains.iter().enumerate() {
        if n > 0 {
            println!();
        }
        println!("{} (--brain {})", kind.name, kind.id);
        let list = catalog::of(kind.id);
        if list.is_empty() {
            println!("  It doesn't say which models it has. It answers with its own choice.");
        }
        let wide = list.iter().map(|m| m.id.chars().count()).max().unwrap_or(0);
        for model in list {
            let efforts: Vec<String> = model.efforts.iter().map(|e| if model.effort.as_deref() == Some(e.id.as_str()) { format!("{}*", e.id) } else { e.id.clone() }).collect();
            let name = if model.name == model.id { String::new() } else { model.name.clone() };
            println!("  {:<wide$}  {:<16}  {}", model.id, name, efforts.join(" "));
        }
    }
    println!("\nPick one: muxin chat --brain <brain> --model <id> --effort <level> \"...\"");
    0
}

fn list(engine: &Engine) -> u8 {
    let chats = super::store(engine).list();
    if chats.is_empty() {
        println!("No chats yet. Start one: muxin chat \"what needs me?\"");
        return 0;
    }
    let now = store::now();
    for chat in chats {
        let brain = brain::kind(&chat.brain).map(|k| k.name).unwrap_or(&chat.brain);
        println!("{}  {:<11}  {:>8} ago  {}", chat.id, brain, crate::util::short_duration((now - chat.updated).max(0) as u64), titled(&chat));
    }
    0
}

/// `/model <said>` in a conversation: the model this chat answers with from
/// its next message, among the ones its program lists. What to tell the user.
fn model(chat: &mut Chat, said: &str) -> String {
    let models = brain::catalog::of(&chat.meta.brain);
    let name = brain::kind(&chat.meta.brain).map(|k| k.name).unwrap_or("It");
    if said.is_empty() {
        let now = chat.meta.model.clone().unwrap_or_else(|| "its own choice".into());
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        return format!("{name} answers with {now}. /model <name> picks another: {}, or default.", ids.join(", "));
    }
    if said.eq_ignore_ascii_case("default") {
        chat.choose(None, None);
        return format!("{name} answers with its own choice of model from here.");
    }
    match brain::catalog::named(&models, said) {
        Some(model) => {
            // An effort the new model doesn't take isn't carried over to it.
            let effort = chat.meta.effort.clone().filter(|e| model.efforts.iter().any(|known| known.id == *e));
            chat.choose(Some(model.id.clone()), effort);
            format!("{name} answers with {} from here.", model.name)
        }
        None => format!("{name} has no model called {said}. `muxin chat --models` lists them."),
    }
}

/// `/effort <said>`: how hard this chat's model thinks from its next message.
fn effort(chat: &mut Chat, said: &str) -> String {
    let models = brain::catalog::of(&chat.meta.brain);
    let model = chat.meta.model.as_deref().and_then(|id| brain::catalog::find(&models, id));
    if said.eq_ignore_ascii_case("default") {
        chat.choose(chat.meta.model.clone(), None);
        return "It thinks as hard as the model would by itself.".into();
    }
    // With no model picked, the levels any of them take.
    let level = match model {
        Some(model) => model.effort_named(said).cloned(),
        None => models.iter().find_map(|m| m.effort_named(said).cloned()),
    };
    match level {
        Some(level) => {
            chat.choose(chat.meta.model.clone(), Some(level.id.clone()));
            format!("Effort: {} from here.", level.name)
        }
        None if said.is_empty() => {
            let now = chat.meta.effort.clone().unwrap_or_else(|| "the model's own".into());
            let takes: Vec<&str> = model.or(models.first()).map(|m| m.efforts.iter().map(|e| e.id.as_str()).collect()).unwrap_or_default();
            format!("Effort: {now}. /effort <level> sets it: {}, or default.", takes.join(", "))
        }
        None => format!("It doesn't take {said}. `muxin chat --models` lists what each model takes."),
    }
}

/// Whether the chat answers here: the user turned it on. The simulated
/// machines keep no settings to turn it on in, so there it does.
pub fn is_on(engine: &Engine) -> bool {
    engine.is_demo() || engine.settings().chat.enabled
}

/// `--on` and `--off`: the switch in Settings › Chat.
fn switch(engine: &Engine, on: bool) -> u8 {
    let mut settings = engine.settings();
    settings.chat.enabled = on;
    match engine.update_settings(settings) {
        Ok(_) if on => println!("The chat is on. Ask with: muxin chat \"what needs me?\""),
        Ok(_) => println!("The chat is off."),
        Err(why) => {
            eprintln!("error: {why}");
            return 1;
        }
    }
    0
}

/// A chat's title, as a list shows it.
fn titled(meta: &store::Meta) -> &str {
    if meta.title.is_empty() { "(nothing asked yet)" } else { &meta.title }
}

/// `--delete <id>`: a chat, with what was said in it and the files attached
/// in it. By its id only: `last` is too easy to get wrong for something
/// that can't be undone.
fn delete(engine: &Engine, id: &str) -> u8 {
    let store = super::store(engine);
    let Some(meta) = store.get(id) else {
        eprintln!("error: There's no chat {id}. `muxin chat --list` shows them.");
        return 1;
    };
    if let Err(e) = store.delete(id) {
        eprintln!("error: Couldn't delete {id}: {e}");
        return 1;
    }
    println!("Deleted {id}  {}", titled(&meta));
    // What isn't Muxin's to delete.
    if let Some(kind) = brain::kind(&meta.brain) {
        eprintln!("{}", dim(&format!("{} keeps its own record of the conversation. That stays where it is.", kind.name)));
    }
    0
}

/// `--rename <id> <title>`: a chat's title. An empty one goes back to the
/// first thing asked in it.
fn rename(engine: &Engine, which: &str, title: &str) -> u8 {
    let store = super::store(engine);
    let meta = match super::find(&store, which) {
        Ok(meta) => meta,
        Err(why) => {
            eprintln!("error: {why}");
            return 1;
        }
    };
    match store.rename(&meta.id, title) {
        Ok(meta) => {
            println!("{}  {}", meta.id, titled(&meta));
            0
        }
        Err(e) => {
            eprintln!("error: Couldn't rename {}: {e}", meta.id);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_conversations_model_and_effort_are_set_by_what_theyre_called() {
        // Claude Code answers in the stand-in chat: its models go by short names.
        let mut chat = crate::chat::tests::chat("term-model", vec![]);
        assert!(model(&mut chat, "").starts_with("Claude Code answers with its own choice. /model <name> picks another: fable, opus, sonnet, haiku, or default."));
        assert_eq!(model(&mut chat, "Opus"), "Claude Code answers with Opus from here.");
        assert_eq!(chat.meta.model.as_deref(), Some("opus"));
        assert_eq!(effort(&mut chat, "extra high"), "Effort: Extra high from here.");
        assert_eq!((chat.meta.model.as_deref(), chat.meta.effort.as_deref()), (Some("opus"), Some("xhigh")));
        assert!(effort(&mut chat, "").starts_with("Effort: xhigh. /effort <level> sets it: low, medium, high, xhigh, max, or default."));
        // What it doesn't have is said, and nothing changes.
        assert_eq!(model(&mut chat, "gpt-9"), "Claude Code has no model called gpt-9. `muxin chat --models` lists them.");
        assert!(effort(&mut chat, "enormous").starts_with("It doesn't take enormous."));
        assert_eq!((chat.meta.model.as_deref(), chat.meta.effort.as_deref()), (Some("opus"), Some("xhigh")));
        // Another model keeps the effort it takes too; "default" gives both back to the program.
        assert_eq!(model(&mut chat, "son"), "Claude Code answers with Sonnet from here.");
        assert_eq!(chat.meta.effort.as_deref(), Some("xhigh"));
        assert_eq!(effort(&mut chat, "default"), "It thinks as hard as the model would by itself.");
        assert_eq!(model(&mut chat, "default"), "Claude Code answers with its own choice of model from here.");
        assert_eq!((chat.meta.model.clone(), chat.meta.effort.clone()), (None, None));
        // And it's kept with the chat.
        assert_eq!(crate::chat::store(&chat.engine).get(&chat.meta.id).unwrap().model, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_chat_is_off_until_its_turned_on() {
        let dir = std::env::temp_dir().join(format!("muxin-chat-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The user's own machines (none here): off to begin with, and nothing in the settings file says otherwise.
        let engine = Engine::new(crate::config::Paths::resolve(Some(dir.clone()), None), false);
        assert!(!is_on(&engine) && !engine.settings().chat.enabled);
        assert_eq!(switch(&engine, true), 0);
        assert!(is_on(&engine));
        let kept = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(kept.contains("[settings.chat]") && kept.contains("enabled = true"), "{kept}");
        // And off again leaves no trace of it there.
        assert_eq!(switch(&engine, false), 0);
        assert!(!is_on(&engine));
        assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("enabled"));
        // The simulated machines have no settings to turn it on in: there it answers.
        let demo = Engine::new(crate::config::Paths::resolve(Some(dir.join("demo-home")), None), true);
        assert!(is_on(&demo) && !demo.settings().chat.enabled);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chats_are_renamed_and_deleted_by_id() {
        let dir = std::env::temp_dir().join(format!("muxin-chat-term-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(crate::config::Paths::resolve(Some(dir), None), true);
        let store = crate::chat::store(&engine);
        let (old, new) = (store.create("codex").unwrap(), store.create("grok").unwrap());
        let mut newer = new.clone();
        newer.updated += 10;
        store.save(&newer).unwrap();

        // By id, or `last` for the one used last.
        assert_eq!(rename(&engine, &old.id, "Runner restarts"), 0);
        assert_eq!(rename(&engine, "last", "The newest"), 0);
        assert_eq!((store.get(&old.id).unwrap().title.as_str(), store.get(&new.id).unwrap().title.as_str()), ("Runner restarts", "The newest"));
        assert_eq!(rename(&engine, "c-gone", "x"), 1);

        // Deleting takes an id: there's no `last` for it.
        assert_eq!(delete(&engine, "last"), 1);
        assert_eq!(delete(&engine, "c-gone"), 1);
        assert_eq!(delete(&engine, &old.id), 0);
        assert_eq!(store.list().into_iter().map(|m| m.id).collect::<Vec<_>>(), [new.id.clone()]);
        assert_eq!(delete(&engine, &old.id), 1);
    }

    #[test]
    fn typed_answers() {
        assert_eq!(parse_answer("y\n", true), Some(Answer::Yes));
        assert_eq!(parse_answer(" Yes ", false), Some(Answer::Yes));
        assert_eq!(parse_answer("a", true), Some(Answer::Always));
        // Not offered: not taken.
        assert_eq!(parse_answer("a", false), None);
        assert_eq!(parse_answer("n", true), Some(Answer::No(String::new())));
        assert_eq!(parse_answer("no it's mid-job\n", true), Some(Answer::No("it's mid-job".into())));
        // Anything else is asked again, never taken as a yes.
        assert_eq!(parse_answer("", true), None);
        assert_eq!(parse_answer("sure", true), None);
    }

    #[test]
    fn a_tool_call_on_one_line() {
        assert_eq!(brief("runner_action", &json!({"runner_id": "beelink/beelink-01", "action": "restart", "wait": true})), "runner_action restart beelink/beelink-01");
        assert_eq!(brief("status", &json!({})), "status");
        assert_eq!(brief("workspace_exec", &json!({"workspace_id": "ws-1", "command": "make test\nmake lint"})), "workspace_exec ws-1 make test");
    }

    #[test]
    fn a_failure_reads_as_muxins_own_words() {
        let not_found = r#"{"error": {"code": "not_found", "message": "No machine \"nope\".", "hint": "muxin machines lists them"}}"#;
        assert_eq!(failure(not_found).as_deref(), Some("No machine \"nope\". (muxin machines lists them)"));
        assert_eq!(failure(r#"{"error": {"code": "declined", "message": "The user said no"}}"#), None);
        assert_eq!(failure("\n  ssh: connection refused\nmore").as_deref(), Some("ssh: connection refused"));
    }

    #[test]
    fn counts_read_short() {
        assert_eq!((count(950), count(12_340), count(1_200_000)), ("950".into(), "12.3k".into(), "1.2M".into()));
    }
}
