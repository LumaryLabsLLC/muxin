//! Machines' desktops for AI agents: a screenshot, and mouse and keyboard
//! input, over the machine's link (the app's live view is in ui/desktop.rs).
//! X11 machines for now; see agent/src/desktop.rs.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::link::{self, Link};
use super::Engine;

/// Desktop frames and input on a link channel.
pub const FRAME: u8 = 20;
pub const INPUT: u8 = 21;
/// Only what changed, as JPEG patches (the live view).
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub const TILES: u8 = 23;

#[derive(Debug, Clone, Serialize)]
pub struct Screenshot {
    #[serde(skip)]
    pub jpeg: Vec<u8>,
    /// The screenshot's size: what input coordinates refer to.
    pub width: u32,
    pub height: u32,
    /// The screen's real size (big screens are scaled down).
    pub screen_width: u32,
    pub screen_height: u32,
}

/// Something to do on a desktop. Coordinates are screenshot pixels.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum DesktopEvent {
    Move { x: f64, y: f64 },
    Click {
        x: f64,
        y: f64,
        /// "left" (default), "right" or "middle".
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        double: bool,
    },
    /// Positive `dy` scrolls down.
    Scroll { x: f64, y: f64, dy: i64 },
    /// One key or a combination: "enter", "ctrl+c", "ctrl+shift+t", "alt+tab".
    Key { key: String },
    Text { text: String },
    /// A pause between steps, in milliseconds (at most 5000).
    Wait { ms: u64 },
}

/// An X keysym for a key name ("a", "enter", "f5", ...).
pub fn keysym(key: &str) -> Option<u32> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        // Printable Latin-1 keysyms are their code points.
        return ((c as u32) >= 0x20 && (c as u32) <= 0xff).then_some(c.to_ascii_lowercase() as u32);
    }
    Some(match key.to_ascii_lowercase().as_str() {
        "space" => 0x20,
        "enter" | "return" => 0xff0d,
        "backspace" => 0xff08,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "shift" => 0xffe1,
        "ctrl" | "control" => 0xffe3,
        "alt" | "option" => 0xffe9,
        "super" | "meta" | "cmd" | "win" => 0xffeb,
        f if f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => 0xffbe + f[1..].parse::<u32>().ok()? - 1,
        _ => return None,
    })
}

/// A desktop stream opened on a link channel; closed when dropped.
struct Opened {
    link: Arc<Link>,
    channel: u32,
    frames: mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
    info: Value,
}

impl Opened {
    fn input(&self, event: Value) {
        self.link.send(self.channel, INPUT, &serde_json::to_vec(&event).unwrap_or_default());
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.link.close_channel(self.channel);
    }
}

impl Engine {
    async fn open_desktop(self: &Arc<Self>, machine_id: &str, fps: u32) -> Result<Opened, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Err("The demo has no desktops to show.".into());
        }
        let link = link::get(&spec).await.ok_or_else(|| format!("Can't reach the agent on {}.", spec.name))?;
        let (channel, frames) = link.open_channel();
        let reply = link.request(json!({ "op": "desktop", "channel": channel, "fps": fps, "quality": 75, "max_width": 1920 }), Duration::from_secs(20)).await;
        match reply {
            Ok(r) if r["ok"].as_bool() == Some(true) => Ok(Opened { link, channel, frames, info: r }),
            Ok(r) => {
                link.close_channel(channel);
                Err(r["error"].as_str().unwrap_or("The machine can't show its desktop.").to_string())
            }
            Err(e) => {
                link.close_channel(channel);
                Err(e)
            }
        }
    }

    /// What's on the machine's screen right now, as a JPEG.
    pub async fn desktop_screenshot(self: &Arc<Self>, machine_id: &str) -> Result<Screenshot, String> {
        let mut d = self.open_desktop(machine_id, 2).await?;
        let size = |k: &str| d.info[k].as_u64().unwrap_or(0) as u32;
        let (width, height, screen_width, screen_height) = (size("width"), size("height"), size("screen_width"), size("screen_height"));
        let first = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some((kind, bytes)) = d.frames.recv().await {
                if kind == FRAME && bytes.len() > 4 {
                    return Some(bytes[4..].to_vec());
                }
            }
            None
        })
        .await;
        match first {
            Ok(Some(jpeg)) => Ok(Screenshot { jpeg, width, height, screen_width, screen_height }),
            Ok(None) => Err("The screen capture stopped before sending a picture.".into()),
            Err(_) => Err("No picture of the screen within 15s.".into()),
        }
    }

    /// Moves, clicks, scrolls and types on the machine's desktop, in order.
    pub async fn desktop_input(self: &Arc<Self>, machine_id: &str, events: &[DesktopEvent]) -> Result<usize, String> {
        if events.is_empty() {
            return Err("Give at least one event.".into());
        }
        let d = self.open_desktop(machine_id, 1).await?;
        let pause = || tokio::time::sleep(Duration::from_millis(40));
        for e in events {
            match e {
                DesktopEvent::Move { x, y } => d.input(json!({ "t": "move", "x": x, "y": y })),
                DesktopEvent::Click { x, y, button, double } => {
                    let b = match button.as_deref().unwrap_or("left") {
                        "right" => 3,
                        "middle" => 2,
                        _ => 1,
                    };
                    for _ in 0..if *double { 2 } else { 1 } {
                        d.input(json!({ "t": "button", "b": b, "down": true, "x": x, "y": y }));
                        d.input(json!({ "t": "button", "b": b, "down": false, "x": x, "y": y }));
                    }
                }
                DesktopEvent::Scroll { x, y, dy } => d.input(json!({ "t": "scroll", "dy": dy, "dx": 0, "x": x, "y": y })),
                DesktopEvent::Key { key } => {
                    let syms: Vec<u32> = key.split('+').map(|k| keysym(k.trim()).ok_or_else(|| format!("Unknown key \"{}\".", k.trim()))).collect::<Result<_, _>>()?;
                    for s in &syms {
                        d.input(json!({ "t": "key", "keysym": s, "down": true }));
                    }
                    for s in syms.iter().rev() {
                        d.input(json!({ "t": "key", "keysym": s, "down": false }));
                    }
                }
                DesktopEvent::Text { text } => d.input(json!({ "t": "text", "s": text })),
                DesktopEvent::Wait { ms } => tokio::time::sleep(Duration::from_millis((*ms).min(5000))).await,
            }
            pause().await;
        }
        // Let the last of it land before the stream closes.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(d);
        Ok(events.len())
    }
}
