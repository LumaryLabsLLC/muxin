//! How much a tool call can change. The chat's gate (gate.rs) decides from
//! this whether to ask the user first:
//!
//! - `Read`: looks, changes nothing. Runs.
//! - `Change`: changes something that can be put back (starts a session,
//!   restarts a runner, edits config.toml). Asks, and the user may say
//!   "don't ask again" for that kind of call.
//! - `Danger`: deletes something, stops work that's running, costs money, or
//!   runs whatever text it's given (a shell command, clicks on a real screen).
//!   Asks every time.
//!
//! The tools' MCP annotations are hints for other clients, and too coarse for
//! this: `runner_action` is one tool whether it reads a log or removes a
//! runner. So every tool is listed here, by name and, where it matters, by
//! its arguments. A test fails when a tool has no line.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Read,
    Change,
    Danger,
}

/// A call's level, or `None` for a tool this table doesn't know.
pub fn known(tool: &str, args: &Value) -> Option<Risk> {
    use Risk::*;
    let arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    let filled = |k: &str| args.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    Some(match tool {
        // Looking.
        "status" | "fleet_status" | "get_machine" | "list_runners" | "list_jobs" | "check_machines" | "get_operation" | "list_operations" | "get_settings" | "list_ssh_hosts"
        | "machine_connection" | "machine_ports" | "machine_services" | "machine_processes" | "compose_project" | "log_sources" | "logs_read" | "agent_usage" => Read,
        "cloud_options" | "cloud_usage" | "vm_options" | "schedule_list" | "schedule_log" | "snippet_list" | "list_files" | "recipe_check" => Read,
        "workspace_list" | "workspace_get" | "workspace_read_file" | "workspace_list_files" | "workspace_changes" => Read,
        "session_list" | "session_read" | "session_wait" | "session_question" | "box_list" => Read,
        "github_inbox" | "github_repos" | "github_accounts" | "github_repo" | "github_item" | "local_checkouts" | "git_status" => Read,
        "desktop_screenshot" | "desktop_displays" | "desktop_windows" => Read,

        // One tool, several things: the argument says which.
        "runner_action" => match arg("action") {
            "logs" => Read,
            "start" | "stop" | "restart" | "update" => Change,
            // "remove", and anything this doesn't know.
            _ => Danger,
        },
        "service_action" => match arg("action") {
            "start" | "restart" | "enable" => Change,
            // Stopping the wrong service (ssh, the network) cuts the machine off.
            _ => Danger,
        },
        "compose_action" => match arg("action") {
            "up" | "restart" | "pull" | "start" | "stop" => Change,
            // "down" removes the project's containers.
            _ => Danger,
        },
        "machine_power" => match arg("power") {
            "on" => Change,
            // Ends the jobs and sessions running there.
            _ => Danger,
        },
        "workspace_sync" => {
            if args.get("delete").and_then(Value::as_bool) == Some(true) {
                Danger
            } else {
                Change
            }
        }
        "github_scopes" => {
            if filled("add") || filled("remove") {
                Change
            } else {
                Read
            }
        }

        // Changes that can be put back.
        "add_machine" | "rename_machine" | "ignore_warning" | "connect_machine" | "direct_connection" | "wake_machine" | "update_settings" => Change,
        "install_runners" | "cloud_power" | "vm_create" | "docker_run" | "docker_pull" | "recipe_apply" => Change,
        "schedule_save" | "schedule_run" | "snippet_save" | "cancel_operation" => Change,
        "workspace_create" | "workspace_write_file" | "workspace_allow" | "workspace_exec" => Change,
        "session_start" | "session_send" | "session_action" | "session_answer" | "session_rename" | "session_handoff" => Change,
        "box_create" | "box_send" | "box_pause" => Change,
        "github_use_account" | "github_favorite" | "git_setup" | "git_sync" => Change,
        "desktop_add_display" | "desktop_set_display" => Change,

        // Costs money every hour it exists.
        "cloud_create" => Danger,
        // Deletes, or ends work that's running.
        "remove_machine" | "cloud_destroy" | "vm_destroy" | "box_destroy" | "workspace_destroy" | "schedule_delete" | "snippet_delete" | "desktop_remove_display" => Danger,
        "session_stop" | "session_restart" | "cancel_run" | "process_signal" => Danger,
        // Replaces files of the same name where it copies to.
        "copy_files" => Danger,
        // Runs what it's given on real machines, or on a real screen.
        "run_on_machines" | "snippet_run" | "desktop_input" => Danger,

        _ => return None,
    })
}

/// A call's level. A tool nobody listed asks every time.
pub fn of(tool: &str, args: &Value) -> Risk {
    known(tool, args).unwrap_or(Risk::Danger)
}

/// What "Yes, and don't ask again" remembers for a call: the tool, or
/// `tool:action` for a tool that does several things. `None` when the answer
/// isn't offered: a `Danger` call, and the tools that take free text
/// (`workspace_exec` runs any command, `session_send` types anything), where
/// one yes would stand for every command after it.
pub fn rule(tool: &str, args: &Value) -> Option<String> {
    if of(tool, args) != Risk::Change || matches!(tool, "workspace_exec" | "session_send") {
        return None;
    }
    let part = match tool {
        "runner_action" | "service_action" | "compose_action" | "session_action" | "git_sync" => args.get("action"),
        "machine_power" | "cloud_power" => args.get("power"),
        "workspace_sync" => args.get("direction"),
        _ => None,
    };
    Some(match part.and_then(Value::as_str).filter(|p| !p.is_empty()) {
        Some(p) => format!("{tool}:{p}"),
        None => tool.to_string(),
    })
}

