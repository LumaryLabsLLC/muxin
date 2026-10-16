//! Whether a newer Muxin is out: GitHub's latest release of
//! LumaryLabsLLC/muxin against this build's version, and how this copy is
//! best updated: Homebrew or the AUR update it, else the release's file for
//! this computer (the DMG, the Windows installer, the .deb or .rpm, the
//! AppImage or the tarball). Only GitHub is asked, and nothing about the user
//! goes with it.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

const LATEST: &str = "https://api.github.com/repos/LumaryLabsLLC/muxin/releases/latest";

/// A newer version, and how to get it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Update {
    /// "0.5.3".
    pub version: String,
    /// Its release page: what's new, and every file.
    pub page: String,
    /// The file for this computer, when one fits and no package manager owns this copy.
    pub download: Option<String>,
    /// The command that updates this copy, when a package manager owns it.
    pub command: Option<String>,
}

/// Asks GitHub. None when this is the latest (or newer: a build from source).
pub async fn check() -> Result<Option<Update>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("muxin/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let reply = client.get(LATEST).header("Accept", "application/vnd.github+json").send().await.map_err(|e| format!("Couldn't reach GitHub: {e}"))?;
    if !reply.status().is_success() {
        return Err(format!("GitHub answered {}.", reply.status()));
    }
    let release: serde_json::Value = reply.json().await.map_err(|e| format!("GitHub's answer didn't make sense: {e}"))?;
    Ok(read(&release, env!("CARGO_PKG_VERSION"), &how_installed()))
}

