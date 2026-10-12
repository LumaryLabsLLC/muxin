//! Opening a machine's folder in your editor: Zed, VS Code or Cursor on
//! this computer, over their own remote-ssh support (the machine's ssh
//! alias, as Muxen reaches it), or directly for this computer's folders.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::Engine;
use crate::config::MachineSpec;

/// An editor Muxen can open folders in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Editor {
    /// "zed", "vscode" or "cursor".
    pub id: &'static str,
    pub name: &'static str,
    /// Its command-line tool.
    #[serde(skip)]
    pub cli: PathBuf,
}

/// The editors this computer has, in the order Muxen prefers them.
pub fn installed() -> Vec<Editor> {
    let home = dirs::home_dir().unwrap_or_default();
    let apps = |name: &str| [PathBuf::from("/Applications").join(name), home.join("Applications").join(name)];
    let on_path = |cmd: &str| -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from))
            .map(|d| d.join(cmd))
            .find(|f| f.is_file())
    };
    #[cfg(not(windows))]
    let find = |app: &str, inside: &str, cmd: &str| apps(app).into_iter().map(|a| a.join(inside)).find(|p| p.is_file()).or_else(|| on_path(cmd));
    #[cfg(windows)]
    let find = |_: &str, _: &str, cmd: &str| {
        let _ = (&apps, &on_path);
        windows_editor(cmd)
    };
    let mut out = Vec::new();
    for (id, name, app, inside, cmd) in [
        ("zed", "Zed", "Zed.app", "Contents/MacOS/cli", "zed"),
        ("cursor", "Cursor", "Cursor.app", "Contents/Resources/app/bin/cursor", "cursor"),
        ("vscode", "VS Code", "Visual Studio Code.app", "Contents/Resources/app/bin/code", "code"),
    ] {
        if let Some(cli) = find(app, inside, cmd) {
            out.push(Editor { id, name, cli });
        }
    }
    out
}

/// Windows: an editor's command (`code.cmd`, `cursor.cmd`, `zed.exe`) on
/// PATH, or where its installer puts it for this user or everyone.
#[cfg(windows)]
fn windows_editor(cmd: &str) -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let known: Vec<PathBuf> = match cmd {
        "code" => vec![local.join(r"Programs\Microsoft VS Code\bin\code.cmd"), program_files.join(r"Microsoft VS Code\bin\code.cmd")],
        "cursor" => vec![local.join(r"Programs\cursor\resources\app\bin\cursor.cmd")],
        "zed" => vec![local.join(r"Programs\Zed\bin\zed.exe"), local.join(r"Programs\Zed\Zed.exe")],
        _ => Vec::new(),
    };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .flat_map(|d| [format!("{cmd}.cmd"), format!("{cmd}.exe")].map(|f| d.join(f)));
    known.into_iter().chain(on_path).find(|p| p.is_file())
}

/// The editor to use: the one chosen (by id) when it's here, else the first one here.
pub fn pick(chosen: &str) -> Option<Editor> {
    let all = installed();
    all.iter().find(|e| e.id == chosen).cloned().or_else(|| all.into_iter().next())
}

/// The editors here, the one chosen in Settings first.
pub fn by_preference(chosen: &str) -> Vec<Editor> {
    let mut all = installed();
    all.sort_by_key(|e| e.id != chosen);
    all
}

/// An editor's own app icon as a PNG (made from its bundle's .icns once,
/// kept in the cache folder), for menus. None until `warm_icons` made it,
/// or when it has none (Linux, or an editor found only on PATH).
pub fn icon(id: &str) -> Option<PathBuf> {
    let path = icon_cache()?.join(format!("{id}.png"));
    path.is_file().then_some(path)
}

fn icon_cache() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("muxen").join("editors"))
}

/// Makes the PNGs `icon` hands out, for the editors here (macOS: sips).
pub fn warm_icons() {
    if !cfg!(target_os = "macos") {
        return;
    }
    let Some(dir) = icon_cache() else { return };
    for e in installed() {
        let out = dir.join(format!("{}.png", e.id));
        if out.is_file() {
            continue;
        }
        // The .app the command-line tool belongs to (a PATH tool may be a link into it).
        let real = std::fs::canonicalize(&e.cli).unwrap_or(e.cli.clone());
        let Some(app) = real.ancestors().find(|p| p.extension().is_some_and(|x| x == "app")) else { continue };
        let plist = app.join("Contents/Info.plist");
        let Ok(o) = std::process::Command::new("plutil").args(["-extract", "CFBundleIconFile", "raw", "-o", "-"]).arg(&plist).output() else { continue };
        let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if name.is_empty() {
            continue;
        }
        let file = if name.ends_with(".icns") { name } else { format!("{name}.icns") };
        let icns = app.join("Contents/Resources").join(file);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("sips").args(["-s", "format", "png", "-Z", "64"]).arg(&icns).arg("--out").arg(&out).output();
    }
}

