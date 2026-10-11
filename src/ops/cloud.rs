//! Cloud machine operations: create a droplet and make it one of your machines,
//! power it off or on, destroy it; and the idle watch that warns, powers off
//! or destroys machines nobody is using (as the user chose in Settings › Cloud).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::inventory_edit;
use super::NewMachine;
use crate::cloud::digitalocean::{Client, same_key};
use crate::cloud::{CloudCreate, CloudOptions, TAILSCALE_KEY, auth, nearest_region, secrets, setup, usage};
use crate::config::{CloudRef, MachineSpec};
use crate::fleet::Engine;

/// When each cloud machine was last in use (this run of the app), and the ones
/// the idle watch already acted on.
#[derive(Default)]
pub struct IdleState {
    pub last_active: HashMap<String, DateTime<Utc>>,
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    acted: std::collections::HashSet<String>,
}

pub static IDLE: Mutex<Option<IdleState>> = Mutex::new(None);

impl Engine {
    async fn do_client(&self) -> Result<Client, String> {
        let method = self.settings().cloud.digitalocean_auth;
        auth::token(&method, &self.paths().dir).await.map(Client::new)
    }

    /// The account, regions, sizes and defaults for a new cloud machine.
    pub async fn cloud_options(self: &Arc<Self>) -> CloudOptions {
        let settings = self.settings().cloud;
        let dir = self.paths().dir.clone();
        let mut out = CloudOptions {
            provider: "digitalocean".into(),
            method: settings.digitalocean_auth.clone(),
            signed_in: auth::signed_in(&settings.digitalocean_auth, &dir),
            oauth_available: auth::oauth_available(),
            default_size: settings.size.clone(),
            ssh_key: crate::probe::default_ssh_key(),
            ..Default::default()
        };
        if self.demo_world().is_some() {
            return demo_options(out);
        }
        if !out.signed_in {
            return out;
        }
        let client = match self.do_client().await {
            Ok(c) => c,
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        };
        let (account, regions, sizes) = tokio::join!(client.account(), client.regions(), client.sizes());
        match (account, regions, sizes) {
            (Ok(a), Ok(r), Ok(s)) => {
                out.account = Some(a);
                let slugs: Vec<String> = r.iter().map(|r| r.slug.clone()).collect();
                let offset = chrono::Local::now().offset().local_minus_utc() / 3600;
                out.default_region = Some(settings.region.clone()).filter(|r| slugs.contains(r)).or_else(|| nearest_region(offset, &slugs)).unwrap_or_default();
                out.regions = r;
                // The everyday sizes: basic ("s-"), general purpose ("g-") and CPU-optimized ("c-") droplets.
                out.sizes = s.into_iter().filter(|s| ["s-", "g-", "c-"].iter().any(|p| s.slug.starts_with(p)) && s.memory_mb >= 1024).collect();
            }
            (a, r, s) => out.error = a.err().or(r.err()).or(s.err()),
        }
        out
    }

