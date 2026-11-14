//! What the brains that ask a provider's API themselves have in common
//! (anthropic.rs, openai.rs): where the API is and the key for it, the
//! conversation kept beside the chat, the stream as it arrives, and running
//! the tools the model calls, each one past the chat's gate first.
//!
//! The provider keeps nothing of a conversation between requests, so Muxin
//! does, in the chat's folder, in the provider's own shapes: what's sent
//! with the next request is exactly what's kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::Kind;
use crate::chat::gate::Gate;
use crate::fleet::Engine;

/// Model answers in one turn before Muxin stops it: a model that keeps
/// calling tools without ever answering.
pub(super) const MOST_STEPS: usize = 60;
/// How much of a tool's result the model is given.
const MOST_OUTPUT: usize = 60_000;
/// No word from the provider for this long, mid-answer: the connection is gone.
pub(super) const QUIET: Duration = Duration::from_secs(180);

/// What such a brain is made with.
pub struct Setup {
    pub engine: Arc<Engine>,
    pub gate: Arc<Gate>,
    pub kind: &'static Kind,
    /// Where the API is: the provider's own address, or the one the user gave.
    pub base: String,
    pub key: String,
    /// The chat's folder, where its conversation is kept.
    pub folder: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub speed: Option<String>,
}

impl Setup {
    /// Where this provider's side of the conversation is kept.
    pub(super) fn kept_at(&self) -> PathBuf {
        self.folder.join(format!("{}.json", self.kind.id))
    }

    /// Who the assistant is, which model it is here, and what Muxin's tools
    /// would have told a program when it connected (here nothing connects,
    /// so it goes along).
    pub(super) fn system(&self, model: &str) -> String {
        format!("{}\n\n{}\n\n{}", self.prompt, self.who_answers(model), crate::mcp::INSTRUCTIONS)
    }

    /// A model isn't told its own name by its maker's API, and asked which
    /// it is, some name another (more so after another model's answers
    /// earlier in the chat). So it's told: by the name the pickers show it
    /// under, and its id.
    fn who_answers(&self, model: &str) -> String {
        let listed = super::catalog::known(&self.engine.paths().dir, self.kind.id);
        let named = match super::catalog::find(&listed, model).map(|m| m.name.as_str()).filter(|name| *name != model) {
            Some(name) => format!("{name} (`{model}`)"),
            None => format!("`{model}`"),
        };
        let through = if self.kind.id == super::CUSTOM { format!("the provider at {}", self.base) } else { self.kind.name.to_string() };
        format!("The model answering in this chat is {named}, reached through {through}. When asked which model or AI you are, say exactly that. An earlier answer in this chat may have been another model's: the user can switch models between messages.")
    }

    /// Runs a tool the model called, past the gate (see ../tools.rs). What
    /// it answered, and whether that's a failure.
    pub(super) async fn run(&self, name: &str, args: Value) -> (String, bool) {
        let result = crate::chat::tools::call(&self.engine, &crate::mcp::Session::named("chat"), &self.gate, name, args).await;
        (super::result_text(&result["content"]), result["isError"] == true)
    }
}

/// Which model answers, as the user was last told: the one asked for, and
/// the one the provider says answered when it calls it something else (a
/// dated name for a short one, or what a router chose).
#[derive(Default)]
pub(super) struct Told {
    asked: Option<String>,
    answered: Option<String>,
}

impl Told {
    /// The model about to be asked: said when it isn't the one said last.
    pub(super) fn asked(&mut self, model: &str, events: &tokio::sync::mpsc::UnboundedSender<super::Event>) {
        if self.asked.as_deref() != Some(model) {
            self.asked = Some(model.to_string());
            self.answered = None;
            let _ = events.send(super::Event::Model(model.to_string()));
        }
    }

    /// The model the provider says answered: said once, when it's another
    /// name than the one asked for.
    pub(super) fn answered(&mut self, model: Option<&str>, events: &tokio::sync::mpsc::UnboundedSender<super::Event>) {
        let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else { return };
        if self.asked.as_deref() != Some(model) && self.answered.as_deref() != Some(model) {
            self.answered = Some(model.to_string());
            let _ = events.send(super::Event::Model(model.to_string()));
        }
    }
}

pub(super) fn http() -> reqwest::Client {
    crate::util::http_client(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(30)))
}

