//! Anthropic's API as a chat's brain: Claude, reached with the API key the
//! user connected (../keys.rs) and paid by the token. It speaks the Messages
//! API, not chat completions.
//!
//! How a turn goes:
//!
//! 1. The conversation so far is sent, whole, with Muxin's tools.
//! 2. The answer comes as a stream of content blocks: thinking, text as
//!    it's written, and the tools the model wants called.
//! 3. Each tool runs (or the user says no), the results go back together as
//!    the next message, and it's sent again. Until the model answers
//!    without calling anything.
//!
//! What the model said is kept and sent back exactly as it came, its
//! thinking blocks included: they're only valid in the conversation that
//! produced them, unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::api::{self, Setup};
use super::catalog::{Effort, Model};
use super::{Brain, Event};
use crate::chat::attach::{Attached, Kind};

/// The API version these requests are written for.
const VERSION: &str = "2026-10-21";
/// Asks that a request the model declines for policy be run again on the
/// model Anthropic names for that, in the same call.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The model that answers when none was picked.
const USUAL_MODEL: &str = "claude-opus-5-5";
/// The only model whose faster way of running needs nothing more than asking.
const FAST_MODEL: &str = "claude-opus-5-5";
/// Room for an answer, when the model's own limit isn't known; and the most
/// asked for when it is.
const SOME_ROOM: u64 = 16_000;
const MOST_ROOM: u64 = 64_000;

pub struct Anthropic {
    setup: Setup,
    kept_at: PathBuf,
    /// The conversation, as the Messages API takes it.
    said: Vec<Value>,
    /// The pictures and PDFs that go with the next message.
    attached: Vec<Attached>,
    /// The user was told which model answers.
    told: api::Told,
    /// What each model asked about so far can do.
    known: HashMap<String, About>,
    /// Seconds to wait before asking again after a passing refusal.
    waits: Vec<u64>,
}

/// What a model can do, as the Models API says.
#[derive(Clone, Debug, PartialEq)]
struct About {
    /// The most it may write in one answer.
    room: u64,
    /// It decides by itself how much to think, when asked to.
    adaptive: bool,
    /// The efforts it takes.
    efforts: Vec<String>,
    /// A request it declines can be run again on another model.
    falls_back: bool,
}

impl Default for About {
    fn default() -> About {
        About { room: SOME_ROOM, adaptive: false, efforts: Vec::new(), falls_back: false }
    }
}

/// What the model answered in one request.
#[derive(Debug, Default)]
struct Answer {
    /// Its content blocks, as they came together.
    blocks: Vec<Value>,
    /// A tool's arguments, in pieces, by its block's place.
    pieces: HashMap<usize, String>,
    /// Tools whose arguments weren't JSON, by id, with what came instead.
    unreadable: HashMap<String, String>,
    input: u64,
    output: u64,
    cached: u64,
    stop: Option<String>,
    /// The stream said it was complete.
    complete: bool,
    /// The model that answered, as the provider calls it.
    model: Option<String>,
}

fn headers(key: &str, fallback: bool) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let value = |text: &str| reqwest::header::HeaderValue::from_str(text).unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
    headers.insert("x-api-key", value(key));
    headers.insert("anthropic-version", value(VERSION));
    if fallback {
        headers.insert("anthropic-beta", value(FALLBACK_BETA));
    }
    headers
}

/// What Anthropic said about a request it refused: `{"error": {"message"}}`.
fn said_why(body: &str) -> Option<String> {
    let answer: Value = serde_json::from_str(body).ok()?;
    answer["error"]["message"].as_str().map(str::to_string)
}

fn address(base: &str, path: &str) -> String {
    format!("{}/v1/{path}", base.trim_end_matches('/').trim_end_matches("/v1"))
}

/// What a model can do, from its entry in the Models API.
fn about(entry: &Value) -> About {
    let can = &entry["capabilities"];
    let efforts = ["low", "medium", "high", "xhigh", "max"].into_iter().filter(|level| can["effort"][*level]["supported"] == true).map(str::to_string).collect();
    About {
        room: entry["max_tokens"].as_u64().map_or(SOME_ROOM, |most| most.min(MOST_ROOM)),
        adaptive: can["thinking"]["types"]["adaptive"]["supported"] == true,
        efforts,
        falls_back: entry["allowed_fallback_models"].as_array().is_some_and(|models| !models.is_empty()),
    }
}

