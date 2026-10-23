//! Moving a session's work to another machine (a handoff there). `carry_pack`
//! packs what the other machine can't get from the repository's remote:
//! commits that aren't on origin (a git bundle; the whole history when there's
//! no origin), uncommitted changes (a patch) and new files git doesn't ignore
//! (a tar), in ~/.muxen/carry/<id>/. Muxin copies that folder across, and
//! `carry_unpack` puts it in a checkout there that starts where the work did
//! (a clone or worktree at `base`, or an empty folder).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// A pack bigger than this is refused: new files that should be ignored, most likely.
const MAX_BYTES: u64 = 500 * 1024 * 1024;

fn folder(id: &str) -> PathBuf {
    crate::home_dir().join("carry").join(id)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn err(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

/// Runs git in `dir`: its output, or what it said went wrong.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0");
    match crate::repos::run_with_timeout(&mut cmd, Duration::from_secs(600))? {
        (true, out, _) => Ok(out.trim_end().to_string()),
        (false, out, e) => Err(e.lines().chain(out.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("git failed").to_string()),
    }
}

/// `{"op": "carry_pack", "pack_id", "cwd"}` (not `id`: a link uses that): packs the work in `cwd`'s checkout.
/// Says where in the checkout `cwd` is (`sub`), its branch, `head`, the
/// commit on origin it builds on (`base`; none when the whole history goes),
/// origin's URL, and how many commits and files go along.
pub fn pack(req: &Value) -> Value {
    let id = req["pack_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return err("which pack?");
    }
    let cwd = crate::sys::expand_home(req["cwd"].as_str().unwrap_or("~"));
    let Ok(root) = git(&cwd, &["rev-parse", "--show-toplevel"]) else {
        return err(format!("{} isn't a git checkout, so its work can't move to another machine.", cwd.display()));
    };
    let root = PathBuf::from(root);
    let Ok(head) = git(&root, &["rev-parse", "HEAD"]) else {
        return err(format!("{} has no commits yet: commit once, then move it.", root.display()));
    };
    let sub = match (cwd.canonicalize(), root.canonicalize()) {
        (Ok(c), Ok(r)) => c.strip_prefix(&r).map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        _ => String::new(),
    };
    let origin = git(&root, &["remote", "get-url", "origin"]).ok().filter(|o| !o.is_empty());
    let branch = git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().filter(|b| !b.is_empty());

    // What origin doesn't have: commits past `base`, or the whole history.
    let (unpushed, base) = match &origin {
        Some(_) => {
            let n: u64 = git(&root, &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"]).ok().and_then(|n| n.parse().ok()).unwrap_or(0);
            let base = if n == 0 {
                Some(head.clone())
            } else {
                git(&root, &["rev-list", "--boundary", "HEAD", "--not", "--remotes=origin"]).ok().and_then(|l| l.lines().find_map(|l| l.strip_prefix('-').map(String::from)))
            };
            (n, base)
        }
        None => (git(&root, &["rev-list", "--count", "HEAD"]).ok().and_then(|n| n.parse().ok()).unwrap_or(0), None),
    };

    let out = folder(id);
    let _ = std::fs::remove_dir_all(&out);
    if let Err(e) = std::fs::create_dir_all(&out) {
        return err(format!("couldn't make {}: {e}", out.display()));
    }
    let fail = |e: String| {
        let _ = std::fs::remove_dir_all(folder(id));
        err(e)
    };
    let bundle = out.join("work.bundle").to_string_lossy().to_string();
    if unpushed > 0 {
        let made = match &base {
            Some(_) => git(&root, &["bundle", "create", &bundle, "HEAD", "--not", "--remotes=origin"]),
            None => git(&root, &["bundle", "create", &bundle, "HEAD"]),
        };
        if let Err(e) = made {
            return fail(format!("couldn't pack its commits: {e}"));
        }
    }
    let patch = out.join("changes.patch");
    if let Err(e) = git(&root, &["diff", "HEAD", "--binary", &format!("--output={}", patch.display())]) {
        return fail(format!("couldn't pack its uncommitted changes: {e}"));
    }
    if std::fs::metadata(&patch).is_ok_and(|m| m.len() == 0) {
        let _ = std::fs::remove_file(&patch);
    }
    // New files git doesn't ignore (not Muxin's own notes).
    let listed = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let new: Vec<&str> = listed.split('\0').filter(|f| !f.is_empty() && !f.starts_with(".muxin/") && !f.starts_with(".muxen/")).collect();
    if !new.is_empty()
        && let Err(e) = tar_files(&root, &new, &out.join("new.tar"))
    {
        return fail(format!("couldn't pack its new files: {e}"));
    }
    let bytes: u64 = std::fs::read_dir(&out).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    if bytes > MAX_BYTES {
        return fail(format!("its work is {} MB with its new files, more than Muxin moves: add what shouldn't go (builds, downloads) to .gitignore.", bytes / 1_000_000));
    }
    let uncommitted = git(&root, &["status", "--porcelain"]).map(|s| s.lines().filter(|l| !l.contains(".muxin/") && !l.contains(".muxen/")).count()).unwrap_or(0);
    json!({
        "ok": true,
        "root": root.to_string_lossy(),
        "sub": sub,
        "branch": branch,
        "head": head,
        "base": base,
        "origin": origin,
        "unpushed": unpushed,
        "uncommitted": uncommitted,
        "bytes": bytes,
    })
}

fn tar_files(root: &Path, files: &[&str], to: &Path) -> Result<(), String> {
    let mut tar = Command::new("tar");
    // A Mac's tar packs extended attributes as "._name" files, which another machine gets as files.
    if cfg!(target_os = "macos") {
        tar.env("COPYFILE_DISABLE", "1").arg("--no-mac-metadata");
    }
    let mut child = tar
        .arg("-C")
        .arg(root)
        .arg("-cf")
        .arg(to)
        .args(["--null", "-T", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tar: {e}"))?;
    let list: Vec<u8> = files.iter().flat_map(|f| f.bytes().chain(std::iter::once(0))).collect();
    child.stdin.take().ok_or("tar took no input")?.write_all(&list).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// `{"op": "carry_unpack", "pack_id", "dir", "branch", "base"?, "note"?}`: puts
/// the pack (copied here) in the checkout in `dir`: a clone or worktree at `base`
/// (on `branch`), or an empty folder for a repository without a remote. Its
/// commits go on `branch` (a branch already there that they don't build on
/// isn't moved: they go on `<branch>-2`), then its changes and new files, and
/// `note` becomes `.muxin/handoff.md`. Says which branch it ended on.
pub fn unpack(req: &Value) -> Value {
    let id = req["pack_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return err("which pack?");
    }
    let pack = folder(id);
    if !pack.is_dir() {
        return err(format!("there's no pack {id} here: the copy didn't arrive"));
    }
    let result = unpack_into(&pack, req);
    let _ = std::fs::remove_dir_all(&pack);
    match result {
        Ok(v) => v,
        Err(e) => err(e),
    }
}

fn unpack_into(pack: &Path, req: &Value) -> Result<Value, String> {
    let dir = crate::sys::expand_home(req["dir"].as_str().unwrap_or(""));
    if !dir.is_dir() {
        return Err(format!("{} isn't a folder here.", dir.display()));
    }
    let branch = req["branch"].as_str().filter(|b| !b.is_empty()).unwrap_or("muxin/handoff").to_string();
    git(&dir, &["check-ref-format", "--branch", &branch]).map_err(|_| format!("\"{branch}\" can't be a branch name."))?;
    if !dir.join(".git").exists() {
        git(&dir, &["init", "-q"])?;
    }
    let mut warnings: Vec<String> = Vec::new();

    // Where the work's commits end: the bundle's tip, else the commit it builds on.
    let bundle = pack.join("work.bundle");
    let target = if bundle.is_file() {
        git(&dir, &["fetch", "-q", &bundle.to_string_lossy(), "HEAD"]).map_err(|e| format!("couldn't add its commits: {e}"))?;
        Some(git(&dir, &["rev-parse", "FETCH_HEAD"])?)
    } else {
        req["base"].as_str().filter(|b| !b.is_empty()).map(String::from)
    };
    let mut on = git(&dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    if let Some(target) = &target {
        match git(&dir, &["rev-parse", "-q", "--verify", "HEAD"]).ok() {
            // An empty repository (no remote): the branch starts at the work.
            None => {
                git(&dir, &["checkout", "-q", "-b", &branch, target])?;
                on = Some(branch.clone());
            }
            Some(current) if current == *target => {}
            Some(current) if git(&dir, &["merge-base", "--is-ancestor", &current, target]).is_ok() && on.is_some() => {
                git(&dir, &["merge", "-q", "--ff-only", target])?;
            }
            // A branch that went its own way here stays as it is.
            Some(_) => {
                let free = (2..100).map(|n| format!("{branch}-{n}")).find(|b| git(&dir, &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")]).is_err()).ok_or("no free branch name")?;
                git(&dir, &["checkout", "-q", "-b", &free, target])?;
                warnings.push(format!("{branch} here had other commits, so the work is on {free}."));
                on = Some(free);
            }
        }
    }
    let patch = pack.join("changes.patch");
    if patch.is_file() && git(&dir, &["apply", "--whitespace=nowarn", &patch.to_string_lossy()]).is_err() {
        // What applies cleanly goes in; the rest is left as .rej files.
        let _ = git(&dir, &["apply", "--reject", "--whitespace=nowarn", &patch.to_string_lossy()]);
        warnings.push("Some of its uncommitted changes didn't apply cleanly: look for .rej files.".into());
    }
    let new = pack.join("new.tar");
    if new.is_file() {
        let out = Command::new("tar").arg("-C").arg(&dir).arg("-xf").arg(&new).stdin(Stdio::null()).output().map_err(|e| format!("tar: {e}"))?;
        if !out.status.success() {
            warnings.push(format!("Some of its new files didn't come along: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    if let Some(note) = req["note"].as_str().filter(|n| !n.is_empty()) {
        let path = dir.join(crate::handoff::NOTE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("couldn't make {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, note).map_err(|e| format!("couldn't write the note: {e}"))?;
        crate::handoff::keep_out_of_git(&dir);
    }
    Ok(json!({
        "ok": true,
        "branch": on,
        "head": git(&dir, &["rev-parse", "--short", "HEAD"]).ok(),
        "warnings": warnings,
    }))
}

/// `{"op": "carry_drop", "pack_id"}`: removes a pack (once it's copied, or after a failed move).
pub fn drop(req: &Value) -> Value {
    let id = req["pack_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return err("which pack?");
    }
    let _ = std::fs::remove_dir_all(folder(id));
    json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("muxen-carry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    fn commit(dir: &Path, file: &str, text: &str, message: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-qm", message]);
    }

    /// A file's text with Unix line endings: git on Windows checks files out with CRLF.
    fn text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
    }

    /// A pack's copy, as Muxin makes it on the other machine.
    fn copy_pack(id: &str, to: &str) {
        let (from, dest) = (folder(id), folder(to));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();
        for e in std::fs::read_dir(&from).unwrap().flatten() {
            std::fs::copy(e.path(), dest.join(e.file_name())).unwrap();
        }
    }

    #[test]
    fn work_moves_to_a_clone_at_its_base() {
        let t = tmp("clone");
        let (origin, here, there) = (t.join("origin.git"), t.join("here"), t.join("there"));
        run(&t, &["init", "-q", "--bare", "-b", "main", &origin.to_string_lossy()]);
        run(&t, &["clone", "-q", &origin.to_string_lossy(), &here.to_string_lossy()]);
        commit(&here, "a.txt", "one\n", "one");
        run(&here, &["push", "-q", "origin", "HEAD:main"]);
        run(&here, &["checkout", "-q", "-b", "feature"]);
        commit(&here, "a.txt", "two\n", "two");
        std::fs::write(here.join("a.txt"), "three\n").unwrap();
        std::fs::create_dir_all(here.join("src")).unwrap();
        std::fs::write(here.join("src/new.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(here.join(".muxin")).unwrap();
        std::fs::write(here.join(".muxin/handoff.md"), "old note").unwrap();

        let id = "t-clone-a";
        let p = pack(&json!({ "pack_id": id, "cwd": here.join("src").to_string_lossy() }));
        assert_eq!(p["ok"], true, "{p}");
        assert_eq!(p["sub"], "src");
        assert_eq!(p["branch"], "feature");
        assert_eq!(p["unpushed"], 1);
        let base = p["base"].as_str().unwrap().to_string();

        // There: a clone at the base, on the branch (as a workspace makes it).
        run(&t, &["clone", "-q", &origin.to_string_lossy(), &there.to_string_lossy()]);
        run(&there, &["checkout", "-q", "-b", "feature", &base]);
        copy_pack(id, "t-clone-b");
        let u = unpack(&json!({ "pack_id": "t-clone-b", "dir": there.to_string_lossy(), "branch": "feature", "base": base, "note": "# Handoff" }));
        assert_eq!(u["ok"], true, "{u}");
        assert_eq!(u["branch"], "feature");
        assert_eq!(text(&there.join("a.txt")), "three\n");
        assert!(there.join("src/new.rs").is_file());
        assert_eq!(std::fs::read_to_string(there.join(".muxin/handoff.md")).unwrap(), "# Handoff");
        let log = Command::new("git").arg("-C").arg(&there).args(["log", "--format=%s"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout), "two\none\n");
        assert!(!folder("t-clone-b").exists(), "the pack goes once it's in");
        let _ = drop(&json!({ "pack_id": id }));
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn a_repository_without_a_remote_moves_whole() {
        let t = tmp("local");
        let (here, there) = (t.join("here"), t.join("there"));
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&there).unwrap();
        run(&here, &["init", "-q", "-b", "main"]);
        commit(&here, "a.txt", "one\n", "one");
        commit(&here, "a.txt", "two\n", "two");
        let p = pack(&json!({ "pack_id": "t-local-a", "cwd": here.to_string_lossy() }));
        assert_eq!(p["ok"], true, "{p}");
        assert_eq!(p["base"], Value::Null);
        assert_eq!(p["unpushed"], 2);
        copy_pack("t-local-a", "t-local-b");
        let u = unpack(&json!({ "pack_id": "t-local-b", "dir": there.to_string_lossy(), "branch": "main" }));
        assert_eq!(u["ok"], true, "{u}");
        assert_eq!(u["branch"], "main");
        assert_eq!(text(&there.join("a.txt")), "two\n");
        let _ = drop(&json!({ "pack_id": "t-local-a" }));
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn a_branch_that_went_its_own_way_isnt_moved() {
        let t = tmp("diverged");
        let (here, there) = (t.join("here"), t.join("there"));
        std::fs::create_dir_all(&here).unwrap();
        run(&here, &["init", "-q", "-b", "main"]);
        commit(&here, "a.txt", "one\n", "one");
        run(&t, &["clone", "-q", &here.to_string_lossy(), &there.to_string_lossy()]);
        commit(&here, "a.txt", "two\n", "two");
        commit(&there, "b.txt", "mine\n", "theirs");
        let p = pack(&json!({ "pack_id": "t-div-a", "cwd": here.to_string_lossy() }));
        copy_pack("t-div-a", "t-div-b");
        let u = unpack(&json!({ "pack_id": "t-div-b", "dir": there.to_string_lossy(), "branch": "main" }));
        assert_eq!(u["ok"], true, "{u} {p}");
        assert_eq!(u["branch"], "main-2");
        let main = Command::new("git").arg("-C").arg(&there).args(["log", "-1", "--format=%s", "main"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&main.stdout).trim(), "theirs");
        let _ = drop(&json!({ "pack_id": "t-div-a" }));
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn only_a_git_checkout_moves() {
        let t = tmp("plain");
        let p = pack(&json!({ "pack_id": "t-plain", "cwd": t.to_string_lossy() }));
        assert_eq!(p["ok"], false);
        assert!(p["error"].as_str().unwrap().contains("isn't a git checkout"));
        let _ = std::fs::remove_dir_all(t);
    }
}
