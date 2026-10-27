//! A machine's displays: its monitors, and virtual ones Muxen adds the way
//! DeskPad does on a Mac: extra monitors on the same desktop, that windows
//! move to and the desktop view shows, with no screen plugged in.
//!
//! Linux (X11): a software dummy plug. A made-up monitor (an EDID Muxen
//! writes, at the size asked for) is plugged into a video output with nothing
//! connected, through the kernel's own switches; the desktop then sets it up
//! like a real one. That takes root: sudo, with the user's password when it
//! asks (the app sends it; nothing is installed). A Mac: CGVirtualDisplay,
//! held by a small `muxen-agent vdisplay` process in the user's session for
//! as long as the monitor should exist.
//!
//! Muxen's own virtual monitors are kept in ~/.muxen/displays.json, so they
//! can be told apart and removed. Ids: on Linux the RandR monitor's name
//! ("HDMI-A-0", "DisplayPort-0"; "all" for the whole desktop), on a Mac the
//! CGDirectDisplayID.

use std::io::Read;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::{home_dir, now};

fn registry_path() -> std::path::PathBuf {
    home_dir().join("displays.json")
}

/// Muxen's virtual monitors, as recorded (some may be gone; see `prune`).
fn registry() -> Vec<Value> {
    std::fs::read_to_string(registry_path()).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok()).unwrap_or_default()
}

fn save(entries: &[Value]) {
    let path = registry_path();
    if entries.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, Value::Array(entries.to_vec()).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Names the user gave displays, by id (`~/.muxen/display-names.json`).
fn names_path() -> std::path::PathBuf {
    home_dir().join("display-names.json")
}

fn names() -> serde_json::Map<String, Value> {
    std::fs::read_to_string(names_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn save_names(names: &serde_json::Map<String, Value>) {
    let path = names_path();
    if names.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let _ = std::fs::write(&path, Value::Object(names.clone()).to_string());
    }
}

/// Whether one of Muxen's virtual monitors still exists: its output is still
/// plugged (Linux), its holder still running (a Mac). Not from a listing:
/// the desktop briefly drops monitors while it rearranges them.
fn alive(entry: &Value) -> bool {
    if let Some(c) = entry["connector"].as_str() {
        return std::fs::read_to_string(format!("/sys/class/drm/{c}/status")).is_ok_and(|s| s.trim() == "connected");
    }
    match entry["pid"].as_u64() {
        Some(pid) => unsafe { libc::kill(pid as i32, 0) == 0 },
        None => false,
    }
}

/// `{"op": "displays"}`: every display, Muxen's virtual ones marked, with
/// the names the user gave them.
pub fn list() -> Value {
    match query() {
        Ok(mut displays) => {
            let mut reg = registry();
            // Forget virtual monitors that are really gone (the machine restarted, one was unplugged).
            let (live, gone): (Vec<Value>, Vec<Value>) = reg.drain(..).partition(alive);
            reg = live;
            gone.iter().for_each(drop_panel);
            if place_panels(&mut reg, &displays) || !gone.is_empty() {
                save(&reg);
            }
            #[cfg(target_os = "linux")]
            if let Some(layout) = layout() {
                for d in displays.iter_mut() {
                    let Some(id) = d["id"].as_str().map(str::to_string) else { continue };
                    let mut sizes: Vec<(u32, u32)> = layout.modes(&id).iter().map(|m| (m.w, m.h)).collect();
                    if sizes.is_empty() {
                        continue;
                    }
                    sizes.sort_by_key(|(w, h)| std::cmp::Reverse(w * h));
                    sizes.dedup();
                    d["modes"] = json!(sizes);
                    if let Some(p) = layout.placed(&id).map(|i| &layout.monitors[i]) {
                        d["scale"] = json!(p.scale);
                    }
                    if let Some(m) = layout.current(&id) {
                        d["scales"] = json!(m.scales);
                    }
                }
            }
            let names = names();
            for d in displays.iter_mut() {
                if let Some(e) = reg.iter().find(|e| e["id"] == d["id"]) {
                    d["virtual"] = json!(true);
                    d["name"] = e["label"].clone();
                }
                if let Some(n) = d["id"].as_str().and_then(|id| names.get(id)) {
                    d["name"] = n.clone();
                }
            }
            json!({ "ok": true, "displays": displays })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// `{"op": "display_rename", "display", "name"}`: what to call a display
/// (real or virtual). An empty name goes back to its own.
pub fn rename(req: &Value) -> Value {
    let id = req["display"].as_str().unwrap_or("");
    let name = req["name"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if id.is_empty() {
        return json!({ "ok": false, "error": "Which display?" });
    }
    let mut all = names();
    if name.is_empty() {
        all.remove(id);
    } else {
        all.insert(id.to_string(), json!(name));
    }
    save_names(&all);
    json!({ "ok": true })
}

/// The displays, from `muxen-agent displays` run where it can see them: in
/// the desktop session (with its DISPLAY on Linux, through the background
/// service on a Mac reached over ssh).
fn query() -> Result<Vec<Value>, String> {
    let reply = in_session(&["displays".to_string()])?;
    match reply["displays"].as_array() {
        Some(d) => Ok(d.clone()),
        None => Err(reply["error"].as_str().unwrap_or("Couldn't read the displays.").to_string()),
    }
}

/// Runs `muxen-agent <args>` where it can see the screens, for its JSON reply.
pub(crate) fn in_session(args: &[String]) -> Result<Value, String> {
    let out = if crate::gui::needed() {
        let mut stream = crate::gui::connect(args)?;
        let mut text = String::new();
        stream.read_to_string(&mut text).map_err(|e| e.to_string())?;
        text
    } else {
        let env = crate::desktop::session_env().ok_or("No graphical session found here (nobody logged in to a desktop?).")?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let output = Command::new(exe).args(args).envs(env).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    serde_json::from_str(out.trim()).map_err(|_| "The desktop session didn't answer.".to_string())
}

/// `muxen-agent displays`: prints the displays as JSON (run in the desktop session).
pub fn run_list() -> i32 {
    let reply = match monitors() {
        Ok(d) => json!({ "displays": d }),
        Err(e) => json!({ "error": e }),
    };
    println!("{reply}");
    0
}

/// `{"op": "display_create", "width", "height", "label"}`: a new virtual monitor.
pub fn create(req: &Value) -> Value {
    let w = req["width"].as_u64().unwrap_or(1920).clamp(640, 7680) as u32;
    let h = req["height"].as_u64().unwrap_or(1080).clamp(480, 4320) as u32;
    // Like plugging in another one: "Display 3" when there are two.
    let label = req["label"].as_str().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string);
    let label = label.unwrap_or_else(|| {
        let count = query().map(|d| d.iter().filter(|d| d["id"] != "all").count()).unwrap_or_else(|_| registry().len() + 1);
        format!("Display {}", count + 1)
    });
    match add(w, h, &label, req["password"].as_str()) {
        Ok(mut entry) => {
            entry["label"] = json!(label);
            entry["width"] = json!(w);
            entry["height"] = json!(h);
            entry["created"] = json!(now());
            let mut reg = registry();
            reg.push(entry.clone());
            if let Ok(displays) = query() {
                place_panels(&mut reg, &displays);
            }
            save(&reg);
            json!({ "ok": true, "display": { "id": entry["id"], "name": label, "width": w, "height": h, "virtual": true } })
        }
        Err(e) => json!({ "ok": false, "password": e == NEEDS_PASSWORD || e == WRONG_PASSWORD, "error": e }),
    }
}

/// `{"op": "display_remove", "display"}`: removes one of Muxen's virtual
/// monitors. (Not "id": a link's requests carry their own.)
pub fn remove(req: &Value) -> Value {
    let id = req["display"].as_str().unwrap_or("");
    let mut reg = registry();
    let Some(i) = reg.iter().position(|e| e["id"].as_str() == Some(id)) else {
        return json!({ "ok": false, "error": format!("{id} isn't a virtual monitor Muxen added.") });
    };
    let entry = reg.remove(i);
    #[cfg(target_os = "linux")]
    let laid_out = layout();
    match take_down(&entry, req["password"].as_str()) {
        Ok(()) => {
            save(&reg);
            drop_panel(&entry);
            // Once it's gone: the others' sizes and scale as they were, and
            // their panels' numbers shifted.
            for _ in 0..25 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                // The X server only notices an output went dark when asked to
                // look again (a full RandR query); the desktop then lets it go.
                #[cfg(target_os = "linux")]
                let _ = xrandr(&[]);
                if let Some(displays) = query().ok().filter(|d| !d.iter().any(|d| d["id"].as_str() == Some(id))) {
                    #[cfg(target_os = "linux")]
                    if let Some(laid_out) = &laid_out {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        restore(laid_out, None, Some(id));
                    }
                    if place_panels(&mut reg, &displays) {
                        save(&reg);
                    }
                    break;
                }
            }
            let mut all = names();
            if all.remove(id).is_some() {
                save_names(&all);
            }
            json!({ "ok": true })
        }
        Err(e) => json!({ "ok": false, "password": e == NEEDS_PASSWORD || e == WRONG_PASSWORD, "error": e }),
    }
}

// ---------------------------------------------------------------------------
// Linux: RandR monitors, outputs switched on with xrandr.

#[cfg(target_os = "linux")]
fn monitors() -> Result<Vec<Value>, String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, n) = x11rb::connect(None).map_err(|e| format!("Can't reach the desktop ({e})."))?;
    let screen = &conn.setup().roots[n];
    let (root, sw, sh) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);
    let monitors = conn.randr_get_monitors(root, true).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.monitors;
    let mut out = Vec::new();
    if monitors.len() > 1 {
        out.push(json!({ "id": "all", "name": "All screens", "x": 0, "y": 0, "width": sw, "height": sh, "main": false }));
    }
    let names: Vec<String> = monitors
        .iter()
        .map(|m| conn.get_atom_name(m.name).ok().and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_default())
        .collect();
    let kinds: Vec<&str> = names.iter().map(|n| output_kind(n)).collect();
    // The sizes each one offers.
    let res = conn.randr_get_screen_resources_current(root).ok().and_then(|c| c.reply().ok());
    let sizes = |m: &x11rb::protocol::randr::MonitorInfo| -> Vec<(u32, u32)> {
        let Some(res) = &res else { return Vec::new() };
        let Some(info) = m.outputs.first().and_then(|o| conn.randr_get_output_info(*o, res.config_timestamp).ok()?.reply().ok()) else { return Vec::new() };
        let mut sizes: Vec<(u32, u32)> = info.modes.iter().filter_map(|id| res.modes.iter().find(|m| m.id == *id)).map(|m| (m.width as u32, m.height as u32)).collect();
        sizes.sort_by_key(|(w, h)| std::cmp::Reverse(w * h));
        sizes.dedup();
        sizes
    };
    for (i, m) in monitors.iter().enumerate() {
        // "HDMI", or "HDMI 2" when there are two.
        let kind = kinds[i];
        let same = kinds.iter().filter(|k| **k == kind).count();
        let name = if same > 1 { format!("{kind} {}", kinds[..=i].iter().filter(|k| **k == kind).count()) } else { kind.to_string() };
        out.push(json!({ "id": names[i], "name": name, "x": m.x, "y": m.y, "width": m.width, "height": m.height, "main": m.primary, "modes": sizes(m), "any_size": true }));
    }
    if out.is_empty() {
        out.push(json!({ "id": "all", "name": "Screen", "x": 0, "y": 0, "width": sw, "height": sh, "main": true }));
    }
    Ok(out)
}

/// What kind of connector an output is, in words ("HDMI-A-0" -> "HDMI").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn output_kind(name: &str) -> &str {
    let n = name.to_ascii_uppercase();
    if n.starts_with("EDP") || n.starts_with("LVDS") || n.starts_with("DSI") {
        "Built-in display"
    } else if n.starts_with("HDMI") {
        "HDMI"
    } else if n.starts_with("DP") || n.starts_with("DISPLAYPORT") {
        "DisplayPort"
    } else if n.starts_with("VGA") {
        "VGA"
    } else if n.starts_with("DVI") {
        "DVI"
    } else if n.starts_with("VIRTUAL") {
        "Virtual"
    } else {
        name
    }
}

/// An output as `xrandr` lists it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Output {
    name: String,
    connected: bool,
    /// x, y, width, height when it's on.
    geometry: Option<(i64, i64, i64, i64)>,
}