/// The newer version in GitHub's answer, if it's newer than `current`.
fn read(release: &serde_json::Value, current: &str, how: &Installed) -> Option<Update> {
    let version = release["tag_name"].as_str()?.trim_start_matches('v').to_string();
    if !newer(&version, current) {
        return None;
    }
    let assets: Vec<(&str, &str)> = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?, a["browser_download_url"].as_str()?)))
        .collect();
    let file = |ending: &str| assets.iter().find(|(name, _)| name.ends_with(ending)).map(|(_, url)| url.to_string());
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let (download, command) = match how {
        Installed::Homebrew => (None, Some("brew upgrade --cask muxin".to_string())),
        Installed::Aur => (None, Some("yay -S muxin-bin".to_string())),
        Installed::Package(ending) => (file(&format!("-linux-{arch}.{ending}")), None),
        Installed::AppImage => (file(&format!("-linux-{arch}.AppImage")), None),
        Installed::Tarball => (file(&format!("-linux-{arch}.tar.gz")), None),
        Installed::App if cfg!(target_os = "macos") => (file("-macos.dmg"), None),
        Installed::App if cfg!(windows) => (file(&format!("-windows-{}-setup.exe", if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" })), None),
        Installed::App => (None, None),
    };
    Some(Update { version, page: release["html_url"].as_str().unwrap_or("https://github.com/LumaryLabsLLC/muxin/releases/latest").to_string(), download, command })
}

/// `a` comes after `b`: their numbers compared in order ("0.10.0" after
/// "0.9.2"); a pre-release ("0.6.0-rc.1") counts as its numbers.
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// How this copy got here, which decides how it's updated.
#[derive(Debug, PartialEq)]
enum Installed {
    Homebrew,
    /// The AUR's muxin-bin.
    Aur,
    /// The release's Linux package: "deb", "rpm" or "pkg.tar.zst".
    Package(&'static str),
    AppImage,
    /// The tarball's install.sh (~/.local/share/muxin-app).
    Tarball,
    /// The macOS app or the Windows installer (or a build of its own).
    App,
}

fn how_installed() -> Installed {
    if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_some() {
        return Installed::AppImage;
    }
    let exe = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).unwrap_or_default();
    let homebrew = ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"].iter().any(|p| Path::new(p).join("Caskroom/muxin").is_dir());
    if exe.to_string_lossy().contains("/Caskroom/muxin/") || (cfg!(target_os = "macos") && homebrew && exe.starts_with("/Applications/Muxin.app")) {
        return Installed::Homebrew;
    }
    if !cfg!(target_os = "linux") {
        return Installed::App;
    }
    if exe.starts_with("/usr/lib/muxin") {
        return linux_package(Path::new("/"));
    }
    if exe.to_string_lossy().contains("/.local/share/muxin-app/") {
        return Installed::Tarball;
    }
    Installed::App
}

/// Which Linux package put the app in /usr/lib/muxin: the AUR's, or the
/// release's for this system (by its package database, then /etc/os-release).
fn linux_package(root: &Path) -> Installed {
    let pacman = root.join("var/lib/pacman/local");
    if let Ok(dir) = std::fs::read_dir(&pacman) {
        let names: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        if names.iter().any(|n| n.starts_with("muxin-bin-")) {
            return Installed::Aur;
        }
        return Installed::Package("pkg.tar.zst");
    }
    let release = std::fs::read_to_string(root.join("etc/os-release")).unwrap_or_default();
    let ids: String = release.lines().filter(|l| l.starts_with("ID=") || l.starts_with("ID_LIKE=")).collect::<Vec<_>>().join(" ").to_lowercase();
    if ["fedora", "rhel", "suse", "centos"].iter().any(|d| ids.contains(d)) || root.join("var/lib/rpm").is_dir() && !root.join("var/lib/dpkg").is_dir() {
        Installed::Package("rpm")
    } else {
        Installed::Package("deb")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_versions() {
        assert!(newer("0.5.3", "0.5.2") && newer("0.10.0", "0.9.9") && newer("1.0", "0.99.99"));
        assert!(!newer("0.5.2", "0.5.2") && !newer("0.5.1", "0.5.2") && !newer("0.6.0-rc.1", "0.6.0"));
    }

    #[test]
    fn picks_how_to_update() {
        let release = json!({
            "tag_name": "v0.5.3", "html_url": "https://github.com/LumaryLabsLLC/muxin/releases/tag/v0.5.3",
            "assets": [
                { "name": "Muxin-0.5.3-macos.dmg", "browser_download_url": "https://x/Muxin-0.5.3-macos.dmg" },
                { "name": "Muxin-0.5.3-linux-x86_64.deb", "browser_download_url": "https://x/Muxin-0.5.3-linux-x86_64.deb" },
                { "name": "Muxin-0.5.3-linux-aarch64.deb", "browser_download_url": "https://x/Muxin-0.5.3-linux-aarch64.deb" },
            ],
        });
        assert_eq!(read(&release, "0.5.3", &Installed::App), None, "the same version");
        let brew = read(&release, "0.5.2", &Installed::Homebrew).unwrap();
        assert_eq!((brew.version.as_str(), brew.command.as_deref(), brew.download), ("0.5.3", Some("brew upgrade --cask muxin"), None));
        let deb = read(&release, "0.5.2", &Installed::Package("deb")).unwrap();
        let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
        assert_eq!(deb.download.as_deref(), Some(format!("https://x/Muxin-0.5.3-linux-{arch}.deb").as_str()));
        if cfg!(target_os = "macos") {
            assert_eq!(read(&release, "0.5.2", &Installed::App).unwrap().download.as_deref(), Some("https://x/Muxin-0.5.3-macos.dmg"));
        }
    }

    #[test]
    fn knows_the_linux_package() {
        let root = std::env::temp_dir().join(format!("muxin-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/os-release"), "ID=ubuntu\nID_LIKE=debian\n").unwrap();
        assert_eq!(linux_package(&root), Installed::Package("deb"));
        std::fs::write(root.join("etc/os-release"), "ID=fedora\n").unwrap();
        assert_eq!(linux_package(&root), Installed::Package("rpm"));
        std::fs::write(root.join("etc/os-release"), "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n").unwrap();
        assert_eq!(linux_package(&root), Installed::Package("rpm"));
        std::fs::create_dir_all(root.join("var/lib/pacman/local/muxin-0.5.2-1")).unwrap();
        assert_eq!(linux_package(&root), Installed::Package("pkg.tar.zst"));
        std::fs::create_dir_all(root.join("var/lib/pacman/local/muxin-bin-0.5.2-1")).unwrap();
        assert_eq!(linux_package(&root), Installed::Aur);
        let _ = std::fs::remove_dir_all(&root);
    }
}