/// The arguments that open `path` on `spec`'s machine in `editor`.
pub fn args(editor: &Editor, spec: &MachineSpec, path: &str) -> Vec<String> {
    if spec.is_local() {
        return vec![path.to_string()];
    }
    let host = match &spec.user {
        Some(u) if !u.is_empty() && !spec.host.contains('@') => format!("{u}@{}", spec.host),
        _ => spec.host.clone(),
    };
    match editor.id {
        "zed" => {
            let port = spec.port.map(|p| format!(":{p}")).unwrap_or_default();
            vec![format!("ssh://{host}{port}{path}")]
        }
        // VS Code and Cursor: Remote - SSH, by the ssh host (its config has the port).
        _ => vec!["--folder-uri".into(), format!("vscode-remote://ssh-remote+{host}{path}")],
    }
}

impl Engine {
    /// Opens a folder of a machine's in an editor here (`editor`: an id, or
    /// "" for the one chosen in Settings). Returns the editor's name.
    pub async fn open_in_editor(self: &Arc<Self>, machine_id: &str, path: &str, editor: &str) -> Result<String, String> {
        self.editor_command(machine_id, path, editor, true).await.map(|(name, _)| name)
    }

    /// The command that opens a machine's folder in an editor here (run it
    /// when `run`): the editor's name and the command line.
    pub async fn editor_command(self: &Arc<Self>, machine_id: &str, path: &str, editor: &str, run: bool) -> Result<(String, Vec<String>), String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        let chosen = if editor.is_empty() { self.settings().editor } else { editor.to_string() };
        let editor = pick(&chosen).ok_or("No editor found here: Muxen opens folders in Zed, Cursor or VS Code.")?;
        if self.demo_world().is_some() {
            return Err("The demo doesn't open editors.".into());
        }
        spec.refuse_if_disconnected()?;
        // Editors want the whole path: "~/code/api" as the machine has it.
        let path = if path.starts_with('/') {
            path.trim_end_matches('/').to_string()
        } else if spec.is_local() {
            let home = dirs::home_dir().unwrap_or_default();
            let rest = path.trim_start_matches('~').trim_start_matches('/');
            home.join(rest).display().to_string()
        } else if super::sessions::agent::is_windows(&spec) {
            // "C:\Users\me\code" as "/c:/Users/me/code", as Remote - SSH names Windows folders.
            let rest = path.trim_start_matches('~').trim_start_matches(['/', '\\']);
            let script = format!("(Resolve-Path -LiteralPath (Join-Path $env:USERPROFILE {})).Path", crate::ops::remote::ps_quote(rest));
            let found = super::sessions::agent::run_ps(&spec, &script, Duration::from_secs(15)).await.map_err(|e| format!("Couldn't find {path} on {}: {e}", spec.name))?;
            windows_path(found.trim())
        } else {
            let rest = path.trim_start_matches('~').trim_start_matches('/');
            let script = if rest.is_empty() { "cd ~ && pwd".to_string() } else { format!("cd ~/{} && pwd", crate::ops::remote::sh_quote(rest)) };
            super::sessions::agent::run_sh(&spec, &script, Duration::from_secs(15)).await.map(|o| o.trim().to_string()).map_err(|e| format!("Couldn't find {path} on {}: {e}", spec.name))?
        };
        let args = args(&editor, &spec, &path);
        if run {
            std::process::Command::new(&editor.cli).args(&args).spawn().map_err(|e| format!("Couldn't start {}: {e}", editor.name))?;
        }
        let mut line = vec![editor.cli.display().to_string()];
        line.extend(args);
        Ok((editor.name.to_string(), line))
    }
}

/// A Windows path as a URI path: `C:\x\y` as `/c:/x/y`.
fn windows_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    match p.split_once(':') {
        Some((drive, rest)) if drive.len() == 1 => format!("/{}:{}", drive.to_ascii_lowercase(), rest.trim_end_matches('/')),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(host: &str, user: Option<&str>, port: Option<u16>) -> MachineSpec {
        let mut s = crate::config::Inventory::parse(&format!("[[machine]]\nname = \"Box\"\nhost = \"{host}\"\n")).unwrap().machines.remove(0);
        s.user = user.map(str::to_string);
        s.port = port;
        s
    }

    #[test]
    fn builds_remote_folder_links() {
        let zed = Editor { id: "zed", name: "Zed", cli: PathBuf::new() };
        let code = Editor { id: "vscode", name: "VS Code", cli: PathBuf::new() };
        assert_eq!(args(&zed, &spec("beelink", None, None), "/home/adib/api"), ["ssh://beelink/home/adib/api"]);
        assert_eq!(args(&zed, &spec("box.lan", Some("me"), Some(2222)), "/srv"), ["ssh://me@box.lan:2222/srv"]);
        assert_eq!(args(&code, &spec("beelink", None, None), "/home/adib/api"), ["--folder-uri", "vscode-remote://ssh-remote+beelink/home/adib/api"]);
        assert_eq!(args(&code, &spec("local", None, None), "/Users/me/x"), ["/Users/me/x"]);
        assert_eq!(windows_path(r"C:\Users\adib\Downloads"), "/c:/Users/adib/Downloads");
        assert_eq!(args(&code, &spec("win11", None, None), "/c:/Users/adib"), ["--folder-uri", "vscode-remote://ssh-remote+win11/c:/Users/adib"]);
    }
}
