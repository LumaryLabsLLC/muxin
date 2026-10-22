//! Boxes: one task each, worked on by an agent CLI in a container of its own
//! on one of the user's machines. The container is the agent's sandbox, so
//! the agent runs with its permission prompts off. A box pauses when its
//! agent has gone quiet (the container stops; its files stay) and picks up
//! where it left off when it's asked something again.
//!
//! Muxin makes a box here (`scripts/create.sh`: Muxin's image, a checkout,
//! the agent's home, the container) and hands it to the machine's agent,
//! which keeps it from then on (agent/src/boxes.rs): it starts the box's
//! agent in a session, pauses it when idle whether or not Muxin is open, and
//! deletes it.
//!
//! PUBLIC API — the UI, the MCP server and the CLI depend on these signatures.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use super::Engine;
use super::sessions::agent as agent_cli;
use super::sessions::git::GitStatus;
use super::workspaces::{self, Repo};
use crate::config::MachineSpec;
use crate::ops::remote::{self, Param, sh_quote, with_params};
use crate::probe::Shell;

const CREATE_SH: &str = include_str!("scripts/create.sh");
const DOCKERFILE: &str = include_str!("scripts/Dockerfile");
const CREATE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// This long without output from its agent, a box pauses (0: never).
pub const IDLE_MINUTES: u32 = 15;

/// How an agent CLI runs in a box, in sh in the checkout, with what it's
/// asked in `$p` (maybe empty): `new` on its task, `resume` where it left off,
/// both after `prepare` (its first-run questions answered in the box's own
/// copy of its settings). `typed`: it takes no prompt on its command line, so
/// the prompt is typed in once it's up. `dirs`: its folders in the home, made
/// on the machine when missing so a sign-in in the box stays there.
struct Launch {
    new: &'static str,
    resume: &'static str,
    prepare: &'static str,
    typed: bool,
    dirs: &'static [&'static str],
}

/// Claude Code: onboarding done, permission prompts off, this folder trusted
/// (and an API key from the machine's shell accepted), in the box's own
/// ~/.claude.json (a copy of the machine's).
const CLAUDE_PREPARE: &str = r#"node -e 'const fs=require("fs"),f=process.env.HOME+"/.claude.json";let c={};try{c=JSON.parse(fs.readFileSync(f,"utf8"))}catch(e){}
c.hasCompletedOnboarding=true;c.bypassPermissionsModeAccepted=true;c.projects=c.projects||{};const p=c.projects[process.cwd()]=c.projects[process.cwd()]||{};p.hasTrustDialogAccepted=true;p.hasCompletedProjectOnboarding=true;
const k=process.env.ANTHROPIC_API_KEY;if(k){const r=c.customApiKeyResponses=c.customApiKeyResponses||{};r.approved=r.approved||[];r.rejected=r.rejected||[];if(!r.approved.includes(k.slice(-20)))r.approved.push(k.slice(-20))}
fs.writeFileSync(f,JSON.stringify(c,null,2))'"#;

/// Codex: this folder trusted for this run. Its `-c` splits the key at dots
/// and takes no quotes, so a path with a dot in it is asked about once instead.
const CODEX_PREPARE: &str = r#"trust=
case "$PWD" in *.*) ;; *) trust="projects.$PWD.trust_level=trusted" ;; esac"#;

/// Amp: the machine's settings with every command allowed, in a file of the box's own.
const AMP_PREPARE: &str = r#"node -e 'const fs=require("fs"),h=process.env.HOME;let c={};try{c=JSON.parse(fs.readFileSync(h+"/.config/amp/settings.json","utf8"))}catch(e){}c["amp.dangerouslyAllowAll"]=true;fs.writeFileSync(h+"/.muxin/amp-settings.json",JSON.stringify(c,null,2))'
export AMP_SETTINGS_FILE="$HOME/.muxin/amp-settings.json""#;