    /// Creates a droplet, waits for it to set itself up, and adds it to the
    /// machines. Returns the operation that tracks it.
    pub fn cloud_create(self: &Arc<Self>, req: CloudCreate) -> Result<u64, String> {
        if self.demo_world().is_some() {
            return Err("The demo doesn't create cloud machines.".into());
        }
        let name = req.name.trim().to_string();
        if name.is_empty() || name.len() > 60 {
            return Err("Give the machine a name (up to 60 characters).".into());
        }
        if self.current().machines.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("You already have a machine called \"{name}\"."));
        }
        let key_path = crate::probe::default_ssh_key().ok_or("There's no SSH key on this computer to sign in with. Make one with `ssh-keygen -t ed25519`.")?;
        let settings = self.settings().cloud;
        let extras = req.install.unwrap_or(settings.install);
        if let Some(bad) = extras.iter().find(|e| !setup::EXTRAS.iter().any(|(id, _)| id == e)) {
            return Err(format!("Muxen doesn't know how to install \"{bad}\". It knows: {}.", setup::EXTRAS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")));
        }
        let op = self.begin_op("cloud-create", format!("Create {name} on DigitalOcean"), None, None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = me.run_cloud_create(op, name, req.region, req.size, key_path, extras).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    async fn run_cloud_create(
        self: &Arc<Self>,
        op: u64,
        name: String,
        region: Option<String>,
        size: Option<String>,
        key_path: String,
        extras: Vec<String>,
    ) -> Result<(), String> {
        let client = self.do_client().await?;
        let options = self.cloud_options().await;
        if let Some(e) = options.error {
            return Err(e);
        }
        let region = region.filter(|r| !r.is_empty()).unwrap_or(options.default_region.clone());
        let size = size.filter(|s| !s.is_empty()).unwrap_or(options.default_size.clone());
        let size_info = options.sizes.iter().find(|s| s.slug == size).ok_or_else(|| format!("DigitalOcean has no size \"{size}\"."))?;
        if !size_info.regions.contains(&region) {
            return Err(format!("{size} isn't available in {region}; pick another size or region."));
        }

        // The SSH key: this computer's default one, added to the account if it isn't there.
        let home = dirs::home_dir().ok_or("No home folder")?;
        let public_key = std::fs::read_to_string(home.join(key_path.trim_start_matches("~/"))).map_err(|e| format!("Couldn't read {key_path}: {e}"))?;
        let keys = client.ssh_keys().await?;
        let key = match keys.into_iter().find(|k| same_key(&k.public_key, &public_key)) {
            Some(k) => k,
            None => {
                self.op_log(op, format!("▸ Adding {key_path} to your DigitalOcean account"));
                client.add_ssh_key(&format!("Muxen · {}", crate::config::local_hostname()), public_key.trim()).await?
            }
        };

        let user = setup::user_name(&std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default());
        let droplet_name = crate::util::slug(&name);
        self.op_log(op, format!("▸ Creating {droplet_name} in {region} ({size}, ${:.3}/h)", size_info.price_hourly));
        let created = client.create_droplet(&droplet_name, &region, &size, key.id, &setup::script(&user, &extras)).await?;
        let id = created.id;

        // Running, with an address.
        self.op_log(op, "▸ Waiting for it to start");
        let mut ip = None;
        for _ in 0..90 {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let d = client.droplet(id).await?;
            if d.status == "active" && d.ipv4.is_some() {
                ip = d.ipv4;
                break;
            }
        }
        let ip = ip.ok_or("It didn't start within 6 minutes. It's still in your DigitalOcean account; destroy it there if you don't want it.")?;
        self.op_log(op, format!("✓ Started at {ip}"));

        // Into the fleet, marked as a cloud machine.
        let machine_id = self.add_machine(NewMachine { name: name.clone(), host: format!("{user}@{ip}") })?;
        let cloud = CloudRef {
            provider: "digitalocean".into(),
            id,
            region: region.clone(),
            size: size.clone(),
            price_hourly: size_info.price_hourly,
            created_at: Utc::now().to_rfc3339(),
        };
        let edit = |text: &str| inventory_edit::set_machine_cloud(text, &machine_id, Some(&cloud));
        inventory_edit::edit_file(self.inventory_path(), edit)?;
        self.reload_inventory_now();

        // Its first boot installs what was asked for; follow along.
        self.op_log(op, "▸ Setting it up (a few minutes)");
        let spec = self.machine(&machine_id).map(|(s, _)| s).ok_or("The new machine went missing from config.toml.")?;
        self.wait_for_setup(op, &spec, Duration::from_secs(20 * 60)).await?;

        if setup::resolve(&extras).contains(&"tailscale") {
            self.join_tailnet(op, &spec).await;
        }

        // Muxen's agent, so terminals, sessions and files work right away.
        self.op_log(op, "▸ Installing Muxen's agent");
        self.run_check(Some(vec![machine_id.clone()])).await;
        let probe = self.machine(&machine_id).and_then(|(_, p)| p);
        match crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await {
            Ok(()) => self.op_log(op, "✓ Agent installed"),
            Err(e) => self.op_log(op, format!("! The agent didn't install yet ({e}); Muxen tries again when you open a terminal")),
        }
        self.op_log(op, format!("✓ {name} is ready"));
        Ok(())
    }

    /// Polls the new machine until its setup script finishes, logging each step once.
    async fn wait_for_setup(self: &Arc<Self>, op: u64, spec: &MachineSpec, timeout: Duration) -> Result<(), String> {
        let started = std::time::Instant::now();
        let mut seen = String::new();
        let check = format!("if [ -f {} ]; then echo muxen-ready; fi; grep -E '^muxen-(step|warn):' {} 2>/dev/null | tail -n 4", setup::READY, setup::LOG);
        while started.elapsed() < timeout {
            tokio::time::sleep(Duration::from_secs(6)).await;
            let Ok(out) = tokio::process::Command::new("ssh").args(crate::probe::ssh_args(spec)).arg(&check).output().await else { continue };
            if !out.status.success() {
                continue; // Not answering ssh yet.
            }
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            for line in text.lines() {
                if let Some(step) = line.strip_prefix("muxen-step: ").or(line.strip_prefix("muxen-warn: ")) {
                    if !seen.contains(&format!("|{step}|")) {
                        seen.push_str(&format!("|{step}|"));
                        let mark = if line.starts_with("muxen-warn") { "!" } else { "▸" };
                        self.op_log(op, format!("  {mark} {step}"));
                    }
                }
            }
            if text.contains("muxen-ready") {
                return Ok(());
            }
        }
        Err(format!("Setup didn't finish within {} minutes. It's been added; check {} on it.", timeout.as_secs() / 60, setup::LOG))
    }

    /// Joins the machine to the user's tailnet: with the saved auth key, or by
    /// opening Tailscale's approval page in the browser here.
    async fn join_tailnet(self: &Arc<Self>, op: u64, spec: &MachineSpec) {
        let hostname = crate::util::slug(&spec.name);
        match secrets::get(&self.paths().dir, TAILSCALE_KEY) {
            Some(key) => {
                let mut cmd = tokio::process::Command::new("ssh");
                cmd.args(crate::probe::ssh_args(spec)).arg(format!("sudo tailscale up --auth-key=file:/dev/stdin --hostname={hostname}"));
                cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
                let joined = match cmd.spawn() {
                    Ok(mut child) => {
                        if let Some(mut stdin) = child.stdin.take() {
                            use tokio::io::AsyncWriteExt;
                            let _ = stdin.write_all(key.as_bytes()).await;
                        }
                        child.wait_with_output().await.map(|o| o.status.success()).unwrap_or(false)
                    }
                    Err(_) => false,
                };
                self.op_log(op, if joined { "✓ Joined your tailnet" } else { "! Couldn't join your tailnet with the saved key" });
            }
            None => {
                // No key: start the login and hand its approval link to the browser.
                let script = format!("sudo sh -c 'nohup tailscale up --hostname={hostname} >/tmp/muxen-tailscale.log 2>&1 &'; sleep 4; grep -o 'https://login.tailscale.com/[^ ]*' /tmp/muxen-tailscale.log | head -n 1");
                let out = tokio::process::Command::new("ssh").args(crate::probe::ssh_args(spec)).arg(script).output().await;
                match out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|u| u.starts_with("https://")) {
                    Some(url) => {
                        self.op_log(op, "▸ Approve it in the browser to join your tailnet");
                        crate::util::open_url(&url);
                    }
                    None => self.op_log(op, "! Tailscale is installed; run `sudo tailscale up` on it to join your tailnet"),
                }
            }
        }
    }

    /// Destroys a cloud machine: deletes the droplet and removes it from Muxen.
    pub fn cloud_destroy(self: &Arc<Self>, machine_id: &str) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxen anymore.")?;
        let cloud = spec.cloud.clone().ok_or_else(|| format!("{} isn't a cloud machine Muxen created.", spec.name))?;
        if self.machine_busy(&spec.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't destroy cloud machines.".into());
        }
        let op = self.begin_op("cloud-destroy", format!("Destroy {}", spec.name), Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = async {
                let client = me.do_client().await?;
                me.op_log(op, format!("▸ Deleting droplet {} from DigitalOcean", cloud.id));
                client.delete_droplet(cloud.id).await?;
                // Its cost this month still counts.
                me.record_destroyed(&spec.id, crate::config::DestroyedCloud { name: spec.name.clone(), cloud: cloud.clone(), destroyed_at: Utc::now() });
                me.op_log(op, format!("▸ Removing {} from config.toml", spec.name));
                // Not remove_machine: this operation is the one keeping the machine busy.
                me.forget_machine(&spec.id)?;
                // Its address may be reused by someone else's droplet.
                if let Some(ip) = spec.host.rsplit('@').next() {
                    let _ = tokio::process::Command::new("ssh-keygen").args(["-R", ip]).output().await;
                }
                me.op_log(op, "✓ Destroyed");
                Ok(())
            }
            .await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    /// Powers a cloud machine off (cleanly) or on. DigitalOcean still bills it while it's off.
    pub fn cloud_power(self: &Arc<Self>, machine_id: &str, on: bool) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxen anymore.")?;
        let cloud = spec.cloud.clone().ok_or_else(|| format!("{} isn't a cloud machine Muxen created.", spec.name))?;
        if self.machine_busy(&spec.id) {
            return Err("An operation is still running on this machine. Wait for it to finish.".into());
        }
        if self.demo_world().is_some() {
            return Err("The demo doesn't power cloud machines on or off.".into());
        }
        let title = format!("{} {}", if on { "Power on" } else { "Power off" }, spec.name);
        let op = self.begin_op(if on { "cloud-power-on" } else { "cloud-power-off" }, title, Some(spec.id.clone()), None);
        let me = self.clone();
        self.runtime().spawn(async move {
            let result = me.run_cloud_power(op, &spec, cloud.id, on).await;
            super::finish(&me, op, result);
        });
        Ok(op)
    }

    /// Asks DigitalOcean whether a droplet that didn't answer a check is
    /// powered off (best effort: not signed in, or a blip, changes nothing).
    pub(crate) async fn refresh_droplet_status(self: &Arc<Self>, machine_id: &str, droplet: u64) {
        let Ok(client) = self.do_client().await else { return };
        if let Ok(d) = client.droplet(droplet).await {
            self.set_droplet_status(machine_id, &d.status);
        }
    }

    /// Powers the droplet on or off and waits until it really is: on means
    /// answering ssh again, off means DigitalOcean reports it off.
    async fn run_cloud_power(self: &Arc<Self>, op: u64, spec: &MachineSpec, droplet: u64, on: bool) -> Result<(), String> {
        let client = self.do_client().await?;
        let want = if on { "active" } else { "off" };
        if client.droplet(droplet).await?.status == want {
            self.set_droplet_status(&spec.id, want);
            self.op_log(op, if on { "It's already on." } else { "It's already off." });
        } else {
            self.op_log(op, if on { "▸ Powering on" } else { "▸ Shutting down" });
            client.droplet_action(droplet, if on { "power_on" } else { "shutdown" }).await?;
            let started = std::time::Instant::now();
            let mut forced = false;
            loop {
                tokio::time::sleep(Duration::from_secs(4)).await;
                // A blip while polling isn't a failure; the next poll tries again.
                if client.droplet(droplet).await.is_ok_and(|d| d.status == want) {
                    self.set_droplet_status(&spec.id, want);
                    break;
                }
                let waited = started.elapsed();
                if !on && !forced && waited > Duration::from_secs(60) {
                    self.op_log(op, "! It didn't shut down within a minute; turning it off");
                    client.droplet_action(droplet, "power_off").await?;
                    forced = true;
                }
                if waited > Duration::from_secs(180) {
                    return Err(format!("DigitalOcean didn't report it {} within 3 minutes.", if on { "on" } else { "off" }));
                }
            }
        }
        if on {
            self.op_log(op, "▸ Waiting for it to answer ssh");
            let started = std::time::Instant::now();
            loop {
                let ok = tokio::process::Command::new("ssh").args(crate::probe::ssh_args(spec)).arg("true").output().await.is_ok_and(|o| o.status.success());
                if ok {
                    break;
                }
                if started.elapsed() > Duration::from_secs(120) {
                    return Err("It's on, but hasn't answered ssh within 2 minutes.".into());
                }
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        }
        self.run_check(Some(vec![spec.id.clone()])).await;
        self.op_log(op, if on { format!("✓ {} is on", spec.name) } else { format!("✓ {} is off (DigitalOcean still bills for it until it's destroyed)", spec.name) });
        Ok(())
    }

    /// What the cloud machines cost: each one (destroyed ones this month
    /// too), the totals, and the account's bill when the sign-in may read it.
    pub async fn cloud_usage(self: &Arc<Self>) -> usage::CloudUsage {
        let now = Utc::now();
        let month = usage::month_start(now);
        let snap = self.current();
        let mut machines = Vec::new();
        let mut if_kept = 0.;
        for m in &snap.machines {
            let Some(c) = &m.cloud else { continue };
            let created = c.created_at.unwrap_or(now);
            if_kept += usage::month_if_kept(c.price_hourly, created, now);
            machines.push(usage::UsageLine {
                machine_id: Some(m.id.clone()),
                name: m.name.clone(),
                region: c.region.clone(),
                size: c.size.clone(),
                power: c.power.clone(),
                price_hourly: c.price_hourly,
                price_monthly: usage::monthly_price(c.price_hourly),
                created_at: c.created_at,
                destroyed_at: None,
                this_month: usage::cost_in_month(c.price_hourly, created, None, month, now),
                since_created: usage::cost_since(c.price_hourly, created, None, now),
            });
        }
        // fold, not sum: an empty f64 sum is -0.0 ("$-0.00").
        let hourly_now = machines.iter().fold(0., |t, l| t + l.price_hourly);
        for h in self.cloud_history() {
            let created = DateTime::parse_from_rfc3339(&h.cloud.created_at).map(|t| t.with_timezone(&Utc)).unwrap_or(h.destroyed_at);
            let this_month = usage::cost_in_month(h.cloud.price_hourly, created, Some(h.destroyed_at), month, now);
            if this_month <= 0. {
                continue;
            }
            if_kept += this_month;
            machines.push(usage::UsageLine {
                machine_id: None,
                name: h.name.clone(),
                region: h.cloud.region.clone(),
                size: h.cloud.size.clone(),
                power: Some("destroyed".into()),
                price_hourly: h.cloud.price_hourly,
                price_monthly: usage::monthly_price(h.cloud.price_hourly),
                created_at: Some(created),
                destroyed_at: Some(h.destroyed_at),
                this_month,
                since_created: usage::cost_since(h.cloud.price_hourly, created, Some(h.destroyed_at), now),
            });
        }
        let this_month = machines.iter().fold(0., |t, l| t + l.this_month);

        let (mut account, mut account_error, mut account_needs_sign_in) = (None, None, false);
        if self.demo_world().is_none() {
            let method = self.settings().cloud.digitalocean_auth;
            match self.do_client().await {
                Ok(client) => match client.balance().await {
                    Ok(b) => account = Some(b),
                    // Not allowed to read the bill: say what would allow it, and what DigitalOcean said.
                    Err(e) if e.contains("refused") => {
                        let said = e.trim_start_matches("DigitalOcean refused: ").trim_end_matches(" (the token may be read-only)").to_string();
                        let granted = (method == "oauth").then(|| auth::granted_scopes(&self.paths().dir)).flatten();
                        account_needs_sign_in = method == "oauth" && granted.as_deref().is_none_or(|g| !g.split_whitespace().any(|s| s == "billing:read"));
                        account_error = Some(match method.as_str() {
                            // It says "not authorized" even with billing:read granted, at least for a new account.
                            "oauth" if !account_needs_sign_in => "DigitalOcean isn't sharing the bill yet; a new account may not have one.".to_string(),
                            "oauth" => format!("Sign in again so Muxen can read your DigitalOcean bill. ({said})"),
                            "doctl" => format!("doctl's token can't read billing; it needs billing:read. ({said})"),
                            _ => format!("This API token can't read billing; it needs billing:read. ({said})"),
                        });
                    }
                    Err(e) => account_error = Some(e),
                },
                Err(_) => account_error = Some("Not signed in to DigitalOcean.".into()),
            }
        }
        usage::CloudUsage {
            currency: "USD",
            month: now.format("%Y-%m").to_string(),
            hourly_now: (hourly_now * 100_000.).round() / 100_000.,
            this_month: usage::round2(this_month),
            month_if_kept: usage::round2(if_kept),
            machines,
            account,
            account_error,
            account_needs_sign_in,
            billing: usage::BILLING,
        }
    }

    /// Watches cloud machines for idleness (every 10 minutes) and does what
    /// Settings › Cloud says. Run by one process (the app).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn start_cloud_idle_watch(self: &Arc<Self>) {
        if self.demo_world().is_some() {
            return;
        }
        let me = self.clone();
        self.runtime().spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                me.idle_tick();
            }
        });
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    fn idle_tick(self: &Arc<Self>) {
        let settings = self.settings().cloud;
        let snapshot = self.current();
        let now = Utc::now();
        let mut actions = Vec::new();
        {
            let mut guard = IDLE.lock().unwrap();
            let state = guard.get_or_insert_with(IdleState::default);
            for m in snapshot.machines.iter().filter(|m| m.cloud.is_some()) {
                let busy = snapshot.sessions.iter().any(|x| x.machine_id == m.id && x.busy)
                    || m.summary.working > 0
                    || m.probe.as_ref().and_then(|p| p.cpu_pct).is_some_and(|c| c > 20.);
                let last = state.last_active.entry(m.id.clone()).or_insert(now);
                if busy {
                    *last = now;
                    state.acted.remove(&m.id);
                    continue;
                }
                let idle_hours = (now - *last).num_minutes() as f64 / 60.;
                if settings.idle_hours > 0 && idle_hours >= settings.idle_hours as f64 && !state.acted.contains(&m.id) && m.status == "online" {
                    state.acted.insert(m.id.clone());
                    actions.push(m.id.clone());
                }
            }
        }
        for id in actions {
            let result = match settings.idle_action.as_str() {
                "poweroff" => self.cloud_power(&id, false).map(|_| ()),
                "destroy" => self.cloud_destroy(&id).map(|_| ()),
                _ => Ok(()), // "warn": the machine shows it (Needs attention).
            };
            if let Err(e) = result {
                tracing::warn!("idle {id}: {e}");
            }
        }
        self.republish();
    }

    /// Signs in to DigitalOcean the chosen way. `token` for "token"; `open`
    /// opens the browser for "oauth".
    pub async fn cloud_sign_in(self: &Arc<Self>, method: &str, token: Option<String>, open: impl FnOnce(&str)) -> Result<String, String> {
        let dir = self.paths().dir.clone();
        match method {
            "token" => auth::save_token(&dir, token.as_deref().unwrap_or(""))?,
            "oauth" => auth::oauth_sign_in(&dir, open).await?,
            "doctl" => {}
            other => return Err(format!("Unknown sign-in \"{other}\".")),
        }
        let mut s = self.settings();
        s.cloud.digitalocean_auth = method.to_string();
        self.update_settings(s)?;
        let account = self.do_client().await?.account().await?;
        Ok(account.email)
    }

    pub fn cloud_sign_out(self: &Arc<Self>) {
        let method = self.settings().cloud.digitalocean_auth;
        auth::sign_out(&method, &self.paths().dir);
    }
}

