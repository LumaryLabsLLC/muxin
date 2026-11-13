//! The tools the chat's brain is given: Muxin's own (crate::mcp), but not
//! all hundred of them in full with every request.
//!
//! A model is sent its tools each time it's asked something, so every tool's
//! description and arguments are paid for on every step of every turn. Most
//! turns use a handful. So the brain gets:
//!
//! - the tools a chat uses day to day (`CORE`), in full;
//! - `use_tool`, which calls any of the others by name. Its description
//!   lists them, a line each: name, arguments, what it does;
//! - `tool_details`, for a listed tool's full description and arguments,
//!   when the line isn't enough to call it right.
//!
//! Nothing is out of reach, and nothing gets past the gate this way: a call
//! through `use_tool` is asked about as the tool it really is.
//!
//! The programs get these through the bridge (`Short`), and Muxin's own
//! brain is handed them directly. One kind of program gets every tool in
//! full instead (`Whole`): one that doesn't send its tools to the model at
//! all, but looks them up as it needs them. There the short list would only
//! hide tools from what it searches. Its calls pass the gate the same.

use std::sync::Arc;

use serde_json::{Value, json};

use super::gate::Gate;
use crate::fleet::Engine;
use crate::mcp::{self, Session};

/// Given in full: looking at what's there, agent sessions, boxes, runners
/// and what GitHub wants. What "what needs me?" and "start one on that"
/// come down to.
pub const CORE: &[&str] = &[
    "status",
    "get_machine",
    "list_runners",
    "list_jobs",
    "runner_action",
    "get_operation",
    "session_list",
    "session_read",
    "session_question",
    "session_wait",
    "session_start",
    "session_send",
    "session_answer",
    "session_stop",
    "box_list",
    "box_create",
    "box_send",
    "github_inbox",
    "github_repo",
    "github_item",
    "local_checkouts",
];

/// Calls a tool that isn't given in full.
pub const USE: &str = "use_tool";
/// Describes tools that aren't given in full.
pub const DETAILS: &str = "tool_details";