fn launch(agent: &str) -> Option<Launch> {
    Some(match agent {
        "claude" => Launch {
            new: r#"claude --dangerously-skip-permissions ${p:+"$p"}"#,
            resume: r#"claude --dangerously-skip-permissions --continue ${p:+"$p"}"#,
            prepare: CLAUDE_PREPARE,
            typed: false,
            dirs: &[".claude"],
        },
        // The folder trusted on its command line, not in the machine's config.toml.
        "codex" => Launch {
            new: r#"codex ${trust:+-c "$trust"} --dangerously-bypass-approvals-and-sandbox ${p:+"$p"}"#,
            resume: r#"codex ${trust:+-c "$trust"} --dangerously-bypass-approvals-and-sandbox resume --last ${p:+"$p"}"#,
            prepare: CODEX_PREPARE,
            typed: false,
            dirs: &[".codex"],
        },
        "amp" => Launch { new: "amp", resume: "amp threads continue --last", prepare: AMP_PREPARE, typed: true, dirs: &[".config/amp", ".local/share/amp"] },
        "opencode" => Launch {
            new: r#"opencode ${p:+--prompt "$p"}"#,
            resume: r#"opencode --continue ${p:+--prompt "$p"}"#,
            prepare: "",
            typed: false,
            dirs: &[".config/opencode", ".local/share/opencode"],
        },
        "gemini" => Launch { new: r#"gemini --yolo ${p:+-i "$p"}"#, resume: r#"gemini --yolo --resume latest ${p:+-i "$p"}"#, prepare: "", typed: false, dirs: &[".gemini"] },
        "pi" => Launch { new: r#"pi ${p:+"$p"}"#, resume: r#"pi --continue ${p:+"$p"}"#, prepare: "", typed: false, dirs: &[".pi"] },
        _ => return None,
    })
}

/// A box's launcher (`~/.muxin/start`) for `agent`, and whether its prompt is
/// typed in: for a box changing agents (see sessions::handoff).
pub(crate) fn launcher(agent: &str) -> Option<(String, bool)> {
    launch(agent).map(|l| (start_script(&l), l.typed))
}

/// The agent CLIs a box can run (ids from `sessions::AGENTS`).
pub const BOX_AGENTS: &[&str] = &["claude", "codex", "amp", "opencode", "gemini", "pi"];

/// Every agent CLI's folders in the home (sign-ins, settings, history): the
/// machine's are mounted into each box, so its agents are signed in as there.
const AGENT_DIRS: &[&str] = &[".claude", ".codex", ".gemini", ".pi", ".config/amp", ".local/share/amp", ".config/opencode", ".local/share/opencode"];

/// The agent folders with databases in them (SQLite). Where Docker runs in a
/// VM (a Mac), a container can't lock files in the machine's folders, so these
/// aren't shared there: the box gets a copy of their sign-in (`DB_COPY`).
const DB_DIRS: &[&str] = &[".codex", ".local/share/opencode"];
const DB_COPY: &[&str] = &[".codex/auth.json", ".codex/config.toml", ".local/share/opencode/auth.json"];

/// One box, as the snapshot lists it.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxInfo {
    /// "box-ab12cd".
    pub id: String,
    pub machine_id: String,
    pub name: String,
    /// "owner/name", or the git URL it was cloned from.
    pub repo: String,
    pub branch: String,
    pub agent: String,
    pub agent_name: String,
    /// "running", "starting" or "paused".
    pub state: String,
    /// Its agent printed something in the last few seconds: it's working.
    pub busy: bool,
    /// An unread notice from its agent ("waiting", "bell" or "exited").
    pub attention: Option<String>,
    /// Its agent's session (`<machine>/<box id>`) while it runs.
    pub session_id: Option<String>,
    /// What it was first asked.
    pub task: String,
    /// The checkout on its machine it was made from (a local project), if it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout: Option<String>,
    /// The checkout on the machine.
    pub path: String,
    pub idle_minutes: u32,
    pub created_at: Option<DateTime<Utc>>,
    /// When it last started or paused.
    pub state_at: Option<DateTime<Utc>>,
    /// What its work changed: branch, files, commits, pull request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitStatus>,
}

/// The box a session belongs to: its agent's (named as the box) or one of
/// its shells ("<box id>-sh-<n>").
pub(crate) fn box_of_session<'a>(name: &str, boxes: &'a [BoxInfo]) -> Option<&'a BoxInfo> {
    boxes.iter().find(|b| name == b.id || name.strip_prefix(b.id.as_str()).and_then(|r| r.strip_prefix("-sh-")).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))
}

/// The boxes in a machine agent's `discover` reply (agents before 0.3 have none).
pub(crate) fn parse(machine_id: &str, reply: &Value) -> Vec<BoxInfo> {
    let ts = |v: &Value| v.as_i64().filter(|t| *t > 0).and_then(|t| Utc.timestamp_opt(t, 0).single());
    reply["boxes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let id = b["id"].as_str().filter(|i| i.starts_with("box-"))?.to_string();
            let agent = b["agent"].as_str().unwrap_or("").to_string();
            let str_of = |k: &str| b[k].as_str().unwrap_or("").to_string();
            Some(BoxInfo {
                machine_id: machine_id.to_string(),
                name: b["name"].as_str().filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| id.clone()),
                repo: str_of("repo"),
                branch: str_of("branch"),
                agent_name: super::sessions::agent(&agent).map(|k| k.name.to_string()).unwrap_or_default(),
                agent,
                state: b["state"].as_str().unwrap_or("paused").to_string(),
                task: str_of("task"),
                checkout: b["checkout"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
                path: str_of("path"),
                idle_minutes: b["idle_minutes"].as_u64().map(|m| m as u32).unwrap_or(IDLE_MINUTES),
                created_at: ts(&b["created"]),
                state_at: ts(&b["state_at"]),
                id,
                ..Default::default()
            })
        })
        .collect()
}