impl Anthropic {
    /// A brain for the chat whose folder is `setup.folder`, going on with the
    /// conversation kept there.
    pub fn new(setup: Setup) -> Anthropic {
        let kept_at = setup.kept_at();
        let said = api::kept(&kept_at);
        Anthropic { setup, kept_at, said, attached: Vec::new(), told: api::Told::default(), known: HashMap::new(), waits: vec![2, 6] }
    }

    /// The model to ask, and what it can do (asked once for each model; a
    /// model the API won't describe is asked plainly).
    async fn model(&mut self, events: &mpsc::UnboundedSender<Event>) -> (String, About) {
        let model = self.setup.model.clone().unwrap_or_else(|| USUAL_MODEL.to_string());
        self.told.asked(&model, events);
        if let Some(known) = self.known.get(&model) {
            return (model, known.clone());
        }
        let http = crate::util::http_client(|| reqwest::Client::builder().timeout(Duration::from_secs(30)));
        let asked = http.get(address(&self.setup.base, &format!("models/{model}"))).headers(headers(&self.setup.key, true)).send().await;
        let found = match asked {
            Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.ok().map(|entry| about(&entry)),
            _ => None,
        };
        let found = found.unwrap_or_default();
        self.known.insert(model.clone(), found.clone());
        (model, found)
    }

    /// What's sent for one answer.
    fn request(&self, model: &str, about: &About) -> Value {
        let mut body = json!({
            "model": model,
            "max_tokens": about.room,
            "stream": true,
            // Who the assistant is, and what the tools would have told a program.
            "system": self.setup.system(model),
            "tools": tools(),
            "messages": self.said,
            // Every request of a conversation starts the same: the tools, the
            // system text, what was said so far. Marked here, the start is
            // kept ready and costs far less to read again.
            "cache_control": { "type": "ephemeral" },
        });
        if about.adaptive {
            body["thinking"] = json!({ "type": "adaptive" });
        }
        // Unsaid, the model thinks as hard and runs as fast as it would by itself.
        if let Some(effort) = self.setup.effort.as_ref().filter(|e| about.efforts.contains(e)) {
            body["output_config"] = json!({ "effort": effort });
        }
        if self.setup.speed.as_deref() == Some("fast") && model == FAST_MODEL {
            body["speed"] = json!("fast");
        }
        if about.falls_back {
            body["fallbacks"] = json!("default");
        }
        body
    }

    /// One answer from the model to the conversation as it stands.
    async fn step(&mut self, events: &mpsc::UnboundedSender<Event>) -> Result<Answer, String> {
        let who = self.setup.kind.name;
        let (model, about) = self.model(events).await;
        let body = self.request(&model, &about);
        let mut waits = self.waits.clone().into_iter();
        let resp = loop {
            let sent = api::http().post(address(&self.setup.base, "messages")).headers(headers(&self.setup.key, about.falls_back)).json(&body).send().await;
            let resp = sent.map_err(|e| format!("Couldn't reach {who}: {e}"))?;
            if resp.status().is_success() {
                break resp;
            }
            let status = resp.status().as_u16();
            let why = api::refused(who, status, said_why(&resp.text().await.unwrap_or_default()).as_deref());
            match waits.next().filter(|_| api::passing(status)) {
                Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                None => return Err(why),
            }
        };
        let mut answer = Answer::default();
        api::read_stream(resp, who, |event| heard(event, &mut answer, who, events)).await?;
        self.told.answered(answer.model.as_deref(), events);
        if !answer.complete {
            return Err(format!("The connection to {who} ended before it finished answering."));
        }
        Ok(answer)
    }
}

