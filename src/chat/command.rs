//! Slash commands: a message that starts with "/" and a command's name is
//! done, not said. The page offers them as "/" is typed; the terminal takes
//! them at its prompt.
//!
//! Only these names. Anything else that starts with a "/" (a path, say) is
//! a message like any other.

/// One command.
#[derive(Debug, PartialEq)]
pub struct Command {
    pub name: &'static str,
    /// Other names it answers to.
    also: &'static [&'static str],
    /// What may follow it ("<title>"); "" when nothing does.
    pub takes: &'static str,
    pub about: &'static str,
    /// It works on the app's page; in the terminal's conversation.
    pub app: bool,
    pub term: bool,
}

pub const COMMANDS: &[Command] = &[
    Command { name: "new", also: &["clear"], takes: "", about: "Start a new chat", app: true, term: true },
    Command { name: "chats", also: &["resume"], takes: "", about: "Go on with an earlier chat", app: true, term: true },
    Command { name: "model", also: &[], takes: "[name]", about: "What answers: the program and its model", app: true, term: true },
    Command { name: "effort", also: &[], takes: "[level]", about: "How hard the model thinks", app: true, term: true },
    Command { name: "ask", also: &[], takes: "", about: "Ask permissions: ask before anything changes", app: true, term: true },
    Command { name: "bypass", also: &[], takes: "", about: "Bypass permissions: run every tool without asking", app: true, term: true },
    Command { name: "attach", also: &[], takes: "[file]", about: "Add a file to your next message", app: true, term: true },
    Command { name: "rename", also: &[], takes: "[title]", about: "Name this chat", app: true, term: true },
    Command { name: "delete", also: &[], takes: "", about: "Delete this chat", app: true, term: false },
    Command { name: "stop", also: &[], takes: "", about: "Stop what it's doing", app: true, term: false },
    Command { name: "help", also: &["?"], takes: "", about: "List these commands", app: false, term: true },
    Command { name: "quit", also: &["exit", "q"], takes: "", about: "End the conversation", app: false, term: true },
];

/// The command `text` is, and what follows its name. None: it's a message.
pub fn parse(text: &str) -> Option<(&'static Command, &str)> {
    let line = text.trim().strip_prefix('/')?;
    let (name, rest) = match line.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (line, ""),
    };
    let name = name.to_lowercase();
    COMMANDS.iter().find(|c| c.name == name || c.also.contains(&name.as_str())).map(|c| (c, rest))
}

/// The command name being typed where the cursor is: the text starts with a
/// "/", and the cursor is still in the word after it.
pub fn typing(text: &str, cursor: usize) -> Option<&str> {
    let so_far = text.get(..cursor)?.strip_prefix('/')?;
    (!so_far.contains(char::is_whitespace)).then_some(so_far)
}

/// The commands `so_far` could become: those whose name starts with it,
/// then (a few letters in) those that are about a word that does.
pub fn matching(so_far: &str, app: bool) -> Vec<&'static Command> {
    let wanted = so_far.to_lowercase();
    let here = COMMANDS.iter().filter(|c| if app { c.app } else { c.term });
    let (mut starts, mut has): (Vec<&Command>, Vec<&Command>) = (Vec::new(), Vec::new());
    for command in here {
        if command.name.starts_with(&wanted) || command.also.iter().any(|a| !wanted.is_empty() && a.starts_with(&wanted)) {
            starts.push(command);
        } else if wanted.len() >= 3 && command.about.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|word| word.starts_with(&wanted)) {
            has.push(command);
        }
    }
    starts.extend(has);
    starts
}

/// A file named after a command, as a shell would give it: in quotes, or
/// with "~" for the home folder.
pub fn path(named: &str) -> std::path::PathBuf {
    let named = named.trim().trim_matches(|c| c == '"' || c == '\'');
    match named.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => named.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_that_starts_with_a_commands_name_is_that_command() {
        fn named(text: &str) -> Option<(&'static str, &str)> {
            parse(text).map(|(c, rest)| (c.name, rest))
        }
        assert_eq!(named("/new"), Some(("new", "")));
        assert_eq!(named("  /Model  opus \n"), Some(("model", "opus")));
        assert_eq!(named("/rename Runner restarts on forge"), Some(("rename", "Runner restarts on forge")));
        assert_eq!(named("/attach ~/logs/build.log"), Some(("attach", "~/logs/build.log")));
        // By its other names.
        assert_eq!(named("/exit"), Some(("quit", "")));
        assert_eq!(named("/clear"), Some(("new", "")));
        // Anything else is a message: a path, a word that only starts like one, a slash in the middle.
        for said in ["/etc/hosts is empty on forge", "/models", "/", "what does /new do?", "new", ""] {
            assert_eq!(named(said), None, "{said}");
        }
    }

    #[test]
    fn the_commands_offered_are_the_ones_that_work_there() {
        let names = |so_far: &str, app: bool| matching(so_far, app).into_iter().map(|c| c.name).collect::<Vec<_>>();
        // Nothing typed after the "/": all of them, as listed.
        assert_eq!(names("", true), ["new", "chats", "model", "effort", "ask", "bypass", "attach", "rename", "delete", "stop"]);
        assert_eq!(names("", false), ["new", "chats", "model", "effort", "ask", "bypass", "attach", "rename", "help", "quit"]);
        // What starts with it, then what's about it.
        assert_eq!(names("e", true), ["effort"]);
        assert_eq!(names("by", true), ["bypass"]);
        assert_eq!(names("permissions", true), ["ask", "bypass"]);
        assert_eq!(names("q", false), ["quit"]);
        assert!(names("zzz", true).is_empty());
        // Every command says what it does, and works somewhere.
        assert!(COMMANDS.iter().all(|c| !c.about.is_empty() && (c.app || c.term)));
    }

    #[test]
    fn a_command_is_being_typed_while_the_cursor_is_in_its_name() {
        assert_eq!(typing("/mo", 3), Some("mo"));
        assert_eq!(typing("/", 1), Some(""));
        assert_eq!(typing("/model opus", 4), Some("mod"));
        // Past the name, or not at the start of the message: no.
        assert_eq!(typing("/model opus", 9), None);
        assert_eq!(typing("say /mo", 7), None);
        assert_eq!(typing("", 0), None);
        assert_eq!(typing("/mo", 9), None);
    }

    #[test]
    fn a_file_is_named_as_a_shell_would() {
        assert_eq!(path(" \"/tmp/a b.log\" "), std::path::PathBuf::from("/tmp/a b.log"));
        assert_eq!(path("~/x.log"), dirs::home_dir().unwrap_or_default().join("x.log"));
        assert_eq!(path("notes.md"), std::path::PathBuf::from("notes.md"));
    }
}
