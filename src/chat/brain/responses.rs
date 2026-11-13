//! Muxin's own brain, on the user's ChatGPT plan (see ../account.rs for the
//! sign-in). No other program: Muxin asks the model itself, through OpenAI's
//! Responses API, and runs the tools the model calls right here, each one
//! past the chat's gate first.
//!
//! How a turn goes:
//!
//! 1. The conversation so far is sent, whole, with Muxin's tools. OpenAI
//!    keeps none of it (`store: false`), so Muxin does, beside the chat.
//! 2. The answer comes as a stream: text as it's written, and the tools the
//!    model wants called.
//! 3. Each tool runs (or the user says no), its result is added to the
//!    conversation, and it's sent again. Until the model answers without
//!    calling anything.
//!
//! What plan requests must look like is OpenAI's rule, not a choice here:
//! streamed, not stored, tools grouped in a namespace, and none of the
//! fields that route refuses (`temperature`, `max_output_tokens`, ...).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Brain, Event};
use crate::chat::account::{self, Endpoints, USAGE_URL, Vault};
use crate::chat::gate::Gate;
use crate::fleet::Engine;

/// The name Muxin's tools are grouped under.
const NAMESPACE: &str = "muxin";
/// Model answers in one turn before Muxin stops it: a model that keeps
/// calling tools without ever answering.
const MOST_STEPS: usize = 60;
/// How much of a tool's result the model is given.
const MOST_OUTPUT: usize = 60_000;
/// No word from OpenAI for this long, mid-answer: the connection is gone.
const QUIET: Duration = Duration::from_secs(180);

/// A model the signed-in account may answer with.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// What requests name it by.
    pub slug: String,
    /// What it's shown as.
    pub name: String,
}

/// The models the signed-in account lists, in its order (the first is what
/// answers when the user hasn't chosen).
pub async fn models(vault: &Vault, at: &Endpoints) -> Result<Vec<Model>, String> {
    let token = account::token(vault, at, None).await?;
    listed(at, &token).await
}