/// What one event of the stream means: adds to the answer, reports the
/// words as they come, or says why the answer failed.
fn heard(event: &Value, answer: &mut Answer, who: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    let at = event["index"].as_u64().unwrap_or(0) as usize;
    match event["type"].as_str().unwrap_or("") {
        "message_start" => {
            if let Some(model) = event["message"]["model"].as_str().filter(|m| !m.is_empty()) {
                answer.model = Some(model.to_string());
            }
            let usage = &event["message"]["usage"];
            let n = |k: &str| usage[k].as_u64().unwrap_or(0);
            answer.cached = n("cache_read_input_tokens");
            answer.input = n("input_tokens") + answer.cached + n("cache_creation_input_tokens");
        }
        "content_block_start" => {
            if answer.blocks.len() <= at {
                answer.blocks.resize(at + 1, Value::Null);
            }
            answer.blocks[at] = event["content_block"].clone();
        }
        "content_block_delta" => {
            let (delta, Some(block)) = (&event["delta"], answer.blocks.get_mut(at)) else { return Ok(()) };
            let add = |block: &mut Value, field: &str, piece: &str| {
                let so_far = block[field].as_str().unwrap_or_default().to_string();
                block[field] = json!(so_far + piece);
            };
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    let piece = delta["text"].as_str().unwrap_or_default();
                    add(block, "text", piece);
                    if !piece.is_empty() {
                        let _ = events.send(Event::Delta(piece.to_string()));
                    }
                }
                "thinking_delta" => add(block, "thinking", delta["thinking"].as_str().unwrap_or_default()),
                // What makes a thinking block valid to send back: kept whole.
                "signature_delta" => block["signature"] = delta["signature"].clone(),
                "input_json_delta" => answer.pieces.entry(at).or_default().push_str(delta["partial_json"].as_str().unwrap_or_default()),
                _ => {}
            }
        }
        "content_block_stop" => {
            let Some(block) = answer.blocks.get_mut(at) else { return Ok(()) };
            match block["type"].as_str() {
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default().to_string();
                    if !text.trim().is_empty() {
                        let _ = events.send(Event::Text(text));
                    }
                }
                // Its arguments came in pieces, as JSON text. Read strictly: what
                // was cut off, or isn't JSON, is never run.
                Some("tool_use") => {
                    let pieces = answer.pieces.remove(&at).unwrap_or_default();
                    match if pieces.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(&pieces) } {
                        Ok(input) if input.is_object() => block["input"] = input,
                        _ => {
                            answer.unreadable.insert(block["id"].as_str().unwrap_or_default().to_string(), pieces);
                            block["input"] = json!({});
                        }
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(stop) = event["delta"]["stop_reason"].as_str() {
                answer.stop = Some(stop.to_string());
            }
            if let Some(output) = event["usage"]["output_tokens"].as_u64() {
                answer.output = output;
            }
        }
        "message_stop" => answer.complete = true,
        "error" => {
            let said = event["error"]["message"].as_str().unwrap_or("it didn't say why");
            return Err(format!("{who} stopped with an error: {said}"));
        }
        _ => {}
    }
    Ok(())
}

/// The model's blocks as they're kept and sent back. As they came, with one
/// exception Anthropic asks for: when a request was run again on another
/// model partway through, what the first model thought and called before
/// that point isn't sent back. Its words are, and the mark stays in place.
fn kept_blocks(blocks: Vec<Value>) -> Vec<Value> {
    let blocks: Vec<Value> = blocks.into_iter().filter(|b| !b.is_null()).collect();
    let Some(switched) = blocks.iter().rposition(|b| b["type"] == "fallback") else { return blocks };
    blocks.into_iter().enumerate().filter(|(at, block)| *at >= switched || matches!(block["type"].as_str(), Some("text" | "fallback"))).map(|(_, block)| block).collect()
}

/// Muxin's tools as the model is given them (the chat's short list, see
/// ../tools.rs). Their arguments stream as they're written, and are read
/// strictly before anything runs (`heard`).
fn tools() -> Value {
    crate::chat::tools::offered().into_iter().map(|t| json!({ "name": t["name"], "description": t["description"], "input_schema": t["inputSchema"], "eager_input_streaming": true })).collect()
}

/// What the user said, as blocks: its words, and the pictures and PDFs that
/// go with them.
fn said_by_user(text: &str, files: &[Attached]) -> Result<Vec<Value>, String> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    for file in files {
        blocks.push(match file.kind {
            Kind::Image(media) => json!({ "type": "image", "source": { "type": "base64", "media_type": media, "data": file.base64()? } }),
            Kind::Pdf => json!({ "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": file.base64()? } }),
            Kind::Text => continue,
        });
    }
    Ok(blocks)
}

