//! What a waiting agent is asking, read off its screen: a choice (Claude
//! Code's and Codex's "Do you want to proceed?" with numbered options, or a
//! question with options) or, when it's simply done and waiting, the end of
//! what it said. The app shows it with a button for each choice and a field
//! to reply, so an agent can be answered without opening its terminal.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// One of a prompt's choices: the key that picks it, and what it says. A
/// numbered option's key is its number ("2"); an option of a menu without
/// numbers is "#<its place>" ("#1"), picked with the arrow keys and Enter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Choice {
    pub key: String,
    pub label: String,
}

/// The keys that pick `key` in a prompt whose cursor is on `selected`:
/// its number, or arrows to it and Enter.
pub fn keys_for(key: &str, selected: Option<&str>, confirm: bool) -> (String, bool) {
    match (key.strip_prefix('#').and_then(|k| k.parse::<i64>().ok()), selected.and_then(|s| s.strip_prefix('#')).and_then(|s| s.parse::<i64>().ok())) {
        (Some(to), from) => {
            let from = from.unwrap_or(0);
            let arrow = if to >= from { "\x1b[B" } else { "\x1b[A" };
            (arrow.repeat((to - from).unsigned_abs() as usize), true)
        }
        (None, _) => (key.to_string(), confirm),
    }
}

/// The menu says a number needs Enter after it ("Press enter to continue",
/// Codex's newer "enter continue · esc back").
fn asks_for_enter(lines: &[&str]) -> bool {
    lines.iter().rev().take(6).any(|l| {
        let l = l.to_lowercase();
        ["press enter to", "enter to confirm", "enter to continue", "enter continue", "enter confirm"].iter().any(|p| l.contains(p))
    })
}

/// A line's indent and text, past a box's side.
fn lead(line: &str) -> (usize, &str) {
    let rest = line.trim_start_matches([' ']).trim_start_matches(['│', '┃', '║']);
    let skipped = line.len() - rest.len();
    let text = rest.trim_start_matches(' ');
    let spaces = rest.len() - text.len();
    // Count the box side as one column, the spaces as theirs.
    (line[..skipped].chars().count() + spaces, text.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║')))
}

/// A menu without numbers ("❯ Yes, I trust this folder" over its siblings):
/// its first and last lines, its choices and the one selected.
fn menu(lines: &[&str]) -> Option<(usize, usize, Vec<Choice>, String)> {
    let at = (0..lines.len()).rev().find(|&i| {
        let (_, t) = lead(lines[i]);
        (t.starts_with("❯ ") || t.starts_with("› ")) && option(lines[i]).is_none()
    })?;
    let (col, sel) = lead(lines[at]);
    let text_col = col + 2;
    let sibling = |i: usize| {
        let (c, t) = lead(lines[i]);
        c == text_col && !t.is_empty() && !is_rule(lines[i])
    };
    let mut first = at;
    while first > 0 && sibling(first - 1) {
        first -= 1;
    }
    let mut last = at;
    while last + 1 < lines.len() && sibling(last + 1) {
        last += 1;
    }
    if last == first {
        return None;
    }
    let choices: Vec<Choice> = (first..=last)
        .map(|i| {
            let label = if i == at { sel.trim_start_matches(['❯', '›']).trim() } else { lead(lines[i]).1 };
            Choice { key: format!("#{}", i - first), label: label.to_string() }
        })
        .collect();
    Some((first, last, choices, format!("#{}", at - first)))
}

/// The question above a prompt's choices: up to a gap, a border or 12 lines.
fn question_above(lines: &[&str]) -> Vec<String> {
    let mut text = Vec::new();
    let mut gaps = 0;
    for line in lines.iter().rev() {
        let t = clean(line);
        if is_rule(line) {
            if text.is_empty() {
                continue;
            }
            break;
        }
        if t.is_empty() {
            gaps += 1;
            if gaps >= 2 && !text.is_empty() {
                break;
            }
            if !text.is_empty() {
                text.push(String::new());
            }
            continue;
        }
        gaps = 0;
        // Your own message, where this turn began: not part of the question.
        if t.starts_with("› ") || t.starts_with("❯ ") || t.starts_with("> ") {
            break;
        }
        text.push(t);
        if text.len() >= 12 {
            break;
        }
    }
    text.reverse();
    while text.first().is_some_and(String::is_empty) {
        text.remove(0);
    }
    text
}

/// What a waiting session shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Question {
    /// The question (or the end of the agent's last message), a line each.
    pub text: Vec<String>,
    /// Its numbered choices; empty when it just waits for a message.
    pub options: Vec<Choice>,
    /// The choice the cursor is on (Enter picks it).
    pub selected: Option<String>,
    /// A number only moves to its choice; Enter takes it ("Press enter to
    /// continue", as Codex says). Claude Code takes a number at once.
    #[serde(skip)]
    pub confirm: bool,
    pub read_at: DateTime<Utc>,
}