/// Outputs and every mode name `xrandr` knows.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_xrandr(text: &str) -> (Vec<Output>, Vec<String>) {
    let mut outputs = Vec::new();
    let mut modes = Vec::new();
    for line in text.lines() {
        if line.starts_with(' ') {
            if let Some(m) = line.split_whitespace().next() {
                modes.push(m.to_string());
            }
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 2 || !matches!(words[1], "connected" | "disconnected") {
            continue;
        }
        // "HDMI-A-0 connected primary 3840x2160+0+0 (...)"
        let geometry = words[2..].iter().find_map(|w| {
            let (size, pos) = w.split_once('+')?;
            let (wd, ht) = size.split_once('x')?;
            let (x, y) = pos.split_once('+')?;
            Some((x.parse().ok()?, y.parse().ok()?, wd.parse().ok()?, ht.parse().ok()?))
        });
        outputs.push(Output { name: words[0].to_string(), connected: words[1] == "connected", geometry });
    }
    (outputs, modes)
}

#[cfg(target_os = "linux")]
fn xrandr(args: &[String]) -> Result<String, String> {
    let env = crate::desktop::session_env().ok_or("No graphical session found here (nobody logged in to a desktop?).")?;
    let out = Command::new("xrandr")
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "xrandr isn't installed here.".to_string() } else { e.to_string() })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("xrandr {} failed", args.join(" ")) } else { err })
    }
}

use crate::root::{NEEDS_PASSWORD, WRONG_PASSWORD};
#[cfg(target_os = "linux")]
use crate::root::as_root;

/// Plugs a made-up monitor (its EDID in the file `$2`) into the video output
/// `$1`, which must have nothing on it, through the kernel's own switches
/// for that: `edid_override` in debugfs and `status` in sysfs. Run as root.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PLUG: &str = r#"set -eu
c=$1
case "$c" in card[0-9]*-*) ;; *) echo "not a video output: $c" >&2; exit 1 ;; esac
case "$c" in *[!A-Za-z0-9-]*) echo "not a video output: $c" >&2; exit 1 ;; esac
[ "$(cat "/sys/class/drm/$c/status")" = disconnected ] || { echo "$c has a screen on it" >&2; exit 1; }
mountpoint -q /sys/kernel/debug || mount -t debugfs none /sys/kernel/debug 2>/dev/null || true
card=${c%%-*}; dir="/sys/kernel/debug/dri/${card#card}/${c#*-}"
[ -e "$dir/edid_override" ] || for d in /sys/kernel/debug/dri/*/"${c#*-}"; do [ -e "$d/edid_override" ] && dir=$d && break; done
[ -e "$dir/edid_override" ] || { echo "this machine's kernel can't take a monitor description" >&2; exit 1; }
cat "$2" > "$dir/edid_override"
echo on > "/sys/class/drm/$c/status"
"#;

/// Unplugs it again. Switched off rather than probed again ("detect"): some
/// drivers (amdgpu) go on reporting a monitor they were told about until a
/// real one is plugged in. (A real screen plugged into that output later
/// shows up after a restart.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const UNPLUG: &str = r#"set -eu
c=$1
case "$c" in card[0-9]*-*) ;; *) echo "not a video output: $c" >&2; exit 1 ;; esac
case "$c" in *[!A-Za-z0-9-]*) echo "not a video output: $c" >&2; exit 1 ;; esac
echo off > "/sys/class/drm/$c/status"
card=${c%%-*}; dir="/sys/kernel/debug/dri/${card#card}/${c#*-}"
[ -e "$dir/edid_override" ] || for d in /sys/kernel/debug/dri/*/"${c#*-}"; do [ -e "$d/edid_override" ] && dir=$d && break; done
[ -e "$dir/edid_override" ] && echo reset > "$dir/edid_override" || true
[ "$(cat "/sys/class/drm/$c/status")" = disconnected ] || { echo "The machine kept the monitor plugged in." >&2; exit 1; }
"#;

/// The video outputs the kernel knows ("card1-DP-1") with nothing on them.
#[cfg(target_os = "linux")]
fn free_connectors() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("card") && n.contains('-') && (n.contains("-DP-") || n.contains("-HDMI-") || n.contains("-DVI-")))
        .filter(|n| std::fs::read_to_string(format!("/sys/class/drm/{n}/status")).is_ok_and(|s| s.trim() == "disconnected"))
        .collect();
    // DisplayPort first: most GPUs have more of those to spare.
    names.sort_by_key(|n| (!n.contains("-DP-"), n.clone()));
    names
}