/// What `box_create` needs.
#[derive(Debug, Clone, Default)]
pub struct CreateBox {
    /// "owner/name" on GitHub, or a git URL.
    pub repo: String,
    /// What the agent is asked to do.
    pub prompt: String,
    /// An agent CLI from `BOX_AGENTS`; Claude Code when empty.
    pub agent: String,
    /// The machine; else the one with Docker and the most free memory.
    pub machine: Option<String>,
    /// Its name; else from the task.
    pub name: Option<String>,
    /// The branch it works on; else "box/<name>".
    pub branch: Option<String>,
    /// What to start from: a branch, tag, commit or "pull/<n>/head"; else the default branch.
    pub git_ref: Option<String>,
    pub cpus: Option<u32>,
    pub memory_gb: Option<u32>,
    pub idle_minutes: Option<u32>,
}

/// A box about to be made: everything worked out but the machine's side.
struct Plan {
    id: String,
    name: String,
    agent: String,
    repo: Repo,
    /// `repo` is a checkout on the box's machine (a local project): cloned
    /// from there, with its uncommitted changes.
    local: bool,
    /// The GitHub repository it pushes to ("owner/name"): `repo`'s, or the checkout's origin's.
    github: Option<String>,
    git_ref: String,
    branch: String,
    task: String,
    cpus: Option<u32>,
    memory_gb: Option<u32>,
    idle_minutes: u32,
}

impl Engine {
    /// Every box on the user's machines, newest first.
    pub fn boxes(&self) -> Vec<BoxInfo> {
        self.current().boxes.clone()
    }

    /// A box by id ("box-ab12cd", or "<machine>/box-ab12cd").
    pub fn find_box(&self, id: &str) -> Result<BoxInfo, String> {
        let bare = id.trim().rsplit('/').next().unwrap_or("");
        self.boxes().into_iter().find(|b| b.id == bare).ok_or_else(|| format!("No box called {bare}. See your boxes with box_list (`muxin box ls`)."))
    }