async fn listed(at: &Endpoints, token: &str) -> Result<Vec<Model>, String> {
    let http = crate::util::http_client(|| reqwest::Client::builder().timeout(Duration::from_secs(30)));
    let resp = http.get(format!("{}/models", at.api)).bearer_auth(token).send().await.map_err(|e| format!("Couldn't reach ChatGPT: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        return Err(refusal(status.as_u16(), &resp.text().await.unwrap_or_default()).words());
    }
    let list: Value = resp.json().await.map_err(|_| "ChatGPT's list of models isn't readable.".to_string())?;
    // The ones meant to be shown, in the order given.
    Ok(list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| m["slug"].as_str().map(|slug| Model { slug: slug.to_string(), name: m["display_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(slug).to_string() }))
        .collect())
}

pub struct ChatGpt {
    engine: Arc<Engine>,
    gate: Arc<Gate>,
    vault: Vault,
    at: Endpoints,
    /// Where this chat's conversation is kept, as it's sent.
    kept_at: PathBuf,
    /// The conversation: what the user said, what the model said and
    /// called, what the tools answered. Sent whole each time.
    said: Vec<Value>,
    prompt: String,
    /// The model that answers; the first one the account lists, until set.
    model: Option<String>,
    /// The user was told which.
    model_said: bool,
}

impl ChatGpt {
    /// A brain for the chat whose folder is `folder`, going on with the
    /// conversation kept there.
    pub fn new(engine: Arc<Engine>, gate: Arc<Gate>, vault: Vault, at: Endpoints, folder: PathBuf, prompt: String, model: Option<String>) -> ChatGpt {
        let kept_at = folder.join("responses.json");
        let said = std::fs::read_to_string(&kept_at).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        ChatGpt { engine, gate, vault, at, kept_at, said, prompt, model: model.filter(|m| !m.trim().is_empty()), model_said: false }
    }

    fn keep(&self) {
        let tmp = self.kept_at.with_extension(format!("json.{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(&self.said).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.kept_at);
        }
    }

    fn http(&self) -> reqwest::Client {
        crate::util::http_client(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(30)))
    }

    /// The model to ask: the one chosen, else the first the account lists.
    /// Says which, the first time.
    async fn model(&mut self, token: &str, events: &mpsc::UnboundedSender<Event>) -> Result<String, String> {
        if let Some(model) = &self.model {
            if !self.model_said {
                self.model_said = true;
                let _ = events.send(Event::Model(model.clone()));
            }
            return Ok(model.clone());
        }
        let first = listed(&self.at, token).await?.into_iter().next().ok_or("Your ChatGPT account lists no model Muxin can use.")?;
        self.model = Some(first.slug.clone());
        self.model_said = true;
        let _ = events.send(Event::Model(first.name));
        Ok(first.slug)
    }

    /// One answer from the model to the conversation as it stands.
    async fn step(&mut self, events: &mpsc::UnboundedSender<Event>) -> Result<Answer, String> {
        let mut refused: Option<String> = None;
        let mut waits = [2u64, 6].into_iter();
        loop {
            let token = account::token(&self.vault, &self.at, refused.as_deref()).await?;
            let model = self.model(&token, events).await?;
            let body = request(&model, &self.prompt, &self.said);
            let sent = self.http().post(format!("{}/responses", self.at.api)).bearer_auth(&token).json(&body).send().await;
            let resp = match sent {
                Ok(resp) => resp,
                Err(e) => return Err(format!("Couldn't reach ChatGPT: {e}")),
            };
            if resp.status().is_success() {
                return read_stream(resp, events).await;
            }
            let status = resp.status().as_u16();
            match refusal(status, &resp.text().await.unwrap_or_default()) {
                // The token was turned away: renewed once, then it's the sign-in.
                Refusal::SignIn(_) if refused.is_none() => refused = Some(token),
                // Not now: a couple more tries, a little apart.
                Refusal::Later(why) => match waits.next() {
                    Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                    None => return Err(why),
                },
                other => return Err(other.words()),
            }
        }
    }

    /// Runs a tool the model called, past the gate. What it answered, and
    /// whether that's a failure.
    async fn run(&self, name: &str, args: Value) -> (String, bool) {
        if !crate::mcp::tools().iter().any(|t| t["name"] == name) {
            return (format!("There's no tool named {name}."), true);
        }
        if let Err(why) = self.gate.check(name, &args).await {
            return (json!({ "error": { "code": "declined", "message": why } }).to_string(), true);
        }
        let result = crate::mcp::call_tool(&self.engine, &crate::mcp::Session::named("chat"), name, args).await;
        (super::result_text(&result["content"]), result["isError"] == true)
    }
}

/// What's sent for one answer.
fn request(model: &str, prompt: &str, said: &[Value]) -> Value {
    json!({
        "model": model,
        // The tools explain themselves to a program's MCP client when it
        // connects; here nothing connects, so what that would have said goes along.
        "instructions": format!("{prompt}\n\n{}", crate::mcp::INSTRUCTIONS),
        "input": said,
        "tools": tools(),
        "store": false,
        "stream": true,
        // Nothing is stored there, so the model's reasoning comes back sealed,
        // to be sent along with the tool results it led to.
        "include": ["reasoning.encrypted_content"],
    })
}

/// Muxin's tools as the model is given them: functions, in one namespace.
fn tools() -> Value {
    let functions: Vec<Value> = crate::mcp::tools().into_iter().map(|t| json!({ "type": "function", "name": t["name"], "description": t["description"], "parameters": t["inputSchema"] })).collect();
    json!([{
        "type": "namespace",
        "name": NAMESPACE,
        "description": "Muxin: the user's machines, the agent sessions and boxes on them, runners, workspaces, files, logs, GitHub, cloud machines.",
        "tools": functions,
    }])
}

/// Why OpenAI turned a request away.
#[derive(Debug, PartialEq)]
enum Refusal {
    /// The sign-in wasn't accepted.
    SignIn(String),
    /// Not available right now; worth trying again shortly.
    Later(String),
    /// No, and asking again won't change it.
    No(String),
}

impl Refusal {
    fn words(self) -> String {
        match self {
            Refusal::SignIn(why) | Refusal::Later(why) | Refusal::No(why) => why,
        }
    }
}

/// What a refused request means, from its status and what came with it: the
/// API's `{"error": {...}}`, or the plainer `{"detail": "..."}` from before
/// a stream opens.
fn refusal(status: u16, body: &str) -> Refusal {
    let answer: Value = serde_json::from_str(body).unwrap_or_default();
    let error = &answer["error"];
    if let Some(code) = error["code"].as_str() {
        return failure(code, error["message"].as_str(), error["param"].as_str());
    }
    let said = error["message"].as_str().or(answer["detail"].as_str()).map(|s| format!(": {s}")).unwrap_or_default();
    match status {
        401 => Refusal::SignIn("ChatGPT didn't accept this sign-in. Sign in again: `muxin chat --sign-in`.".into()),
        503 => Refusal::Later("ChatGPT plan use isn't available right now. Try again in a little while.".into()),
        _ => Refusal::No(format!("ChatGPT refused the request ({status}){said}")),
    }
}

/// What one of OpenAI's error codes means for the user.
fn failure(code: &str, message: Option<&str>, param: Option<&str>) -> Refusal {
    match code {
        "subscription_sharing_usage_limit_exceeded" => Refusal::No(format!("You've reached a usage limit of your ChatGPT plan, or the limit you set for Muxin. Manage usage: {USAGE_URL}")),
        "subscription_sharing_user_not_eligible" => Refusal::No("This ChatGPT account can't use its plan in other apps.".into()),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => Refusal::Later("ChatGPT plan use isn't available right now. Try again in a little while.".into()),
        "subscription_sharing_unsupported_capability" => Refusal::No(format!("ChatGPT plan use doesn't allow something in Muxin's request ({}).", param.unwrap_or("it didn't say what"))),
        "subscription_sharing_invalid_user" => Refusal::SignIn("ChatGPT didn't accept this sign-in. Sign in again: `muxin chat --sign-in`.".into()),
        _ => Refusal::No(match message {
            Some(message) => format!("ChatGPT refused the request: {message} ({code})"),
            None => format!("ChatGPT refused the request ({code})."),
        }),
    }
}

/// What the model answered in one request.
#[derive(Debug, Default)]
struct Answer {
    /// Everything it produced, as it's to be sent back: its reasoning
    /// (sealed), its words, the tools it called.
    items: Vec<Value>,
    input: u64,
    output: u64,
    cached: u64,
    /// OpenAI said it's complete. An answer isn't one until then.
    complete: bool,
}

/// The stream as it arrives, cut into its events. A piece of the stream can
/// end anywhere, in the middle of an event or of a character.
#[derive(Default)]
struct Events {
    waiting: Vec<u8>,
}

impl Events {
    /// Adds a piece; the events it completes.
    fn push(&mut self, piece: &[u8]) -> Vec<Value> {
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

/// What one event of the stream means: adds to the answer, reports what
/// the user should see, or says why the answer failed.
fn heard(event: &Value, answer: &mut Answer, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    match event["type"].as_str().unwrap_or("") {
        "response.output_text.delta" => {
            if let Some(piece) = event["delta"].as_str().filter(|p| !p.is_empty()) {
                let _ = events.send(Event::Delta(piece.to_string()));
            }
        }
        "response.output_item.done" => {
            let item = &event["item"];
            if item["type"] == "message" {
                let text: String = item["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect();
                let _ = events.send(Event::Text(text));
            }
            answer.items.push(item.clone());
        }
        "response.completed" => {
            let response = &event["response"];
            let usage = &response["usage"];
            answer.input = usage["input_tokens"].as_u64().unwrap_or(0);
            answer.output = usage["output_tokens"].as_u64().unwrap_or(0);
            answer.cached = usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            if answer.items.is_empty() {
                answer.items = response["output"].as_array().cloned().unwrap_or_default();
            }
            answer.complete = true;
        }
        "response.failed" => {
            let error = &event["response"]["error"];
            return Err(failure(error["code"].as_str().unwrap_or("unknown_error"), error["message"].as_str(), error["param"].as_str()).words());
        }
        "response.incomplete" => {
            let why = event["response"]["incomplete_details"]["reason"].as_str().unwrap_or("it didn't say why");
            return Err(format!("ChatGPT stopped before it finished ({}).", why.replace('_', " ")));
        }
        "error" => {
            let error = if event["error"].is_object() { &event["error"] } else { event };
            return Err(failure(error["code"].as_str().unwrap_or("unknown_error"), error["message"].as_str(), error["param"].as_str()).words());
        }
        _ => {}
    }
    Ok(())
}

/// Reads an answer's stream to its end.
async fn read_stream(mut resp: reqwest::Response, events: &mpsc::UnboundedSender<Event>) -> Result<Answer, String> {
    let mut answer = Answer::default();
    let mut stream = Events::default();
    loop {
        let piece = match tokio::time::timeout(QUIET, resp.chunk()).await {
            Ok(Ok(Some(piece))) => piece,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(format!("The connection to ChatGPT broke off: {e}")),
            Err(_) => return Err("ChatGPT went quiet in the middle of answering.".into()),
        };
        for event in stream.push(&piece) {
            heard(&event, &mut answer, events)?;
        }
    }
    // Only an answer OpenAI called complete is one.
    if answer.complete { Ok(answer) } else { Err("The connection to ChatGPT ended before it finished answering.".into()) }
}

/// Puts right a conversation that was cut off: a tool the model called that
/// never got its answer (the user stopped the turn) gets one saying so. A
/// call without an answer can't be sent.
fn mend(said: &mut Vec<Value>) {
    let answered: Vec<String> = said.iter().filter(|i| i["type"] == "function_call_output").filter_map(|i| i["call_id"].as_str().map(String::from)).collect();
    let open: Vec<String> = said.iter().filter(|i| i["type"] == "function_call").filter_map(|i| i["call_id"].as_str()).filter(|id| !answered.iter().any(|a| a == id)).map(String::from).collect();
    for call_id in open {
        said.push(json!({ "type": "function_call_output", "call_id": call_id, "output": "The user stopped the turn before this ran." }));
    }
}

/// A call's arguments without the ones the model filled in with nothing.
/// Some models give every argument a value, and write "" or null for the
/// ones they don't mean to use; a tool would take that as said (`replacing:
/// ""` as a snippet named nothing). An argument the tool requires is kept
/// as it came, for the tool to refuse in its own words.
fn tidy(name: &str, mut args: Value) -> Value {
    let tools = crate::mcp::tools();
    let required: Vec<&str> = tools.iter().find(|t| t["name"] == name).and_then(|t| t["inputSchema"]["required"].as_array()).into_iter().flatten().filter_map(Value::as_str).collect();
    if let Some(map) = args.as_object_mut() {
        map.retain(|key, value| required.contains(&key.as_str()) || !(value.is_null() || value.as_str().is_some_and(|s| s.is_empty())));
    }
    args
}

/// A tool's result as the model gets it: all of it, unless it's very long.
fn for_model(mut text: String) -> String {
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

#[async_trait::async_trait]
impl Brain for ChatGpt {
    async fn turn(&mut self, text: &str, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
        mend(&mut self.said);
        self.said.push(json!({ "role": "user", "content": text }));
        self.keep();
        let (mut input, mut output, mut cached) = (0, 0, 0);
        for _ in 0..MOST_STEPS {
            let answer = self.step(events).await?;
            input += answer.input;
            output += answer.output;
            cached += answer.cached;
            let calls: Vec<Value> = answer.items.iter().filter(|i| i["type"] == "function_call").cloned().collect();
            self.said.extend(answer.items);
            self.keep();
            if calls.is_empty() {
                let _ = events.send(Event::Usage { input, output, cached, cost: None });
                return Ok(());
            }
            for call in calls {
                let id = call["call_id"].as_str().unwrap_or_default().to_string();
                let name = call["name"].as_str().unwrap_or_default().to_string();
                let args = call["arguments"].as_str().map(|a| if a.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(a) });
                let (said, failed) = match args {
                    Some(Ok(args)) => {
                        // What's shown, asked about and run is the same thing.
                        let args = tidy(&name, args);
                        let _ = events.send(Event::Tool { id: id.clone(), name: name.clone(), args: args.clone() });
                        self.run(&name, args).await
                    }
                    _ => {
                        let _ = events.send(Event::Tool { id: id.clone(), name: name.clone(), args: json!({}) });
                        ("The arguments weren't readable JSON, so it didn't run.".to_string(), true)
                    }
                };
                let _ = events.send(Event::Result { id: id.clone(), text: said.clone(), is_error: failed });
                self.said.push(json!({ "type": "function_call_output", "call_id": id, "output": for_model(said) }));
                self.keep();
            }
        }
        Err("It went on calling tools without answering, so Muxin stopped it.".into())
    }

    async fn stop(&mut self) {
        // Dropping the turn ended its request; `mend` settles what it left open.
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::chat::account::tests::{FakeOpenAi, signed_in};
    use crate::chat::gate::{self, Answer as Said, Mode};
    use crate::chat::testing::{self, Asked, Server};
    use crate::config::Paths;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": name, "namespace": "muxin", "arguments": args.to_string(), "status": "completed"}})
    }

    fn says(text: &str) -> Vec<Value> {
        let (a, b) = text.split_at(text.len() / 2);
        vec![
            json!({"type": "response.output_text.delta", "delta": a}),
            json!({"type": "response.output_text.delta", "delta": b}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}}),
        ]
    }

    fn completed(input: u64, output: u64) -> Value {
        json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": input, "output_tokens": output, "input_tokens_details": {"cached_tokens": input / 2}}}})
    }

    /// A stand-in for the Responses API that gives its answers in order.
    async fn api(answers: Vec<testing::Answer>) -> Server {
        let left = Mutex::new(VecDeque::from(answers));
        Server::start(move |asked: &Asked| match asked.path.as_str() {
            "/v1/models" => testing::json(200, json!({"models": [{"slug": "hidden", "visibility": "hide"}, {"slug": "gpt-test", "display_name": "Test", "visibility": "list"}]})),
            "/v1/responses" => left.lock().unwrap().pop_front().unwrap_or((500, "text/plain", "nothing left to answer".into())),
            _ => (404, "text/plain", String::new()),
        })
        .await
    }

    struct Rig {
        brain: ChatGpt,
        gate: Arc<Gate>,
        asks: mpsc::UnboundedReceiver<gate::Event>,
        folder: PathBuf,
    }

    fn rig(name: &str, api: &Server, mode: Mode) -> Rig {
        let folder = std::env::temp_dir().join(format!("muxin-responses-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let engine = Engine::new(Paths::resolve(Some(folder.join("home")), None), true);
        let (gate, asks) = Gate::new(mode, vec![]);
        let at = Endpoints { api: format!("{}/v1", api.url), ..Endpoints::default() };
        let brain = ChatGpt::new(engine, gate.clone(), signed_in(name), at, folder.clone(), "You are Muxin's assistant.".into(), None);
        Rig { brain, gate, asks, folder }
    }

    async fn turn(brain: &mut ChatGpt, text: &str) -> (Vec<Event>, Result<(), String>) {
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
        let api = api(vec![testing::events(&[call("call_1", "status", json!({})), completed(1000, 20)]), testing::events(&[says("Nine machines."), vec![completed(1400, 8)]].concat())]).await;
        let mut rig = rig("turn", &api, Mode::Ask);
        let (seen, outcome) = turn(&mut rig.brain, "how many machines?").await;
        assert_eq!(outcome, Ok(()));

        // What the user saw: which model, the tool and its result, the words as they came, what it used.
        assert_eq!(seen[0], Event::Model("Test".into()));
        let seen = &seen[1..];
        assert!(matches!(&seen[0], Event::Tool { id, name, .. } if id == "call_1" && name == "status"));
        assert!(matches!(&seen[1], Event::Result { id, text, is_error: false } if id == "call_1" && text.contains("\"machines\"")));
        assert_eq!(seen[2..5], [Event::Delta("Nine ma".into()), Event::Delta("chines.".into()), Event::Text("Nine machines.".into())]);
        assert_eq!(seen[5], Event::Usage { input: 2400, output: 28, cached: 1200, cost: None });
        // A read asked nobody.
        assert!(rig.asks.try_recv().is_err());

        // The first request: on the user's token, streamed and not stored, tools in a namespace.
        let sent = api.asked_at("/v1/responses");
        assert_eq!(sent[0].headers["authorization"], "Bearer access-1");
        let first = sent[0].json();
        assert_eq!((first["model"].as_str(), first["store"].as_bool(), first["stream"].as_bool()), (Some("gpt-test"), Some(false), Some(true)));
        assert_eq!(first["input"], json!([{"role": "user", "content": "how many machines?"}]));
        assert!(first["instructions"].as_str().unwrap().starts_with("You are Muxin's assistant."));
        assert_eq!((first["tools"][0]["type"].as_str(), first["tools"][0]["name"].as_str()), (Some("namespace"), Some("muxin")));
        let functions = first["tools"][0]["tools"].as_array().unwrap();
        assert_eq!(functions.len(), crate::mcp::tools().len());
        assert!(functions.iter().all(|f| f["type"] == "function" && f["parameters"]["type"] == "object" && f["name"].is_string()));
        // None of what that route refuses.
        for field in ["temperature", "top_p", "max_output_tokens", "metadata", "user", "truncation", "previous_response_id", "prompt_cache_retention"] {
            assert!(first.get(field).is_none(), "{field}");
        }
        // The second: the same conversation, with the call as the model made it and what it answered.
        let second = sent[1].json();
        let input = second["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!((input[1]["type"].as_str(), input[1]["call_id"].as_str(), input[1]["namespace"].as_str()), (Some("function_call"), Some("call_1"), Some("muxin")));
        assert_eq!((input[2]["type"].as_str(), input[2]["call_id"].as_str()), (Some("function_call_output"), Some("call_1")));
        assert!(input[2]["output"].as_str().unwrap().contains("\"machines\""));

        // Kept beside the chat, and gone on with by a brain made later.
        let later = ChatGpt::new(rig.brain.engine.clone(), rig.gate.clone(), rig.brain.vault.clone(), rig.brain.at.clone(), rig.folder.clone(), String::new(), None);
        assert_eq!(later.said.len(), 4);
        assert_eq!(later.said[3]["type"], "message");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_waits_at_the_gate_and_a_no_is_told_to_the_model() {
        let save = json!({"name": "Uptime", "command": "uptime"});
        let api = api(vec![testing::events(&[call("call_1", "snippet_save", save), completed(10, 1)]), testing::events(&[says("Left it."), vec![completed(10, 1)]].concat())]).await;
        let Rig { mut brain, gate, mut asks, .. } = rig("gate", &api, Mode::Ask);
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
        let second = api.asked_at("/v1/responses")[1].json();
        assert!(second["input"][2]["output"].as_str().unwrap().contains("not now"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_openai_refuses_is_said_in_words() {
        let limit = json!({"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}});
        let api = api(vec![
            testing::json(429, limit),
            testing::events(&[json!({"type": "response.output_text.delta", "delta": "Half an"}), json!({"type": "response.failed", "response": {"error": {"code": "subscription_sharing_usage_limit_exceeded"}}})]),
            testing::events(&[json!({"type": "response.output_text.delta", "delta": "cut off"})]),
            testing::events(&[json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}}})]),
            testing::json(403, json!({"error": {"code": "subscription_sharing_user_not_eligible"}})),
            testing::json(403, json!({"detail": "region not served"})),
        ])
        .await;
        let mut rig = rig("refused", &api, Mode::Ask);
        // Before a stream, and after one began: the limit, with where to manage it.
        for _ in 0..2 {
            let why = turn(&mut rig.brain, "hello").await.1.unwrap_err();
            assert!(why.contains("usage limit") && why.contains("https://chatgpt.com/settings/usage"), "{why}");
        }
        assert!(turn(&mut rig.brain, "hello").await.1.unwrap_err().contains("ended before it finished"));
        assert!(turn(&mut rig.brain, "hello").await.1.unwrap_err().contains("stopped before it finished (max output tokens)"));
        assert!(turn(&mut rig.brain, "hello").await.1.unwrap_err().contains("can't use its plan in other apps"));
        assert!(turn(&mut rig.brain, "hello").await.1.unwrap_err().contains("(403): region not served"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_turned_away_is_renewed_once_and_the_request_sent_again() {
        let openai = FakeOpenAi::start("user-1", "chatgpt.tokens.use.direct offline_access").await;
        let api = api(vec![testing::json(401, json!({"detail": "expired"})), testing::events(&[says("Hello."), vec![completed(5, 1)]].concat())]).await;
        let mut rig = rig("renewed", &api, Mode::Ask);
        rig.brain.at = Endpoints { api: format!("{}/v1", api.url), ..openai.at.clone() };
        assert_eq!(turn(&mut rig.brain, "hello").await.1, Ok(()));
        let sent = api.asked_at("/v1/responses");
        assert_eq!((sent[0].headers["authorization"].as_str(), sent[1].headers["authorization"].as_str()), ("Bearer access-1", "Bearer access-2"));

        // Turned away again with the new one: that's the sign-in, not something to keep retrying.
        let api = self::api(vec![testing::json(401, json!({})), testing::json(401, json!({}))]).await;
        rig.brain.at.api = format!("{}/v1", api.url);
        assert!(turn(&mut rig.brain, "hello").await.1.unwrap_err().contains("didn't accept this sign-in"));
        assert_eq!(api.asked_at("/v1/responses").len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_accounts_models_are_the_ones_meant_to_be_shown() {
        let api = api(vec![]).await;
        let at = Endpoints { api: format!("{}/v1", api.url), ..Endpoints::default() };
        let got = models(&signed_in("models"), &at).await.unwrap();
        assert_eq!(got, [Model { slug: "gpt-test".into(), name: "Test".into() }]);
        assert_eq!(api.asked_at("/v1/models")[0].headers["authorization"], "Bearer access-1");
        // Not signed in: said, and nothing is asked.
        assert!(models(&crate::chat::account::tests::vault("models-none"), &at).await.unwrap_err().contains("Sign in with ChatGPT first"));
        assert_eq!(api.asked_at("/v1/models").len(), 1);
    }

    #[test]
    fn the_stream_is_cut_into_events_wherever_its_pieces_end() {
        let whole = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"héllo\"}\n\n: a comment\n\nevent: x\r\ndata: {\"type\":\"x\"}\r\n\r\ndata: not json\n\ndata: {\"type\":\"unfinished\"";
        let bytes = whole.as_bytes();
        // In one piece, and a byte at a time (which splits the é).
        for size in [bytes.len(), 1, 7] {
            let mut events = Events::default();
            let got: Vec<Value> = bytes.chunks(size).flat_map(|piece| events.push(piece)).collect();
            assert_eq!(got, [json!({"type": "response.output_text.delta", "delta": "héllo"}), json!({"type": "x"})], "pieces of {size}");
        }
    }

    #[test]
    fn a_call_left_without_its_answer_gets_one() {
        let mut said = vec![
            json!({"role": "user", "content": "do two things"}),
            json!({"type": "function_call", "call_id": "a", "name": "status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "a", "output": "ok"}),
            json!({"type": "function_call", "call_id": "b", "name": "session_stop", "arguments": "{}"}),
        ];
        mend(&mut said);
        assert_eq!(said.len(), 5);
        assert_eq!((said[4]["type"].as_str(), said[4]["call_id"].as_str()), (Some("function_call_output"), Some("b")));
        // Nothing open: nothing added.
        mend(&mut said);
        assert_eq!(said.len(), 5);
    }

    #[test]
    fn arguments_filled_in_with_nothing_are_left_out() {
        // As GPT sent it: every argument given, the unused ones empty.
        let sent = json!({"name": "Mem", "command": "free -m", "folder": "", "replacing": "", "note": null});
        assert_eq!(tidy("snippet_save", sent), json!({"name": "Mem", "command": "free -m"}));
        // Something the tool requires stays, even empty: the tool says what's wrong with it.
        assert_eq!(tidy("get_machine", json!({"machine_id": ""})), json!({"machine_id": ""}));
        assert_eq!(tidy("snippet_save", json!({"name": "", "command": ""})), json!({"command": ""}));
        // Values that mean something are never touched: false, 0, an empty list.
        let kept = json!({"machine_id": "forge", "wait": false, "count": 0, "labels": []});
        assert_eq!(tidy("install_runners", kept.clone()), kept);
        assert_eq!(tidy("status", json!({})), json!({}));
        assert_eq!(tidy("not_a_tool", json!({"a": ""})), json!({}));
    }

    #[test]
    fn a_very_long_result_is_cut_for_the_model() {
        assert_eq!(for_model("short".into()), "short");
        let cut = for_model("é".repeat(MOST_OUTPUT));
        assert!(cut.len() < MOST_OUTPUT + 80 && cut.contains("more bytes not shown"));
    }

    #[test]
    fn refusals_are_told_apart() {
        assert!(matches!(refusal(401, "{}"), Refusal::SignIn(_)));
        assert!(matches!(refusal(503, r#"{"detail":"direct routing unavailable"}"#), Refusal::Later(_)));
        assert!(matches!(refusal(503, r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#), Refusal::Later(_)));
        assert!(matches!(refusal(401, r#"{"error":{"code":"subscription_sharing_invalid_user"}}"#), Refusal::SignIn(_)));
        let unsupported = refusal(400, r#"{"error":{"code":"subscription_sharing_unsupported_capability","param":"tools[0]"}}"#);
        assert_eq!(unsupported, Refusal::No("ChatGPT plan use doesn't allow something in Muxin's request (tools[0]).".into()));
        assert_eq!(refusal(403, r#"{"error":{"code":"chatpass_v2_scope_not_authorized","message":"scope"}}"#), Refusal::No("ChatGPT refused the request: scope (chatpass_v2_scope_not_authorized)".into()));
    }
}
