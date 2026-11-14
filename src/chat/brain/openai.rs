//! A provider that speaks chat completions as OpenAI defined them: OpenAI
//! itself, xAI, Google, OpenRouter, DeepSeek, and whatever else answers at
//! an address the user gives. Reached with the API key the user connected
//! (../keys.rs), and paid by the token.
//!
//! How a turn goes:
//!
//! 1. The conversation so far is sent, whole, with Muxin's tools.
//! 2. The answer comes as a stream: text as it's written, and the tools the
//!    model wants called, their arguments in pieces.
//! 3. Each tool runs (or the user says no), its result joins the
//!    conversation, and it's sent again. Until the model answers without
//!    calling anything.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::api::{self, Setup};
use super::catalog::{Effort, Model};
use super::{Brain, Event};
use crate::chat::attach::{Attached, Kind};

pub struct OpenAi {
    setup: Setup,
    kept_at: PathBuf,
    /// The conversation, as chat completions take it: the user's messages,
    /// the model's (with the tools it called), each tool's answer.
    said: Vec<Value>,
    /// The pictures that go with the next message.
    attached: Vec<Attached>,
    /// Which model the user was told answers.
    told: api::Told,
    /// Seconds to wait before asking again after a passing refusal.
    waits: Vec<u64>,
}

/// What the model answered in one request.
#[derive(Debug, Default)]
struct Answer {
    text: String,
    calls: Vec<Call>,
    /// What it thought, in the provider's own shape (OpenRouter's
    /// `reasoning_details`): sent back as it came with the tools it called.
    thought: Vec<Value>,
    input: u64,
    output: u64,
    cached: u64,
    /// What the request cost in dollars, from a provider that says.
    cost: Option<f64>,
    /// The model that answered, as the provider calls it.
    model: Option<String>,
    /// Why it stopped: "stop", "tool_calls", "length", "content_filter".
    finish: Option<String>,
}

/// A tool the model wants called, as its pieces came together.
#[derive(Debug, Default, Clone, PartialEq)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAi {
    /// A brain for the chat whose folder is `setup.folder`, going on with the
    /// conversation kept there.
    pub fn new(setup: Setup) -> OpenAi {
        let kept_at = setup.kept_at();
        let said = api::kept(&kept_at);
        OpenAi { setup, kept_at, said, attached: Vec::new(), told: api::Told::default(), waits: vec![2, 6] }
    }

    fn who(&self) -> &'static str {
        self.setup.kind.name
    }

    /// The model to ask: the one chosen, else the first the provider lists.
    /// Says which, the first time.
    async fn model(&mut self, events: &mpsc::UnboundedSender<Event>) -> Result<String, String> {
        let model = match &self.setup.model {
            Some(model) => model.clone(),
            None => {
                let first = models(self.setup.kind, &self.setup.base, &self.setup.key).await?.into_iter().next();
                let first = first.ok_or_else(|| format!("{} lists no model to answer with.", self.who()))?;
                self.setup.model = Some(first.id.clone());
                first.id
            }
        };
        self.told.asked(&model, events);
        Ok(model)
    }

    /// What's sent for one answer.
    fn request(&self, model: &str) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": self.setup.system(model) })];
        messages.extend(self.said.iter().cloned());
        let mut body = json!({
            "model": model,
            "messages": messages,
            "tools": tools(),
            "stream": true,
            // What the request used comes at the stream's end only when asked for.
            "stream_options": { "include_usage": true },
        });
        // Unsaid, the model thinks as hard and runs as fast as it would by itself.
        if let Some(effort) = &self.setup.effort {
            // OpenRouter takes it in a setting of its own, for every model it has.
            if self.setup.kind.id == "openrouter" {
                body["reasoning"] = json!({ "effort": effort });
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }
        if self.setup.speed.as_deref() == Some("fast") {
            body["service_tier"] = json!("priority");
        }
        body
    }

    /// One answer from the model to the conversation as it stands.
    async fn step(&mut self, events: &mpsc::UnboundedSender<Event>) -> Result<Answer, String> {
        let who = self.who();
        let model = self.model(events).await?;
        let body = self.request(&model);
        let mut waits = self.waits.clone().into_iter();
        let resp = loop {
            let sent = api::http().post(format!("{}/chat/completions", self.setup.base.trim_end_matches('/'))).bearer_auth(&self.setup.key).json(&body).send().await;
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
        api::read_stream(resp, who, |chunk| heard(chunk, &mut answer, who, events)).await?;
        self.told.answered(answer.model.as_deref(), events);
        match answer.finish.as_deref() {
            None if answer.text.is_empty() && answer.calls.is_empty() => Err(format!("The connection to {who} ended before it answered.")),
            Some("length") => Err(format!("{who} stopped at its length limit before it finished. Ask again, or for less at a time.")),
            Some("content_filter") => Err(format!("{who} declined to answer this.")),
            _ => {
                if !answer.text.trim().is_empty() {
                    let _ = events.send(Event::Text(answer.text.clone()));
                }
                Ok(answer)
            }
        }
    }
}