/// Plugs a made-up `w`×`h` monitor into a free output, like connecting a real
/// one: the desktop sets it up (wallpaper, panels, windows) the same way.
#[cfg(target_os = "linux")]
fn add(w: u32, h: u32, label: &str, password: Option<&str>) -> Result<Value, String> {
    let used: Vec<String> = registry().iter().filter_map(|e| e["connector"].as_str().map(str::to_string)).collect();
    let connector = free_connectors().into_iter().find(|c| !used.contains(c)).ok_or("This machine has no free video output for another monitor.")?;
    let (before, _) = parse_xrandr(&xrandr(&[])?);
    let laid_out = layout();
    let file = home_dir().join(format!("edid-{}.bin", std::process::id()));
    std::fs::write(&file, edid(w, h, label)).map_err(|e| e.to_string())?;
    let plugged = as_root(PLUG, &[&connector, &file.to_string_lossy()], password);
    let _ = std::fs::remove_file(&file);
    plugged?;
    // The desktop notices it like a real monitor; find the output it shows up as.
    let mut output = None;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (now, _) = parse_xrandr(&xrandr(&[])?);
        if let Some(o) = now.iter().find(|o| o.connected && !before.iter().any(|b| b.name == o.name && b.connected)) {
            output = Some((o.name.clone(), o.geometry.is_some()));
            break;
        }
    }
    let Some((name, _)) = output else {
        let _ = as_root(UNPLUG, &[&connector], password);
        return Err("The machine didn't pick up the new monitor.".into());
    };
    // The desktop lays out the new set of monitors its own way, which can
    // change the ones already there (a 4K screen dropped to 1080p). Once it
    // settles, put those back as they were and the new one to their right.
    std::thread::sleep(std::time::Duration::from_millis(2000));
    if let Some(laid_out) = laid_out {
        restore(&laid_out, Some(&name), None);
        return Ok(json!({ "id": name, "connector": connector }));
    }
    let (now, _) = parse_xrandr(&xrandr(&[])?);
    let s = |v: &str| v.to_string();
    let mut args = Vec::new();
    for b in before.iter().filter(|b| b.connected) {
        if let (Some((x, y, w, h)), Some(n)) = (b.geometry, now.iter().find(|n| n.name == b.name)) {
            if n.geometry != b.geometry {
                args.extend([s("--output"), b.name.clone(), s("--mode"), format!("{w}x{h}"), s("--pos"), format!("{x}x{y}")]);
            }
        }
    }
    let right = before.iter().filter(|b| b.connected).filter_map(|b| b.geometry).map(|(x, _, w, _)| x + w).max().unwrap_or(0);
    let new_at = now.iter().find(|n| n.name == name).and_then(|n| n.geometry);
    if !args.is_empty() || new_at.is_none_or(|(x, y, ..)| (x, y) != (right, 0)) {
        args.extend([s("--output"), name.clone(), s("--auto"), s("--pos"), format!("{right}x0")]);
        xrandr(&args)?;
    }
    Ok(json!({ "id": name, "connector": connector }))
}

#[cfg(target_os = "linux")]
fn take_down(entry: &Value, password: Option<&str>) -> Result<(), String> {
    let connector = entry["connector"].as_str().ok_or("Muxen doesn't know which output it's on.")?;
    as_root(UNPLUG, &[connector], password)
}

// Cinnamon puts its panels (the menu, the window list) on the main monitor
// only, so a new monitor would be bare wallpaper with nothing to click. Each
// of Muxen's monitors gets copies of the main monitor's panels (the same
// applets, pinned apps, sizes; not its shortcuts), and loses them when it
// goes. Panels name their monitor by number (the RandR order, main first),
// which shifts as monitors come and go, so they're placed again whenever the
// list changes. Other desktops: left as they are.

/// The user's desktop session bus, for a command run from outside the session.
#[cfg(target_os = "linux")]
fn session_bus(cmd: &mut Command) {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path=/run/user/{}/bus", unsafe { libc::getuid() }));
    }
}

/// `gsettings` in the user's desktop session; its output when it worked.
#[cfg(target_os = "linux")]
fn gsettings(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("gsettings");
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    session_bus(&mut cmd);
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The strings in a GVariant list: `['1:0:top', '2:0:bottom']`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn strings(list: &str) -> Vec<String> {
    list.split('\'').skip(1).step_by(2).map(str::to_string).collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gvariant(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", "))
}

/// Cinnamon's panels and applets, when Cinnamon is the desktop.
#[cfg(target_os = "linux")]
fn cinnamon_panels() -> Option<(Vec<String>, Vec<String>)> {
    let running = Command::new("pgrep").args(["-x", "cinnamon"]).stdout(Stdio::null()).status().is_ok_and(|s| s.success());
    if !running {
        return None;
    }
    let panels = strings(&gsettings(&["get", "org.cinnamon", "panels-enabled"])?);
    let applets = strings(&gsettings(&["get", "org.cinnamon", "enabled-applets"])?);
    Some((panels, applets))
}

/// Cinnamon's per-panel settings kept as "<panel id>:<value>" lists.
#[cfg(target_os = "linux")]
const PANEL_LISTS: [&str; 4] = ["panels-height", "panels-autohide", "panels-show-delay", "panels-hide-delay"];
/// ... and as JSON lists of {"panelId": id, ...}.
#[cfg(target_os = "linux")]
const PANEL_ZONES: [&str; 3] = ["panel-zone-icon-sizes", "panel-zone-symbolic-icon-sizes", "panel-zone-text-sizes"];

#[cfg(target_os = "linux")]
fn cinnamon_list(key: &str) -> Option<Vec<String>> {
    gsettings(&["get", "org.cinnamon", key]).map(|v| strings(&v))
}

#[cfg(target_os = "linux")]
fn cinnamon_json(key: &str) -> Option<Vec<Value>> {
    let raw = gsettings(&["get", "org.cinnamon", key])?;
    let text = raw.trim().trim_start_matches('\'').trim_end_matches('\'').replace("\\'", "'").replace("\\\\", "\\");
    serde_json::from_str(&text).ok()
}

#[cfg(target_os = "linux")]
fn set_cinnamon_json(key: &str, items: Vec<Value>) {
    let text = Value::Array(items).to_string();
    gsettings(&["set", "org.cinnamon", key, &format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))]);
}

/// Where Cinnamon keeps each applet's settings: `<uuid>/<instance>.json`.
#[cfg(target_os = "linux")]
fn spices() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/cinnamon/spices")
}

/// Gives a copied applet the original's settings (its pinned apps, ...),
/// without its keyboard shortcuts (the menu's Super key stays with the main
/// screen's). Whether there were any.
#[cfg(target_os = "linux")]
fn copy_applet_settings(uuid: &str, from: &str, to: u64) -> bool {
    let dir = spices().join(uuid);
    let Some(mut settings) = std::fs::read_to_string(dir.join(format!("{from}.json"))).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return false };
    if let Some(all) = settings.as_object_mut() {
        for s in all.values_mut().filter(|s| s["type"] == "keybinding") {
            s["value"] = json!("");
        }
    }
    std::fs::write(dir.join(format!("{to}.json")), settings.to_string()).is_ok()
}

/// Whether an applet may run more than once (its `max-instances`; one when
/// it doesn't say): the tray, sound, network ... stay on the main screen.
#[cfg(target_os = "linux")]
fn copyable(uuid: &str) -> bool {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    [home.join(".local/share/cinnamon/applets"), "/usr/share/cinnamon/applets".into()]
        .iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(uuid).join("metadata.json")).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|m| m["max-instances"].as_i64())
        .is_some_and(|n| n < 0 || n > 1)
}

/// Panel id, monitor number and side of a `panels-enabled` entry ("3:1:bottom").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn panel_parts(p: &str) -> Option<(u64, usize, &str)> {
    let mut it = p.splitn(3, ':');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?))
}

/// The panels a virtual monitor has (Muxen's first ones were a single "panel").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn panels_of(entry: &Value) -> Vec<u64> {
    match entry["panels"].as_array() {
        Some(ids) => ids.iter().filter_map(Value::as_u64).collect(),
        None => entry["panel"].as_u64().into_iter().collect(),
    }
}