/// How long a machine has been idle, as the idle watch sees it.
pub fn idle_since(machine_id: &str) -> Option<DateTime<Utc>> {
    IDLE.lock().ok()?.as_ref()?.last_active.get(machine_id).copied()
}

/// A made-up account, regions and sizes for the demo (it never creates anything).
fn demo_options(mut out: CloudOptions) -> CloudOptions {
    use crate::cloud::{Account, Region, Size};
    let regions = ["nyc1", "nyc3", "sfo3", "tor1", "ams3", "fra1", "lon1", "sgp1", "blr1", "syd1", "atl1", "ric1"];
    let sizes: [(&str, u32, u32, u32, f64); 6] =
        [("s-1vcpu-1gb", 1, 1, 25, 6.), ("s-1vcpu-2gb", 1, 2, 50, 12.), ("s-2vcpu-2gb", 2, 2, 60, 18.), ("s-2vcpu-4gb", 2, 4, 80, 24.), ("s-4vcpu-8gb", 4, 8, 160, 48.), ("s-8vcpu-16gb", 8, 16, 320, 96.)];
    let all: Vec<String> = regions.iter().map(|r| r.to_string()).collect();
    out.signed_in = true;
    out.account = Some(Account { email: "you@example.com".into(), team: Some("Demo team".into()), droplet_limit: 25, status: "active".into() });
    let name = |slug: &str| {
        let city = match &slug[..3] {
            "nyc" => "New York",
            "sfo" => "San Francisco",
            "tor" => "Toronto",
            "ams" => "Amsterdam",
            "fra" => "Frankfurt",
            "lon" => "London",
            "sgp" => "Singapore",
            "blr" => "Bangalore",
            "syd" => "Sydney",
            "atl" => "Atlanta",
            _ => "Richmond",
        };
        format!("{city} {}", &slug[3..])
    };
    out.regions = regions.iter().map(|r| Region { slug: r.to_string(), name: name(r), sizes: Vec::new() }).collect();
    out.sizes = sizes
        .iter()
        .map(|(slug, vcpus, gb, disk, monthly)| Size {
            slug: slug.to_string(),
            vcpus: *vcpus,
            memory_mb: gb * 1024,
            disk_gb: *disk,
            price_hourly: (monthly / 672. * 1000.).round() / 1000.,
            price_monthly: *monthly,
            regions: all.clone(),
            description: String::new(),
        })
        .collect();
    out.default_region = "nyc3".into();
    out
}
