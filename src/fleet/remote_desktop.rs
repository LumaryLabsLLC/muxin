//! A machine's own remote desktop (RDP or VNC), in this computer's app for
//! it: the machine's port opened here over its link (see ports.rs, so it
//! works away from home and needs nothing opened in a firewall), then
//! Windows App or Remote Desktop Connection, Screen Sharing, Remmina or
//! FreeRDP started on it. It shows the sign-in screen too, so it works when
//! nobody is signed in there and Muxin's own desktop view has no screen.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Rdp,
    Vnc,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Rdp => "Remote Desktop",
            Protocol::Vnc => "VNC",
        }
    }

    /// Where it opens here: not its usual port, which Remote Desktop
    /// Connection and Screen Sharing keep for this computer itself.
    fn local_port(self) -> u16 {
        match self {
            Protocol::Rdp => 3390,
            Protocol::Vnc => 5901,
        }
    }
}

const RDP_PORT: u16 = 3389;
/// VNC servers listen on 5900 + their display number.
const VNC_PORTS: std::ops::RangeInclusive<u16> = 5900..=5909;

/// A machine's remote desktop, open here.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteDesktop {
    pub machine_id: String,
    pub protocol: Protocol,
    /// Where it is here: "localhost:3390".
    pub address: String,
    /// The app it opened in; None when this computer has none for it.
    pub app: Option<String>,
    /// The port there, for closing it (`Engine::stop_forward`).
    pub port: u16,
}

impl RemoteDesktop {
    /// What happened, in a sentence.
    pub fn summary(&self, machine: &str) -> String {
        match &self.app {
            Some(app) => format!("Opened {machine} in {app}, at {} here.", self.address),
            None => format!("{machine}'s {} is at {} here. Open it in your {} app.", self.protocol.name(), self.address, self.protocol.name()),
        }
    }
}

/// The system a machine runs, from its platform ("windows-x64").
#[derive(Debug, Clone, Copy, PartialEq)]
enum System {
    Windows,
    Mac,
    Linux,
}

impl System {
    fn of(platform: &str) -> System {
        if platform.starts_with("windows") {
            System::Windows
        } else if platform.starts_with("darwin") {
            System::Mac
        } else {
            System::Linux
        }
    }
}

/// "Open with Remote Desktop", "Open with Screen Sharing": the menu item and button.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn open_label(platform: &str) -> &'static str {
    match System::of(platform) {
        System::Windows => "Open with Remote Desktop",
        System::Mac => "Open with Screen Sharing",
        System::Linux => "Open with RDP or VNC",
    }
}

/// Which protocol and port to use, from what listens there: the one asked
/// for, else the system's own, else the other one. Err says how to turn one on.
fn choose(system: System, name: &str, wanted: Option<Protocol>, listening: &[u16]) -> Result<(Protocol, u16), String> {
    let rdp = listening.contains(&RDP_PORT).then_some((Protocol::Rdp, RDP_PORT));
    let vnc = VNC_PORTS.clone().find(|p| listening.contains(p)).map(|p| (Protocol::Vnc, p));
    let found = match (wanted, system) {
        (Some(Protocol::Rdp), _) => rdp,
        (Some(Protocol::Vnc), _) => vnc,
        (None, System::Mac) => vnc.or(rdp),
        (None, _) => rdp.or(vnc),
    };
    found.ok_or_else(|| match (wanted, system) {
        (Some(Protocol::Rdp), System::Mac) => format!("{name} is a Mac: it takes VNC (Screen Sharing), not Remote Desktop."),
        (Some(Protocol::Vnc), System::Windows) => format!("No VNC server runs on {name}. Windows has none built in: install one such as TightVNC, or use Remote Desktop."),
        (_, System::Windows) => format!("Remote Desktop is off on {name}. Turn it on there in Settings › System › Remote Desktop (on Windows Server: Server Manager › Local Server). Windows Home can't be reached this way."),
        (_, System::Mac) => format!("Screen Sharing is off on {name}. Turn it on there in System Settings › General › Sharing."),
        (Some(Protocol::Rdp), System::Linux) => format!("Nothing on {name} takes Remote Desktop. Install xrdp there, or turn on GNOME's Remote Desktop."),
        (Some(Protocol::Vnc), System::Linux) => format!("No VNC server runs on {name}. Start one there, such as x11vnc or TigerVNC."),
        (None, System::Linux) => format!("Nothing on {name} takes Remote Desktop or VNC. Install xrdp, turn on GNOME's Remote Desktop, or run a VNC server such as x11vnc."),
    })
}

