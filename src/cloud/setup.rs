//! What a new cloud machine runs on its first boot (cloud-init `user_data`):
//! the user's account with sudo and their SSH key, git, then each extra the
//! user picked. Progress goes to /var/log/muxen-setup.log as "muxen-step: ..."
//! lines, and /var/lib/muxen/ready marks the end. A failing extra doesn't stop
//! the rest.

pub const READY: &str = "/var/lib/muxen/ready";
pub const LOG: &str = "/var/log/muxen-setup.log";

/// What a new machine can come with: (id, name).
pub const EXTRAS: &[(&str, &str)] = &[
    ("docker", "Docker"),
    ("gh", "GitHub CLI"),
    ("node", "Node.js"),
    ("python", "Python"),
    ("go", "Go"),
    ("rust", "Rust"),
    ("php", "PHP"),
    ("ruby", "Ruby"),
    ("java", "Java"),
    ("bun", "Bun"),
    ("deno", "Deno"),
    ("dotnet", ".NET"),
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    ("gemini", "Gemini CLI"),
    ("opencode", "OpenCode"),
    ("amp", "Amp"),
    ("pi", "Pi"),
    ("tailscale", "Tailscale"),
];

/// The extras in groups, for choosing them: (group, ids).
pub const GROUPS: &[(&str, &[&str])] = &[
    ("Tools", &["docker", "gh"]),
    ("Languages", &["node", "python", "go", "rust", "php", "ruby", "java", "bun", "deno", "dotnet"]),
    ("Agent CLIs", &["claude", "codex", "gemini", "opencode", "amp", "pi"]),
    ("Network", &["tailscale"]),
];

/// An extra's name, by id.
pub fn extra_name(id: &str) -> &'static str {
    EXTRAS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n).unwrap_or("")
}

/// The extras new machines come with unless the user picks otherwise.
pub fn default_extras() -> Vec<String> {
    ["docker", "gh", "node", "claude", "codex", "tailscale"].map(String::from).to_vec()
}

/// npm packages of the agent CLIs (they need Node.js).
pub(crate) fn npm_package(id: &str) -> Option<&'static str> {
    match id {
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        "gemini" => Some("@google/gemini-cli"),
        "opencode" => Some("opencode-ai"),
        "amp" => Some("@sourcegraph/amp"),
        // Pi's own install steps aren't needed (its README installs it this way).
        "pi" => Some("--ignore-scripts @earendil-works/pi-coding-agent"),
        _ => None,
    }
}

/// The extras as they'll install: known ones only, in catalog order, with
/// Node.js added when an agent CLI needs it.
pub fn resolve(picked: &[String]) -> Vec<&'static str> {
    let wants = |id: &str| picked.iter().any(|p| p == id) || (id == "node" && picked.iter().any(|p| npm_package(p).is_some()));
    EXTRAS.iter().map(|(id, _)| *id).filter(|id| wants(id)).collect()
}

/// A Linux user name for `name` (this computer's user, usually).
pub fn user_name(name: &str) -> String {
    let clean: String = name.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(32).collect();
    match clean.chars().next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => clean,
        _ => "muxen".into(),
    }
}