    /// Starts making a box (an operation) and returns the operation's id and
    /// the new box's.
    pub fn box_create(self: &Arc<Self>, req: CreateBox) -> Result<(u64, String), String> {
        if self.demo_world().is_some() {
            return Err("The demo doesn't make boxes.".into());
        }
        let agent = if req.agent.trim().is_empty() { "claude".to_string() } else { req.agent.trim().to_ascii_lowercase() };
        if launch(&agent).is_none() {
            return Err(format!("A box can't run \"{agent}\". It runs: {}.", BOX_AGENTS.join(", ")));
        }
        let task = req.prompt.trim().to_string();
        let gh_host = workspaces::github_web_host(&self.settings().github.api_url);
        let repo = workspaces::normalize_repo(&req.repo, &gh_host)?;
        let git_ref = req.git_ref.map(|r| r.trim().to_string()).unwrap_or_default();
        if !git_ref.is_empty() {
            workspaces::validate_git_ref(&git_ref)?;
        }
        let name = match req.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
            Some(n) if n.chars().count() > 80 || n.chars().any(char::is_control) => return Err("The box's name is too long or has odd characters.".into()),
            Some(n) => n,
            None => name_from(&task, &repo.short_name),
        };
        let branch = req.branch.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()).unwrap_or_else(|| format!("box/{}", short_slug(&name)));
        workspaces::validate_git_ref(&branch)?;
        // A folder on a machine: a checkout there (a local project), so the box goes there too.
        let local = repo.url.starts_with('/') || repo.url.starts_with("~/");
        if local && req.machine.as_deref().is_none_or(|m| m.trim().is_empty()) {
            return Err(format!("Say which machine {} is on.", repo.shown));
        }
        let spec = self.box_machine(req.machine.as_deref())?;
        let github = match local {
            true => self.checkouts_of(&spec.id).into_iter().find(|c| workspaces::same_folder(&c.path, &repo.url)).and_then(|c| c.repo),
            false => repo.on_github.then(|| workspaces::github_full_name(&repo)).flatten(),
        };
        let taken: Vec<String> = self.boxes().into_iter().map(|b| b.id).collect();
        let id = loop {
            let id = format!("box-{}", workspaces::random_id(6));
            if !taken.contains(&id) {
                break id;
            }
        };
        let plan = Plan { id: id.clone(), name, agent, repo, local, github, git_ref, branch, task, cpus: req.cpus, memory_gb: req.memory_gb, idle_minutes: req.idle_minutes.unwrap_or(IDLE_MINUTES) };
        let op = self.begin_op("box", format!("Make box {} on {}", plan.name, spec.name), Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = me.run_box_create(op, &spec, &plan).await;
            if let Err(e) = &result {
                me.op_log(op, format!("error: {e}"));
                // Whatever was made goes again.
                let _ = agent_cli::ctl(&spec, &json!({ "op": "box_delete", "box_id": plan.id })).await;
            }
            me.op_finish(op, result);
            let _ = me.refresh_sessions(&spec.id).await;
        });
        Ok((op, id))
    }

    /// The machine a box goes on: the one asked for, or the one with Docker and
    /// the most free memory.
    fn box_machine(&self, asked: Option<&str>) -> Result<MachineSpec, String> {
        let snap = self.current();
        let usable = |m: &crate::fleet::model::Machine| m.status == "online" && !m.platform.starts_with("windows") && !m.is_disconnected();
        let has_docker = |m: &crate::fleet::model::Machine| m.probe.as_ref().is_some_and(|p| p.docker.is_some());
        let m = match asked.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => {
                let m = snap.machines.iter().find(|m| m.id == a || m.name.eq_ignore_ascii_case(a)).ok_or_else(|| format!("No machine called {a}."))?;
                if m.platform.starts_with("windows") {
                    return Err(format!("Boxes don't run on Windows machines yet ({}). Pick a Linux machine or a Mac with Docker.", m.name));
                }
                if m.status != "online" {
                    return Err(format!("{} isn't online.", m.name));
                }
                m
            }
            None => snap
                .machines
                .iter()
                .filter(|m| usable(m) && has_docker(m))
                .max_by_key(|m| m.probe.as_ref().and_then(|p| p.mem_total.zip(p.mem_used)).map(|(t, u)| t.saturating_sub(u)).unwrap_or(0))
                .ok_or("None of your machines can run a box: it needs one that's online with Docker (a Linux machine, or a Mac with Docker Desktop, OrbStack or Colima).")?,
        };
        self.machine(&m.id).map(|(s, _)| s).ok_or_else(|| format!("{} isn't in Muxin anymore.", m.name))
    }

    async fn run_box_create(self: &Arc<Self>, op: u64, spec: &MachineSpec, plan: &Plan) -> Result<(), String> {
        self.op_log(op, format!("▸ Making {} ({}) on {} with {}", plan.id, plan.name, spec.name, plan.agent));
        let probe = self.machine(&spec.id).and_then(|(_, p)| p);
        // The box's agent keeps it from here on: it has to be this build's.
        agent_cli::ensure(spec, probe.as_ref()).await?;

        // GitHub: the token clones a private repo and lets the agent push and
        // open pull requests; commits are the user's, with their noreply address.
        let settings = self.settings().github;
        // A checkout Muxin hasn't looked at yet: where it pushes, asked of it.
        let github = match (&plan.github, plan.local) {
            (None, true) => checkout_github(spec, &plan.repo.url, &workspaces::github_web_host(&settings.api_url)).await,
            (g, _) => g.clone(),
        };
        // The GitHub host it pushes to: its URL's, or (a checkout) GitHub's when its origin is there.
        let gh_host = match (plan.repo.on_github, &github) {
            (true, _) => Some(plan.repo.host.clone()),
            (false, Some(_)) => Some(workspaces::github_web_host(&settings.api_url)),
            (false, None) => None,
        };
        let token = match &gh_host {
            Some(h) => self.github().git_credentials(&settings).await.filter(|(host, _)| host == h).map(|(_, t)| t),
            None => None,
        };
        let identity = match self.github().viewer(&settings).await {
            Ok(v) => self.github().commit_identity(&settings, &v.login).await.ok(),
            Err(_) => None,
        };
        if token.is_none() && gh_host.is_some() {
            self.op_log(op, "! No GitHub token: the agent can't push or open pull requests from the box");
        }
        let gh_host = gh_host.unwrap_or_default();

        let l = launch(&plan.agent).ok_or("That agent can't run in a box.")?;
        let mut env: Vec<(String, String)> = vec![("MUXIN_BOX".into(), plan.id.clone())];
        if let Some(t) = &token {
            env.push(("GH_TOKEN".into(), t.clone()));
            if gh_host != "github.com" {
                env.push(("GH_HOST".into(), gh_host.clone()));
                env.push(("GH_ENTERPRISE_TOKEN".into(), t.clone()));
            }
        }
        let files = home_files(&l, &env, identity.as_ref(), token.as_ref().map(|_| gh_host.as_str()));
        let encoded: Vec<String> = files.iter().map(|(path, mode, text)| format!("{path}\t{mode}\t{}", B64.encode(text))).collect();
        // Signed in as on the machine: its agent folders mounted (the box's
        // agent's made when missing, so a sign-in in the box stays), and a
        // copy of Claude Code's ~/.claude.json (its MCP servers and the like).
        let mounts: Vec<String> = AGENT_DIRS.iter().map(|d| d.to_string()).collect();
        let ensure: Vec<String> = l.dirs.iter().map(|d| d.to_string()).collect();
        let copy: Vec<String> = vec![".claude.json".into()];
        let db_dirs: Vec<String> = DB_DIRS.iter().map(|d| d.to_string()).collect();
        let db_copy: Vec<String> = DB_COPY.iter().map(|d| d.to_string()).collect();
        // Only a clone from GitHub needs the token (a checkout is cloned from its folder).
        let header = token.as_ref().filter(|_| !plan.local).map(|t| format!("AUTHORIZATION: basic {}", B64.encode(format!("x-access-token:{t}")))).unwrap_or_default();
        let image = format!("muxen-box:{:08x}", fnv(DOCKERFILE.as_bytes()) as u32);
        let container = format!("muxen-{}", plan.id);
        let hostname = short_slug(&plan.name);
        let cpus = plan.cpus.map(|c| c.clamp(1, 64).to_string()).unwrap_or_default();
        let memory = plan.memory_gb.map(|m| format!("{}g", m.clamp(1, 512))).unwrap_or_default();
        let files_param = encoded.join("\n");
        let script = with_params(
            Shell::Sh,
            &[
                ("OR_ID", Param::Str(&plan.id)),
                ("OR_CONTAINER", Param::Str(&container)),
                ("OR_HOSTNAME", Param::Str(&hostname)),
                ("OR_IMAGE", Param::Str(&image)),
                ("OR_DOCKERFILE", Param::Str(DOCKERFILE)),
                ("OR_REPO", Param::Str(&plan.repo.url)),
                ("OR_LOCAL", Param::Bool(plan.local)),
                ("OR_REPO_SHOWN", Param::Str(&plan.repo.shown)),
                ("OR_REF", Param::Str(&plan.git_ref)),
                ("OR_BRANCH", Param::Str(&plan.branch)),
                ("OR_AUTH", Param::Str(&header)),
                ("OR_FILES", Param::Str(&files_param)),
                ("OR_COPY", Param::List(&copy)),
                ("OR_MOUNTS", Param::List(&mounts)),
                ("OR_ENSURE", Param::List(&ensure)),
                ("OR_DB_DIRS", Param::List(&db_dirs)),
                ("OR_DB_COPY", Param::List(&db_copy)),
                ("OR_CPUS", Param::Str(&cpus)),
                ("OR_MEM", Param::Str(&memory)),
            ],
            CREATE_SH,
        );
        let mut secrets: Vec<String> = env.iter().filter(|(k, _)| k != "MUXIN_BOX" && k != "GH_HOST").map(|(_, v)| v.clone()).collect();
        if !header.is_empty() {
            secrets.push(header.clone());
            secrets.push(header.trim_start_matches("AUTHORIZATION: basic ").to_string());
        }
        let printed = remote::run_lines(self, op, spec, Shell::Sh, script, &secrets, CREATE_TIMEOUT).await?;
        let field = |key: &str| printed.iter().find_map(|l| l.strip_prefix(&format!("{key}="))).map(|v| v.trim().to_string());
        let record = json!({
            "id": plan.id,
            "name": plan.name,
            "repo": github.clone().unwrap_or_else(|| plan.repo.shown.clone()),
            "checkout": plan.local.then(|| plan.repo.shown.clone()),
            "branch": plan.branch,
            "agent": plan.agent,
            "task": plan.task,
            "image": image,
            "container": container,
            "docker": field("docker").unwrap_or_default(),
            "type_prompt": l.typed,
            "idle_minutes": plan.idle_minutes,
            "created": Utc::now().timestamp(),
        });
        agent_cli::ctl(spec, &json!({ "op": "box_save", "box": record })).await?;
        self.op_log(op, format!("▸ Starting {} in the box", super::sessions::agent(&plan.agent).map(|k| k.name).unwrap_or(&plan.agent)));
        let prompt = (!plan.task.is_empty()).then_some(plan.task.as_str());
        agent_cli::ctl(spec, &json!({ "op": "box_resume", "box_id": plan.id, "prompt": prompt })).await?;
        self.op_log(op, format!("✓ {} is working on it", plan.id));
        Ok(())
    }

    /// Asks a box's agent something: typed in when it's running, else the box
    /// starts again and its agent continues with it. No text just starts it.
    pub async fn box_send(self: &Arc<Self>, id: &str, text: Option<&str>) -> Result<BoxInfo, String> {
        let b = self.find_box(id)?;
        if self.demo_world().is_some() {
            return Err("The demo's boxes are only for show.".into());
        }
        let (spec, probe) = self.machine(&b.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", b.machine_id))?;
        agent_cli::ensure(&spec, probe.as_ref()).await?;
        let text = text.map(str::trim).filter(|t| !t.is_empty());
        agent_cli::ctl(&spec, &json!({ "op": "box_resume", "box_id": b.id, "prompt": text })).await?;
        let _ = self.refresh_sessions(&spec.id).await;
        self.find_box(&b.id)
    }

    /// Opens a shell in a box, beside its agent: bash in its container and
    /// checkout (a paused box starts again first). Pausing the box ends it.
    pub async fn box_shell(self: &Arc<Self>, id: &str, cols: u16, rows: u16) -> Result<super::sessions::SessionInfo, String> {
        let mut b = self.find_box(id)?;
        if self.demo_world().is_some() {
            return Err("The demo's boxes are only for show.".into());
        }
        if b.state != "running" {
            b = self.box_send(&b.id, None).await?;
        }
        let (spec, probe) = self.machine(&b.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", b.machine_id))?;
        agent_cli::ensure(&spec, probe.as_ref()).await?;
        let reply = agent_cli::ctl(&spec, &json!({ "op": "box_shell", "box_id": b.id, "cols": cols, "rows": rows })).await?;
        let name = reply["session"].as_str().ok_or("The machine's agent didn't name the shell.")?.to_string();
        let found = self.refresh_sessions(&spec.id).await?;
        found.sessions.into_iter().find(|s| s.name == name).ok_or_else(|| format!("The shell in {} ended right away.", b.id))
    }

    /// Pauses a box: its agent ends and its container stops; its files stay.
    pub async fn box_pause(self: &Arc<Self>, id: &str) -> Result<BoxInfo, String> {
        let b = self.find_box(id)?;
        if self.demo_world().is_some() {
            return Err("The demo's boxes are only for show.".into());
        }
        let (spec, probe) = self.machine(&b.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", b.machine_id))?;
        agent_cli::ensure(&spec, probe.as_ref()).await?;
        agent_cli::ctl(&spec, &json!({ "op": "box_pause", "box_id": b.id })).await?;
        let _ = self.refresh_sessions(&spec.id).await;
        self.find_box(&b.id)
    }

    /// Deletes a box: its container and its folder, with any work in it that
    /// wasn't pushed. Its branch on GitHub stays.
    pub async fn box_destroy(self: &Arc<Self>, id: &str) -> Result<BoxInfo, String> {
        let b = self.find_box(id)?;
        if self.demo_world().is_some() {
            return Err("The demo's boxes are only for show.".into());
        }
        let (spec, probe) = self.machine(&b.machine_id).ok_or_else(|| format!("{} isn't in Muxin anymore.", b.machine_id))?;
        agent_cli::ensure(&spec, probe.as_ref()).await?;
        agent_cli::ctl(&spec, &json!({ "op": "box_delete", "box_id": b.id })).await?;
        let _ = self.refresh_sessions(&spec.id).await;
        Ok(b)
    }
}

/// The GitHub repository ("owner/name") a checkout on a machine pushes to: its origin's.
async fn checkout_github(spec: &MachineSpec, path: &str, gh_host: &str) -> Option<String> {
    let folder = match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
        None => sh_quote(path),
    };
    let url = remote::output(spec, Shell::Sh, format!("git -C {folder} remote get-url origin\n"), Duration::from_secs(30)).await.ok()?;
    github_name(url.trim(), gh_host)
}

/// "owner/name" of a git URL on the GitHub host: https, or ssh (git@host:owner/name).
pub(crate) fn github_name(url: &str, gh_host: &str) -> Option<String> {
    let ssh = [format!("git@{gh_host}:"), format!("ssh://git@{gh_host}/")];
    let https = match ssh.iter().find_map(|p| url.strip_prefix(p.as_str())) {
        Some(path) => format!("https://{gh_host}/{path}"),
        None => url.to_string(),
    };
    let repo = workspaces::normalize_repo(&https, gh_host).ok()?;
    repo.on_github.then(|| workspaces::github_full_name(&repo)).flatten()
}

/// The files of a box's home Muxin writes: path (under /home/box), mode and contents.
fn home_files(l: &Launch, env: &[(String, String)], identity: Option<&(String, String)>, git_host: Option<&str>) -> Vec<(String, &'static str, String)> {
    let mut files: Vec<(String, &'static str, String)> = Vec::new();
    files.push((".muxin/start".into(), "755", start_script(l)));
    let env_text: String = env.iter().map(|(k, v)| format!("{k}={}\n", sh_quote(v))).collect();
    files.push((".muxin/env".into(), "600", env_text));
    files.push((".bashrc".into(), "644", BASHRC.into()));
    files.push((".profile".into(), "644", ". \"$HOME/.bashrc\"\n".into()));

    let mut gitconfig = String::new();
    if let Some((name, email)) = identity {
        gitconfig.push_str(&format!("[user]\n\tname = {}\n\temail = {}\n", git_value(name), git_value(email)));
    }
    if let Some(host) = git_host {
        files.push((".muxin/git-credential".into(), "755", GIT_CREDENTIAL.into()));
        gitconfig.push_str(&format!("[credential \"https://{host}\"]\n\thelper = /home/box/.muxin/git-credential\n"));
        // A box has no ssh keys: ssh remotes (a checkout's, say) go over https, with the token.
        gitconfig.push_str(&format!("[url \"https://{host}/\"]\n\tinsteadOf = git@{host}:\n\tinsteadOf = ssh://git@{host}/\n"));
    }
    gitconfig.push_str("[init]\n\tdefaultBranch = main\n[push]\n\tautoSetupRemote = true\n");
    files.push((".gitconfig".into(), "644", gitconfig));
    files
}

/// `~/.muxin/start` in a box: what its session runs.
fn start_script(l: &Launch) -> String {
    format!(
        r#"# Starts this box's agent (Muxin wrote this; the box's session runs it):
# the first time on its task, after that where it left off, asked what's in
# ~/.muxin/next.
cd "${{MUXIN_WORKDIR:-$PWD}}" || exit 1
if [ -f "$HOME/.muxin/env" ]; then set -a; . "$HOME/.muxin/env"; set +a; fi
p=
if [ -f "$HOME/.muxin/next" ]; then p=$(cat "$HOME/.muxin/next"); rm -f "$HOME/.muxin/next"; fi
{prepare}
if [ -f "$HOME/.muxin/started" ]; then
  if [ -f .agents/resume ]; then
    echo "▸ Running .agents/resume"
    if [ -x .agents/resume ]; then ./.agents/resume; else sh .agents/resume; fi
  fi
  exec {resume}
fi
touch "$HOME/.muxin/started"
exec {new}
"#,
        resume = l.resume,
        new = l.new,
        prepare = l.prepare
    )
}

const BASHRC: &str = r#"# A Muxin box: its GitHub token, and the agent CLIs on the PATH, for shells too.
if [ -f "$HOME/.muxin/env" ]; then set -a; . "$HOME/.muxin/env"; set +a; fi
case ":$PATH:" in *:/opt/agents/bin:*) ;; *) PATH="/opt/agents/bin:$PATH" ;; esac
PS1='\u@\h:\w\$ '
"#;