/// Adds what the user said (or the tools answered) to the conversation. Two
/// messages from the user in a row become one, since turns take turns.
fn say(said: &mut Vec<Value>, blocks: Vec<Value>) {
    match said.last_mut().filter(|last| last["role"] == "user") {
        Some(last) => {
            let mut all = match last["content"].take() {
                Value::Array(blocks) => blocks,
                Value::String(text) => vec![json!({ "type": "text", "text": text })],
                _ => Vec::new(),
            };
            all.extend(blocks);
            last["content"] = Value::Array(all);
        }
        None => said.push(json!({ "role": "user", "content": blocks })),
    }
}

/// Puts right a conversation that was cut off: tools the model called that
/// never got their answers (the user stopped the turn) get ones saying so.
/// A call without an answer can't be sent.
fn mend(said: &mut Vec<Value>) {
    let Some(last) = said.last().filter(|last| last["role"] == "assistant") else { return };
    let open: Vec<Value> = last["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| json!({ "type": "tool_result", "tool_use_id": b["id"], "content": "The user stopped the turn before this ran.", "is_error": true }))
        .collect();
    if !open.is_empty() {
        say(said, open);
    }
}

/// The models Anthropic lists (`GET /v1/models`), in its order, each with
/// the efforts it takes.
pub async fn models(kind: &super::Kind, base: &str, key: &str) -> Result<Vec<Model>, String> {
    let who = kind.name;
    let http = crate::util::http_client(|| reqwest::Client::builder().timeout(Duration::from_secs(30)));
    let resp = http.get(address(base, "models?limit=1000")).headers(headers(key, false)).send().await.map_err(|e| format!("Couldn't reach {who}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(api::refused(who, status, said_why(&resp.text().await.unwrap_or_default()).as_deref()));
    }
    let list: Value = resp.json().await.map_err(|_| format!("{who}'s list of models isn't readable."))?;
    Ok(listed(&list))
}

/// The list as it's read: `{"data": [{"id", "display_name", "capabilities"}]}`.
fn listed(list: &Value) -> Vec<Model> {
    list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?.to_string();
            let efforts = about(entry).efforts.iter().map(|level| Effort::of(level)).collect();
            Some(Model { name: entry["display_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&id).to_string(), efforts, effort: None, fast: id == FAST_MODEL, id })
        })
        .collect()
}

#[async_trait::async_trait]
impl Brain for Anthropic {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        let who = self.setup.kind.name;
        mend(&mut self.said);
        say(&mut self.said, said_by_user(text, &std::mem::take(&mut self.attached))?);
        api::keep(&self.kept_at, &self.said);
        let (mut input, mut output, mut cached) = (0, 0, 0);
        for _ in 0..api::MOST_STEPS {
            let answer = self.step(events).await?;
            input += answer.input;
            output += answer.output;
            cached += answer.cached;
            let Answer { blocks, unreadable, stop, .. } = answer;
            let stop = stop.unwrap_or_default();
            // Declined, or out of room: what it was in the middle of isn't kept,
            // and no tool it was calling runs.
            if stop == "refusal" {
                return Err(format!("{who} declined to answer this."));
            }
            if stop == "max_tokens" {
                return Err(format!("{who} stopped at its length limit before it finished. Ask again, or for less at a time."));
            }
            let blocks = kept_blocks(blocks);
            let calls: Vec<Value> = blocks.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            self.said.push(json!({ "role": "assistant", "content": blocks }));
            api::keep(&self.kept_at, &self.said);
            if calls.is_empty() || stop != "tool_use" {
                let _ = events.send(Event::Usage { input, output, cached, cost: None });
                return Ok(());
            }
            // The tools' answers go back together, as the next message.
            let mut results = Vec::new();
            for call in calls {
                let id = call["id"].as_str().unwrap_or_default().to_string();
                let name = call["name"].as_str().unwrap_or_default().to_string();
                let (said, failed) = match unreadable.get(&id) {
                    None => {
                        let _ = events.send(Event::Tool { id: id.clone(), name: name.clone(), args: call["input"].clone() });
                        self.setup.run(&name, call["input"].clone()).await
                    }
                    // Told what it sent, so it can send it again properly.
                    Some(sent) => {
                        let _ = events.send(Event::Tool { id: id.clone(), name: name.clone(), args: json!({}) });
                        (format!("The arguments weren't readable JSON, so it didn't run. They were: {sent}"), true)
                    }
                };
                let _ = events.send(Event::Result { id: id.clone(), text: said.clone(), is_error: failed });
                let mut result = json!({ "type": "tool_result", "tool_use_id": id, "content": api::for_model(said) });
                if failed {
                    result["is_error"] = json!(true);
                }
                results.push(result);
                // Kept as it goes: a turn stopped halfway is mended from here.
                let mut so_far = self.said.clone();
                say(&mut so_far, results.clone());
                api::keep(&self.kept_at, &so_far);
            }
            say(&mut self.said, results);
            api::keep(&self.kept_at, &self.said);
        }
        Err("It went on calling tools without answering, so Muxin stopped it.".into())
    }

    async fn stop(&mut self) {
        // Dropping the turn ended its request; `mend` settles what it left open.
    }

    // A picture or a PDF is a block of the message, beside its text.
    fn attach(&mut self, files: Vec<Attached>) -> Result<(), String> {
        self.attached = files;
        Ok(())
    }

    // Every request names its model and effort: the next one names these.
    fn choose(&mut self, model: Option<String>, effort: Option<String>) {
        let model = model.filter(|m| !m.trim().is_empty());
        self.setup.model = model;
        self.setup.effort = effort.filter(|e| !e.trim().is_empty());
    }

    fn speed(&mut self, speed: Option<String>) {
        self.setup.speed = speed.filter(|s| !s.trim().is_empty());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::chat::gate::{self, Gate, Mode};
    use crate::chat::testing::{self, Asked, Server};
    use crate::config::Paths;
    use crate::fleet::Engine;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn started(input: u64) -> Value {
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "usage": {"input_tokens": input, "cache_read_input_tokens": input * 2, "cache_creation_input_tokens": 5}}})
    }

    fn stopped(reason: &str, output: u64) -> Vec<Value> {
        vec![json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": output}}), json!({"type": "message_stop"})]
    }

    /// A thinking block with its signature, then a call of `name`, its
    /// arguments in two pieces.
    fn thinks_and_calls(id: &str, name: &str, args: &str) -> Vec<Value> {
        let (a, b) = args.split_at(args.len() / 2);
        vec![
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Look first."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": a}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": b}}),
            json!({"type": "content_block_stop", "index": 1}),
        ]
    }

    fn says(text: &str) -> Vec<Value> {
        let (a, b) = text.split_at(text.len() / 2);
        vec![
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": a}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": b}}),
            json!({"type": "content_block_stop", "index": 0}),
        ]
    }

    fn opus() -> Value {
        json!({"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_tokens": 128000, "allowed_fallback_models": ["claude-opus-4-8"], "capabilities": {
            "thinking": {"supported": true, "types": {"enabled": {"supported": false}, "adaptive": {"supported": true}}},
            "effort": {"supported": true, "low": {"supported": true}, "medium": {"supported": true}, "high": {"supported": true}, "xhigh": {"supported": true}, "max": {"supported": true}},
        }})
    }

    fn haiku() -> Value {
        json!({"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "max_tokens": 64000, "capabilities": {"thinking": {"supported": true, "types": {"adaptive": {"supported": false}}}, "effort": {"supported": false}}})
    }

    /// A stand-in for Anthropic's API that gives its answers in order.
    async fn anthropic(answers: Vec<testing::Answer>) -> Server {
        let left = Mutex::new(VecDeque::from(answers));
        Server::start(move |asked: &Asked| match asked.path.as_str() {
            "/v1/models" => testing::json(200, json!({"data": [opus(), haiku()], "has_more": false})),
            "/v1/models/claude-opus-5-5" => testing::json(200, opus()),
            "/v1/models/claude-haiku-4-5" => testing::json(200, haiku()),
            "/v1/messages" => left.lock().unwrap().pop_front().unwrap_or((500, "text/plain", "nothing left to answer".into())),
            _ => (404, "application/json", json!({"type": "error", "error": {"type": "not_found_error", "message": "no such model"}}).to_string()),
        })
        .await
    }

    struct Rig {
        brain: Anthropic,
        asks: mpsc::UnboundedReceiver<gate::Event>,
    }

    fn rig(name: &str, api: &Server, model: Option<&str>) -> Rig {
        let folder = std::env::temp_dir().join(format!("muxin-anthropic-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let engine = Engine::new(Paths::resolve(Some(folder.join("home")), None), true);
        let (gate, asks) = Gate::new(Mode::Ask, vec![]);
        let setup = Setup {
            engine,
            gate,
            kind: crate::chat::brain::kind("anthropic").unwrap(),
            base: api.url.clone(),
            key: "sk-ant-test".into(),
            folder,
            prompt: "You are Muxin's assistant.".into(),
            model: model.map(str::to_string),
            effort: None,
            speed: None,
        };
        let mut brain = Anthropic::new(setup);
        brain.waits = vec![0];
        Rig { brain, asks }
    }

    async fn turn(brain: &mut Anthropic, text: &str) -> (Vec<Event>, Result<(), String>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = brain.turn(text, &tx).await;
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(event);
        }
        (seen, outcome)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_calls_a_tool_sends_its_result_back_and_answers() {
        let api = anthropic(vec![
            testing::events(&[vec![started(100)], thinks_and_calls("toolu_1", "status", "{}"), stopped("tool_use", 20)].concat()),
            testing::events(&[vec![started(150)], says("Nine machines."), stopped("end_turn", 8)].concat()),
        ])
        .await;
        let mut rig = rig("turn", &api, None);
        let (seen, outcome) = turn(&mut rig.brain, "how many machines?").await;
        assert_eq!(outcome, Ok(()));

        // With no model picked, the usual one.
        assert_eq!(seen[0], Event::Model("claude-opus-5-5".into()));
        let seen = &seen[1..];
        assert!(matches!(&seen[0], Event::Tool { id, name, .. } if id == "toolu_1" && name == "status"));
        assert!(matches!(&seen[1], Event::Result { id, text, is_error: false } if id == "toolu_1" && text.contains("\"machines\"")));
        assert_eq!(seen[2..5], [Event::Delta("Nine ma".into()), Event::Delta("chines.".into()), Event::Text("Nine machines.".into())]);
        // What it read counts what came from the cache and what went into it.
        assert_eq!(seen[5], Event::Usage { input: 100 + 200 + 5 + 150 + 300 + 5, output: 28, cached: 500, cost: None });
        assert!(rig.asks.try_recv().is_err());

        // The first request: the key and the version as headers, streamed, Muxin's short list as tools.
        let sent = api.asked_at("/v1/messages");
        assert_eq!((sent[0].headers["x-api-key"].as_str(), sent[0].headers["anthropic-version"].as_str()), ("sk-ant-test", "2026-10-21"));
        let first = sent[0].json();
        assert_eq!((first["model"].as_str(), first["stream"].as_bool(), first["max_tokens"].as_u64()), (Some("claude-opus-5-5"), Some(true), Some(64000)));
        assert!(first["system"].as_str().unwrap().starts_with("You are Muxin's assistant."));
        assert_eq!(first["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "how many machines?"}]}]));
        let tools = first["tools"].as_array().unwrap();
        assert_eq!(tools.len(), crate::chat::tools::CORE.len() + 2);
        assert!(tools.iter().all(|t| t["name"].is_string() && t["input_schema"]["type"] == "object" && t["eager_input_streaming"] == true));
        // As the model's entry says it can: it thinks as much as it decides to, and a declined request is run again.
        assert_eq!((&first["thinking"], &first["fallbacks"], &first["cache_control"]), (&json!({"type": "adaptive"}), &json!("default"), &json!({"type": "ephemeral"})));
        assert_eq!(sent[0].headers["anthropic-beta"], "server-side-fallback-2026-07-01");
        // Nothing said about effort or speed, so nothing asked for.
        assert!(first.get("output_config").is_none() && first.get("speed").is_none());

        // The second: what the model said, exactly as it came (its thinking and its signature too), then the tool's answer.
        let second = sent[1].json();
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1], json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Look first.", "signature": "sig-abc"},
            {"type": "tool_use", "id": "toolu_1", "name": "status", "input": {}},
        ]}));
        assert_eq!((messages[2]["role"].as_str(), messages[2]["content"][0]["type"].as_str(), messages[2]["content"][0]["tool_use_id"].as_str()), (Some("user"), Some("tool_result"), Some("toolu_1")));
        assert!(messages[2]["content"][0]["content"].as_str().unwrap().contains("\"machines\""));
        // The model was asked about once, not with every request.
        assert_eq!(api.asked_at("/v1/models/claude-opus-5-5").len(), 1);
        // Kept beside the chat, to go on with later.
        assert_eq!(api::kept(&rig.brain.kept_at).len(), 4);

        // An effort the model takes and the faster way go with the next request; an effort it doesn't take isn't sent.
        let about = rig.brain.known["claude-opus-5-5"].clone();
        rig.brain.choose(Some("claude-opus-5-5".into()), Some("xhigh".into()));
        rig.brain.speed(Some("fast".into()));
        let body = rig.brain.request("claude-opus-5-5", &about);
        assert_eq!((&body["output_config"], &body["speed"]), (&json!({"effort": "xhigh"}), &json!("fast")));
        rig.brain.choose(Some("claude-opus-5-5".into()), Some("ultra".into()));
        assert!(rig.brain.request("claude-opus-5-5", &about).get("output_config").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_is_asked_only_what_it_can_do() {
        let api = anthropic(vec![testing::events(&[vec![started(10)], says("Hi."), stopped("end_turn", 2)].concat())]).await;
        let mut rig = rig("plain", &api, Some("claude-haiku-4-5"));
        rig.brain.choose(Some("claude-haiku-4-5".into()), Some("high".into()));
        rig.brain.speed(Some("fast".into()));
        assert_eq!(turn(&mut rig.brain, "hello").await.1, Ok(()));
        let sent = api.asked_at("/v1/messages");
        let body = sent[0].json();
        // No thinking it can't decide on, no effort, no faster way, nothing to fall back to; room as its limit allows.
        for field in ["thinking", "output_config", "speed", "fallbacks"] {
            assert!(body.get(field).is_none(), "{field}");
        }
        assert_eq!(body["max_tokens"], 64000);
        assert!(sent[0].headers.get("anthropic-beta").is_none());
        // A model the API won't describe is asked plainly, with some room.
        let unknown = About::default();
        assert_eq!((unknown.room, unknown.adaptive, unknown.falls_back), (SOME_ROOM, false, false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_was_cut_off_or_declined_runs_nothing_and_is_said() {
        let cut = [vec![started(10)], thinks_and_calls("toolu_1", "snippet_save", "{\"name\": \"Upt"), stopped("max_tokens", 5)].concat();
        let declined = [vec![started(10)], says("I can"), stopped("refusal", 2)].concat();
        let garbled = [vec![started(10)], thinks_and_calls("toolu_2", "status", "{not json"), stopped("tool_use", 5)].concat();
        let api = anthropic(vec![
            testing::events(&cut),
            testing::events(&declined),
            testing::events(&garbled),
            testing::events(&[vec![started(10)], says("Sorry."), stopped("end_turn", 2)].concat()),
            testing::json(401, json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})),
            testing::json(529, json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})),
            testing::events(&[vec![started(10)], says("Back."), stopped("end_turn", 2)].concat()),
            testing::events(&[started(10), json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})]),
            testing::events(&[vec![started(10)], says("Half")].concat()),
        ])
        .await;
        let mut rig = rig("cut", &api, Some("claude-opus-5-5"));
        let why = |outcome: (Vec<Event>, Result<(), String>)| outcome.1.unwrap_err();
        // Out of room in the middle of a tool's arguments: the tool doesn't run, and nobody is asked about it.
        assert!(why(turn(&mut rig.brain, "save it").await).contains("stopped at its length limit"));
        assert!(rig.asks.try_recv().is_err());
        assert_eq!(why(turn(&mut rig.brain, "and this?").await), "Anthropic declined to answer this.");
        // Arguments that aren't JSON: not run, and the model is told what it sent.
        let (seen, outcome) = turn(&mut rig.brain, "status?").await;
        assert_eq!(outcome, Ok(()));
        assert!(seen.iter().any(|e| matches!(e, Event::Result { text, is_error: true, .. } if text.starts_with("The arguments weren't readable JSON") && text.ends_with("{not json"))));
        let retold = api.asked_at("/v1/messages")[3].json();
        let last = retold["messages"].as_array().unwrap().last().cloned().unwrap();
        assert_eq!((last["content"][0]["type"].as_str(), last["content"][0]["is_error"].as_bool()), (Some("tool_result"), Some(true)));
        // Three things the user said in a row (two went unanswered) are one message, since turns take turns.
        assert_eq!(retold["messages"][0]["content"].as_array().unwrap().len(), 3);

        assert_eq!(why(turn(&mut rig.brain, "hello").await), "Anthropic didn't accept this API key. Connect it again in Settings › AI.");
        // Overloaded: asked once more, and answered.
        assert_eq!(turn(&mut rig.brain, "hello").await.1, Ok(()));
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "Anthropic stopped with an error: Overloaded");
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "The connection to Anthropic ended before it finished answering.");
    }

    #[test]
    fn a_turn_that_fell_back_is_kept_without_what_the_first_model_thought() {
        let blocks = vec![
            json!({"type": "thinking", "thinking": "a", "signature": "s1"}),
            json!({"type": "text", "text": "Starting."}),
            json!({"type": "tool_use", "id": "t1", "name": "status", "input": {}}),
            json!({"type": "fallback", "from": {"model": "claude-fable-5-2"}, "to": {"model": "claude-opus-4-8"}}),
            json!({"type": "thinking", "thinking": "b", "signature": "s2"}),
            json!({"type": "tool_use", "id": "t2", "name": "status", "input": {}}),
        ];
        let kept = kept_blocks(blocks.clone());
        // Before the switch only the words stay; the mark stays where it was; after it, everything.
        assert_eq!(kept.iter().map(|b| b["type"].as_str().unwrap()).collect::<Vec<_>>(), ["text", "fallback", "thinking", "tool_use"]);
        assert_eq!(kept[3]["id"], "t2");
        // Nothing fell back: as it came, less only a place nothing arrived for.
        let plain = vec![blocks[0].clone(), Value::Null, blocks[2].clone()];
        assert_eq!(kept_blocks(plain), [blocks[0].clone(), blocks[2].clone()]);
    }

    #[test]
    fn a_turn_stopped_halfway_is_mended_and_messages_from_the_user_join() {
        let mut said = vec![json!({"role": "user", "content": [{"type": "text", "text": "one"}]}), json!({"role": "assistant", "content": [{"type": "text", "text": "Checking."}, {"type": "tool_use", "id": "t1", "name": "status", "input": {}}]})];
        mend(&mut said);
        assert_eq!(said[2], json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "The user stopped the turn before this ran.", "is_error": true}]}));
        // What's said next joins that message.
        say(&mut said, vec![json!({"type": "text", "text": "two"})]);
        assert_eq!((said.len(), said[2]["content"].as_array().unwrap().len()), (3, 2));
        // Nothing open: nothing added.
        let before = said.clone();
        mend(&mut said);
        assert_eq!(said, before);
    }

    #[test]
    fn a_picture_and_a_pdf_are_blocks_of_what_the_user_said() {
        let dir = std::env::temp_dir().join(format!("muxin-anthropic-attach-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("shot.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join("paper.pdf"), b"%PDF").unwrap();
        let files = [Attached { name: "shot.png".into(), path: dir.join("shot.png"), kind: Kind::Image("image/png") }, Attached { name: "paper.pdf".into(), path: dir.join("paper.pdf"), kind: Kind::Pdf }];
        assert_eq!(said_by_user("What's this?", &files).unwrap(), [
            json!({"type": "text", "text": "What's this?"}),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQID"}}),
            json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERg=="}}),
        ]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_models_listed_come_with_the_efforts_each_takes() {
        let api = anthropic(vec![]).await;
        let kind = crate::chat::brain::kind("anthropic").unwrap();
        let got = models(kind, &api.url, "sk-ant-test").await.unwrap();
        assert_eq!(got.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.fast)).collect::<Vec<_>>(), [("claude-opus-5-5", "Claude Opus 5.5", true), ("claude-haiku-4-5", "Claude Haiku 4.5", false)]);
        assert_eq!(got[0].efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high", "xhigh", "max"]);
        assert!(got[1].efforts.is_empty());
        let asked = &api.asked_at("/v1/models")[0];
        assert_eq!((asked.headers["x-api-key"].as_str(), asked.query["limit"].as_str()), ("sk-ant-test", "1000"));
        // An address given with or without its /v1 is the same place.
        assert_eq!(address("https://api.anthropic.com/v1/", "messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(address("https://api.anthropic.com", "models/x"), "https://api.anthropic.com/v1/models/x");
    }
}
