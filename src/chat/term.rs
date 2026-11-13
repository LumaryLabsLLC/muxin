//! The chat in a terminal: `muxin chat`. One question and its answer, or a
//! conversation typed line by line.
//!
//! The assistant's words go to stdout and nothing else does, so its answer
//! can be piped. What it does along the way (tools, what a turn used) and the
//! questions it needs answered go to stderr.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::account::{self, Endpoints, USAGE_URL, Vault};
use super::gate::{Answer, Mode, Request};
use super::risk::{self, Risk};
use super::{Chat, Options, Update, brain, store};
use crate::fleet::Engine;

/// What `muxin chat` was asked to do.
pub struct Ask {
    /// What to say. Empty: a conversation, typed line by line.
    pub prompt: String,
    pub options: Options,
    /// Show the chats instead.
    pub list: bool,
    /// Show the models there are to answer with instead (of `options.brain`, when given).
    pub models: bool,
    /// Files that go with the first message.
    pub attach: Vec<std::path::PathBuf>,
    /// Sign in with ChatGPT instead (or out; `forget`: and forget which account).
    pub sign_in: bool,
    pub sign_out: bool,
    pub forget: bool,
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
    if ask.list {
        return list(&engine);
    }
    if ask.models {
        return models(&engine, ask.options.brain.as_deref()).await;
    }
    if ask.sign_out {
        return sign_out(&Vault::of(&engine.paths().dir), ask.forget).await;
    }
    if ask.sign_in {
        return sign_in(&Vault::of(&engine.paths().dir)).await;
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
        Mode::Bypass => "doesn't ask before it changes things",
    };
    eprintln!("{}", dim(&format!("Chat {} · {name} · {mode}", chat.meta.id)));
    if chat.meta.brain == brain::CHATGPT {
        eprintln!("{}", dim(&format!("Using ChatGPT plan. Manage usage: {USAGE_URL}")));
    }

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
    eprintln!("{}", dim("Type a message. /attach <file> adds a file to the next one; /bypass and /ask change whether it asks first; /quit ends."));
    loop {
        eprint!("{} ", bold("›"));
        let _ = std::io::stderr().flush();
        let Some(line) = read_line() else { break };
        match line.trim() {
            "" => continue,
            "/quit" | "/exit" | "/q" => break,
            "/bypass" => {
                chat.gate().set_mode(Mode::Bypass);
                eprintln!("{}", dim("It won't ask before it changes things, in this chat."));
            }
            "/ask" => {
                chat.gate().set_mode(Mode::Ask);
                eprintln!("{}", dim("It asks before it changes anything."));
            }
            attach if attach.starts_with("/attach") => {
                // A path as a shell would give it: quoted, or with "~" for the home folder.
                let named = attach["/attach".len()..].trim().trim_matches(|c| c == '"' || c == '\'');
                let path = match named.strip_prefix("~/") {
                    Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                    None => named.into(),
                };
                match if named.is_empty() { Err("Which file? /attach <file>".to_string()) } else { chat.attach(&[path]) } {
                    Ok(()) => eprintln!("{}", dim(&format!("{named} goes with your next message."))),
                    Err(why) => eprintln!("{}", dim(&format!("  ✗ {why}"))),
                }
            }
            text => {
                let _ = turn(&mut chat, text, &stop).await;
            }
        }
    }
    eprintln!("{}", dim(&format!("Go on with it later: muxin chat --chat {}", chat.meta.id)));
    0
}

/// `muxin chat --sign-in`: signs in with ChatGPT, in the browser.
async fn sign_in(vault: &Vault) -> u8 {
    eprintln!("Opening ChatGPT's sign-in in your browser…");
    let outcome = account::sign_in(vault, &Endpoints::default(), |address| {
        crate::util::open_url(address);
        // Not when it says which account it's for: that part isn't for a screen or a log.
        if !address.contains("id_token_hint=") {
            eprintln!("{}\n{address}", dim("If it didn't open, go to:"));
        }
    })
    .await;
    match outcome {
        Ok(account) if account.plan => {
            let who = if account.email.is_empty() { String::new() } else { format!(" as {}", account.email) };
            println!("Signed in{who}.");
            if !account.welcomed {
                // Said once, the first time.
                println!("You're using your ChatGPT plan: eligible requests in Muxin's chat use it. Manage usage: {USAGE_URL}");
                account::welcomed(vault);
            }
            println!("Ask with it: muxin chat --brain chatgpt \"what needs me?\"");
            0
        }
        Ok(_) => {
            eprintln!("You signed in, but didn't allow Muxin to use your ChatGPT plan, so the chat can't answer with it. Sign in again and allow it.");
            1
        }
        Err(why) => {
            eprintln!("error: {why}");
            1
        }
    }
}

/// `muxin chat --sign-out`.
async fn sign_out(vault: &Vault, forget: bool) -> u8 {
    let was = account::current(vault).is_some();
    match account::sign_out(vault, &Endpoints::default(), forget).await {
        Ok(_) if !was => println!("{}", if forget { "You weren't signed in. Muxin has forgotten the account it was registered for." } else { "You weren't signed in with ChatGPT." }),
        Ok(true) => println!("Signed out of ChatGPT."),
        Ok(false) => println!("Signed out here. ChatGPT couldn't be told, so Muxin may still be listed there: you can disconnect it in ChatGPT's settings."),
        Err(why) => {
            eprintln!("error: {why}");
            return 1;
        }
    }
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
async fn models(engine: &Engine, only: Option<&str>) -> u8 {
    use brain::catalog;
    let vault = Vault::of(&engine.paths().dir);
    let plan = account::current(&vault).is_some_and(|a| a.plan);
    let brains: Vec<&brain::Kind> = brain::available(plan).into_iter().filter(|k| only.is_none_or(|id| id == k.id)).collect();
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
        let list = match kind.program {
            Some(_) => catalog::of(kind.id),
            None => match brain::responses::models(&vault, &Endpoints::default()).await {
                Ok(list) => list,
                Err(why) => {
                    println!("  {why}");
                    continue;
                }
            },
        };
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
        let title = if chat.title.is_empty() { "(nothing asked yet)" } else { &chat.title };
        println!("{}  {:<11}  {:>8} ago  {title}", chat.id, brain, crate::util::short_duration((now - chat.updated).max(0) as u64));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