/// Gives every virtual monitor in `reg` copies of the main monitor's panels
/// and points them at its monitor's current number. True when `reg` changed.
#[cfg(target_os = "linux")]
fn place_panels(reg: &mut [Value], displays: &[Value]) -> bool {
    let screens: Vec<&Value> = displays.iter().filter(|d| d["id"] != "all").collect();
    let order: Vec<&str> = screens.iter().filter_map(|d| d["id"].as_str()).collect();
    // Mid-rearrangement (one missing): the next listing will do it.
    if reg.is_empty() || reg.iter().any(|e| e["id"].as_str().is_none_or(|id| !order.contains(&id))) {
        return false;
    }
    if cinnamon_panels().is_none() {
        return false;
    }
    let mut changed = false;
    // Muxen's first panel (a menu and a window list of its own): replaced by copies.
    for e in reg.iter_mut().filter(|e| e.get("panel").is_some()) {
        drop_panel(e);
        if let Some(o) = e.as_object_mut() {
            o.remove("panel");
        }
        changed = true;
    }
    let Some((before, applets_before)) = cinnamon_panels() else { return changed };
    let (mut panels, mut applets) = (before.clone(), applets_before.clone());
    let owned: Vec<u64> = reg.iter().flat_map(panels_of).collect();
    let main = screens.iter().position(|d| d["main"] == true).unwrap_or(0);
    let templates: Vec<(u64, String)> =
        before.iter().filter_map(|p| panel_parts(p)).filter(|(id, monitor, _)| *monitor == main && !owned.contains(id)).map(|(id, _, side)| (id, side.to_string())).collect();
    let mut next_panel = before.iter().filter_map(|p| panel_parts(p)).map(|p| p.0).chain(owned.iter().copied()).max().unwrap_or(0) + 1;
    let mut next_applet = applets.iter().filter_map(|a| a.rsplit(':').next()?.parse::<u64>().ok()).max().unwrap_or(0) + 1;
    let mut copies = Vec::new();
    for e in reg.iter_mut() {
        let index = order.iter().position(|id| e["id"] == *id).unwrap_or(0);
        if e["panels"].is_array() {
            // A panel the user took away stays away.
            for id in panels_of(e) {
                if let Some(slot) = panels.iter_mut().find(|s| panel_parts(s).is_some_and(|p| p.0 == id)) {
                    let side = slot.rsplit(':').next().unwrap_or("bottom").to_string();
                    *slot = format!("{id}:{index}:{side}");
                }
            }
            continue;
        }
        let (mut mine, mut files) = (Vec::new(), Vec::new());
        for (t, side) in &templates {
            let n = next_panel;
            next_panel += 1;
            panels.push(format!("{n}:{index}:{side}"));
            for a in applets_before.iter().filter(|a| a.starts_with(&format!("panel{t}:"))) {
                // panel<id>:<zone>:<order>:<uuid>:<instance>
                let parts: Vec<&str> = a.split(':').collect();
                let [_, zone, place, uuid, from] = parts[..] else { continue };
                if !copyable(uuid) {
                    continue;
                }
                let to = next_applet;
                next_applet += 1;
                applets.push(format!("panel{n}:{zone}:{place}:{uuid}:{to}"));
                if copy_applet_settings(uuid, from, to) {
                    files.push(json!([uuid, to]));
                }
            }
            copies.push((*t, n));
            mine.push(n);
        }
        e["panels"] = json!(mine);
        e["applet_settings"] = json!(files);
        changed = true;
    }
    // Their heights, icon sizes, ... like the panels they copy.
    if !copies.is_empty() {
        for key in PANEL_LISTS {
            let Some(mut list) = cinnamon_list(key) else { continue };
            for (t, n) in &copies {
                if let Some(value) = list.iter().find_map(|x| x.strip_prefix(&format!("{t}:"))).map(str::to_string) {
                    list.retain(|x| !x.starts_with(&format!("{n}:")));
                    list.push(format!("{n}:{value}"));
                }
            }
            gsettings(&["set", "org.cinnamon", key, &gvariant(&list)]);
        }
        for key in PANEL_ZONES {
            let Some(mut items) = cinnamon_json(key) else { continue };
            for (t, n) in &copies {
                if let Some(mut item) = items.iter().find(|i| i["panelId"] == json!(t)).cloned() {
                    items.retain(|i| i["panelId"] != json!(n));
                    item["panelId"] = json!(n);
                    items.push(item);
                }
            }
            set_cinnamon_json(key, items);
        }
    }
    // The panels first: Cinnamon loads their applets once they're there.
    if panels != before {
        gsettings(&["set", "org.cinnamon", "panels-enabled", &gvariant(&panels)]);
    }
    if applets != applets_before {
        gsettings(&["set", "org.cinnamon", "enabled-applets", &gvariant(&applets)]);
    }
    changed
}

/// Takes away the panels a virtual monitor had, with their settings.
#[cfg(target_os = "linux")]
fn drop_panel(entry: &Value) {
    let ids = panels_of(entry);
    if ids.is_empty() {
        return;
    }
    let Some((panels, applets)) = cinnamon_panels() else { return };
    let ours = |p: &str| panel_parts(p).is_some_and(|p| ids.contains(&p.0));
    let on_ours = |a: &str| ids.iter().any(|i| a.starts_with(&format!("panel{i}:")));
    let kept_applets: Vec<String> = applets.iter().filter(|a| !on_ours(a)).cloned().collect();
    if kept_applets != applets {
        gsettings(&["set", "org.cinnamon", "enabled-applets", &gvariant(&kept_applets)]);
    }
    let kept: Vec<String> = panels.iter().filter(|p| !ours(p)).cloned().collect();
    if kept != panels {
        gsettings(&["set", "org.cinnamon", "panels-enabled", &gvariant(&kept)]);
    }
    for key in PANEL_LISTS {
        if let Some(list) = cinnamon_list(key) {
            let kept: Vec<String> = list.iter().filter(|x| !ids.iter().any(|i| x.starts_with(&format!("{i}:")))).cloned().collect();
            if kept != list {
                gsettings(&["set", "org.cinnamon", key, &gvariant(&kept)]);
            }
        }
    }
    for key in PANEL_ZONES {
        if let Some(items) = cinnamon_json(key) {
            let before = items.len();
            let kept: Vec<Value> = items.into_iter().filter(|i| !ids.iter().any(|id| i["panelId"] == json!(id))).collect();
            if kept.len() != before {
                set_cinnamon_json(key, kept);
            }
        }
    }
    // The applets' settings on those panels.
    for a in applets.iter().filter(|a| on_ours(a)) {
        let parts: Vec<&str> = a.split(':').collect();
        if let [_, _, _, uuid, instance] = parts[..] {
            let _ = std::fs::remove_file(spices().join(uuid).join(format!("{instance}.json")));
        }
    }
}

/// CVT reduced-blanking timings for `w`×`h` at 60 Hz: what a monitor of that
/// size would ask for.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Timing {
    w: u32,
    h: u32,
    /// Pixel clock in units of 10 kHz.
    clock: u32,
    h_blank: u32,
    h_front: u32,
    h_sync: u32,
    v_blank: u32,
    v_front: u32,
    v_sync: u32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn timing(w: u32, h: u32) -> Timing {
    let w = w / 8 * 8;
    let v_sync = match (w * 3 == h * 4, w * 9 == h * 16, w * 10 == h * 16, w * 4 == h * 5) {
        (true, ..) => 4,
        (_, true, ..) => 5,
        (_, _, true, _) => 6,
        (_, _, _, true) => 7,
        _ => 10,
    };
    let h_period = (1_000_000.0 / 60.0 - 460.0) / h as f64;
    let v_blank = ((460.0 / h_period).floor() as u32 + 1).max(3 + v_sync + 6);
    let mhz = ((60.0 * (w + 160) as f64 * (h + v_blank) as f64 / 1_000_000.0) / 0.25).floor() * 0.25;
    Timing { w, h, clock: (mhz * 100.0).round() as u32, h_blank: 160, h_front: 48, h_sync: 32, v_blank, v_front: 3, v_sync }
}

/// The detailed timing of one mode, as an EDID writes it (18 bytes).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dtd(t: &Timing) -> [u8; 18] {
    // About 96 dpi.
    let (mm_w, mm_h) = (t.w * 254 / 960, t.h * 254 / 960);
    let mut d = [0u8; 18];
    d[0..2].copy_from_slice(&(t.clock as u16).to_le_bytes());
    d[2] = (t.w & 0xff) as u8;
    d[3] = (t.h_blank & 0xff) as u8;
    d[4] = (((t.w >> 8) << 4) | (t.h_blank >> 8)) as u8;
    d[5] = (t.h & 0xff) as u8;
    d[6] = (t.v_blank & 0xff) as u8;
    d[7] = (((t.h >> 8) << 4) | (t.v_blank >> 8)) as u8;
    d[8] = (t.h_front & 0xff) as u8;
    d[9] = (t.h_sync & 0xff) as u8;
    d[10] = (((t.v_front & 15) << 4) | (t.v_sync & 15)) as u8;
    d[11] = (((t.h_front >> 8) << 6) | ((t.h_sync >> 8) << 4) | ((t.v_front >> 4) << 2) | (t.v_sync >> 4)) as u8;
    d[12] = (mm_w & 0xff) as u8;
    d[13] = (mm_h & 0xff) as u8;
    d[14] = (((mm_w >> 8) << 4) | (mm_h >> 8)) as u8;
    d[17] = 0x1A; // digital separate sync, +hsync -vsync
    d
}

/// Bigger modes a real monitor offers, beyond what the base block lists
/// (its standard timings stop at 2288 pixels wide).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LARGE_MODES: &[(u32, u32)] = &[(3840, 2160), (3200, 1800), (2560, 1600), (2560, 1440), (2048, 1152)];