const GIT_CREDENTIAL: &str = r#"#!/bin/sh
# Git's credentials in this box: the GitHub token Muxin gave it.
[ "$1" = get ] || exit 0
[ -f "$HOME/.muxin/env" ] && . "$HOME/.muxin/env"
[ -n "${GH_TOKEN:-}" ] || exit 0
echo username=x-access-token
echo "password=$GH_TOKEN"
"#;

/// A value for .gitconfig: quoted, with its quotes and backslashes escaped.
fn git_value(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A box's name from its task: its first words.
fn name_from(task: &str, repo: &str) -> String {
    let words: Vec<&str> = task.split_whitespace().take(6).collect();
    if words.is_empty() {
        return repo.to_string();
    }
    let mut name = words.join(" ");
    if name.chars().count() > 48 {
        name = name.chars().take(48).collect::<String>().trim_end().to_string();
    }
    name.trim_end_matches(['.', ',', ':', ';']).to_string()
}

/// A slug short enough for a branch or a hostname ("box" for one without letters).
fn short_slug(s: &str) -> String {
    if !s.chars().any(|c| c.is_ascii_alphanumeric()) {
        return "box".into();
    }
    let mut out: String = crate::util::slug(s).chars().take(40).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_box_agent_launches() {
        for a in BOX_AGENTS {
            assert!(launch(a).is_some(), "{a}");
            assert!(super::super::sessions::agent(a).is_some(), "{a} is a session agent");
        }
        assert!(launch("shell").is_none());
    }

    #[test]
    fn the_start_script_continues_after_the_first_time() {
        let s = start_script(&launch("codex").unwrap());
        let resume = s.find("resume --last").unwrap();
        let started = s.find("touch \"$HOME/.muxin/started\"").unwrap();
        assert!(resume < started, "resume is the path once started");
        // Trusted on its command line: the machine's own config.toml stays as it is.
        assert!(s.contains(r#"trust="projects.$PWD.trust_level=trusted""#));
        assert!(s.find("trust=").unwrap() < s.find(r#"exec codex ${trust:+-c "$trust"} --dangerously-bypass-approvals-and-sandbox ${p:+"$p"}"#).unwrap());
        // Claude Code's first-run answers go in the box's own ~/.claude.json, before it starts.
        let claude = start_script(&launch("claude").unwrap());
        assert!(claude.find("hasTrustDialogAccepted").unwrap() < claude.find("exec claude").unwrap());
    }

    #[test]
    fn names_and_branches_from_the_task() {
        assert_eq!(name_from("Fix the login redirect loop when the session expires.", "api"), "Fix the login redirect loop when");
        assert_eq!(name_from("  ", "api"), "api");
        assert_eq!(short_slug("Fix the login redirect loop when"), "fix-the-login-redirect-loop-when");
        assert_eq!(short_slug("!!!"), "box");
    }

    #[test]
    fn home_files_carry_the_github_token_and_identity_only() {
        let env = vec![("GH_TOKEN".to_string(), "ghp_x".to_string())];
        let files = home_files(&launch("claude").unwrap(), &env, Some(&("Ada \"L\"".into(), "1+ada@users.noreply.github.com".into())), Some("github.com"));
        let get = |p: &str| files.iter().find(|(path, _, _)| path == p).map(|(_, mode, text)| (*mode, text.clone()));
        let (mode, env_text) = get(".muxin/env").unwrap();
        assert_eq!(mode, "600");
        assert!(env_text.contains("GH_TOKEN='ghp_x'"));
        let (_, git) = get(".gitconfig").unwrap();
        assert!(git.contains(r#"name = "Ada \"L\"""#));
        assert!(git.contains("[credential \"https://github.com\"]"));
        // The agents' own folders come from the machine, not from Muxin.
        assert!(files.iter().all(|(p, _, _)| !p.starts_with(".claude") && !p.starts_with(".codex") && !p.starts_with(".config")));
    }

    #[test]
    fn each_agent_keeps_its_folders_on_the_machine() {
        for a in BOX_AGENTS {
            let l = launch(a).unwrap();
            assert!(!l.dirs.is_empty(), "{a}");
            assert!(l.dirs.iter().all(|d| AGENT_DIRS.contains(d)), "{a}'s folders are mounted");
        }
    }

    #[test]
    fn a_checkouts_origin_names_its_github_repo() {
        assert_eq!(github_name("https://github.com/acme/api.git", "github.com").as_deref(), Some("acme/api"));
        assert_eq!(github_name("git@github.com:acme/api.git", "github.com").as_deref(), Some("acme/api"));
        assert_eq!(github_name("ssh://git@github.com/acme/api", "github.com").as_deref(), Some("acme/api"));
        assert_eq!(github_name("git@gitlab.com:acme/api.git", "github.com"), None);
        assert_eq!(github_name("/srv/git/api.git", "github.com"), None);
    }

    #[test]
    fn sessions_belong_to_their_box() {
        let boxes = vec![BoxInfo { id: "box-ab12cd".into(), ..Default::default() }];
        assert_eq!(box_of_session("box-ab12cd", &boxes).map(|b| b.id.as_str()), Some("box-ab12cd"));
        assert_eq!(box_of_session("box-ab12cd-sh-2", &boxes).map(|b| b.id.as_str()), Some("box-ab12cd"));
        assert!(box_of_session("box-ab12cd-sh-", &boxes).is_none());
        assert!(box_of_session("claude-3f9a", &boxes).is_none());
    }

    #[test]
    fn parses_the_agents_boxes() {
        let reply = json!({ "boxes": [
            { "id": "box-ab12cd", "name": "Fix login", "repo": "acme/api", "branch": "box/fix-login", "agent": "codex", "state": "running", "task": "Fix login", "path": "/home/a/muxen/boxes/box-ab12cd/repo", "created": 1_700_000_000, "state_at": 1_700_000_100 },
            { "id": "not-a-box" },
        ] });
        let found = parse("beelink", &reply);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].agent_name, "Codex");
        assert_eq!(found[0].idle_minutes, IDLE_MINUTES);
        assert_eq!(found[0].machine_id, "beelink");
        assert!(parse("beelink", &json!({})).is_empty());
    }
}