/// The account to sign in as: the one Muxin connects as (the ssh user, as
/// the machine's settings or the ssh config say).
async fn user_of(spec: &crate::config::MachineSpec) -> Option<String> {
    if let Some(u) = spec.user.clone().or_else(|| spec.host.split_once('@').map(|(u, _)| u.to_string())).filter(|u| !u.is_empty()) {
        return Some(u);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::process::Command::new("ssh").arg("-G").args(crate::probe::ssh_args(spec)).output()).await.ok()?.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("user ").map(|u| u.trim().to_string())).filter(|u| !u.is_empty())
}

/// A Remote Desktop file for `address`, as Windows App and Remote Desktop Connection read it.
fn rdp_file(address: &str, user: Option<&str>) -> String {
    let mut out = format!("full address:s:{address}\r\n");
    if let Some(u) = user {
        out.push_str(&format!("username:s:{u}\r\n"));
    }
    out
}

/// Writes the Remote Desktop file for a machine, in the temp folder.
fn write_rdp_file(machine_id: &str, address: &str, user: Option<&str>) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("muxin-{machine_id}.rdp"));
    std::fs::write(&path, rdp_file(address, user)).ok()?;
    Some(path)
}

/// The first of `programs` this computer has, on PATH.
fn on_path<'a>(programs: &[&'a str]) -> Option<(&'a str, PathBuf)> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    programs.iter().find_map(|p| dirs.iter().map(|d| d.join(p)).find(|f| f.is_file()).map(|f| (*p, f)))
}