pub fn script(user: &str, extras: &[String]) -> String {
    let extras = resolve(extras);
    let mut s = String::from(
        r#"#!/bin/bash
# Set up by Muxin. Progress: /var/log/muxen-setup.log
exec >>/var/log/muxen-setup.log 2>&1
export DEBIAN_FRONTEND=noninteractive
step() { echo "muxen-step: $*"; }
extra() { name="$1"; shift; ( set -e; "$@" ) || echo "muxen-warn: $name didn't install"; }
apt_install() { apt-get -o DPkg::Lock::Timeout=600 install -y "$@"; }
as_user() { sudo -u "$USER_NAME" -H bash -lc "$1"; }
"#,
    );
    s.push_str(&format!("USER_NAME='{user}'\n"));
    s.push_str(
        r#"
step "Creating your account"
id -u "$USER_NAME" >/dev/null 2>&1 || useradd -m -s /bin/bash -G sudo "$USER_NAME"
echo "$USER_NAME ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/90-muxen && chmod 440 /etc/sudoers.d/90-muxen
install -d -m 700 -o "$USER_NAME" -g "$USER_NAME" "/home/$USER_NAME/.ssh"
install -m 600 -o "$USER_NAME" -g "$USER_NAME" /root/.ssh/authorized_keys "/home/$USER_NAME/.ssh/authorized_keys"

step "Updating packages and installing git"
apt-get -o DPkg::Lock::Timeout=600 update -y
apt_install git curl ca-certificates unzip jq build-essential

# Every login shell (terminals, agent sessions) finds what's installed below.
cat > /etc/profile.d/muxen-paths.sh <<'PATHS'
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:$HOME/.deno/bin:/usr/local/go/bin:$HOME/go/bin:$PATH"
PATHS
"#,
    );
    for id in &extras {
        let block = match *id {
            "docker" => r#"
step "Installing Docker"
docker_() { curl -fsSL https://get.docker.com | sh; usermod -aG docker "$USER_NAME"; }
extra Docker docker_
"#
            .to_string(),
            "gh" => r#"
step "Installing the GitHub CLI"
gh_() {
  install -d -m 755 /etc/apt/keyrings
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg
  chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list
  apt-get -o DPkg::Lock::Timeout=600 update -y
  apt_install gh
}
extra "GitHub CLI" gh_
"#
            .to_string(),
            "node" => r#"
step "Installing Node.js"
node_() { curl -fsSL https://deb.nodesource.com/setup_22.x | bash -; apt_install nodejs; }
extra Node.js node_
"#
            .to_string(),
            "python" => r#"
step "Installing Python tools"
python_() { apt_install python3 python3-pip python3-venv python3-dev; as_user 'curl -LsSf https://astral.sh/uv/install.sh | sh'; }
extra Python python_
"#
            .to_string(),
            "go" => r#"
step "Installing Go"
go_() {
  v=$(curl -fsSL 'https://go.dev/dl/?mode=json' | jq -r '.[0].version')
  arch=$(dpkg --print-architecture)
  rm -rf /usr/local/go && curl -fsSL "https://go.dev/dl/$v.linux-$arch.tar.gz" | tar -C /usr/local -xz
}
extra Go go_
"#
            .to_string(),
            "rust" => r#"
step "Installing Rust"
rust_() { as_user 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'; }
extra Rust rust_
"#
            .to_string(),
            "php" => r#"
step "Installing PHP and Composer"
php_() {
  apt_install php-cli php-mbstring php-xml php-curl php-zip php-sqlite3 php-intl
  curl -fsSL https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer
}
extra PHP php_
"#
            .to_string(),
            "ruby" => r#"
step "Installing Ruby"
ruby_() { apt_install ruby-full; }
extra Ruby ruby_
"#
            .to_string(),
            "java" => r#"
step "Installing Java"
java_() { apt_install default-jdk-headless maven; }
extra Java java_
"#
            .to_string(),
            "bun" => r#"
step "Installing Bun"
bun_() { as_user 'curl -fsSL https://bun.sh/install | bash'; }
extra Bun bun_
"#
            .to_string(),
            "deno" => r#"
step "Installing Deno"
deno_() { as_user 'curl -fsSL https://deno.land/install.sh | sh -s -- -y'; }
extra Deno deno_
"#
            .to_string(),
            "dotnet" => r#"
step "Installing .NET"
dotnet_() { apt_install dotnet-sdk-8.0; }
extra .NET dotnet_
"#
            .to_string(),
            "tailscale" => r#"
step "Installing Tailscale"
tailscale_() { curl -fsSL https://tailscale.com/install.sh | sh; }
extra Tailscale tailscale_
"#
            .to_string(),
            other => match (npm_package(other), EXTRAS.iter().find(|(i, _)| *i == other)) {
                (Some(pkg), Some((_, name))) => format!("\nstep \"Installing {name}\"\nextra \"{name}\" npm install -g {pkg}\n"),
                _ => String::new(),
            },
        };
        s.push_str(&block);
    }
    s.push_str(
        r#"
step "Ready"
mkdir -p /var/lib/muxen && touch /var/lib/muxen/ready
"#,
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_and_extras() {
        assert_eq!(user_name("adibhanna"), "adibhanna");
        assert_eq!(user_name("Adib Hanna"), "adibhanna");
        assert_eq!(user_name("9lives"), "muxen");
        let langs = script("me", &["go", "rust", "php"].map(String::from));
        assert!(langs.contains("go.dev/dl") && langs.contains("sh.rustup.rs") && langs.contains("getcomposer.org"));
        assert!(GROUPS.iter().flat_map(|(_, ids)| ids.iter()).count() == EXTRAS.len(), "every extra is in a group");
        let picked: Vec<String> = ["codex", "tailscale", "bogus"].map(String::from).to_vec();
        assert_eq!(resolve(&picked), vec!["node", "codex", "tailscale"], "an agent CLI brings Node.js; unknown ones are dropped");
        let all = script("me", &picked);
        assert!(all.contains("@openai/codex") && all.contains("tailscale.com/install.sh") && all.contains("nodesource"));
        assert!(!all.contains("get.docker.com") && !all.contains("claude-code"));
        let bare = script("me", &[]);
        assert!(bare.contains("install git") || bare.contains("installing git"));
        assert!(!bare.contains("docker") && bare.contains("/var/lib/muxen/ready"));
    }
}