/// A monitor's self-description for a `w`×`h` screen called `name`: EDID 1.3
/// with that size as its preferred mode, plus every common size (720p up to
/// 4K) so it can be switched in the machine's display settings like a real
/// one; the bigger ones in a CTA-861 extension block. 256 bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn edid(w: u32, h: u32, name: &str) -> Vec<u8> {
    let t = timing(w, h);
    let (mm_w, mm_h) = (t.w * 254 / 960, t.h * 254 / 960);
    let mut e = vec![0u8; 256];
    e[0..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
    e[8..10].copy_from_slice(&[0x36, 0xAE]); // "MXN"
    e[10..12].copy_from_slice(&1u16.to_le_bytes());
    e[12..16].copy_from_slice(&(now() as u32).to_le_bytes());
    e[16] = 1;
    e[17] = 36; // 2026
    e[18] = 1;
    e[19] = 3;
    e[20] = 0x80; // digital
    e[21] = (mm_w / 10).min(255) as u8;
    e[22] = (mm_h / 10).min(255) as u8;
    e[23] = 120; // gamma 2.2
    e[24] = 0x0A; // RGB, preferred timing first
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Established timings: 640×480, 800×600 and 1024×768 at 60 Hz.
    e[35] = 0x21;
    e[36] = 0x08;
    // Standard timings, at 60 Hz: (width / 8 - 31, aspect bits | refresh - 60).
    let standard: [(u32, u8); 8] = [(1280, 0xC0), (1280, 0x00), (1280, 0x80), (1440, 0x00), (1600, 0xC0), (1680, 0x00), (1920, 0xC0), (1920, 0x00)];
    for (i, (sw, aspect)) in standard.iter().enumerate() {
        e[38 + i * 2] = (sw / 8 - 31) as u8;
        e[39 + i * 2] = *aspect;
    }
    e[54..72].copy_from_slice(&dtd(&t));
    // Its name (up to 13 characters).
    let mut n: Vec<u8> = name.bytes().filter(|b| b.is_ascii_graphic() || *b == b' ').take(13).collect();
    if n.len() < 13 {
        n.push(b'\n');
    }
    n.resize(13, b' ');
    e[72..77].copy_from_slice(&[0, 0, 0, 0xFC, 0]);
    e[77..90].copy_from_slice(&n);
    // Range limits: 50-75 Hz, 30-160 kHz, up to 600 MHz.
    e[90..102].copy_from_slice(&[0, 0, 0, 0xFD, 0, 50, 75, 30, 160, 60, 0, 0x0A]);
    e[102..108].copy_from_slice(&[b' '; 6]);
    e[108..113].copy_from_slice(&[0, 0, 0, 0x10, 0]);
    e[126] = 1; // one extension
    e[127] = checksum(&e[..127]);
    // CTA-861 extension: no data blocks, the bigger modes as detailed timings.
    let x = &mut e[128..];
    x[0] = 0x02;
    x[1] = 0x03;
    x[2] = 4;
    let mut at = 4;
    for &(lw, lh) in LARGE_MODES.iter().filter(|m| **m != (t.w, t.h)) {
        if at + 18 > 127 {
            break;
        }
        x[at..at + 18].copy_from_slice(&dtd(&timing(lw, lh)));
        at += 18;
    }
    x[127] = checksum(&x[..127]);
    e
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn checksum(bytes: &[u8]) -> u8 {
    let sum: u32 = bytes.iter().map(|b| *b as u32).sum();
    ((256 - sum % 256) % 256) as u8
}

// ---------------------------------------------------------------------------
// Linux: a monitor's size and scale through the desktop's own display service
// (Mutter's DisplayConfig, in Cinnamon and GNOME), as its Display settings
// set them, so the desktop lays itself out for them; RandR (sizes only)
// elsewhere. Applied as temporary changes: a lasting one makes the desktop
// ask "Keep these settings?" on its own screen. It drops them when monitors
// come and go, so adding or removing one puts them back.

/// A GVariant in the text form `gdbus` prints: tuples and arrays are lists.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Gv {
    Str(String),
    Num(f64),
    Bool(bool),
    List(Vec<Gv>),
    Dict(Vec<(Gv, Gv)>),
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Gv {
    fn parse(text: &str) -> Option<Gv> {
        gv_value(text.as_bytes(), &mut 0)
    }

    fn at(&self, i: usize) -> Option<&Gv> {
        self.list().get(i)
    }

    fn list(&self) -> &[Gv] {
        match self {
            Gv::List(l) => l,
            _ => &[],
        }
    }

    fn str(&self) -> &str {
        match self {
            Gv::Str(s) => s,
            _ => "",
        }
    }

    fn num(&self) -> f64 {
        match self {
            Gv::Num(n) => *n,
            _ => 0.0,
        }
    }

    fn get(&self, key: &str) -> Option<&Gv> {
        match self {
            Gv::Dict(d) => d.iter().find(|(k, _)| k.str() == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gv_value(b: &[u8], at: &mut usize) -> Option<Gv> {
    let skip = |at: &mut usize| {
        while b.get(*at).is_some_and(u8::is_ascii_whitespace) {
            *at += 1;
        }
    };
    skip(at);
    let c = *b.get(*at)?;
    match c {
        b'(' | b'[' | b'{' => {
            let close = match c {
                b'(' => b')',
                b'[' => b']',
                _ => b'}',
            };
            *at += 1;
            let (mut items, mut pairs) = (Vec::new(), Vec::new());
            loop {
                skip(at);
                if *b.get(*at)? == close {
                    *at += 1;
                    break;
                }
                let v = gv_value(b, at)?;
                if c == b'{' {
                    skip(at);
                    if *b.get(*at)? != b':' {
                        return None;
                    }
                    *at += 1;
                    pairs.push((v, gv_value(b, at)?));
                } else {
                    items.push(v);
                }
                skip(at);
                match *b.get(*at)? {
                    b',' => *at += 1,
                    x if x == close => {}
                    _ => return None,
                }
            }
            Some(if c == b'{' { Gv::Dict(pairs) } else { Gv::List(items) })
        }
        b'<' => {
            *at += 1;
            let v = gv_value(b, at)?;
            skip(at);
            if *b.get(*at)? != b'>' {
                return None;
            }
            *at += 1;
            Some(v)
        }
        b'\'' | b'"' => {
            *at += 1;
            let mut text = Vec::new();
            loop {
                let ch = *b.get(*at)?;
                *at += 1;
                match ch {
                    b'\\' => {
                        text.push(*b.get(*at)?);
                        *at += 1;
                    }
                    _ if ch == c => break,
                    _ => text.push(ch),
                }
            }
            Some(Gv::Str(String::from_utf8_lossy(&text).into_owned()))
        }
        b'@' => {
            // A type ("@a{sv} {}"): the value follows.
            while b.get(*at).is_some_and(|x| !x.is_ascii_whitespace()) {
                *at += 1;
            }
            gv_value(b, at)
        }
        b'-' | b'+' | b'.' | b'0'..=b'9' => {
            let start = *at;
            while b.get(*at).is_some_and(|x| x.is_ascii_digit() || b".-+eE".contains(x)) {
                *at += 1;
            }
            std::str::from_utf8(&b[start..*at]).ok()?.parse().ok().map(Gv::Num)
        }
        _ => {
            let start = *at;
            while b.get(*at).is_some_and(|x| x.is_ascii_alphanumeric() || *x == b'_') {
                *at += 1;
            }
            match &b[start..*at] {
                b"true" => Some(Gv::Bool(true)),
                b"false" => Some(Gv::Bool(false)),
                b"" => None,
                // A type ("uint32 0"): the value follows.
                _ => gv_value(b, at),
            }
        }
    }
}

/// Where the display service is: Cinnamon's, else GNOME's.
#[cfg(target_os = "linux")]
const DISPLAY_SERVICES: [&str; 2] = ["org.cinnamon.Muffin.DisplayConfig", "org.gnome.Mutter.DisplayConfig"];

#[cfg(target_os = "linux")]
fn gdbus(service: &str, method: &str, args: &[&str]) -> Result<String, String> {
    let path = format!("/{}", service.replace('.', "/"));
    let mut cmd = Command::new("gdbus");
    cmd.args(["call", "--session", "--timeout", "10", "--dest", service, "--object-path", &path, "--method", &format!("{service}.{method}")]).args(args);
    session_bus(&mut cmd);
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("The desktop said no.").to_string())
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct Mode {
    id: String,
    w: u32,
    h: u32,
    hz: f64,
    scales: Vec<f64>,
    current: bool,
}

/// A monitor as the desktop lays it out: where, how scaled, which outputs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct Placed {
    x: i64,
    y: i64,
    scale: f64,
    transform: u64,
    primary: bool,
    outputs: Vec<String>,
}

/// The desktop's monitors, as its display service has them.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct Layout {
    service: &'static str,
    serial: u64,
    /// Each output (by connector name, "HDMI-A-0") and its modes.
    outputs: Vec<(String, Vec<Mode>)>,
    monitors: Vec<Placed>,
    /// Positions count real pixels (X11), else scaled ones.
    physical: bool,
    /// Every monitor has the same scale (X11).
    one_scale: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Layout {
    fn read(service: &'static str, text: &str) -> Option<Layout> {
        let state = Gv::parse(text.trim())?;
        let outputs = state
            .at(1)?
            .list()
            .iter()
            .map(|o| {
                let name = o.at(0).and_then(|i| i.at(0)).map(|c| c.str().to_string()).unwrap_or_default();
                let modes = o
                    .at(1)
                    .map(Gv::list)
                    .unwrap_or_default()
                    .iter()
                    .map(|m| Mode {
                        id: m.at(0).map(|v| v.str().to_string()).unwrap_or_default(),
                        w: m.at(1).map(Gv::num).unwrap_or(0.) as u32,
                        h: m.at(2).map(Gv::num).unwrap_or(0.) as u32,
                        hz: m.at(3).map(Gv::num).unwrap_or(0.),
                        scales: m.at(5).map(|s| s.list().iter().map(Gv::num).collect()).unwrap_or_default(),
                        current: m.at(6).and_then(|p| p.get("is-current")) == Some(&Gv::Bool(true)),
                    })
                    .collect();
                (name, modes)
            })
            .collect();
        let monitors = state
            .at(2)?
            .list()
            .iter()
            .map(|l| Placed {
                x: l.at(0).map(Gv::num).unwrap_or(0.) as i64,
                y: l.at(1).map(Gv::num).unwrap_or(0.) as i64,
                scale: l.at(2).map(Gv::num).unwrap_or(1.),
                transform: l.at(3).map(Gv::num).unwrap_or(0.) as u64,
                primary: l.at(4) == Some(&Gv::Bool(true)),
                outputs: l.at(5).map(Gv::list).unwrap_or_default().iter().filter_map(|o| o.at(0).map(|c| c.str().to_string())).collect(),
            })
            .collect();
        let props = state.at(3);
        Some(Layout {
            service,
            serial: state.at(0)?.num() as u64,
            outputs,
            monitors,
            physical: props.and_then(|p| p.get("layout-mode")).map(Gv::num) != Some(1.),
            one_scale: props.and_then(|p| p.get("global-scale-required")) == Some(&Gv::Bool(true)),
        })
    }

    fn modes(&self, output: &str) -> &[Mode] {
        self.outputs.iter().find(|(o, _)| o == output).map(|(_, m)| m.as_slice()).unwrap_or_default()
    }

    fn current(&self, output: &str) -> Option<&Mode> {
        self.modes(output).iter().find(|m| m.current)
    }

    /// The best mode of `output` at `w`×`h`: 60 Hz or the nearest under it.
    fn mode_at(&self, output: &str, (w, h): (u32, u32)) -> Option<&Mode> {
        let key = |m: &Mode| (m.hz <= 61., (m.hz * 1000.) as i64);
        self.modes(output).iter().filter(|m| (m.w, m.h) == (w, h)).max_by_key(|m| key(m))
    }

    fn placed(&self, output: &str) -> Option<usize> {
        self.monitors.iter().position(|p| p.outputs.iter().any(|o| o == output))
    }

    /// How wide a monitor is in layout units, at `size` (else its own).
    fn width(&self, p: &Placed, size: Option<(u32, u32)>) -> i64 {
        let Some((w, h)) = size.or_else(|| p.outputs.first().and_then(|o| self.current(o)).map(|m| (m.w, m.h))) else { return 0 };
        let w = if p.transform % 2 == 1 { h } else { w };
        if self.physical { w as i64 } else { (w as f64 / p.scale).round() as i64 }
    }

    /// Asks the desktop for `monitors`, each output at the size in `sizes`
    /// (else the one it has).
    #[cfg(target_os = "linux")]
    fn apply(&self, monitors: &[Placed], sizes: &[(String, (u32, u32))]) -> Result<(), String> {
        let mut parts = Vec::new();
        for p in monitors {
            let mut outputs = Vec::new();
            for o in &p.outputs {
                let mode = match sizes.iter().find(|(n, _)| n == o) {
                    Some((_, size)) => self.mode_at(o, *size),
                    None => self.current(o),
                }
                .ok_or("That size isn't one this monitor offers.")?;
                if !mode.scales.is_empty() && !mode.scales.iter().any(|s| (s - p.scale).abs() < 0.01) {
                    return Err(format!("This monitor can't be shown at {}% at that size.", (p.scale * 100.).round()));
                }
                outputs.push(format!("('{o}', '{}', @a{{sv}} {{}})", mode.id));
            }
            parts.push(format!("({}, {}, {:?}, {}, {}, [{}])", p.x, p.y, p.scale, p.transform, p.primary, outputs.join(", ")));
        }
        let config = format!("[{}]", parts.join(", "));
        gdbus(self.service, "ApplyMonitorsConfig", &[&self.serial.to_string(), "1", &config, "@a{sv} {}"]).map(|_| ())
    }
}

/// The desktop's monitors, when it has a display service.
#[cfg(target_os = "linux")]
fn layout() -> Option<Layout> {
    DISPLAY_SERVICES.iter().find_map(|service| Layout::read(service, &gdbus(service, "GetCurrentState", &[]).ok()?))
}

/// Puts the desktop back the way `before` had it after a monitor came or
/// went (it resets sizes and scale then): the ones still there as they were,
/// a new one (`added`) to their right, the ones right of a removed one moved
/// over.
#[cfg(target_os = "linux")]
fn restore(before: &Layout, added: Option<&str>, removed: Option<&str>) {
    let Some(now) = layout() else { return };
    let scale = before.monitors.iter().find(|p| p.primary).or(before.monitors.first()).map(|p| p.scale).unwrap_or(1.);
    let gap = removed.and_then(|r| before.placed(r)).map(|i| {
        let p = &before.monitors[i];
        (p.x + before.width(p, None), before.width(p, None))
    });
    let mut monitors = Vec::new();
    let mut sizes = Vec::new();
    for p in &now.monitors {
        let was = p.outputs.first().and_then(|o| before.placed(o)).map(|i| &before.monitors[i]);
        match was {
            Some(was) => {
                let mut q = was.clone();
                if let Some((edge, width)) = gap {
                    if q.x >= edge {
                        q.x -= width;
                    }
                }
                for o in &q.outputs {
                    if let Some(m) = before.current(o) {
                        sizes.push((o.clone(), (m.w, m.h)));
                    }
                }
                monitors.push(q);
            }
            None if added.is_some_and(|a| p.outputs.iter().any(|o| o == a)) => {
                let right = before.monitors.iter().map(|q| q.x + before.width(q, None)).max().unwrap_or(0);
                let mut q = Placed { x: right, y: 0, scale, transform: 0, primary: false, outputs: p.outputs.clone() };
                // A scale it can't do: as it came.
                if p.outputs.first().and_then(|o| now.current(o)).is_some_and(|m| !m.scales.is_empty() && !m.scales.iter().any(|s| (s - scale).abs() < 0.01)) {
                    q.scale = p.scale;
                }
                monitors.push(q);
            }
            None => monitors.push(p.clone()),
        }
    }
    if let Err(e) = now.apply(&monitors, &sizes) {
        eprintln!("displays: couldn't put the monitors back: {e}");
    }
}

/// `{"op": "display_set", "display", "width"?, "height"?, "scale"?}`: a
/// monitor's size, and how much the desktop scales things up on it (one
/// scale for them all on X11).
pub fn set(req: &Value) -> Value {
    let id = req["display"].as_str().unwrap_or("");
    let size = req["width"].as_u64().zip(req["height"].as_u64()).map(|(w, h)| (w as u32, h as u32));
    let scale = req["scale"].as_f64();
    if id.is_empty() || id == "all" || (size.is_none() && scale.is_none()) {
        return json!({ "ok": false, "error": "Which monitor, and what size or scale?" });
    }
    match set_display(id, size, scale) {
        Ok(()) => {
            if let Some((w, h)) = size {
                let mut reg = registry();
                if let Some(e) = reg.iter_mut().find(|e| e["id"] == id) {
                    e["width"] = json!(w);
                    e["height"] = json!(h);
                    save(&reg);
                }
            }
            json!({ "ok": true })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

#[cfg(target_os = "linux")]
fn set_display(id: &str, size: Option<(u32, u32)>, scale: Option<f64>) -> Result<(), String> {
    // A size the monitor doesn't list: give it one (X11 takes any).
    if let Some((w, h)) = size {
        let (outputs, modes) = parse_xrandr(&xrandr(&[])?);
        let listed = layout().map(|l| l.mode_at(id, (w, h)).is_some()).unwrap_or_else(|| outputs.iter().any(|o| o.name == id) && modes.iter().any(|m| m.starts_with(&format!("{w}x{h}"))));
        if !listed {
            add_mode(id, w, h)?;
        }
    }
    let Some(layout) = layout() else {
        if scale.is_some() {
            return Err("This desktop doesn't let Muxen change its scale.".into());
        }
        return xrandr_size(id, size.unwrap_or_default());
    };
    let i = layout.placed(id).ok_or("That monitor isn't on.")?;
    let mut monitors = layout.monitors.clone();
    let target = monitors[i].clone();
    let old = layout.width(&target, None);
    if let Some(s) = scale {
        for (j, p) in monitors.iter_mut().enumerate() {
            if j == i || layout.one_scale {
                // Scaled positions grow and shrink with the scale.
                if !layout.physical && layout.one_scale {
                    p.x = (p.x as f64 * p.scale / s).round() as i64;
                    p.y = (p.y as f64 * p.scale / s).round() as i64;
                }
                p.scale = s;
            }
        }
    }
    // The ones to its right move over when it gets wider or narrower.
    let new = layout.width(&monitors[i], size);
    if !(layout.one_scale && !layout.physical && scale.is_some()) {
        let edge = target.x + old;
        for p in monitors.iter_mut() {
            if p.x >= edge {
                p.x += new - old;
            }
        }
    }
    let sizes: Vec<(String, (u32, u32))> = size.map(|s| target.outputs.iter().map(|o| (o.clone(), s)).collect()).unwrap_or_default();
    layout.apply(&monitors, &sizes)
}

/// Gives an output a `w`×`h` mode at 60 Hz (CVT reduced blanking, like the
/// monitors Muxen adds), then waits for the desktop to see it.
#[cfg(target_os = "linux")]
fn add_mode(output: &str, w: u32, h: u32) -> Result<(), String> {
    let t = timing(w, h);
    let name = format!("{w}x{h}_muxen");
    let (hs, vs) = (t.w + t.h_front, t.h + t.v_front);
    let line = [
        format!("{:.2}", t.clock as f64 / 100.),
        t.w.to_string(),
        hs.to_string(),
        (hs + t.h_sync).to_string(),
        (t.w + t.h_blank).to_string(),
        t.h.to_string(),
        vs.to_string(),
        (vs + t.v_sync).to_string(),
        (t.h + t.v_blank).to_string(),
    ];
    // Already made for another output: only added to this one.
    let mut args = vec!["--newmode".to_string(), name.clone()];
    args.extend(line);
    args.extend(["+hsync".to_string(), "-vsync".to_string()]);
    let _ = xrandr(&args);
    xrandr(&["--addmode".to_string(), output.to_string(), name]).map_err(|_| format!("This monitor won't take {w}×{h}."))?;
    for _ in 0..10 {
        if layout().is_none_or(|l| l.mode_at(output, (w, h)).is_some()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(())
}

/// A monitor's size through RandR, the ones to its right moved over.
#[cfg(target_os = "linux")]
fn xrandr_size(id: &str, (w, h): (u32, u32)) -> Result<(), String> {
    let (outputs, _) = parse_xrandr(&xrandr(&[])?);
    let (x, _, old_w, _) = outputs.iter().find(|o| o.name == id).and_then(|o| o.geometry).ok_or("That monitor isn't on.")?;
    let (_, modes) = parse_xrandr(&xrandr(&[])?);
    let ours = format!("{w}x{h}_muxen");
    let mode = if modes.contains(&ours) && !modes.contains(&format!("{w}x{h}")) { ours } else { format!("{w}x{h}") };
    let mut args = vec!["--output".to_string(), id.to_string(), "--mode".into(), mode];
    for o in outputs.iter().filter(|o| o.name != id) {
        if let Some((ox, oy, ..)) = o.geometry.filter(|g| g.0 >= x + old_w) {
            args.extend(["--output".to_string(), o.name.clone(), "--pos".into(), format!("{}x{oy}", ox + w as i64 - old_w)]);
        }
    }
    xrandr(&args).map(|_| ())
}

#[cfg(target_os = "macos")]
fn set_display(id: &str, size: Option<(u32, u32)>, scale: Option<f64>) -> Result<(), String> {
    let Some((w, h)) = size else {
        return Err(if scale.is_some() { "On a Mac, pick a size: it's sharp when the monitor has a Retina version of it." } else { "What size?" }.into());
    };
    let reply = in_session(&["display-mode".into(), id.into(), w.to_string(), h.to_string()])?;
    match reply["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_display(_: &str, _: Option<(u32, u32)>, _: Option<f64>) -> Result<(), String> {
    Err("Not on this kind of machine yet.".into())
}

// ---------------------------------------------------------------------------
// A Mac: CoreGraphics displays; virtual ones held by `muxen-agent vdisplay`.

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn muxen_display_list(ids: *mut u32, max: usize) -> usize;
    fn muxen_display(id: u32, pixels_w: *mut usize, pixels_h: *mut usize, points_w: *mut f64, points_h: *mut f64, x: *mut f64, y: *mut f64);
    fn muxen_display_builtin(id: u32) -> i32;
    fn muxen_display_name(id: u32, buf: *mut u8, len: usize) -> usize;
    fn muxen_display_main() -> u32;
    fn muxen_display_modes(id: u32, sizes: *mut u32, max: usize) -> usize;
    fn muxen_display_set_mode(id: u32, w: u32, h: u32, err: *mut u8, errlen: usize) -> i32;
    fn muxen_vdisplay_create(w: u32, h: u32, name: *const std::ffi::c_char, err: *mut u8, errlen: usize) -> u32;
    fn muxen_vdisplay_run();
}

#[cfg(target_os = "macos")]
fn monitors() -> Result<Vec<Value>, String> {
    let mut ids = [0u32; 16];
    let n = unsafe { muxen_display_list(ids.as_mut_ptr(), ids.len()) };
    if n == 0 {
        return Err("No display here.".into());
    }
    let main = unsafe { muxen_display_main() };
    let mut external = 0;
    let own_name = |id: u32| {
        let mut buf = [0u8; 256];
        let n = unsafe { muxen_display_name(id, buf.as_mut_ptr(), buf.len()) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
    };
    Ok(ids[..n]
        .iter()
        .map(|&id| {
            let (mut pw, mut ph, mut tw, mut th, mut x, mut y) = (0usize, 0usize, 0f64, 0f64, 0f64, 0f64);
            unsafe { muxen_display(id, &mut pw, &mut ph, &mut tw, &mut th, &mut x, &mut y) };
            let name = match own_name(id) {
                Some(n) => n,
                None if unsafe { muxen_display_builtin(id) } == 1 => "Built-in display".to_string(),
                None => {
                    external += 1;
                    format!("Display {external}")
                }
            };
            let mut modes = [(0u32, 0u32); 64];
            let count = unsafe { muxen_display_modes(id, modes.as_mut_ptr() as *mut u32, modes.len()) };
            let mut sizes: Vec<(u32, u32)> = modes[..count].to_vec();
            sizes.sort_by_key(|(w, h)| std::cmp::Reverse(w * h));
            sizes.dedup();
            let scale = if tw > 0. { (pw as f64 / tw * 100.).round() / 100. } else { 1. };
            json!({ "id": id.to_string(), "name": name, "x": x, "y": y, "width": tw, "height": th, "pixel_width": pw, "pixel_height": ph, "main": id == main, "modes": sizes, "scale": scale })
        })
        .collect())
}

/// `muxen-agent display-mode ID W H`: switches a display to that size (in
/// points; a Retina version of it when there is one). Run in the session.
#[cfg(target_os = "macos")]
pub fn run_mode(args: &[String]) -> i32 {
    let num = |i: usize| args.get(i).and_then(|a| a.parse::<u32>().ok()).unwrap_or(0);
    let mut err = [0u8; 256];
    let ok = unsafe { muxen_display_set_mode(num(0), num(1), num(2), err.as_mut_ptr(), err.len()) } == 0;
    let reply = if ok {
        json!({ "ok": true })
    } else {
        let end = err.iter().position(|b| *b == 0).unwrap_or(err.len());
        json!({ "error": String::from_utf8_lossy(&err[..end]) })
    };
    println!("{reply}");
    0
}

/// Starts the process that holds the new display (in the desktop session) and
/// waits for its id.
#[cfg(target_os = "macos")]
fn add(w: u32, h: u32, label: &str, _password: Option<&str>) -> Result<Value, String> {
    let args = vec!["vdisplay".to_string(), w.to_string(), h.to_string(), label.to_string()];
    let line = if crate::gui::needed() {
        let mut stream = crate::gui::connect(&args)?;
        let mut text = String::new();
        stream.read_to_string(&mut text).map_err(|e| e.to_string())?;
        text
    } else {
        start_holder(&args)?
    };
    let reply: Value = serde_json::from_str(line.trim()).map_err(|_| "The virtual display didn't start.".to_string())?;
    match (reply["display"].as_u64(), reply["pid"].as_u64()) {
        (Some(id), Some(pid)) => Ok(json!({ "id": id.to_string(), "pid": pid })),
        _ => Err(reply["error"].as_str().unwrap_or("The virtual display didn't start.").to_string()),
    }
}

/// Runs `muxen-agent vdisplay ...` on its own (it outlives this process) and
/// returns its first line, with its pid added.
#[cfg(target_os = "macos")]
pub fn start_holder(args: &[String]) -> Result<String, String> {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    let stdout = child.stdout.take().ok_or("no output")?;
    let mut line = String::new();
    std::io::BufReader::new(stdout).read_line(&mut line).map_err(|e| e.to_string())?;
    let mut reply: Value = serde_json::from_str(line.trim()).unwrap_or_else(|_| json!({ "error": "The virtual display didn't start." }));
    reply["pid"] = json!(pid);
    // It keeps running; reap it later if it ends on its own.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(reply.to_string())
}

#[cfg(target_os = "macos")]
fn take_down(entry: &Value, _password: Option<&str>) -> Result<(), String> {
    let pid = entry["pid"].as_u64().ok_or("No process holds it.")? as i32;
    // Only our own holder: it was started with this registry entry.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    Ok(())
}

/// `muxen-agent vdisplay <width> <height> <label>`: makes a virtual display,
/// says its id on stdout, and holds it until the process ends.
#[cfg(target_os = "macos")]
pub fn run_holder(args: &[String]) -> i32 {
    use std::io::Write;
    let num = |i: usize, d: u32| args.get(i).and_then(|a| a.parse::<u32>().ok()).unwrap_or(d);
    let (w, h) = (num(0, 1920), num(1, 1080));
    let label = args.get(2).cloned().unwrap_or_else(|| "Virtual".into());
    // Named like a real one: it shows as this in System Settings › Displays.
    let name = std::ffi::CString::new(label).unwrap_or_default();
    let mut err = [0u8; 256];
    let id = unsafe { muxen_vdisplay_create(w, h, name.as_ptr(), err.as_mut_ptr(), err.len()) };
    let mut out = std::io::stdout();
    if id == 0 {
        let why = String::from_utf8_lossy(&err[..err.iter().position(|b| *b == 0).unwrap_or(err.len())]).into_owned();
        let _ = writeln!(out, "{}", json!({ "error": format!("Couldn't make a virtual display: {why}") }));
        return 1;
    }
    let _ = writeln!(out, "{}", json!({ "display": id }));
    let _ = out.flush();
    unsafe { muxen_vdisplay_run() };
    0
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn monitors() -> Result<Vec<Value>, String> {
    Err("Displays work on Linux and macOS so far.".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn add(_: u32, _: u32, _: &str, _: Option<&str>) -> Result<Value, String> {
    Err("Virtual monitors work on Linux and macOS so far.".into())
}

#[cfg(not(target_os = "linux"))]
fn place_panels(_: &mut [Value], _: &[Value]) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn drop_panel(_: &Value) {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn take_down(_: &Value, _: Option<&str>) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_display_service() {
        let text = "(uint32 58, [(('DisplayPort-0', 'MUN', 'Display 2', '0x6acbefea'), [('1920x1080@59.93', 1920, 1080, 59.93, 1.0, [1.0, 2.0, 3.0], {'is-current': <true>, 'is-preferred': <true>}), ('1280x720@60', 1280, 720, 60.0, 1.0, [1.0, 2.0], {})], {'is-builtin': <false>, 'display-name': <'MUN 23\\'s'>}), (('HDMI-A-0', 'UGR', 'UGREEN', '0x2'), [('3840x2160@60', 3840, 2160, 60.0, 1.0, [1.0, 2.0], {'is-current': <true>})], {})], [(3840, 0, 2.0, uint32 0, false, [('DisplayPort-0', 'MUN', 'Display 2', '0x6acbefea')], @a{sv} {}), (0, 0, 2.0, 0, true, [('HDMI-A-0', 'UGR', 'UGREEN', '0x2')], {})], {'layout-mode': <uint32 2>, 'global-scale-required': <true>})\n";
        let l = Layout::read("org.cinnamon.Muffin.DisplayConfig", text).expect("parses");
        assert_eq!(l.serial, 58);
        assert!(l.physical && l.one_scale);
        assert_eq!(l.current("DisplayPort-0").map(|m| (m.w, m.h, m.id.as_str())), Some((1920, 1080, "1920x1080@59.93")));
        assert_eq!(l.mode_at("DisplayPort-0", (1280, 720)).map(|m| m.id.as_str()), Some("1280x720@60"));
        assert_eq!(l.monitors[1].outputs, ["HDMI-A-0"]);
        assert!(l.monitors[1].primary);
        assert_eq!(l.width(&l.monitors[0], None), 1920);
        assert_eq!(l.width(&l.monitors[0], Some((3840, 2160))), 3840);
    }

    #[test]
    fn edits_gvariant_lists() {
        let panels = strings("['1:0:top', '2:0:bottom']");
        assert_eq!(panels, ["1:0:top", "2:0:bottom"]);
        assert_eq!(gvariant(&panels), "['1:0:top', '2:0:bottom']");
        assert!(strings("@as []").is_empty());
    }

    #[test]
    fn reads_xrandr() {
        let text = r"Screen 0: minimum 320 x 200, current 3840 x 2160, maximum 16384 x 16384
DisplayPort-0 disconnected (normal left inverted right x axis y axis)
HDMI-A-0 connected primary 3840x2160+0+0 (normal left inverted right x axis y axis) 800mm x 450mm
   1920x1080     60.00 + 120.00
   3840x2160     60.00*   50.00
DisplayPort-1 disconnected 1920x1080+3840+0 (normal left inverted right x axis y axis) 0mm x 0mm
";
        let (outputs, modes) = parse_xrandr(text);
        assert_eq!(outputs.len(), 3);
        assert!(!outputs[0].connected && outputs[0].geometry.is_none());
        assert_eq!(outputs[1].geometry, Some((0, 0, 3840, 2160)));
        assert_eq!(outputs[2].geometry, Some((3840, 0, 1920, 1080)));
        assert_eq!(modes, vec!["1920x1080", "3840x2160"]);
    }

    #[test]
    fn names_outputs() {
        assert_eq!(output_kind("HDMI-A-0"), "HDMI");
        assert_eq!(output_kind("DisplayPort-1"), "DisplayPort");
        assert_eq!(output_kind("DP-2"), "DisplayPort");
        assert_eq!(output_kind("eDP-1"), "Built-in display");
        assert_eq!(output_kind("weird-0"), "weird-0");
    }

    #[test]
    fn describes_a_monitor() {
        let e = edid(1920, 1080, "Display 2");
        assert_eq!(e.len(), 256);
        assert_eq!(&e[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
        assert_eq!(e[..128].iter().map(|b| *b as u32).sum::<u32>() % 256, 0, "base checksum");
        assert_eq!(e[128..].iter().map(|b| *b as u32).sum::<u32>() % 256, 0, "extension checksum");
        assert_eq!((e[126], e[128]), (1, 0x02), "a CTA extension follows");
        // Every detailed timing: the size asked for first, then the bigger ones.
        let size = |d: &[u8]| ((d[2] as u32) | ((d[4] as u32 >> 4) << 8), (d[5] as u32) | ((d[7] as u32 >> 4) << 8));
        let mut sizes = vec![size(&e[54..72])];
        sizes.extend(e[132..].chunks(18).take_while(|d| d.len() == 18 && d[0..2] != [0, 0]).map(size));
        assert_eq!(sizes, [(1920, 1080), (3840, 2160), (3200, 1800), (2560, 1600), (2560, 1440), (2048, 1152)]);
        // A 4K one doesn't list itself twice.
        let k = edid(3840, 2160, "4K");
        assert_eq!(size(&k[54..72]), (3840, 2160));
        assert_eq!(size(&k[132..150]), (3200, 1800));
        // The detailed timing: 138.50 MHz, 1920 × 1080.
        assert_eq!(u16::from_le_bytes([e[54], e[55]]), 13850);
        assert_eq!((e[56] as u32) | ((e[58] as u32 >> 4) << 8), 1920);
        assert_eq!((e[59] as u32) | ((e[61] as u32 >> 4) << 8), 1080);
        assert_eq!(&e[77..87], b"Display 2\n");
        // 4K still fits the format.
        let k = edid(3840, 2160, "A very long monitor name");
        assert_eq!(k[..128].iter().map(|b| *b as u32).sum::<u32>() % 256, 0);
        assert_eq!(k[128..].iter().map(|b| *b as u32).sum::<u32>() % 256, 0);
        assert_eq!(&k[77..90], b"A very long m");
    }

    #[test]
    fn times_like_cvt() {
        // cvt -r 1920 1080: 138.50 1920 1968 2000 2080 1080 1083 1088 1111 +hsync -vsync
        let t = timing(1920, 1080);
        assert_eq!(t.clock, 13850);
        assert_eq!((t.w + t.h_front, t.w + t.h_front + t.h_sync, t.w + t.h_blank), (1968, 2000, 2080));
        assert_eq!((t.h + t.v_front, t.h + t.v_front + t.v_sync, t.h + t.v_blank), (1083, 1088, 1111));
    }
}