/// Starts this computer's app for it on `address`; the app's name, or None
/// when there's none here.
fn launch(protocol: Protocol, machine_id: &str, port: u16, user: Option<&str>) -> Option<String> {
    let address = format!("localhost:{port}");
    let spawn = |cmd: &mut Command| cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok();
    if cfg!(target_os = "macos") {
        let opened = |what: &str| Command::new("open").arg(what).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        return match protocol {
            Protocol::Rdp => {
                let file = write_rdp_file(machine_id, &address, user)?;
                opened(&file.display().to_string()).then(|| {
                    let home = dirs::home_dir().unwrap_or_default();
                    ["Windows App", "Microsoft Remote Desktop"]
                        .into_iter()
                        .find(|a| [PathBuf::from("/Applications"), home.join("Applications")].iter().any(|d| d.join(format!("{a}.app")).exists()))
                        .unwrap_or("your Remote Desktop app")
                        .to_string()
                })
            }
            // Screen Sharing, built in.
            Protocol::Vnc => {
                let url = match user {
                    Some(u) => format!("vnc://{u}@{address}"),
                    None => format!("vnc://{address}"),
                };
                opened(&url).then(|| "Screen Sharing".to_string())
            }
        };
    }
    if cfg!(windows) {
        return match protocol {
            // Remote Desktop Connection, built in.
            Protocol::Rdp => {
                let file = write_rdp_file(machine_id, &address, user)?;
                spawn(Command::new("mstsc.exe").arg(&file)).then(|| "Remote Desktop Connection".to_string())
            }
            // A VNC viewer that took vnc:// links, else TigerVNC's or TightVNC's on PATH.
            Protocol::Vnc => {
                let registered = Command::new("reg").args(["query", r"HKCR\vnc\shell\open\command"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
                if registered && spawn(Command::new("rundll32.exe").args(["url.dll,FileProtocolHandler", &format!("vnc://{address}")])) {
                    return Some("your VNC viewer".into());
                }
                let (name, path) = on_path(&["vncviewer.exe", "tvnviewer.exe"])?;
                spawn(Command::new(path).arg(format!("localhost::{port}"))).then(|| name.trim_end_matches(".exe").to_string())
            }
        };
    }
    let who = user.map(|u| format!("{u}@")).unwrap_or_default();
    let candidates: Vec<(&str, Vec<String>)> = match protocol {
        Protocol::Rdp => {
            let mut freerdp = vec![format!("/v:{address}")];
            freerdp.extend(user.map(|u| format!("/u:{u}")));
            vec![("remmina", vec!["-c".into(), format!("rdp://{who}{address}")]), ("xfreerdp3", freerdp.clone()), ("sdl-freerdp3", freerdp.clone()), ("xfreerdp", freerdp)]
        }
        Protocol::Vnc => vec![("remmina", vec!["-c".into(), format!("vnc://{who}{address}")]), ("vncviewer", vec![format!("localhost::{port}")])],
    };
    candidates.into_iter().find_map(|(name, args)| {
        let (_, path) = on_path(&[name])?;
        spawn(Command::new(path).args(&args)).then(|| name.to_string())
    })
}

impl Engine {
    /// Opens a machine's own remote desktop in this computer's app for it:
    /// `protocol`, or the one it has (RDP first, VNC on a Mac). Its port
    /// stays open here (Servers lists it) until closed or Muxin quits.
    pub async fn open_remote_desktop(self: &Arc<Self>, machine_id: &str, protocol: Option<Protocol>) -> Result<RemoteDesktop, String> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if spec.is_local() {
            return Err(format!("{} is this computer.", spec.name));
        }
        spec.refuse_if_disconnected()?;
        let platform = self.current().machines.iter().find(|m| m.id == spec.id).map(|m| m.platform.clone()).unwrap_or_default();
        let listening = self.listening(&spec.id).await?;
        let (protocol, port) = choose(System::of(&platform), &spec.name, protocol, &listening)?;
        let forward = self.forward(&spec.id, port, None, Some(protocol.local_port())).await?;
        let user = user_of(&spec).await;
        let (id, local) = (spec.id.clone(), forward.local_port);
        let app = tokio::task::spawn_blocking(move || launch(protocol, &id, local, user.as_deref())).await.unwrap_or(None);
        Ok(RemoteDesktop { machine_id: spec.id, protocol, address: format!("localhost:{local}"), app, port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_what_listens() {
        assert_eq!(choose(System::Windows, "srv", None, &[22, 3389, 5985]), Ok((Protocol::Rdp, 3389)));
        assert_eq!(choose(System::Mac, "mini", None, &[22, 5900]), Ok((Protocol::Vnc, 5900)));
        // Linux: RDP when both, else the VNC display that's up.
        assert_eq!(choose(System::Linux, "box", None, &[3389, 5900]), Ok((Protocol::Rdp, 3389)));
        assert_eq!(choose(System::Linux, "box", None, &[5901]), Ok((Protocol::Vnc, 5901)));
        // Windows with a VNC server and Remote Desktop off.
        assert_eq!(choose(System::Windows, "srv", None, &[5900]), Ok((Protocol::Vnc, 5900)));
        assert_eq!(choose(System::Windows, "srv", Some(Protocol::Vnc), &[3389, 5900]), Ok((Protocol::Vnc, 5900)));
    }

    #[test]
    fn says_how_to_turn_it_on() {
        assert!(choose(System::Windows, "srv", None, &[22]).unwrap_err().starts_with("Remote Desktop is off on srv."));
        assert!(choose(System::Mac, "mini", None, &[22]).unwrap_err().starts_with("Screen Sharing is off on mini."));
        assert!(choose(System::Mac, "mini", Some(Protocol::Rdp), &[5900]).unwrap_err().contains("is a Mac"));
        assert!(choose(System::Linux, "box", None, &[]).unwrap_err().contains("xrdp"));
    }

    #[test]
    fn writes_rdp_files() {
        assert_eq!(rdp_file("localhost:3390", Some("adib")), "full address:s:localhost:3390\r\nusername:s:adib\r\n");
        assert_eq!(rdp_file("localhost:3391", None), "full address:s:localhost:3391\r\n");
    }

    #[test]
    fn labels() {
        assert_eq!(open_label("windows-x64"), "Open with Remote Desktop");
        assert_eq!(open_label("darwin-arm64"), "Open with Screen Sharing");
    }
}
