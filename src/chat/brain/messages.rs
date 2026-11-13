//! The lines Claude Code prints when it runs without its terminal
//! (`--output-format stream-json`), which Grok Build prints too
//! (`--output-format streaming-messages-json`): one JSON object a line, with
//! Anthropic's Messages shapes inside. Read here once, for both.

use serde_json::Value;

use super::{Event, result_text};

/// What one line means: what to report, and whether the turn is over (and
/// how). `who` is the program, for an error's words. `tool` gives a tool the
/// model called the name and arguments Muxin knows it by.
pub(crate) fn read(line: &Value, who: &str, tool: &dyn Fn(&str, &Value) -> (String, Value)) -> (Vec<Event>, Option<Result<(), String>>) {
    let mut events = Vec::new();
    let blocks = || line["message"]["content"].as_array().cloned().unwrap_or_default();
    match line["type"].as_str().unwrap_or("") {
        "system" if line["subtype"] == "init" => {
            if let Some(id) = line["session_id"].as_str() {
                events.push(Event::Session(id.to_string()));
            }
            // "unknown" is what Grok writes when it doesn't know yet.
            if let Some(model) = line["model"].as_str().filter(|m| !m.is_empty() && *m != "unknown") {
                events.push(Event::Model(model.to_string()));
            }
        }
        "stream_event" => {
            let delta = &line["event"]["delta"];
            if line["event"]["type"] == "content_block_delta" && delta["type"] == "text_delta" {
                events.push(Event::Delta(delta["text"].as_str().unwrap_or_default().to_string()));
            }
        }
        "assistant" => {
            for b in blocks() {
                match b["type"].as_str() {
                    Some("text") => events.push(Event::Text(b["text"].as_str().unwrap_or_default().to_string())),
                    Some("tool_use") => {
                        let (name, args) = tool(b["name"].as_str().unwrap_or_default(), &b["input"]);
                        events.push(Event::Tool { id: b["id"].as_str().unwrap_or_default().to_string(), name, args });
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for b in blocks() {
                if b["type"] == "tool_result" {
                    events.push(Event::Result {
                        id: b["tool_use_id"].as_str().unwrap_or_default().to_string(),
                        text: result_text(&b["content"]),
                        is_error: b["is_error"].as_bool().unwrap_or(false),
                    });
                }
            }
        }
        "result" => {
            let u = &line["usage"];
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let input = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
            // A turn that never reached the model used nothing: nothing to say about it.
            if input + n("output_tokens") > 0 {
                events.push(Event::Usage {
                    input,
                    output: n("output_tokens"),
                    cached: n("cache_read_input_tokens"),
                    // Grok writes 0 for a cost it wasn't told: that's "unknown", not "free".
                    cost: line["total_cost_usd"].as_f64().filter(|cost| *cost > 0.0),
                });
            }
            let failed = line["is_error"].as_bool().unwrap_or(false) || line["subtype"].as_str().is_some_and(|s| s != "success");
            let end = if failed {
                let said = line["result"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_string);
                let said = said.or_else(|| line["errors"].as_array().and_then(|e| e.iter().filter_map(Value::as_str).next()).map(str::to_string));
                Err(said.unwrap_or_else(|| format!("{who} stopped: {}.", line["subtype"].as_str().unwrap_or("an error").replace('_', " "))))
            } else {
                Ok(())
            };
            return (events, Some(end));
        }
        // What's printed when a turn couldn't even begin.
        "error" => return (events, Some(Err(line["message"].as_str().filter(|m| !m.trim().is_empty()).map(str::to_string).unwrap_or_else(|| format!("{who} reported an error."))))),
        _ => {}
    }
    (events, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn as_is(name: &str, input: &Value) -> (String, Value) {
        (name.to_string(), input.clone())
    }

    #[test]
    fn a_cost_of_nothing_is_a_cost_not_known() {
        let usage = json!({"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 5});
        let (events, end) = read(&json!({"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0, "usage": usage}), "Grok Build", &as_is);
        assert_eq!(events, [Event::Usage { input: 100, output: 5, cached: 90, cost: None }]);
        assert_eq!(end, Some(Ok(())));
        let (events, _) = read(&json!({"type": "result", "subtype": "success", "total_cost_usd": 0.0127, "usage": usage}), "Grok Build", &as_is);
        assert_eq!(events, [Event::Usage { input: 100, output: 5, cached: 90, cost: Some(0.0127) }]);
    }

    #[test]
    fn a_turn_that_couldnt_finish_says_why_in_the_programs_words_or_ours() {
        let end = |line: Value| read(&line, "Grok Build", &as_is).1;
        assert_eq!(end(json!({"type": "error", "message": "Couldn't start session: not signed in"})), Some(Err("Couldn't start session: not signed in".into())));
        assert_eq!(end(json!({"type": "error"})), Some(Err("Grok Build reported an error.".into())));
        assert_eq!(end(json!({"type": "result", "subtype": "error_during_execution", "errors": ["The model is overloaded."]})), Some(Err("The model is overloaded.".into())));
        assert_eq!(end(json!({"type": "result", "subtype": "error_max_turns"})), Some(Err("Grok Build stopped: error max turns.".into())));
    }

    #[test]
    fn a_model_not_known_yet_isnt_named() {
        let (events, _) = read(&json!({"type": "system", "subtype": "init", "session_id": "s-1", "model": "unknown"}), "Grok Build", &as_is);
        assert_eq!(events, [Event::Session("s-1".into())]);
    }
}