/// What a provider said about a request it refused: `{"error": {"message"}}`,
/// or `{"error": "..."}`, or `{"message": "..."}`.
fn said_why(body: &str) -> Option<String> {
    let answer: Value = serde_json::from_str(body).ok()?;
    answer["error"]["message"].as_str().or(answer["error"].as_str()).or(answer["message"].as_str()).map(str::to_string)
}

/// What one piece of the stream means: adds to the answer, reports the
/// words as they come, or says why the answer failed.
fn heard(chunk: &Value, answer: &mut Answer, who: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    if !chunk["error"].is_null() {
        let said = chunk["error"]["message"].as_str().or(chunk["error"].as_str()).unwrap_or("it didn't say why");
        return Err(format!("{who} stopped with an error: {said}"));
    }
    if let Some(model) = chunk["model"].as_str().filter(|m| !m.is_empty()) {
        answer.model = Some(model.to_string());
    }
    let usage = &chunk["usage"];
    if usage.is_object() {
        answer.input = usage["prompt_tokens"].as_u64().unwrap_or(answer.input);
        answer.output = usage["completion_tokens"].as_u64().unwrap_or(answer.output);
        answer.cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(answer.cached);
        answer.cost = usage["cost"].as_f64().or(answer.cost);
    }
    let choice = &chunk["choices"][0];
    for piece in choice["delta"]["reasoning_details"].as_array().into_iter().flatten() {
        thought(&mut answer.thought, piece);
    }
    if let Some(piece) = choice["delta"]["content"].as_str().filter(|p| !p.is_empty()) {
        answer.text.push_str(piece);
        let _ = events.send(Event::Delta(piece.to_string()));
    }
    // A call's pieces say which call they belong to by its place in the list.
    for piece in choice["delta"]["tool_calls"].as_array().into_iter().flatten() {
        let at = piece["index"].as_u64().unwrap_or(0) as usize;
        if answer.calls.len() <= at {
            answer.calls.resize(at + 1, Call::default());
        }
        let call = &mut answer.calls[at];
        if let Some(id) = piece["id"].as_str().filter(|id| !id.is_empty()) {
            call.id = id.to_string();
        }
        if let Some(name) = piece["function"]["name"].as_str() {
            call.name.push_str(name);
        }
        if let Some(arguments) = piece["function"]["arguments"].as_str() {
            call.arguments.push_str(arguments);
        }
    }
    if let Some(finish) = choice["finish_reason"].as_str() {
        answer.finish = Some(finish.to_string());
    }
    Ok(())
}

/// Adds a piece of what the model thought to the pieces so far. Thinking
/// in words comes a few words at a time: those join the piece they belong
/// to (the same place in the list). Anything else is a piece of its own.
fn thought(so_far: &mut Vec<Value>, piece: &Value) {
    let words = |p: &Value| p["type"] == "reasoning.text";
    if let Some(last) = so_far.last_mut().filter(|last| words(last) && words(piece) && (last["index"] == piece["index"] || last["index"].is_null() || piece["index"].is_null())) {
        let text = format!("{}{}", last["text"].as_str().unwrap_or_default(), piece["text"].as_str().unwrap_or_default());
        last["text"] = json!(text);
        // What comes once (its signature, id and format) is kept from the piece that has it.
        for once in ["signature", "id", "format"] {
            if last[once].is_null() && !piece[once].is_null() {
                last[once] = piece[once].clone();
            }
        }
        return;
    }
    so_far.push(piece.clone());
}

/// Muxin's tools as the model is given them (the chat's short list, see
/// ../tools.rs): functions.
fn tools() -> Value {
    crate::chat::tools::offered().into_iter().map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["inputSchema"] } })).collect()
}

