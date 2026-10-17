//! A panic hook that writes what happened to a crash log before the program
//! ends, so a crash leaves more behind than "abort() called". The app uses
//! this file too (`#[path]` in its main.rs).

use std::any::Any;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Where the hook writes. The app moves it once it knows its config folder.
static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Past this size the log starts over, so crashes in a loop can't fill the disk.
const MAX_LOG: u64 = 1024 * 1024;

/// Appends each panic to `log`, then lets the previous hook do what it did
/// (print it to stderr). `program` names the build, such as "muxin 0.5.6".
pub fn install(program: &'static str, log: PathBuf) {
    log_to(log);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let text = report(program, unix_now(), &thread, message(info.payload()), &location, &backtrace);
        if let Some(log) = log_path() {
            let _ = write(&log, &text);
        }
        previous(info);
    }));
}

/// Writes crashes to `log` from now on, such as one in a `--config-dir`.
pub fn log_to(log: PathBuf) {
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
}

fn log_path() -> Option<PathBuf> {
    LOG.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// For the app's tests.
#[cfg(test)]
#[allow(dead_code)]
pub fn current_log() -> Option<PathBuf> {
    log_path()
}

/// One crash, as it goes in the log.
fn report(program: &str, when: u64, thread: &str, message: &str, location: &str, backtrace: &str) -> String {
    format!("=== {program} panicked at unix time {when}\nthread: {thread}\nat: {location}\n{message}\n\nbacktrace:\n{}\n\n", backtrace.trim_end())
}

/// What `panic!` was given: a `&str` or a `String`, anything else unknown.
fn message(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("(no message)")
}

fn write(log: &PathBuf, text: &str) -> std::io::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let start_over = std::fs::metadata(log).is_ok_and(|m| m.len() > MAX_LOG);
    let mut file = std::fs::OpenOptions::new().create(true).write(true).append(!start_over).truncate(start_over).open(log)?;
    file.write_all(text.as_bytes())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_what_happened_and_where() {
        let text = report("muxin 0.5.6", 1_760_000_000, "main", "byte index 5 is out of range", "src/ui/input.rs:367:42", "   0: muxin::main\n");
        assert_eq!(
            text,
            "=== muxin 0.5.6 panicked at unix time 1760000000\nthread: main\nat: src/ui/input.rs:367:42\nbyte index 5 is out of range\n\nbacktrace:\n   0: muxin::main\n\n"
        );
    }

    #[test]
    fn message_reads_str_and_string_payloads() {
        let s: Box<dyn Any + Send> = Box::new("static");
        let owned: Box<dyn Any + Send> = Box::new(String::from("formatted 42"));
        let other: Box<dyn Any + Send> = Box::new(42);
        assert_eq!(message(s.as_ref()), "static");
        assert_eq!(message(owned.as_ref()), "formatted 42");
        assert_eq!(message(other.as_ref()), "(no message)");
    }

    #[test]
    fn log_appends_then_starts_over_when_big() {
        let dir = std::env::temp_dir().join(format!("muxen-crash-test-{}", std::process::id()));
        let log = dir.join("crash.log");
        let _ = std::fs::remove_file(&log);
        write(&log, "one\n").unwrap();
        write(&log, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        std::fs::write(&log, vec![b'x'; MAX_LOG as usize + 1]).unwrap();
        write(&log, "three\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "three\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