/// Whether a saved rule covers a call: the same `tool:action`, or the bare
/// tool (every action of it that's a `Change`).
pub fn covered(rules: &[String], tool: &str, args: &Value) -> bool {
    let Some(key) = rule(tool, args) else { return false };
    rules.iter().any(|r| *r == key || r == tool)
}

/// A call in a few lines, for the card that asks about it: the tool, then
/// each argument on its own line (long ones cut).
pub fn describe(tool: &str, args: &Value) -> Vec<String> {
    let mut lines = vec![tool.to_string()];
    if let Some(map) = args.as_object() {
        for (k, v) in map {
            let text = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let text = text.trim();
            if text.is_empty() || text == "null" {
                continue;
            }
            let mut shown: String = text.chars().take(400).collect();
            if shown.len() < text.len() {
                shown.push('…');
            }
            lines.push(format!("  {k}: {}", shown.replace('\n', "\n    ")));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_tool_has_a_level() {
        for t in crate::mcp::tools() {
            let name = t["name"].as_str().unwrap();
            assert!(known(name, &json!({})).is_some(), "{name} has no risk level in chat/risk.rs");
        }
    }

    #[test]
    fn nothing_marked_read_only_asks_and_nothing_else_runs_freely() {
        // The annotations and this table must agree on what only looks, with
        // the tools that read or change by their arguments as the exceptions.
        let by_argument = ["runner_action", "github_scopes"];
        for t in crate::mcp::tools() {
            let name = t["name"].as_str().unwrap();
            if by_argument.contains(&name) {
                continue;
            }
            let read_only = t["annotations"]["readOnlyHint"] == true;
            assert_eq!(known(name, &json!({})) == Some(Risk::Read), read_only, "{name}");
        }
    }

    #[test]
    fn arguments_decide_for_tools_that_do_several_things() {
        assert_eq!(of("runner_action", &json!({"action": "logs"})), Risk::Read);
        assert_eq!(of("runner_action", &json!({"action": "restart"})), Risk::Change);
        assert_eq!(of("runner_action", &json!({"action": "remove"})), Risk::Danger);
        assert_eq!(of("service_action", &json!({"action": "restart"})), Risk::Change);
        assert_eq!(of("service_action", &json!({"action": "stop"})), Risk::Danger);
        assert_eq!(of("compose_action", &json!({"action": "down"})), Risk::Danger);
        assert_eq!(of("machine_power", &json!({"power": "on"})), Risk::Change);
        assert_eq!(of("machine_power", &json!({"power": "off"})), Risk::Danger);
        assert_eq!(of("workspace_sync", &json!({"direction": "pull"})), Risk::Change);
        assert_eq!(of("workspace_sync", &json!({"direction": "pull", "delete": true})), Risk::Danger);
        assert_eq!(of("github_scopes", &json!({})), Risk::Read);
        assert_eq!(of("github_scopes", &json!({"add": ["acme"]})), Risk::Change);
        // A missing or made-up action is never the mild case.
        assert_eq!(of("service_action", &json!({})), Risk::Danger);
        assert_eq!(of("runner_action", &json!({})), Risk::Danger);
        assert_eq!(of("compose_action", &json!({"action": "nuke"})), Risk::Danger);
        assert_eq!(of("machine_power", &json!({"power": "explode"})), Risk::Danger);
        assert_eq!(of("a_tool_from_the_future", &json!({})), Risk::Danger);
    }

    #[test]
    fn rules_name_the_action_and_are_never_offered_for_danger_or_free_text() {
        assert_eq!(rule("runner_action", &json!({"action": "restart"})).as_deref(), Some("runner_action:restart"));
        assert_eq!(rule("session_start", &json!({"agent": "codex"})).as_deref(), Some("session_start"));
        assert_eq!(rule("machine_power", &json!({"power": "on"})).as_deref(), Some("machine_power:on"));
        assert_eq!(rule("runner_action", &json!({"action": "remove"})), None);
        assert_eq!(rule("cloud_destroy", &json!({})), None);
        assert_eq!(rule("status", &json!({})), None);
        assert_eq!(rule("workspace_exec", &json!({"command": "ls"})), None);
        assert_eq!(rule("session_send", &json!({"text": "y"})), None);
    }

    #[test]
    fn a_rule_covers_its_action_and_a_bare_tool_covers_its_changes_only() {
        let rules = vec!["runner_action:restart".to_string(), "service_action".to_string()];
        assert!(covered(&rules, "runner_action", &json!({"action": "restart"})));
        assert!(!covered(&rules, "runner_action", &json!({"action": "stop"})));
        assert!(covered(&rules, "service_action", &json!({"action": "restart"})));
        // "service_action" was saved, but stopping one still asks.
        assert!(!covered(&rules, "service_action", &json!({"action": "stop"})));
        assert!(!covered(&rules, "cloud_destroy", &json!({})));
    }

    #[test]
    fn a_card_shows_the_tool_and_its_arguments() {
        let lines = describe("workspace_exec", &json!({"workspace_id": "ws-1", "command": "make test\nmake lint", "background": false, "note": ""}));
        assert_eq!(lines[0], "workspace_exec");
        assert!(lines.contains(&"  workspace_id: ws-1".to_string()));
        assert!(lines.iter().any(|l| l == "  command: make test\n    make lint"));
        assert!(!lines.iter().any(|l| l.contains("note")));
    }
}