/// What the user said, as the conversation keeps it: plain words, or its
/// parts when a picture goes with them (as a `data:` address, kept in the
/// conversation so the model still has it on later turns).
fn said_by_user(text: &str, files: &[Attached]) -> Result<Value, String> {
    if files.is_empty() {
        return Ok(json!({ "role": "user", "content": text }));
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    for file in files {
        if let Kind::Image(media) = file.kind {
            parts.push(json!({ "type": "image_url", "image_url": { "url": format!("data:{media};base64,{}", file.base64()?) } }));
        }
    }
    Ok(json!({ "role": "user", "content": parts }))
}

/// Puts right a conversation that was cut off: a tool the model called that
/// never got its answer (the user stopped the turn) gets one saying so. A
/// call without an answer can't be sent.
fn mend(said: &mut Vec<Value>) {
    let answered: Vec<String> = said.iter().filter(|m| m["role"] == "tool").filter_map(|m| m["tool_call_id"].as_str().map(String::from)).collect();
    let mut at = 0;
    while at < said.len() {
        let open: Vec<String> = said[at]["tool_calls"].as_array().into_iter().flatten().filter_map(|c| c["id"].as_str()).filter(|id| !answered.iter().any(|a| a == id)).map(String::from).collect();
        // Right after the message that called them, past the answers it did get.
        let mut after = at + 1;
        while after < said.len() && said[after]["role"] == "tool" {
            after += 1;
        }
        for id in open {
            said.insert(after, json!({ "role": "tool", "tool_call_id": id, "content": "The user stopped the turn before this ran." }));
            after += 1;
        }
        at = after;
    }
}

/// The models a provider lists (`GET <base>/models`), the ones a chat can
/// answer with: the newest first when it says when each came out, and
/// OpenRouter's the most used first.
pub async fn models(kind: &super::Kind, base: &str, key: &str) -> Result<Vec<Model>, String> {
    let who = kind.name;
    let http = crate::util::http_client(|| reqwest::Client::builder().timeout(Duration::from_secs(30)));
    // OpenRouter gives its list to anyone, so the list proves no key: what it
    // says about the key itself does.
    if kind.id == "openrouter" {
        let resp = http.get(format!("{}/key", base.trim_end_matches('/'))).bearer_auth(key).send().await.map_err(|e| format!("Couldn't reach {who}: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(api::refused(who, status, said_why(&resp.text().await.unwrap_or_default()).as_deref()));
        }
    }
    // Its hundreds come the most used first, so the ones most people want are at the top.
    let order = if kind.id == "openrouter" { "?sort=most-popular" } else { "" };
    let resp = http.get(format!("{}/models{order}", base.trim_end_matches('/'))).bearer_auth(key).send().await.map_err(|e| format!("Couldn't reach {who}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(api::refused(who, status, said_why(&resp.text().await.unwrap_or_default()).as_deref()));
    }
    let list: Value = resp.json().await.map_err(|_| format!("{who}'s list of models isn't readable."))?;
    Ok(listed(kind.id, &list))
}

/// The list as it's read: `{"data": [{"id", "name"?, "created"?}]}`.
/// OpenRouter says more about each model: what a request to it may carry
/// (`supported_parameters`), where it's asked (`endpoints`), and how hard
/// it can be told to think (`reasoning`).
fn listed(provider: &str, list: &Value) -> Vec<Model> {
    // What isn't for a conversation: models that draw, speak, listen or index.
    const NOT_CHAT: [&str; 12] = ["embedding", "embed-", "tts", "whisper", "dall-e", "moderation", "image", "audio", "realtime", "transcribe", "rerank", "imagen"];
    let usual = || ["low", "medium", "high"].map(Effort::of).to_vec();
    // How hard to think can be said to these; the others take it as it comes.
    let efforts: Vec<Effort> = if matches!(provider, "openai" | "xai") { usual() } else { Vec::new() };
    let mut rows: Vec<(u64, Model)> = list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            // Google names its models "models/gemini-...": the part after the slash is what's sent.
            let id = m["id"].as_str()?.trim_start_matches("models/").to_string();
            if id.is_empty() || NOT_CHAT.iter().any(|word| id.to_lowercase().contains(word)) {
                return None;
            }
            // The assistant works by calling tools: a model that can't is no use here.
            if m["supported_parameters"].as_array().is_some_and(|takes| !takes.iter().any(|t| t == "tools")) {
                return None;
            }
            // Nor is one that only answers hours later, in a batch.
            if m["endpoints"].as_array().is_some_and(|at| !at.iter().any(|e| e.as_str().is_some_and(|e| e.ends_with("/chat/completions")))) {
                return None;
            }
            let (efforts, effort) = match m["reasoning"].as_object() {
                // The levels this model takes, when it names them; every usual one when it takes any.
                Some(thinks) => {
                    let mut levels: Vec<Effort> = match thinks.get("supported_efforts") {
                        Some(Value::Array(levels)) => levels.iter().filter_map(Value::as_str).map(Effort::of).collect(),
                        Some(Value::Null) => usual(),
                        _ => Vec::new(),
                    };
                    levels.sort_by_key(|e| super::catalog::rank(&e.id));
                    let own = thinks.get("default_effort").and_then(Value::as_str).filter(|d| levels.iter().any(|e| e.id == *d)).map(str::to_string);
                    (levels, own)
                }
                None => (efforts.clone(), None),
            };
            let name = m["name"].as_str().or(m["display_name"].as_str()).filter(|n| !n.is_empty()).unwrap_or(&id).to_string();
            Some((m["created"].as_u64().unwrap_or(0), Model { name, efforts, effort, fast: provider == "openai", id }))
        })
        .collect();
    // OpenRouter's order is the one asked for (see `models`): it's kept.
    if provider != "openrouter" && rows.iter().all(|(created, _)| *created > 0) {
        rows.sort_by(|a, b| b.0.cmp(&a.0));
    }
    rows.into_iter().map(|(_, model)| model).collect()
}

#[async_trait::async_trait]
impl Brain for OpenAi {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        mend(&mut self.said);
        self.said.push(said_by_user(text, &std::mem::take(&mut self.attached))?);
        api::keep(&self.kept_at, &self.said);
        let (mut input, mut output, mut cached) = (0, 0, 0);
        let mut cost: Option<f64> = None;
        for _ in 0..api::MOST_STEPS {
            let answer = self.step(events).await?;
            input += answer.input;
            output += answer.output;
            cached += answer.cached;
            if let Some(spent) = answer.cost {
                cost = Some(cost.unwrap_or(0.0) + spent);
            }
            // Kept as it's to be sent back: its words, and the tools it called.
            let mut message = json!({ "role": "assistant", "content": if answer.text.is_empty() { Value::Null } else { json!(answer.text) } });
            if !answer.calls.is_empty() {
                message["tool_calls"] = answer.calls.iter().map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.arguments } })).collect();
            }
            // What it thought goes back as it came: a model that thinks goes on
            // from there once its tools have answered, and some refuse without it.
            if !answer.thought.is_empty() {
                message["reasoning_details"] = json!(answer.thought);
            }
            self.said.push(message);
            api::keep(&self.kept_at, &self.said);
            if answer.calls.is_empty() {
                let _ = events.send(Event::Usage { input, output, cached, cost });
                return Ok(());
            }
            for call in answer.calls {
                let args = if call.arguments.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(&call.arguments) };
                let (said, failed) = match args {
                    Ok(args) => {
                        let _ = events.send(Event::Tool { id: call.id.clone(), name: call.name.clone(), args: args.clone() });
                        self.setup.run(&call.name, args).await
                    }
                    Err(_) => {
                        let _ = events.send(Event::Tool { id: call.id.clone(), name: call.name.clone(), args: json!({}) });
                        ("The arguments weren't readable JSON, so it didn't run.".to_string(), true)
                    }
                };
                let _ = events.send(Event::Result { id: call.id.clone(), text: said.clone(), is_error: failed });
                self.said.push(json!({ "role": "tool", "tool_call_id": call.id, "content": api::for_model(said) }));
                api::keep(&self.kept_at, &self.said);
            }
        }
        Err("It went on calling tools without answering, so Muxin stopped it.".into())
    }

    async fn stop(&mut self) {
        // Dropping the turn ended its request; `mend` settles what it left open.
    }

    // A picture is a part of the message, beside its text. Nothing else is taken.
    fn attach(&mut self, files: Vec<Attached>) -> Result<(), String> {
        if let Some(file) = files.iter().find(|f| !matches!(f.kind, Kind::Image(_))) {
            return Err(format!("{} can't be given {} here: it takes pictures and text. Claude Code and Anthropic read PDFs.", self.who(), file.name));
        }
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
    use crate::chat::gate::{self, Answer as Said, Gate, Mode};
    use crate::chat::testing::{self, Asked, Server};
    use crate::config::Paths;
    use crate::fleet::Engine;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    fn used(input: u64, output: u64) -> Value {
        json!({"id": "c1", "choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output, "prompt_tokens_details": {"cached_tokens": input / 2}}})
    }

    /// The model calls `name` with `args`, the arguments in two pieces.
    fn calls(id: &str, name: &str, args: &str) -> Vec<Value> {
        let (a, b) = args.split_at(args.len() / 2);
        vec![
            chunk(json!({"role": "assistant", "content": null, "tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]}), None),
            chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": a}}]}), None),
            chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": b}}]}), None),
            chunk(json!({}), Some("tool_calls")),
        ]
    }

    fn says(text: &str) -> Vec<Value> {
        let (a, b) = text.split_at(text.len() / 2);
        vec![chunk(json!({"role": "assistant", "content": a}), None), chunk(json!({"content": b}), None), chunk(json!({}), Some("stop"))]
    }

    /// A stand-in for a provider that gives its answers in order.
    async fn provider(answers: Vec<testing::Answer>) -> Server {
        let left = Mutex::new(VecDeque::from(answers));
        Server::start(move |asked: &Asked| match asked.path.as_str() {
            "/v1/models" => testing::json(200, json!({"object": "list", "data": [
                {"id": "text-embedding-9", "created": 30}, {"id": "gpt-old", "created": 10}, {"id": "gpt-test", "created": 20}, {"id": "tts-2", "created": 40},
            ]})),
            "/v1/chat/completions" => left.lock().unwrap().pop_front().unwrap_or((500, "text/plain", "nothing left to answer".into())),
            _ => (404, "text/plain", String::new()),
        })
        .await
    }

    struct Rig {
        brain: OpenAi,
        gate: Arc<Gate>,
        asks: mpsc::UnboundedReceiver<gate::Event>,
    }

    fn rig(name: &str, provider: &Server, model: Option<&str>) -> Rig {
        let folder = std::env::temp_dir().join(format!("muxin-openai-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let engine = Engine::new(Paths::resolve(Some(folder.join("home")), None), true);
        let (gate, asks) = Gate::new(Mode::Ask, vec![]);
        let setup = Setup {
            engine,
            gate: gate.clone(),
            kind: crate::chat::brain::kind("openai").unwrap(),
            base: format!("{}/v1", provider.url),
            key: "sk-test".into(),
            folder,
            prompt: "You are Muxin's assistant.".into(),
            model: model.map(str::to_string),
            effort: None,
            speed: None,
        };
        let mut brain = OpenAi::new(setup);
        brain.waits = vec![0];
        Rig { brain, gate, asks }
    }

    async fn turn(brain: &mut OpenAi, text: &str) -> (Vec<Event>, Result<(), String>) {
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
        let api = provider(vec![testing::events(&[calls("call_1", "status", "{}"), vec![used(1000, 20)]].concat()), testing::events(&[says("Nine machines."), vec![used(1400, 8)]].concat())]).await;
        let mut rig = rig("turn", &api, None);
        let (seen, outcome) = turn(&mut rig.brain, "how many machines?").await;
        assert_eq!(outcome, Ok(()));

        // With no model picked, the newest one for a conversation that the provider lists.
        assert_eq!(seen[0], Event::Model("gpt-test".into()));
        let seen = &seen[1..];
        assert!(matches!(&seen[0], Event::Tool { id, name, .. } if id == "call_1" && name == "status"));
        assert!(matches!(&seen[1], Event::Result { id, text, is_error: false } if id == "call_1" && text.contains("\"machines\"")));
        assert_eq!(seen[2..5], [Event::Delta("Nine ma".into()), Event::Delta("chines.".into()), Event::Text("Nine machines.".into())]);
        assert_eq!(seen[5], Event::Usage { input: 2400, output: 28, cached: 1200, cost: None });
        // A read asked nobody.
        assert!(rig.asks.try_recv().is_err());

        // The first request: the key as a bearer token, streamed, Muxin's short list as functions.
        let sent = api.asked_at("/v1/chat/completions");
        assert_eq!(sent[0].headers["authorization"], "Bearer sk-test");
        let first = sent[0].json();
        assert_eq!((first["model"].as_str(), first["stream"].as_bool(), first["stream_options"]["include_usage"].as_bool()), (Some("gpt-test"), Some(true), Some(true)));
        assert_eq!(first["messages"][0]["role"], "system");
        assert!(first["messages"][0]["content"].as_str().unwrap().starts_with("You are Muxin's assistant."));
        assert_eq!(first["messages"][1], json!({"role": "user", "content": "how many machines?"}));
        let functions = first["tools"].as_array().unwrap();
        assert_eq!(functions.len(), crate::chat::tools::CORE.len() + 2);
        assert!(functions.iter().all(|f| f["type"] == "function" && f["function"]["name"].is_string() && f["function"]["parameters"]["type"] == "object"));
        // Nothing said about effort or speed, so nothing asked for.
        assert!(first.get("reasoning_effort").is_none() && first.get("service_tier").is_none());
        // The second: the call as the model made it, and what it answered.
        let second = sent[1].json();
        let messages = second["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2], json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "status", "arguments": "{}"}}]}));
        assert_eq!((messages[3]["role"].as_str(), messages[3]["tool_call_id"].as_str()), (Some("tool"), Some("call_1")));
        assert!(messages[3]["content"].as_str().unwrap().contains("\"machines\""));

        // Kept beside the chat, and gone on with by a brain made later.
        assert_eq!(api::kept(&rig.brain.kept_at).len(), 4);
        // A model, an effort and a speed chosen since go with the next request.
        rig.brain.choose(Some("gpt-old".into()), Some("high".into()));
        rig.brain.speed(Some("fast".into()));
        let body = rig.brain.request("gpt-old");
        assert_eq!((body["model"].as_str(), body["reasoning_effort"].as_str(), body["service_tier"].as_str()), (Some("gpt-old"), Some("high"), Some("priority")));
        // Who it is here goes with every request: the model, and what it's reached through.
        assert!(body["messages"][0]["content"].as_str().unwrap().contains("The model answering in this chat is `gpt-old`, reached through OpenAI."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_waits_at_the_gate_and_a_no_is_told_to_the_model() {
        let save = json!({"name": "Uptime", "command": "uptime"}).to_string();
        let api = provider(vec![testing::events(&calls("call_1", "snippet_save", &save)), testing::events(&says("Left it."))]).await;
        let Rig { mut brain, gate, mut asks } = rig("gate", &api, Some("gpt-test"));
        let answering = tokio::spawn(async move {
            let Some(gate::Event::Asked(request)) = asks.recv().await else { panic!("asked") };
            assert_eq!((request.tool.as_str(), request.args["name"].as_str()), ("snippet_save", Some("Uptime")));
            gate.answer(request.id, Said::No("not now".into()));
        });
        let (seen, outcome) = turn(&mut brain, "save it").await;
        answering.await.unwrap();
        assert_eq!(outcome, Ok(()));
        assert!(matches!(&seen[2], Event::Result { text, is_error: true, .. } if text.contains("declined") && text.contains("not now")));
        // The model was told, in the call's answer.
        let second = api.asked_at("/v1/chat/completions")[1].json();
        assert!(second["messages"][3]["content"].as_str().unwrap().contains("not now"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_a_provider_refuses_is_said_in_words_and_a_passing_refusal_is_tried_again() {
        let api = provider(vec![
            testing::json(401, json!({"error": {"message": "Incorrect API key provided", "code": "invalid_api_key"}})),
            testing::json(404, json!({"error": {"message": "The model `gpt-test` does not exist"}})),
            testing::json(429, json!({"error": {"message": "Rate limit reached"}})),
            testing::events(&says("Hello.")),
            testing::events(&[chunk(json!({"content": "Half an"}), None), chunk(json!({}), Some("length"))]),
            testing::events(&[json!({"error": {"message": "upstream fell over"}})]),
            testing::events(&[]),
        ])
        .await;
        let mut rig = rig("refused", &api, Some("gpt-test"));
        let why = |outcome: (Vec<Event>, Result<(), String>)| outcome.1.unwrap_err();
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "OpenAI didn't accept this API key. Connect it again in Settings › AI.");
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "OpenAI refused the request (404): The model `gpt-test` does not exist");
        // Told to slow down: asked once more, and answered.
        assert_eq!(turn(&mut rig.brain, "hello").await.1, Ok(()));
        assert!(why(turn(&mut rig.brain, "hello").await).contains("stopped at its length limit"));
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "OpenAI stopped with an error: upstream fell over");
        assert_eq!(why(turn(&mut rig.brain, "hello").await), "The connection to OpenAI ended before it answered.");
    }

    #[test]
    fn a_call_left_without_its_answer_gets_one_right_after_it() {
        let call = |id: &str| json!({"id": id, "type": "function", "function": {"name": "status", "arguments": "{}"}});
        let mut said = vec![
            json!({"role": "user", "content": "one"}),
            json!({"role": "assistant", "content": null, "tool_calls": [call("a"), call("b")]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ran"}),
            json!({"role": "user", "content": "two"}),
        ];
        mend(&mut said);
        assert_eq!(said.len(), 5);
        assert_eq!(said[3], json!({"role": "tool", "tool_call_id": "b", "content": "The user stopped the turn before this ran."}));
        assert_eq!(said[4]["content"], "two");
        // Mended, there's nothing more to mend.
        let before = said.clone();
        mend(&mut said);
        assert_eq!(said, before);
    }

    #[test]
    fn a_picture_is_a_part_of_what_the_user_said_and_a_pdf_isnt_taken() {
        let dir = std::env::temp_dir().join(format!("muxin-openai-attach-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("shot.png"), [1u8, 2, 3]).unwrap();
        let shot = Attached { name: "shot.png".into(), path: dir.join("shot.png"), kind: Kind::Image("image/png") };
        assert_eq!(said_by_user("hi", &[]).unwrap(), json!({"role": "user", "content": "hi"}));
        assert_eq!(said_by_user("What's this?", &[shot]).unwrap()["content"], json!([{"type": "text", "text": "What's this?"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQID"}}]));
    }

    #[test]
    fn openrouter_lists_what_can_call_tools_with_each_models_own_efforts() {
        // Entries as OpenRouter gives them, cut to what's read.
        let chat = ["/api/v1/chat/completions", "/api/v1/completions", "/api/v1/messages", "/api/v1/responses"];
        let list = json!({"data": [
            {"id": "anthropic/claude-opus-5.5", "name": "Anthropic: Claude Opus 5.5", "created": 50, "endpoints": chat,
             "supported_parameters": ["include_reasoning", "max_tokens", "reasoning", "tool_choice", "tools"],
             "reasoning": {"mandatory": true, "supported_efforts": ["max", "xhigh", "high", "medium", "low"], "default_effort": "high"}},
            // Answered hours later, in a batch: not for a conversation, though it takes tools.
            {"id": "anthropic/claude-opus-5.5:batch", "name": "Anthropic: Claude Opus 5.5 (batch)", "created": 50, "endpoints": ["/api/v1/batches"],
             "supported_parameters": ["reasoning", "tools"], "reasoning": {"supported_efforts": ["high", "low"]}},
            // Can't call tools.
            {"id": "moondream/moondream3-preview", "name": "Moondream 3", "created": 60, "endpoints": chat, "supported_parameters": ["max_tokens", "temperature"]},
            // Thinks, takes any level.
            {"id": "x/any", "name": "X: Any", "created": 90, "endpoints": chat, "supported_parameters": ["reasoning", "tools"], "reasoning": {"supported_efforts": null, "default_effort": "none"}},
            // Thinks, with no say in how hard.
            {"id": "x/fixed", "name": "X: Fixed", "created": 30, "endpoints": chat, "supported_parameters": ["reasoning", "tools"], "reasoning": {"mandatory": true}},
            // Doesn't think.
            {"id": "x/plain:free", "name": "X: Plain (free)", "created": 20, "endpoints": chat, "supported_parameters": ["tools"]},
        ]});
        // In the order given (the most used first), whenever each came out.
        let models = listed("openrouter", &list);
        let efforts = |m: &Model| m.efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(
            models.iter().map(|m| (m.id.as_str(), efforts(m), m.effort.as_deref())).collect::<Vec<_>>(),
            [("anthropic/claude-opus-5.5", "low medium high xhigh max".to_string(), Some("high")), ("x/any", "low medium high".to_string(), None), ("x/fixed", String::new(), None), ("x/plain:free", String::new(), None)]
        );
        assert_eq!(models[0].name, "Anthropic: Claude Opus 5.5");
        assert!(models.iter().all(|m| !m.fast));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn openrouter_proves_the_key_takes_effort_its_own_way_and_gets_its_thinking_back() {
        // Its list is public: only what it says about the key tells a good one.
        let thinking = |text: &str, signature: Value| chunk(json!({"reasoning_details": [{"type": "reasoning.text", "text": text, "signature": signature, "format": "anthropic-claude-v1", "index": 0}]}), None);
        let sealed = json!({"type": "reasoning.encrypted", "data": "c2VhbGVk", "id": "r1", "format": "anthropic-claude-v1", "index": 1});
        let first = [vec![thinking("Count ", Value::Null), thinking("them.", json!("sig-1")), chunk(json!({"reasoning_details": [sealed.clone()]}), None)], calls("call_1", "status", "{}"), vec![json!({"choices": [], "usage": {"prompt_tokens": 900, "completion_tokens": 30, "cost": 0.25}})]].concat();
        let second = [says("Nine."), vec![json!({"choices": [], "usage": {"prompt_tokens": 1200, "completion_tokens": 4, "cost": 0.125}})]].concat();
        let left = Mutex::new(VecDeque::from(vec![testing::events(&first), testing::events(&second)]));
        let api = Server::start(move |asked: &Asked| match (asked.path.as_str(), asked.headers.get("authorization").map(String::as_str)) {
            ("/v1/key", Some("Bearer sk-or-good")) => testing::json(200, json!({"data": {"label": "sk-or-...ood", "limit_remaining": null}})),
            ("/v1/key", _) => testing::json(401, json!({"error": {"message": "User not found.", "code": 401}})),
            ("/v1/models", _) => testing::json(200, json!({"data": [{"id": "x/a", "name": "X: A"}]})),
            ("/v1/chat/completions", _) => left.lock().unwrap().pop_front().unwrap_or((500, "text/plain", "nothing left to answer".into())),
            _ => (404, "text/plain", String::new()),
        })
        .await;
        let openrouter = crate::chat::brain::kind("openrouter").unwrap();
        let base = format!("{}/v1", api.url);
        assert_eq!(models(openrouter, &base, "sk-or-made-up").await.unwrap_err(), "OpenRouter didn't accept this API key. Connect it again in Settings › AI.");
        assert_eq!(models(openrouter, &base, "sk-or-good").await.unwrap().len(), 1);
        // Its list is asked for the most used first.
        assert_eq!(api.asked_at("/v1/models").last().unwrap().query.get("sort").map(String::as_str), Some("most-popular"));
        // Another provider's list is asked for with the key, which proves it: nothing else is asked.
        assert!(api.asked_at("/v1/key").len() == 2 && models(crate::chat::brain::kind("xai").unwrap(), &base, "sk-x").await.is_ok() && api.asked_at("/v1/key").len() == 2);

        let mut rig = rig("openrouter", &api, Some("x/a"));
        rig.brain.setup.kind = openrouter;
        rig.brain.setup.key = "sk-or-good".into();
        rig.brain.choose(Some("x/a".into()), Some("xhigh".into()));
        let (seen, outcome) = turn(&mut rig.brain, "how many machines?").await;
        assert_eq!(outcome, Ok(()));
        // What both requests cost, added up.
        assert_eq!(seen.last(), Some(&Event::Usage { input: 2100, output: 34, cached: 0, cost: Some(0.375) }));
        let sent = api.asked_at("/v1/chat/completions");
        let asked = sent[0].json();
        assert_eq!(asked["reasoning"], json!({"effort": "xhigh"}));
        assert!(asked.get("reasoning_effort").is_none());
        // What it thought goes back with the call: its words joined, signed once, the sealed piece as it came.
        let back = &sent[1].json()["messages"][2];
        assert_eq!(back["tool_calls"][0]["id"], "call_1");
        assert_eq!(back["reasoning_details"], json!([{"type": "reasoning.text", "text": "Count them.", "signature": "sig-1", "format": "anthropic-claude-v1", "index": 0}, sealed]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_model_is_told_which_it_is_and_the_user_which_one_answered() {
        // The provider calls the model by a dated name: said once, after the short one asked for.
        let dated = |text: &str| says(text).into_iter().map(|mut chunk| { chunk["model"] = json!("gpt-test-0101"); chunk }).collect::<Vec<_>>();
        let api = provider(vec![testing::events(&dated("One.")), testing::events(&dated("Two.")), testing::events(&says("Three."))]).await;
        let mut rig = rig("told", &api, Some("gpt-test"));
        // By the name the pickers show it under, when its list has one.
        crate::chat::brain::catalog::keep(&rig.brain.setup.engine.paths().dir, "openai", &[Model { id: "gpt-test".into(), name: "GPT Test".into(), efforts: Vec::new(), effort: None, fast: false }]);
        let models = |seen: &[Event]| seen.iter().filter_map(|e| if let Event::Model(name) = e { Some(name.clone()) } else { None }).collect::<Vec<_>>();
        assert_eq!(models(&turn(&mut rig.brain, "one").await.0), ["gpt-test", "gpt-test-0101"]);
        assert!(models(&turn(&mut rig.brain, "two").await.0).is_empty());
        let system = api.asked_at("/v1/chat/completions")[0].json()["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(system.contains("The model answering in this chat is GPT Test (`gpt-test`), reached through OpenAI. When asked which model or AI you are, say exactly that."), "{system}");
        // Another model picked: it's told its own name, and the user that it answers now.
        rig.brain.choose(Some("gpt-old".into()), None);
        assert_eq!(models(&turn(&mut rig.brain, "three").await.0), ["gpt-old"]);
        assert!(api.asked_at("/v1/chat/completions")[2].json()["messages"][0]["content"].as_str().unwrap().contains("is `gpt-old`, reached through OpenAI."));
    }

    #[test]
    fn thinking_in_words_joins_its_own_piece_and_no_other() {
        let words = |text: &str, index: u64| json!({"type": "reasoning.text", "text": text, "signature": null, "index": index});
        let mut so_far = Vec::new();
        for piece in [words("a", 0), words("b", 0), words("c", 1), json!({"type": "reasoning.summary", "summary": "s", "index": 2}), json!({"type": "reasoning.summary", "summary": "t", "index": 2}), words("d", 3)] {
            thought(&mut so_far, &piece);
        }
        assert_eq!(so_far.iter().map(|p| p["text"].as_str().or(p["summary"].as_str()).unwrap()).collect::<Vec<_>>(), ["ab", "c", "s", "t", "d"]);
    }

    #[test]
    fn the_models_listed_are_the_ones_for_a_conversation_newest_first() {
        let list = json!({"data": [
            {"id": "gpt-old", "created": 10}, {"id": "text-embedding-9", "created": 30}, {"id": "gpt-new", "created": 20}, {"id": "dall-e-9", "created": 40},
        ]});
        let models = listed("openai", &list);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-new", "gpt-old"]);
        // OpenAI's take an effort and a faster tier.
        assert_eq!(models[0].efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low", "medium", "high"]);
        assert!(models[0].fast);
        // A gateway names its models, and keeps its own order when it doesn't date them.
        let gateway = listed("openrouter", &json!({"data": [{"id": "x/b", "name": "X: B"}, {"id": "x/a", "name": "X: A"}]}));
        assert_eq!(gateway.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.fast)).collect::<Vec<_>>(), [("x/b", "X: B", false), ("x/a", "X: A", false)]);
        assert!(gateway[0].efforts.is_empty());
        // Google's ids come with a folder, which isn't sent.
        assert_eq!(listed("google", &json!({"data": [{"id": "models/gemini-9-pro"}]}))[0].id, "gemini-9-pro");
        assert!(listed("deepseek", &json!({"data": [{"id": "deepseek-chat"}]}))[0].efforts.is_empty());
        assert!(listed("openai", &json!({})).is_empty());
    }
}