/// The conversation kept at `path`: a list, as it's sent.
pub(super) fn kept(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub(super) fn keep(path: &Path, said: &[Value]) {
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(said).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// A tool's result as the model gets it: all of it, unless it's very long.
pub(super) fn for_model(mut text: String) -> String {
    if text.len() > MOST_OUTPUT {
        let mut cut = MOST_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let rest = text.len() - cut;
        text.truncate(cut);
        text.push_str(&format!("\n… ({rest} more bytes not shown; ask for less at a time)"));
    }
    text
}

/// Why a provider turned a request away, for the user: by its status, with
/// what the provider said when it said something.
pub(super) fn refused(who: &str, status: u16, said: Option<&str>) -> String {
    let said = said.map(str::trim).filter(|s| !s.is_empty());
    match status {
        401 => format!("{who} didn't accept this API key. Connect it again in Settings › AI."),
        // Forbidden is the key's doing when nothing more is said; a provider
        // that declines what was asked says why.
        403 => match said {
            Some(said) => format!("{who} declined the request: {said}"),
            None => format!("{who} didn't accept this API key. Connect it again in Settings › AI."),
        },
        402 => format!("{who} says this account can't pay for the request{}", said.map(|s| format!(": {s}")).unwrap_or_else(|| ".".into())),
        429 => match said {
            Some(said) => format!("{who} says to slow down, or that the account is out of credit: {said}"),
            None => format!("{who} says to slow down, or that the account is out of credit."),
        },
        _ => match said {
            Some(said) => format!("{who} refused the request ({status}): {said}"),
            None => format!("{who} refused the request ({status})."),
        },
    }
}

/// Whether a refusal is worth another try a little later: too many requests
/// at once, or the provider's own trouble.
pub(super) fn passing(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 529)
}

/// The stream as it arrives, cut into its events. A piece of the stream can
/// end anywhere, in the middle of an event or of a character.
#[derive(Default)]
pub(super) struct Events {
    waiting: Vec<u8>,
}

impl Events {
    /// Adds a piece; the events it completes, each one's data as JSON. (A
    /// last line that isn't JSON, like "[DONE]", is nothing.)
    pub(super) fn push(&mut self, piece: &[u8]) -> Vec<Value> {
        self.waiting.extend_from_slice(piece);
        let mut done = Vec::new();
        // An event ends at an empty line.
        while let Some((end, skip)) = [&b"\n\n"[..], b"\r\n\r\n"].iter().filter_map(|gap| self.waiting.windows(gap.len()).position(|w| w == *gap).map(|at| (at, gap.len()))).min() {
            let block: Vec<u8> = self.waiting.drain(..end + skip).collect();
            let text = String::from_utf8_lossy(&block[..end]);
            let data: String = text.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect::<Vec<_>>().join("\n");
            if let Ok(event) = serde_json::from_str::<Value>(&data) {
                done.push(event);
            }
        }
        done
    }
}

/// Reads a stream to its end, handing each event to `on`. `Err` when the
/// connection breaks or goes quiet, or when `on` says the answer failed.
pub(super) async fn read_stream(mut resp: reqwest::Response, who: &str, mut on: impl FnMut(&Value) -> Result<(), String>) -> Result<(), String> {
    let mut stream = Events::default();
    loop {
        let piece = match tokio::time::timeout(QUIET, resp.chunk()).await {
            Ok(Ok(Some(piece))) => piece,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => return Err(format!("The connection to {who} broke off: {e}")),
            Err(_) => return Err(format!("{who} went quiet in the middle of answering.")),
        };
        for event in stream.push(&piece) {
            on(&event)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_stream_is_cut_into_events_wherever_its_pieces_end() {
        let whole = "event: a\ndata: {\"n\":1}\n\ndata: {\"n\":2,\"text\":\"h\u{e9}llo\"}\r\n\r\ndata: [DONE]\n\ndata: {\"n\":3}\n\n";
        let bytes = whole.as_bytes();
        // Cut at every place, a character's middle included.
        for cut in 0..bytes.len() {
            let mut events = Events::default();
            let mut got = events.push(&bytes[..cut]);
            got.extend(events.push(&bytes[cut..]));
            assert_eq!(got, [json!({"n": 1}), json!({"n": 2, "text": "h\u{e9}llo"}), json!({"n": 3})], "cut at {cut}");
        }
    }

    #[test]
    fn a_very_long_result_is_cut_for_the_model() {
        assert_eq!(for_model("short".into()), "short");
        let long = for_model("é".repeat(MOST_OUTPUT));
        assert!(long.len() < MOST_OUTPUT + 80 && long.ends_with("more bytes not shown; ask for less at a time)"), "{}", long.len());
    }

    #[test]
    fn a_refusal_is_said_in_words() {
        assert_eq!(refused("xAI", 401, Some("bad key")), "xAI didn't accept this API key. Connect it again in Settings › AI.");
        assert_eq!(refused("OpenAI", 404, Some(" The model `gpt-9` does not exist ")), "OpenAI refused the request (404): The model `gpt-9` does not exist");
        assert_eq!(refused("DeepSeek", 500, None), "DeepSeek refused the request (500).");
        assert!(refused("OpenRouter", 429, Some("quota")).ends_with("out of credit: quota"));
        assert_eq!(refused("OpenRouter", 403, Some("The input was flagged")), "OpenRouter declined the request: The input was flagged");
        assert!(refused("OpenAI", 403, None).starts_with("OpenAI didn't accept this API key."));
        assert!(passing(429) && passing(529) && !passing(400) && !passing(401));
    }
}