#[cfg(test)]
impl Question {
    /// It's asking for a decision (not just waiting for the next message).
    fn is_choice(&self) -> bool {
        !self.options.is_empty()
    }
}

/// Box drawing and the cursor marks prompts draw with.
fn clean(line: &str) -> String {
    let trimmed = line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘'));
    trimmed.trim_end().to_string()
}

/// A line that's only a rule or a border.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let across = |c: char| matches!(c, '─' | '━' | '═' | '-' | '╌' | '┄' | '▔' | '▁');
    t.chars().any(across) && t.chars().all(|c| across(c) || matches!(c, '╭' | '╮' | '╰' | '╯' | '│' | '┃' | ' ' | '┌' | '┐' | '└' | '┘'))
}

/// An agent's status line, not something it said: "✻ Baked for 3s",
/// "⏵⏵ auto mode on", "esc to interrupt".
fn is_status(line: &str) -> bool {
    line.starts_with(['✻', '✽', '✶', '✳', '✢', '✺', '⏵', '·', '*']) || line.contains("esc to interrupt") || line.contains("for shortcuts")
}

/// "❯ 1. Yes" → (selected, "1", "Yes").
fn option(line: &str) -> Option<(bool, String, String)> {
    let t = clean(line);
    let (selected, rest) = match t.chars().next()? {
        '❯' | '›' | '>' | '▶' | '→' | '●' => (true, t[t.chars().next()?.len_utf8()..].trim_start()),
        _ => (false, t.as_str()),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = &rest[digits.len()..];
    let label = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")).or_else(|| after.strip_prefix(".")).map(str::trim)?;
    (!label.is_empty()).then(|| (selected, digits, label.to_string()))
}

/// Reads a waiting session's screen. `screen` is its text, bottom last.
pub fn read(screen: &str) -> Question {
    let lines: Vec<&str> = screen.lines().collect();
    // Only the bottom: what's being asked is at the end.
    let from = lines.len().saturating_sub(40);
    let lines = &lines[from..];
    let read_at = Utc::now();

    // A choice: numbered options in a row (1, 2, 3...), nearest the bottom.
    let mut end = None;
    for i in (0..lines.len()).rev() {
        if option(lines[i]).is_some() {
            end = Some(i);
            break;
        }
    }
    if let Some(last) = end {
        // Up from the last option: options, with at most two wrapped lines
        // between them.
        let mut first = last;
        'up: loop {
            for k in 1..=3 {
                let Some(j) = first.checked_sub(k) else { break 'up };
                if option(lines[j]).is_some() {
                    first = j;
                    continue 'up;
                }
                // A rule can part a menu's last options from the rest.
                if is_rule(lines[j]) {
                    continue;
                }
                if clean(lines[j]).is_empty() {
                    break 'up;
                }
            }
            break;
        }
        let mut options: Vec<Choice> = Vec::new();
        let mut selected = None;
        // The options, a wrapped label's next lines joined to it, then any
        // lines wrapped from the last one.
        let mut tail = last + 1;
        while tail < lines.len() && tail <= last + 2 && !clean(lines[tail]).is_empty() && !is_rule(lines[tail]) && option(lines[tail]).is_none() && !clean(lines[tail]).contains(" to select") && !clean(lines[tail]).contains(" to confirm") {
            tail += 1;
        }
        for line in &lines[first..tail] {
            match option(line) {
                Some((sel, key, label)) => {
                    if sel {
                        selected = Some(key.clone());
                    }
                    options.push(Choice { key, label });
                }
                None => {
                    if let Some(prev) = options.last_mut() {
                        let more = clean(line);
                        if !more.is_empty() && !is_rule(line) {
                            // A description under it ("The color red"), or the rest of a long label.
                            prev.label.push_str(if more.starts_with(char::is_uppercase) { " · " } else { " " });
                            prev.label.push_str(&more);
                        }
                    }
                }
            }
        }
        let numbered = options.iter().enumerate().all(|(n, c)| c.key == (n + 1).to_string());
        // An input box under the list: it's the agent's message (steps it
        // wrote), not a prompt waiting for a pick.
        let input_below = lines[last + 1..].iter().any(|l| {
            let t = clean(l);
            (t.starts_with('>') || t.starts_with('❯') || t.starts_with('›')) && option(l).is_none()
        });
        if options.len() >= 2 && numbered && !input_below {
            let text = question_above(&lines[..first]);
            return Question { text, options, selected, confirm: asks_for_enter(lines), read_at };
        }
    }

    // A menu without numbers (Claude Code's "Do you trust this folder?").
    if let Some((first, last, options, selected)) = menu(lines) {
        let input_below = lines[last + 1..].iter().any(|l| {
            let t = clean(l);
            (t.starts_with('>') || t.starts_with('❯') || t.starts_with('›')) && option(l).is_none()
        });
        if !input_below {
            return Question { text: question_above(&lines[..first]), options, selected: Some(selected), confirm: false, read_at };
        }
    }

    // No choice: the end of what it said, above its input box.
    let mut bottom = lines.len();
    // Skip the input box and the hints under it ("? for shortcuts").
    for i in (0..lines.len()).rev() {
        let t = clean(lines[i]);
        if t.starts_with('>') || t.starts_with('❯') || t.starts_with('›') {
            bottom = i;
            break;
        }
    }
    // Its last paragraph, past status lines ("✻ Baked for 3s").
    let mut text: Vec<String> = Vec::new();
    for line in lines[..bottom].iter().rev() {
        let t = clean(line);
        if is_rule(line) || t.is_empty() {
            if text.is_empty() {
                continue;
            }
            break;
        }
        if text.is_empty() && is_status(&t) {
            continue;
        }
        text.push(t.trim_start_matches(['⏺', '●', '•']).trim_start().to_string());
        if text.len() >= 8 {
            break;
        }
    }
    text.reverse();
    while text.first().is_some_and(String::is_empty) {
        text.remove(0);
    }
    Question { text, options: Vec::new(), selected: None, confirm: false, read_at }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(q: &Question) -> Vec<&str> {
        q.options.iter().map(|c| c.key.as_str()).collect()
    }

    #[test]
    fn reads_claude_codes_permission_prompt() {
        let screen = "⏺ I'll run the tests.\n\n╭──────────────────────────────────────────────╮\n│ Bash command                                 │\n│                                              │\n│   npm test                                   │\n│   Run the test suite                         │\n│                                              │\n│ Do you want to proceed?                      │\n│ ❯ 1. Yes                                     │\n│   2. Yes, and don't ask again for npm test   │\n│   commands in /home/adib/api                 │\n│   3. No, and tell Claude what to do          │\n│   differently (esc)                          │\n╰──────────────────────────────────────────────╯\n";
        let q = read(screen);
        assert!(q.is_choice());
        assert_eq!(keys(&q), ["1", "2", "3"]);
        assert_eq!(q.options[1].label, "Yes, and don't ask again for npm test commands in /home/adib/api");
        assert_eq!(q.options[2].label, "No, and tell Claude what to do differently (esc)");
        assert_eq!(q.selected.as_deref(), Some("1"));
        assert_eq!(q.text.first().map(String::as_str), Some("Bash command"));
        assert!(q.text.contains(&"npm test".to_string()));
        assert_eq!(q.text.last().map(String::as_str), Some("Do you want to proceed?"));
    }

    #[test]
    fn reads_an_unboxed_prompt_and_codex() {
        let claude = " Edit file\n src/app.ts\n\n Do you want to make this edit to app.ts?\n ❯ 1. Yes\n   2. Yes, allow all edits during this session (shift+tab)\n   3. No, and tell Claude what to do differently (esc)\n";
        let q = read(claude);
        assert_eq!(keys(&q), ["1", "2", "3"]);
        assert_eq!(q.text.last().map(String::as_str), Some("Do you want to make this edit to app.ts?"));
        let codex = "› Run the tests\n\n• Read 3 files\n\nWould you like to run the following command?\n\n  $ cargo test\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for this command (a)\n  3. No, and tell Codex what to do differently (esc)\n\n  Press enter to confirm or esc to cancel\n";
        let q = read(codex);
        assert_eq!(keys(&q), ["1", "2", "3"]);
        assert_eq!(q.options[0].label, "Yes, proceed (y)");
        assert_eq!(q.selected.as_deref(), Some("1"));
        assert!(q.text.contains(&"$ cargo test".to_string()));
        assert!(!q.text.iter().any(|l| l.contains("Run the tests")), "{:?}", q.text);
    }

    #[test]
    fn reads_claude_codes_question_with_descriptions() {
        // Claude Code 2.1 asking a multiple-choice question, as read off a real screen.
        let screen = "❯ Ask me with a multiple-choice question which color I like.\n──────────────────\n ☐ Color\n\nWhich color do you like?\n\n❯ 1. Red\n     The color red\n  2. Green\n     The color green\n  3. Blue\n     The color blue\n  4. Type something.\n──────────────────\n  5. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
        let q = read(screen);
        assert_eq!(keys(&q), ["1", "2", "3", "4", "5"]);
        assert_eq!(q.options[0].label, "Red · The color red");
        assert_eq!(q.options[4].label, "Chat about this");
        assert_eq!(q.selected.as_deref(), Some("1"));
        assert_eq!(q.text.first().map(String::as_str), Some("☐ Color"));
        assert_eq!(q.text.last().map(String::as_str), Some("Which color do you like?"));
    }

    #[test]
    fn reads_a_menu_without_numbers() {
        // Claude Code 2's first question in a new folder, as read off a real screen.
        let screen = "────────────────────\n Accessing workspace:\n\n /tmp\n\n Quick safety check: Is this a project you created or one you trust?\n\n Claude Code'll be able to read, edit, and execute files here.\n\n Security guide\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel\n";
        let q = read(screen);
        assert!(q.is_choice());
        assert_eq!(q.options.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(keys(&q), ["#0", "#1"]);
        assert_eq!(q.selected.as_deref(), Some("#0"));
        assert_eq!(q.text.last().map(String::as_str), Some("Security guide"));
        assert_eq!(keys_for("#1", q.selected.as_deref(), false), ("\x1b[B".to_string(), true));
        assert_eq!(keys_for("#0", Some("#2"), false), ("\x1b[A\x1b[A".to_string(), true));
        assert_eq!(keys_for("2", Some("1"), false), ("2".to_string(), false));
        // A menu that waits for Enter after the number (Codex).
        assert_eq!(keys_for("1", Some("1"), true), ("1".to_string(), true));
    }

    #[test]
    fn reads_a_finished_agent() {
        let screen = "⏺ I fixed the failing test: the coupon is now applied\n  before tax, and the totals match. Want me to open a pull request?\n\n╭────────────────────────────────────╮\n│ >                                  │\n╰────────────────────────────────────╯\n  ? for shortcuts\n";
        let q = read(screen);
        assert!(!q.is_choice());
        assert_eq!(q.text.first().map(String::as_str), Some("I fixed the failing test: the coupon is now applied"));
        assert_eq!(q.text.last().map(String::as_str), Some("before tax, and the totals match. Want me to open a pull request?"));
        // Claude Code 2.1, as read off a real screen: its reply, not the status line.
        let real = "❯ Run the shell command uname -r and tell me the kernel version.\n\n● Bash(uname -r)\n  ⎿  7.0.0-38-generic\n  ⎿  Allowed by auto mode classifier\n\n● 7.0.0-38-generic\n\n✻ Baked for 3s · done 1:07 PM\n\n──────────────────────────────\n❯\n──────────────────────────────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n";
        let q = read(real);
        assert!(!q.is_choice());
        assert_eq!(q.text, vec!["7.0.0-38-generic".to_string()]);
        // A long prompt of yours wraps under its ❯: not a menu.
        let wrapped = "❯ Fix the failing test in billing and then\n  run the whole suite again\n\n● Done: 214 passed.\n\n──────\n❯\n──────\n";
        assert!(!read(wrapped).is_choice());
    }

    #[test]
    fn a_numbered_list_in_a_message_isnt_a_choice() {
        // Steps the agent wrote (they don't start at 1 in a row at the bottom, or there's one).
        let screen = "⏺ Done. Next:\n  1. Run the migrations\n\n> \n";
        assert!(!read(screen).is_choice());
        let skipped = "⏺ Options:\n  2. Two\n  4. Four\n\n> \n";
        assert!(!read(skipped).is_choice());
        let steps = "⏺ Done. Next steps:\n  1. Run the migrations\n  2. Deploy to staging\n\n╭──────╮\n│ >    │\n╰──────╯\n";
        let q = read(steps);
        assert!(!q.is_choice());
        assert_eq!(q.text.last().map(String::as_str), Some("2. Deploy to staging"));
    }

    #[test]
    fn codex_menus_wait_for_enter() {
        let screen = "> You are in /home/adib/muxen/workspaces/ws-cqp5kr\n\n  Do you trust the contents of this directory?\n\n› 1. Yes, continue\n  2. No, quit\n\n  Press enter to continue\n";
        let q = read(screen);
        assert_eq!(q.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "2"]);
        assert!(q.confirm);
        let claude = "Do you want to proceed?\n❯ 1. Yes\n  2. No\n";
        assert!(!read(claude).confirm);
        // Codex 0.163's folder screen.
        let newer = "  Folder access\n  /tmp/app\n  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings.\n› 1. Trust and continue\n  2. Back to Agent Command Center\n  enter continue · esc back\n";
        let q = read(newer);
        assert_eq!(q.options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["1", "2"]);
        assert!(q.confirm);
        assert_eq!(keys_for("1", q.selected.as_deref(), q.confirm), ("1".to_string(), true));
    }
}