/// The tools a chat's brain is offered, as MCP describes tools.
pub fn offered() -> Vec<Value> {
    let all = mcp::tools();
    let (core, rest): (Vec<Value>, Vec<Value>) = all.into_iter().partition(|t| t["name"].as_str().is_some_and(|n| CORE.contains(&n)));
    let lines: Vec<String> = rest.iter().map(line).collect();
    let mut offered = core;
    offered.push(json!({
        "name": USE,
        "description": format!(
            "Calls one of Muxin's other tools by its name. Each line below is one: its name, its arguments (one ending in ? can be left out), and what it does. \
When a line isn't enough to call it right, {DETAILS} gives its full description and what each argument takes.\n\n{}",
            lines.join("\n")
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The tool's name, from the list." },
                "arguments": { "type": "object", "description": "Its arguments, by name.", "additionalProperties": true }
            },
            "required": ["name"],
            "additionalProperties": false
        },
        // What it does depends on what it calls; Muxin asks the user by that.
        "annotations": { "title": "Use a tool", "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true },
    }));
    offered.push(json!({
        "name": DETAILS,
        "description": format!("The full description of tools listed under {USE}, and what each of their arguments takes."),
        "inputSchema": {
            "type": "object",
            "properties": { "names": { "type": "array", "items": { "type": "string" }, "description": "The tools' names." } },
            "required": ["names"],
            "additionalProperties": false
        },
        "annotations": { "title": "Tool details", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
    }));
    offered
}

/// A tool on one line: `name(arg, arg?): what it does`.
fn line(tool: &Value) -> String {
    let name = tool["name"].as_str().unwrap_or_default();
    let required: Vec<&str> = tool["inputSchema"]["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let optional = tool["inputSchema"]["properties"].as_object().into_iter().flat_map(|p| p.keys()).filter(|k| !required.contains(&k.as_str())).map(|k| format!("{k}?"));
    let arguments: Vec<String> = required.iter().map(|r| r.to_string()).chain(optional).collect();
    format!("{name}({}): {}", arguments.join(", "), summary(tool["description"].as_str().unwrap_or_default()))
}

/// A description's first sentence, kept short.
fn summary(description: &str) -> String {
    const MOST: usize = 110;
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    // A sentence ends at a full stop before a space; "e.g." and "(Linux)." inside one don't end it early enough to matter.
    let first = flat.split_once(". ").map(|(first, _)| first).unwrap_or(&flat).trim_end_matches('.');
    if first.chars().count() <= MOST {
        return first.to_string();
    }
    let cut: String = first.chars().take(MOST).collect();
    let cut = cut.rsplit_once(' ').map(|(before, _)| before).unwrap_or(&cut);
    format!("{}…", cut.trim_end_matches([',', ';', ':']))
}

fn known(name: &str) -> Option<Value> {
    mcp::tools().into_iter().find(|t| t["name"] == name)
}

/// A call's arguments without the ones the model filled in with nothing.
/// Some models give every argument a value, and write "" or null for the
/// ones they don't mean to use; a tool would take that as said (`replacing:
/// ""` as a snippet named nothing). An argument the tool requires is kept
/// as it came, for the tool to refuse in its own words.
fn tidy(tool: &Value, mut args: Value) -> Value {
    let required: Vec<&str> = tool["inputSchema"]["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if let Some(map) = args.as_object_mut() {
        map.retain(|key, value| required.contains(&key.as_str()) || !(value.is_null() || value.as_str().is_some_and(|s| s.is_empty())));
    }
    args
}

/// The tool a call is really of, and its arguments: `use_tool` unwrapped,
/// and arguments filled in with nothing left out. `Err` is what to tell the
/// model when there's no such tool.
pub fn real(name: &str, args: Value) -> Result<(String, Value), String> {
    let (name, args) = if name == USE {
        let inner = args["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| format!("{USE} needs the name of the tool to call."))?;
        if inner == USE || inner == DETAILS {
            return Err(format!("{inner} isn't called through {USE}: call it directly."));
        }
        let arguments = match &args["arguments"] {
            Value::Null => json!({}),
            // Some models write the arguments as text.
            Value::String(text) if text.trim().is_empty() => json!({}),
            Value::String(text) => serde_json::from_str(text).map_err(|_| "The arguments weren't readable JSON.".to_string())?,
            other => other.clone(),
        };
        (inner.to_string(), arguments)
    } else {
        (name.to_string(), args)
    };
    let tool = known(&name).ok_or_else(|| format!("There's no tool named {name}. The names are in {USE}'s description."))?;
    Ok((name, tidy(&tool, args)))
}

/// A call as it's shown and kept: the tool it's really of. (A call that
/// can't be made sense of is shown as it came.)
pub fn shown(name: &str, args: Value) -> (String, Value) {
    if name == DETAILS {
        return (name.to_string(), args);
    }
    real(name, args.clone()).unwrap_or((name.to_string(), args))
}

fn failed(code: &str, message: String) -> Value {
    close_up(mcp::tool_result(json!({ "error": { "code": code, "message": message } }), None, true))
}

/// The full description of the tools named.
fn details(args: &Value) -> Value {
    let names: Vec<&str> = match &args["names"] {
        Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
        Value::String(one) => vec![one.as_str()],
        _ => Vec::new(),
    };
    if names.is_empty() {
        return failed("invalid", format!("{DETAILS} needs the names of the tools to describe."));
    }
    let (found, missing): (Vec<(&str, Option<Value>)>, Vec<(&str, Option<Value>)>) = names.iter().map(|n| (*n, known(n))).partition(|(_, tool)| tool.is_some());
    let tools: Vec<Value> = found.into_iter().filter_map(|(_, tool)| tool).map(|t| json!({ "name": t["name"], "description": t["description"], "arguments": t["inputSchema"] })).collect();
    let mut out = json!({ "tools": tools });
    if !missing.is_empty() {
        out["no_such_tool"] = json!(missing.iter().map(|(name, _)| *name).collect::<Vec<_>>());
    }
    close_up(mcp::tool_result(out, None, false))
}

/// Runs a call from the chat's brain: describes tools, or runs the tool it's
/// really of once the gate lets it through. The outcome as an MCP tool result.
pub async fn call(engine: &Arc<Engine>, session: &Session, gate: &Gate, name: &str, args: Value) -> Value {
    if name == DETAILS {
        return details(&args);
    }
    let (name, args) = match real(name, args) {
        Ok(call) => call,
        Err(why) => return failed("not_found", why),
    };
    if let Err(why) = gate.check(&name, &args).await {
        return failed("declined", why);
    }
    close_up(mcp::call_tool(engine, session, &name, args).await)
}

/// A result with its JSON written without the line breaks and indenting
/// that are there for people: a model reads it the same, and every space is
/// paid for. Text that isn't JSON (a command's output, a file) stays as it is.
fn close_up(mut result: Value) -> Value {
    for block in result["content"].as_array_mut().into_iter().flatten() {
        if block["type"] == "text"
            && let Some(text) = block["text"].as_str()
            && text.trim_start().starts_with(['{', '['])
            && let Ok(json) = serde_json::from_str::<Value>(text)
        {
            block["text"] = Value::String(json.to_string());
        }
    }
    result
}

/// What the bridge serves a brain that's another program: the short list,
/// and every call past the chat's gate.
pub struct Short {
    gate: Arc<Gate>,
}

impl Short {
    pub fn new(gate: Arc<Gate>) -> Arc<Short> {
        Arc::new(Short { gate })
    }
}

#[async_trait::async_trait]
impl mcp::Front for Short {
    fn tools(&self) -> Vec<Value> {
        offered()
    }

    async fn call(&self, engine: &Arc<Engine>, session: &Session, name: &str, args: Value) -> Value {
        call(engine, session, &self.gate, name, args).await
    }
}

/// What the bridge serves a program that finds tools by itself: every tool
/// in full, and every call past the chat's gate.
pub struct Whole {
    gate: Arc<Gate>,
}

impl Whole {
    pub fn new(gate: Arc<Gate>) -> Arc<Whole> {
        Arc::new(Whole { gate })
    }
}

#[async_trait::async_trait]
impl mcp::Front for Whole {
    fn tools(&self) -> Vec<Value> {
        mcp::tools()
    }

    async fn call(&self, engine: &Arc<Engine>, session: &Session, name: &str, args: Value) -> Value {
        call(engine, session, &self.gate, name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::gate::{self, Answer, Mode};
    use crate::config::Paths;

    fn size(tools: &[Value]) -> usize {
        tools.iter().map(|t| json!({ "name": t["name"], "description": t["description"], "parameters": t["inputSchema"] }).to_string().len()).sum()
    }

    #[test]
    fn the_brain_gets_the_everyday_tools_and_a_line_for_each_of_the_rest() {
        let all = mcp::tools();
        let offered = offered();
        assert_eq!(offered.len(), CORE.len() + 2);
        // Every core tool is a real one, given as it is.
        for name in CORE {
            let full = all.iter().find(|t| t["name"] == *name).unwrap_or_else(|| panic!("{name} isn't a tool"));
            assert_eq!(offered.iter().find(|t| t["name"] == *name), Some(full));
        }
        // Every other tool has exactly one line, with its arguments.
        let list = offered.iter().find(|t| t["name"] == USE).unwrap()["description"].as_str().unwrap().to_string();
        for tool in &all {
            let name = tool["name"].as_str().unwrap();
            let lines = list.lines().filter(|l| l.starts_with(&format!("{name}("))).count();
            assert_eq!(lines, if CORE.contains(&name) { 0 } else { 1 }, "{name}");
        }
        assert!(list.contains("\nvm_create(name, system, "), "{}", list.lines().find(|l| l.starts_with("vm_create")).unwrap_or(""));
        assert!(list.contains("\nget_settings(): "));
        // And it's well under half of what all of them in full come to.
        let (short, whole) = (size(&offered), size(&all));
        assert!(short * 100 < whole * 45, "{short} of {whole}");
        // Each is a tool as MCP describes one.
        for tool in &offered {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(tool["annotations"]["readOnlyHint"].is_boolean() && tool["description"].as_str().is_some_and(|d| d.len() > 40));
        }
    }

    #[test]
    fn a_line_is_a_tools_name_arguments_and_first_sentence() {
        let tool = json!({"name": "copy_files", "description": "Copies files and folders from one machine to another, through Muxin. Files of the same name are replaced.", "inputSchema": {"properties": {"from": {}, "to": {}, "paths": {}, "wait": {}}, "required": ["from", "paths"]}});
        assert_eq!(line(&tool), "copy_files(from, paths, to?, wait?): Copies files and folders from one machine to another, through Muxin");
        assert_eq!(summary("One sentence only."), "One sentence only");
        assert_eq!(summary("  Spread\n over   lines. And more."), "Spread over lines");
        let long = summary(&format!("{} end. More.", "word ".repeat(40)));
        assert!(long.chars().count() <= 111 && long.ends_with('…') && !long.contains("end"), "{long}");
    }

    #[test]
    fn a_call_through_use_tool_is_the_tool_it_names() {
        let restart = json!({"runner_id": "forge/forge-03", "action": "restart"});
        assert_eq!(real(USE, json!({"name": "runner_action", "arguments": restart})).unwrap(), ("runner_action".to_string(), restart.clone()));
        // Called directly, core or not: the same.
        assert_eq!(real("runner_action", restart.clone()).unwrap().0, "runner_action");
        assert_eq!(real("vm_options", json!({})).unwrap(), ("vm_options".to_string(), json!({})));
        // Arguments written as text, or left out.
        assert_eq!(real(USE, json!({"name": "vm_create", "arguments": "{\"name\": \"w\", \"system\": \"windows-11\"}"})).unwrap().1, json!({"name": "w", "system": "windows-11"}));
        assert_eq!(real(USE, json!({"name": "get_settings"})).unwrap(), ("get_settings".to_string(), json!({})));
        assert_eq!(real(USE, json!({"name": " get_settings ", "arguments": ""})).unwrap().0, "get_settings");
        // What can't be called says why.
        assert!(real(USE, json!({"arguments": {}})).unwrap_err().contains("needs the name"));
        assert!(real(USE, json!({"name": "format_disk"})).unwrap_err().contains("no tool named format_disk"));
        assert!(real(USE, json!({"name": USE, "arguments": {"name": "status"}})).unwrap_err().contains("call it directly"));
        assert!(real(USE, json!({"name": "vm_create", "arguments": "{not json"})).unwrap_err().contains("readable JSON"));
        // Shown as what it is; and as it came when it makes no sense.
        assert_eq!(shown(USE, json!({"name": "cloud_destroy", "arguments": {"machine_id": "dev-1"}})), ("cloud_destroy".to_string(), json!({"machine_id": "dev-1"})));
        assert_eq!(shown(USE, json!({"name": "nope"})), (USE.to_string(), json!({"name": "nope"})));
        assert_eq!(shown(DETAILS, json!({"names": ["vm_create"]})).0, DETAILS);
    }

    #[test]
    fn arguments_filled_in_with_nothing_are_left_out() {
        // As GPT sent it: every argument given, the unused ones empty.
        let sent = json!({"name": "Mem", "command": "free -m", "folder": "", "replacing": "", "note": null});
        assert_eq!(real("snippet_save", sent).unwrap().1, json!({"name": "Mem", "command": "free -m"}));
        // Something the tool requires stays, even empty: the tool says what's wrong with it.
        assert_eq!(real("get_machine", json!({"machine_id": ""})).unwrap().1, json!({"machine_id": ""}));
        assert_eq!(real("snippet_save", json!({"name": "", "command": ""})).unwrap().1, json!({"command": ""}));
        // Values that mean something are never touched: false, 0, an empty list.
        let kept = json!({"machine_id": "forge", "wait": false, "count": 0, "labels": []});
        assert_eq!(real(USE, json!({"name": "install_runners", "arguments": kept.clone()})).unwrap().1, kept);
    }

    fn demo(name: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!("muxin-chat-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Engine::new(Paths::resolve(Some(dir), None), true)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_goes_to_the_model_without_its_indenting() {
        let (gate, _asks) = Gate::new(Mode::Ask, vec![]);
        let engine = demo("close");
        // As any other client gets it, and as the chat's brain does: the same thing, smaller.
        let plain = mcp::call_tool(&engine, &Session::named("chat"), "status", json!({})).await;
        let close = call(&engine, &Session::named("chat"), &gate, "status", json!({})).await;
        let (plain_text, close_text) = (plain["content"][0]["text"].as_str().unwrap(), close["content"][0]["text"].as_str().unwrap());
        assert!(plain_text.contains("\n  ") && !close_text.contains('\n'));
        assert!(close_text.len() * 10 < plain_text.len() * 8, "{} of {}", close_text.len(), plain_text.len());
        assert_eq!(serde_json::from_str::<Value>(close_text).unwrap()["counts"], serde_json::from_str::<Value>(plain_text).unwrap()["counts"]);
        assert_eq!(close["structuredContent"]["counts"], plain["structuredContent"]["counts"]);
        // Text that isn't JSON is left alone, line breaks and all.
        let text = json!({"content": [{"type": "text", "text": "exit code: 0\n{not json\n  indented"}, {"type": "image", "data": "x"}]});
        assert_eq!(close_up(text.clone()), text);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_program_that_finds_tools_itself_gets_them_all_and_is_gated_the_same() {
        use mcp::Front;
        let engine = demo("whole");
        let (gate, mut asks) = Gate::new(Mode::Ask, vec![]);
        let whole = Whole::new(gate.clone());
        assert_eq!(whole.tools(), mcp::tools());
        assert!(whole.tools().iter().any(|t| t["name"] == "snippet_save") && !whole.tools().iter().any(|t| t["name"] == USE));
        // A change waits for the user, whichever list the tool was found in.
        let calling = tokio::spawn(async move { whole.call(&engine, &Session::named("chat"), "snippet_save", json!({"name": "Uptime", "command": "uptime"})).await });
        let Some(gate::Event::Asked(request)) = asks.recv().await else { panic!("asked") };
        assert_eq!(request.tool, "snippet_save");
        gate.answer(request.id, Answer::No(String::new()));
        assert_eq!(calling.await.unwrap()["structuredContent"]["error"]["code"], "declined");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn details_are_given_in_full_and_ask_nobody() {
        let (gate, mut asks) = Gate::new(Mode::Ask, vec![]);
        let out = call(&demo("details"), &Session::named("chat"), &gate, DETAILS, json!({"names": ["vm_create", "nope"]})).await;
        assert_eq!(out["isError"], false);
        let tools = out["structuredContent"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0]["description"].as_str().unwrap().len() > 200 && tools[0]["arguments"]["properties"]["system"].is_object());
        assert_eq!(out["structuredContent"]["no_such_tool"], json!(["nope"]));
        assert!(asks.try_recv().is_err());
        assert_eq!(call(&demo("details2"), &Session::named("chat"), &gate, DETAILS, json!({})).await["isError"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_through_use_tool_is_asked_about_as_what_it_is() {
        let engine = demo("gate");
        let (gate, mut asks) = Gate::new(Mode::Ask, vec![]);
        // A read through it runs, asking nobody.
        let listed = call(&engine, &Session::named("chat"), &gate, USE, json!({"name": "snippet_list"})).await;
        assert_eq!(listed["isError"], false);
        assert!(asks.try_recv().is_err());

        // A change through it waits, and the question names the real tool and its arguments.
        let calling = {
            let (engine, gate) = (engine.clone(), gate.clone());
            tokio::spawn(async move { call(&engine, &Session::named("chat"), &gate, USE, json!({"name": "snippet_save", "arguments": {"name": "Uptime", "command": "uptime", "folder": ""}})).await })
        };
        let Some(gate::Event::Asked(request)) = asks.recv().await else { panic!("asked") };
        assert_eq!((request.tool.as_str(), request.rule.as_deref()), ("snippet_save", Some("snippet_save")));
        assert_eq!(request.args, json!({"name": "Uptime", "command": "uptime"}));
        gate.answer(request.id, Answer::No("not yet".into()));
        let refused = calling.await.unwrap();
        assert_eq!((refused["isError"].as_bool(), refused["structuredContent"]["error"]["code"].as_str()), (Some(true), Some("declined")));

        // Something that isn't a tool never reaches the gate.
        let none = call(&engine, &Session::named("chat"), &gate, USE, json!({"name": "format_disk"})).await;
        assert_eq!(none["structuredContent"]["error"]["code"], "not_found");
        assert!(matches!(asks.try_recv(), Ok(gate::Event::Answered { .. })));
        assert!(asks.try_recv().is_err());
    }
}
