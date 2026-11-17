//! Cloud machine operations: create a droplet and make it one of your machines,
//! power it off or on, destroy it; and the idle watch that warns, powers off
//! or destroys machines nobody is using (as the user chose in Settings › Cloud).
//!
//! A droplet is billed from the moment DigitalOcean makes it, so Muxin never
//! has one it doesn't list. Creating writes down what it's about to ask for
//! (cloud::pending), and the droplet goes into config.toml the moment
//! DigitalOcean answers, as a machine with no address yet
//! (`MachineSpec::awaits_address`). If the Muxin that asked is quit, crashes
//! or loses the answer, the next machine check of any Muxin catches up
//! (`cloud_catch_up`): it finds a droplet whose answer was lost by its name
//! and when it was made, and fills in an address once the droplet has
//! started. Until then the machine is listed, costed (`cloud_usage`) and can
//! be destroyed; and what was asked for and can't be settled is shown to the
//! user (`status.attention`) for as long as that's so.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::inventory_edit;
use crate::cloud::digitalocean::{Client, Droplet, Failure, same_key};
use crate::cloud::{CloudCreate, CloudOptions, TAILSCALE_KEY, auth, nearest_region, pending, secrets, setup, usage};
use crate::config::{CloudRef, MachineSpec};
use crate::fleet::Engine;

/// When each cloud machine was last in use (this run of the app), and the ones
/// the idle watch already acted on. By machine id, for the droplet that was
/// under that id then (`droplets`): a machine destroyed and made again under
/// the same name is another droplet, and starts with a clock of its own.
#[derive(Default)]
pub struct IdleState {
    pub last_active: HashMap<String, DateTime<Utc>>,
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    droplets: HashMap<String, u64>,
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    acted: std::collections::HashSet<String>,
}

impl IdleState {
    fn forget(&mut self, machine_id: &str) {
        self.last_active.remove(machine_id);
        self.droplets.remove(machine_id);
        self.acted.remove(machine_id);
    }

    /// The idle clock of a machine's droplet, started now if this is the
    /// first look at it. A clock kept for another droplet (the machine that
    /// had this id before) isn't this one's, and goes.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    fn clock(&mut self, machine_id: &str, droplet: u64, now: DateTime<Utc>) -> &mut DateTime<Utc> {
        if self.droplets.get(machine_id).is_some_and(|d| *d != droplet) {
            self.forget(machine_id);
        }
        self.droplets.insert(machine_id.to_string(), droplet);
        self.last_active.entry(machine_id.to_string()).or_insert(now)
    }

    /// Drops what's kept for machines that aren't cloud machines anymore
    /// (destroyed or removed, by this Muxin or another).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    fn keep_only(&mut self, machine_ids: &std::collections::HashSet<&str>) {
        self.last_active.retain(|id, _| machine_ids.contains(id.as_str()));
        self.droplets.retain(|id, _| machine_ids.contains(id.as_str()));
        self.acted.retain(|id| machine_ids.contains(id.as_str()));
    }
}

pub static IDLE: Mutex<Option<IdleState>> = Mutex::new(None);

/// Forgets how long a machine has been idle: it's gone (destroyed, or taken
/// out of Muxin), and one made later under its name isn't it.
pub(crate) fn forget_idle(machine_id: &str) {
    if let Some(state) = IDLE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        state.forget(machine_id);
    }
}

/// How many times the Muxin that asked for a droplet looks for it when the
/// answer was lost, and how long between looks: until DigitalOcean makes nothing for
/// it anymore (`pending::WINDOW_SECS` and the clocks' slack), and done well
/// before another Muxin takes the entry up (`pending::SETTLE_SECS`).
const LOOKS: u32 = 12;
const LOOK_EVERY: Duration = Duration::from_secs(15);

/// How long a destroy keeps asking DigitalOcean to delete a droplet it's
/// still making: (between asks, asks). Two minutes.
const DELETE_WAIT: (Duration, u32) = (Duration::from_secs(5), 24);

/// What DigitalOcean has for a droplet that was asked for and not heard of.
enum Look {
    /// The droplet it made.
    Made(Droplet),
    /// Nothing, and it makes nothing for it anymore.
    NotMade,
    /// Nothing so far; it still can.
    NotYet,
    /// More than one that fits (their ids): which is Muxin's can't be told.
    Several(Vec<u64>),
}

/// Where a cloud machine with no address stands.
pub(crate) enum Start {
    /// Started: its ssh destination ("user@203.0.113.7"), now in config.toml.
    Started(String),
    NotYet,
    /// DigitalOcean has no such droplet (in the account that's signed in).
    Gone,
}

/// Why DigitalOcean couldn't be asked about a machine. `lasting`: asking
/// again changes nothing until the user does something (signs in again).
pub(crate) struct Blocked {
    pub message: String,
    pub lasting: bool,
}

impl From<Failure> for Blocked {
    fn from(f: Failure) -> Blocked {
        Blocked { lasting: f.signed_out(), message: f.message }
    }
}

impl From<String> for Blocked {
    fn from(message: String) -> Blocked {
        Blocked { message, lasting: false }
    }
}

impl Engine {
    async fn do_client(&self) -> Result<Client, String> {
        #[cfg(test)]
        if let Some(s) = crate::cloud::digitalocean::stand_in::of(&self.paths().dir) {
            return Ok(Client::for_stand_in(&s.api));
        }
        let method = self.settings().cloud.digitalocean_auth;
        auth::token(&method, &self.paths().dir).await.map(Client::new)
    }

    /// Whether there's a DigitalOcean sign-in to use (without asking DigitalOcean).
    fn do_signed_in(&self) -> bool {
        #[cfg(test)]
        if crate::cloud::digitalocean::stand_in::of(&self.paths().dir).is_some() {
            return true;
        }
        auth::signed_in(&self.settings().cloud.digitalocean_auth, &self.paths().dir)
    }

    /// How long a new droplet gets to start: (between asks, asks). 6 minutes.
    fn start_wait(&self) -> (Duration, u32) {
        #[cfg(test)]
        if let Some(wait) = crate::cloud::digitalocean::stand_in::of(&self.paths().dir).and_then(|s| s.start_wait) {
            return wait;
        }
        (Duration::from_secs(4), 90)
    }

    /// How long to pause between asks in the other waits: `usual`, or a
    /// test's shorter one.
    fn pause(&self, usual: Duration) -> Duration {
        #[cfg(test)]
        if let Some(pause) = crate::cloud::digitalocean::stand_in::of(&self.paths().dir).and_then(|s| s.pause) {
            return pause;
        }
        usual
    }

    /// The account, regions, sizes and defaults for a new cloud machine.
    pub async fn cloud_options(self: &Arc<Self>) -> CloudOptions {
        let settings = self.settings().cloud;
        let mut out = CloudOptions {
            provider: "digitalocean".into(),
            method: settings.digitalocean_auth.clone(),
            signed_in: self.do_signed_in(),
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
                // The everyday sizes: basic ("s-"), general purpose ("g-") and CPU-optimized ("c-") droplets.
                out.sizes = s.into_iter().filter(|s| ["s-", "g-", "c-"].iter().any(|p| s.slug.starts_with(p)) && s.memory_mb >= 1024).collect();
                // Defaults that work: a region with sizes on offer (DigitalOcean
                // offers none in some datacenters), and a size on offer there.
                let on_offer: Vec<String> = r.iter().map(|r| r.slug.clone()).filter(|slug| crate::cloud::offered(&out.sizes, slug).next().is_some()).collect();
                let offset = chrono::Local::now().offset().local_minus_utc() / 3600;
                out.default_region = Some(settings.region.clone()).filter(|r| on_offer.contains(r)).or_else(|| nearest_region(offset, &on_offer)).unwrap_or_default();
                let mut there = crate::cloud::offered(&out.sizes, &out.default_region);
                if !crate::cloud::offered(&out.sizes, &out.default_region).any(|s| s.slug == out.default_size) {
                    out.default_size = there.next().map(|s| s.slug.clone()).unwrap_or_default();
                }
                out.regions = r;
            }
            (a, r, s) => out.error = a.err().or(r.err()).or(s.err()),
        }
        out
    }

    /// Creates a droplet, adds it to the machines the moment it exists, and
    /// waits for it to start and set itself up. Returns the operation that
    /// tracks it.
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
        #[cfg(not(feature = "recipes"))]
        if let Some(bad) = extras.iter().find(|e| !setup::EXTRAS.iter().any(|(id, _)| id == e)) {
            return Err(format!("Muxin doesn't know how to install \"{bad}\". It knows: {}.", setup::EXTRAS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")));
        }
        // With recipes, any component, as for a VM (they're applied over ssh, below).
        #[cfg(feature = "recipes")]
        {
            let catalog = crate::recipe::catalog()?;
            catalog.resolve(&extras).map_err(|e| if e.starts_with("No component") { format!("{e} Muxin knows: {}.", catalog.ids().join(", ")) } else { e })?;
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
        // A create that was cut off earlier is settled first, so the same
        // droplet is never asked for twice.
        for started in self.catch_up(&client).await {
            self.trigger_check(Some(started));
        }
        if self.current().machines.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("You already have a machine called \"{name}\"."));
        }
        let dir = self.paths().dir.clone();
        let droplet_name = crate::util::slug(&name);
        // Two droplets of one name couldn't be told apart, should an answer be lost.
        let asked_already = || format!("Muxin has asked DigitalOcean for \"{name}\" and hasn't heard back. It's finding out whether the droplet was made: try again in a few minutes.");
        if pending::read(&dir).iter().any(|a| a.droplet.eq_ignore_ascii_case(&droplet_name)) {
            return Err(asked_already());
        }
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
                client.add_ssh_key(&format!("Muxin · {}", crate::config::local_hostname()), public_key.trim()).await?
            }
        };

        let user = local_user();
        // Written down before asking: the droplet is billed from the moment
        // DigitalOcean makes it, whether or not its answer gets here. With
        // how far its clock is from this one's (it has answered by now):
        // when it made a droplet is by its clock.
        let asked = pending::Asked {
            name: name.clone(),
            droplet: droplet_name.clone(),
            region: region.clone(),
            size: size.clone(),
            price_hourly: size_info.price_hourly,
            user: user.clone(),
            asked_at: Utc::now(),
            clock_ahead_secs: client.clock_ahead(),
            problem: None,
        };
        // The name is taken from here on: checked again (the asks above took
        // a moment, and another create may have started meanwhile) and
        // written down in one step.
        self.refresh_from_disk();
        if self.current().machines.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("You already have a machine called \"{name}\"."));
        }
        if !pending::claim(&dir, &asked)? {
            return Err(asked_already());
        }
        self.op_log(op, format!("▸ Creating {droplet_name} in {region} ({size}, ${:.3}/h)", size_info.price_hourly));
        // With recipes, the extras are applied over ssh once its first boot is done (below).
        #[cfg(feature = "recipes")]
        let (recipe, extras) = (extras, Vec::<String>::new());
        let made = match client.create_droplet(&droplet_name, &region, &size, key.id, &setup::script(&user, &extras)).await {
            Ok(d) => d,
            // Turned down, or never sent: there's no droplet.
            Err(e) if !e.maybe_made => {
                pending::remove(&dir, &asked);
                return Err(e.message);
            }
            // The answer was lost. DigitalOcean may have made it all the same.
            Err(e) => {
                let said = sentence(e.message.trim_end_matches(" Try again in a moment."));
                self.op_log(op, format!("! {said}"));
                self.op_log(op, "▸ Asking DigitalOcean whether it made the droplet all the same");
                self.look_for_made(op, &client, &asked, &said).await?
            }
        };

        // Into the fleet right away, marked as a cloud machine: it exists
        // and is billed, so it's listed, costed and can be destroyed from
        // here on, whatever happens to this operation. Its address comes later.
        let machine_id = self
            .record_droplet(&asked, &made)
            .map_err(|e| format!("DigitalOcean made the droplet ({}), and it's billed from now, but Muxin couldn't add it to config.toml: {} Muxin tries again at the next machine check.", made.id, sentence(&e)))?;
        self.op_log(op, "✓ Created; it's in your machines, and DigitalOcean bills for it from now");

        // Running, with an address.
        self.op_log(op, "▸ Waiting for it to start");
        let (every, asks) = self.start_wait();
        let mut host = None;
        // How many asks in a row DigitalOcean has said it has no such droplet.
        let mut gone = 0;
        for _ in 0..asks {
            tokio::time::sleep(every).await;
            // Another Muxin may have destroyed it meanwhile.
            self.refresh_from_disk();
            if !self.still_droplet(&machine_id, made.id) {
                return Err(format!("{name} was destroyed (or taken out of config.toml) while it was being created."));
            }
            match self.fill_address(&client, &machine_id).await {
                Ok(Start::Started(h)) => {
                    host = Some(h);
                    break;
                }
                Ok(Start::NotYet) => gone = 0,
                // Destroyed from Muxin, the droplet goes a moment before the
                // machine does: that's waited out (the check above ends this
                // then), and isn't "deleted at DigitalOcean".
                Ok(Start::Gone) if gone < 2 || self.machine_busy(&machine_id) => gone += 1,
                Ok(Start::Gone) => {
                    return Err(format!(
                        "DigitalOcean doesn't have {name}'s droplet ({}) anymore: it was deleted there while it was starting, so it isn't billed. It's still in your machines: destroy it from its page to take it out.",
                        made.id
                    ));
                }
                // Signed out meanwhile: no amount of waiting gets its address.
                Err(b) if b.lasting => {
                    return Err(format!(
                        "{} {name} was created and stays in your machines, and DigitalOcean bills for it: once you're signed in again, Muxin adds its address at a machine check (what it was to come with isn't installed then).",
                        sentence(&b.message)
                    ));
                }
                // A blip while asking isn't a failure; the next ask tries again.
                Err(_) => {}
            }
        }
        let host = host.ok_or_else(|| {
            format!("It didn't start within 6 minutes. It stays in your machines as {name}, and DigitalOcean bills for it: Muxin adds its address at a machine check once it has started (what it was to come with isn't installed then). Destroy it if you don't want it.")
        })?;
        self.op_log(op, format!("✓ Started at {}", host.rsplit('@').next().unwrap_or(&host)));
        self.trigger_check(Some(machine_id.clone()));

        // Its first boot installs what was asked for; follow along.
        self.op_log(op, "▸ Setting it up (a few minutes)");
        let spec = self.machine(&machine_id).map(|(s, _)| s).ok_or("The new machine went missing from config.toml.")?;
        self.wait_for_setup(op, &spec, made.id, Duration::from_secs(20 * 60)).await?;
        #[cfg(feature = "recipes")]
        self.recipe_setup(op, &machine_id, &recipe).await;
        #[cfg(feature = "recipes")]
        let extras = recipe;

        if setup::resolve(&extras).contains(&"tailscale") {
            self.join_tailnet(op, &spec).await;
        }

        // Muxin's agent, so terminals, sessions and files work right away.
        self.op_log(op, "▸ Installing Muxin's agent");
        self.run_check(Some(vec![machine_id.clone()])).await;
        let probe = self.machine(&machine_id).and_then(|(_, p)| p);
        match crate::fleet::sessions::agent::ensure(&spec, probe.as_ref()).await {
            Ok(()) => self.op_log(op, "✓ Agent installed"),
            Err(e) => self.op_log(op, format!("! The agent didn't install yet ({e}); Muxin tries again when you open a terminal")),
        }
        self.connect_directly_after_setup(op, &machine_id).await;
        self.op_log(op, format!("✓ {name} is ready"));
        Ok(())
    }

    /// Polls the new machine until its setup script finishes, logging each step once.
    async fn wait_for_setup(self: &Arc<Self>, op: u64, spec: &MachineSpec, droplet: u64, timeout: Duration) -> Result<(), String> {
        let started = std::time::Instant::now();
        let mut seen = String::new();
        let check = format!("if [ -f {} ]; then echo muxen-ready; fi; grep -E '^muxen-(step|warn):' {} 2>/dev/null | tail -n 4", setup::READY, setup::LOG);
        let every = self.pause(Duration::from_secs(6));
        while started.elapsed() < timeout {
            tokio::time::sleep(every).await;
            // Destroyed meanwhile, by this Muxin or another (which only
            // config.toml shows): its address may be someone else's machine by now.
            self.refresh_from_disk();
            if !self.still_droplet(&spec.id, droplet) {
                return Err(format!("{} was destroyed (or taken out of config.toml) while it was being set up.", spec.name));
            }
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

    /// Looks for the droplet of a create whose answer was lost until it's
    /// sure: there it is, or DigitalOcean makes nothing for it anymore. The
    /// Muxin that asked does this itself, while its operation runs: a command
    /// has nothing running later to look again with.
    async fn look_for_made(self: &Arc<Self>, op: u64, client: &Client, asked: &pending::Asked, said: &str) -> Result<Droplet, String> {
        let dir = self.paths().dir.clone();
        let pause = self.pause(LOOK_EVERY);
        // Done a good while before another Muxin takes the entry up.
        let give_up = asked.asked_at + chrono::Duration::seconds(pending::SETTLE_SECS - 60);
        let mut told = false;
        let mut trouble = None;
        for look in 1..=LOOKS {
            self.refresh_from_disk();
            match self.find_made(client, asked).await {
                Ok(Look::Made(d)) => {
                    self.op_log(op, "✓ It did");
                    return Ok(d);
                }
                Ok(Look::NotMade) => {
                    pending::remove(&dir, asked);
                    self.republish();
                    return Err(format!("{said} DigitalOcean made no droplet for it, so nothing is billed. Try again."));
                }
                Ok(Look::Several(ids)) => {
                    let why = several(asked, &ids);
                    pending::set_problem(&dir, asked, Some(why.clone()));
                    self.republish();
                    return Err(format!("{said} {why}"));
                }
                Ok(Look::NotYet) => {
                    trouble = None;
                    if !told {
                        self.op_log(op, "▸ It isn't listed. DigitalOcean can still make it for a couple of minutes: looking again until then");
                        told = true;
                    }
                }
                Err(why) => trouble = Some(why),
            }
            if look == LOOKS || Utc::now() >= give_up {
                break;
            }
            tokio::time::sleep(pause).await;
        }
        // Not settled: it stays written down, is shown to the user, and is
        // looked for at every machine check.
        let why = match trouble {
            Some(e) => format!("Muxin couldn't find out whether DigitalOcean made the droplet all the same ({}).", e.trim_end_matches('.')),
            None => "DigitalOcean doesn't list the droplet yet, and may still be making it.".to_string(),
        };
        pending::set_problem(&dir, asked, Some(why.clone()));
        self.republish();
        Err(format!("{said} {why} Muxin asks again at every machine check, and adds it to your machines if it's there; until then it's listed under what needs attention."))
    }

    /// Brings Muxin level with DigitalOcean after a create that was cut off
    /// (Muxin quit or crashed, or the answer was lost): a droplet asked for
    /// and never heard of is looked for by name and added to the machines,
    /// and a cloud machine with no address gets one once its droplet has
    /// started (and is checked then, without waiting for the check). Run
    /// with every machine check, by any Muxin process; it asks DigitalOcean
    /// nothing when there's nothing to catch up with. Best effort: not
    /// signed in, or a blip, and the next check tries again.
    pub(crate) async fn cloud_catch_up(self: &Arc<Self>) {
        for id in self.cloud_catch_up_quietly().await {
            self.trigger_check(Some(id));
        }
    }

    /// The same, for a machine check: a machine that got its address is
    /// checked before this returns, so the check that started it has it.
    /// `named`: the machines that check names. One of them that DigitalOcean
    /// said it doesn't have is asked about once more (the user may have
    /// signed in to the account it's in since).
    pub(crate) async fn cloud_catch_up_and_check(self: &Arc<Self>, named: Vec<String>) {
        let mut started = self.cloud_catch_up_quietly().await;
        let snap = self.current();
        for id in named {
            let gone = snap.machines.iter().any(|m| m.id == id && m.droplet_gone());
            if gone && matches!(self.cloud_start(&id).await, Ok(Start::Started(_))) {
                started.push(id);
            }
        }
        if !started.is_empty() {
            self.check_now(started).await;
        }
    }

    /// Catches up; returns the machines that got their address.
    async fn cloud_catch_up_quietly(self: &Arc<Self>) -> Vec<String> {
        if self.demo_world().is_some() || !self.cloud_behind() {
            return Vec::new();
        }
        match self.do_client().await {
            Ok(client) => self.catch_up(&client).await,
            Err(e) => {
                tracing::warn!("a cloud machine is waiting on DigitalOcean, and Muxin can't ask it: {e}");
                // Said where the user reads it: what's waiting says why it still is.
                let dir = self.paths().dir.clone();
                let now = Utc::now();
                for asked in pending::read(&dir).into_iter().filter(|a| a.settled(now)) {
                    pending::set_problem(&dir, &asked, Some(format!("Muxin can't ask DigitalOcean whether it made it: {}", sentence(&e))));
                }
                self.set_cloud_trouble(Some(sentence(&e)));
                Vec::new()
            }
        }
    }

    /// A check of these machines, behind a box: a machine check starts the
    /// catch-up that asks for this one.
    fn check_now(self: &Arc<Self>, machine_ids: Vec<String>) -> futures::future::BoxFuture<'static, ()> {
        let me = self.clone();
        Box::pin(async move { me.run_check(Some(machine_ids)).await })
    }

    /// Whether there's anything to catch up with (without asking DigitalOcean).
    pub(crate) fn cloud_behind(&self) -> bool {
        let now = Utc::now();
        let dir = &self.paths().dir;
        (pending::any(dir) && pending::read(dir).iter().any(|a| a.settled(now))) || !self.awaiting_address().is_empty()
    }

    /// The cloud machines whose droplets may still start: with no address,
    /// and not ones DigitalOcean said it doesn't have (those are asked about
    /// again only by name).
    fn awaiting_address(&self) -> Vec<String> {
        let snap = self.current();
        snap.machines.iter().filter(|m| m.awaits_address() && m.cloud.as_ref().is_some_and(|c| c.power.as_deref() != Some("gone"))).map(|m| m.id.clone()).collect()
    }

    /// Returns the machines that got their address.
    async fn catch_up(self: &Arc<Self>, client: &Client) -> Vec<String> {
        if self.demo_world().is_some() || !self.cloud_behind() {
            return Vec::new();
        }
        // One at a time: two checks at once would both write the same machine.
        static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one = ONE.lock().await;
        self.refresh_from_disk();
        let dir = self.paths().dir.clone();
        let now = Utc::now();
        // Asked for and never heard of. Younger ones are left alone: the
        // Muxin that asked is still looking for its droplet itself.
        for asked in pending::read(&dir).into_iter().filter(|a| a.settled(now)) {
            // What keeps it from being settled, for the user to read.
            let problem = match self.find_made(client, &asked).await {
                Ok(Look::Made(made)) => match self.record_droplet(&asked, &made) {
                    Ok(id) => {
                        tracing::info!("DigitalOcean made droplet {} for a create that was cut off; it's in your machines as {id}", made.id);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("DigitalOcean made droplet {} for a create that was cut off, and Muxin couldn't add it to config.toml: {e}", made.id);
                        Some(format!("DigitalOcean made it (droplet {}), and Muxin couldn't add it to config.toml: {}", made.id, sentence(&e)))
                    }
                },
                // DigitalOcean answered, has no such droplet, and makes none for it anymore.
                Ok(Look::NotMade) => {
                    pending::remove(&dir, &asked);
                    continue;
                }
                Ok(Look::NotYet) => None,
                Ok(Look::Several(ids)) => Some(several(&asked, &ids)),
                Err(e) => {
                    tracing::warn!("couldn't ask DigitalOcean whether it made {}: {e}", asked.droplet);
                    Some(format!("Muxin couldn't ask DigitalOcean whether it made it: {}", sentence(&e)))
                }
            };
            pending::set_problem(&dir, &asked, problem);
        }
        // Made, and without an address when last looked.
        let mut started = Vec::new();
        let mut trouble = None;
        for id in self.awaiting_address() {
            match self.fill_address(client, &id).await {
                Ok(Start::Started(host)) => {
                    tracing::info!("{id} has started; it's at {host}");
                    started.push(id);
                }
                Ok(Start::NotYet) => {}
                Ok(Start::Gone) => tracing::warn!("DigitalOcean has no droplet for {id} in the account that's signed in"),
                Err(b) => {
                    tracing::warn!("couldn't ask DigitalOcean about {id}: {}", b.message);
                    if b.lasting {
                        trouble = Some(sentence(&b.message));
                    }
                }
            }
        }
        self.set_cloud_trouble(trouble);
        self.republish();
        started
    }

    /// What DigitalOcean has for what was asked: the droplet it made, or
    /// whether it still can make one.
    async fn find_made(&self, client: &Client, asked: &pending::Asked) -> Result<Look, String> {
        let named = client.droplets_named(&asked.droplet).await?;
        // Its clock as that answer had it.
        let ahead = client.clock_ahead();
        let known: Vec<u64> = self.current().machines.iter().filter_map(|m| m.cloud.as_ref().map(|c| c.id)).collect();
        Ok(match asked.made(&named, &known, ahead).as_slice() {
            [one] => Look::Made((*one).clone()),
            [] if asked.closed(Utc::now(), ahead) => Look::NotMade,
            [] => Look::NotYet,
            several => Look::Several(several.iter().map(|d| d.id).collect()),
        })
    }

    /// Writes a droplet DigitalOcean made into config.toml, as a machine
    /// with no address yet, and crosses it off what was asked for. Returns
    /// the machine's id.
    fn record_droplet(self: &Arc<Self>, asked: &pending::Asked, made: &Droplet) -> Result<String, String> {
        // Billed from when DigitalOcean made it, as it says.
        let created_at = if DateTime::parse_from_rfc3339(&made.created_at).is_ok() { made.created_at.clone() } else { asked.asked_at.to_rfc3339() };
        let cloud = CloudRef {
            provider: "digitalocean".into(),
            id: made.id,
            region: asked.region.clone(),
            size: asked.size.clone(),
            price_hourly: asked.price_hourly,
            created_at,
        };
        // A machine took its name meanwhile: the droplet is written down all the same.
        let names = [asked.name.clone(), format!("{} ({})", asked.name, made.id)];
        let mut written = Err(String::new());
        for name in &names {
            let mut id = None;
            let edit = |text: &str| {
                let (out, new_id) = inventory_edit::add_cloud_machine(text, name, &asked.user, &cloud)?;
                id = Some(new_id);
                Ok(out)
            };
            written = inventory_edit::edit_file(self.inventory_path(), edit).and_then(|()| id.ok_or_else(|| "The machine wasn't added.".to_string()));
            if written.is_ok() {
                break;
            }
        }
        let id = written?;
        pending::remove(&self.paths().dir, asked);
        // Whatever was kept for a machine that had this id before isn't this droplet's.
        forget_idle(&id);
        self.reload_inventory_now();
        self.set_droplet_status(&id, if made.status.is_empty() { "new" } else { &made.status });
        Ok(id)
    }

    /// Whether the machine is still in Muxin, and still that droplet.
    fn still_droplet(&self, machine_id: &str, droplet: u64) -> bool {
        self.machine(machine_id).is_some_and(|(spec, _)| spec.cloud.as_ref().is_some_and(|c| c.id == droplet))
    }

    /// Asks DigitalOcean where a cloud machine with no address stands, and
    /// gives it the one its droplet has once that has started
    /// ("user@203.0.113.7", written to config.toml).
    async fn fill_address(self: &Arc<Self>, client: &Client, machine_id: &str) -> Result<Start, Blocked> {
        let (spec, _) = self.machine(machine_id).ok_or_else(|| "That machine isn't in Muxin anymore.".to_string())?;
        if spec.is_checkable() {
            return Ok(Start::Started(spec.host.trim().to_string()));
        }
        let cloud = spec.cloud.as_ref().ok_or_else(|| format!("{} isn't a cloud machine Muxin created.", spec.name))?;
        let droplet = match client.find_droplet(cloud.id).await {
            Ok(d) => d,
            // Deleted there (in its panel, with doctl), or the sign-in is
            // another account's or team's. Kept, so every check doesn't ask again.
            Err(f) if f.gone() => {
                self.set_droplet_status(machine_id, "gone");
                return Ok(Start::Gone);
            }
            Err(f) => return Err(f.into()),
        };
        if !droplet.status.is_empty() {
            self.set_droplet_status(machine_id, &droplet.status);
        }
        // "new" has an address before anything answers at it.
        let Some(ip) = droplet.ipv4.filter(|_| matches!(droplet.status.as_str(), "active" | "off")) else { return Ok(Start::NotYet) };
        let user = spec.user.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(local_user);
        let host = format!("{}@{ip}", user.trim());
        let edit = |text: &str| inventory_edit::set_machine_address(text, machine_id, &host);
        inventory_edit::edit_file(self.inventory_path(), edit)?;
        self.reload_inventory_now();
        Ok(Start::Started(host))
    }

    /// Asks DigitalOcean now whether a cloud machine that has no address has
    /// started (a check of it by name): its address is filled in if so.
    pub(crate) async fn cloud_start(self: &Arc<Self>, machine_id: &str) -> Result<Start, String> {
        let client = self.do_client().await?;
        match self.fill_address(&client, machine_id).await {
            Ok(start) => {
                self.set_cloud_trouble(None);
                Ok(start)
            }
            Err(b) => {
                if b.lasting {
                    self.set_cloud_trouble(Some(sentence(&b.message)));
                }
                Err(b.message)
            }
        }
    }

    /// Deletes a droplet, asking again for a while when DigitalOcean can't
    /// yet: one it's still making has "a pending event" until it's made.
    async fn delete_droplet(self: &Arc<Self>, op: u64, client: &Client, droplet: u64, name: &str) -> Result<(), String> {
        let (every, asks) = (self.pause(DELETE_WAIT.0), DELETE_WAIT.1);
        let mut ask = 0;
        loop {
            ask += 1;
            match client.delete_droplet(droplet).await {
                Ok(()) => return Ok(()),
                Err(f) if f.busy() && ask < asks => {
                    if ask == 1 {
                        self.op_log(op, "▸ DigitalOcean is still working on it and can't delete it yet; asking again for up to two minutes");
                    }
                    tokio::time::sleep(every).await;
                }
                Err(f) if f.busy() => {
                    return Err(format!(
                        "DigitalOcean can't delete {name} yet, because it's still working on it ({}). It's still in your machines, and billed: destroy it again in a few minutes.",
                        f.message.trim_end_matches('.')
                    ));
                }
                Err(f) => return Err(f.message),
            }
        }
    }

    /// Destroys a cloud machine: deletes the droplet and removes it from Muxin.
    pub fn cloud_destroy(self: &Arc<Self>, machine_id: &str) -> Result<u64, String> {
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxin anymore.")?;
        let cloud = spec.cloud.clone().ok_or_else(|| format!("{} isn't a cloud machine Muxin created.", spec.name))?;
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
                me.delete_droplet(op, &client, cloud.id, &spec.name).await?;
                // Its cost this month still counts.
                me.record_destroyed(&spec.id, crate::config::DestroyedCloud { name: spec.name.clone(), cloud: cloud.clone(), destroyed_at: Utc::now() });
                me.op_log(op, format!("▸ Removing {} from config.toml", spec.name));
                // Not remove_machine: this operation is the one keeping the machine busy.
                me.forget_machine(&spec.id)?;
                // Its address may be reused by someone else's droplet.
                if let Some(ip) = spec.host.rsplit('@').next().map(str::trim).filter(|ip| !ip.is_empty()) {
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
        let (spec, _) = self.machine(machine_id).ok_or("That machine isn't in Muxin anymore.")?;
        let cloud = spec.cloud.clone().ok_or_else(|| format!("{} isn't a cloud machine Muxin created.", spec.name))?;
        if spec.awaits_address() {
            return Err(format!("{} hasn't started yet, so there's nothing to power {}. Muxin adds its address once it has; destroy it if you don't want it.", spec.name, if on { "on" } else { "off" }));
        }
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
                let ok = crate::ops::remote::answers(spec).await;
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
        // A create that was cut off counts too: find it first.
        self.cloud_catch_up().await;
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
                            "oauth" => format!("Sign in again so Muxin can read your DigitalOcean bill. ({said})"),
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
            // A machine that's gone takes its clock with it: one made later
            // under its name hasn't been idle since the old one was.
            state.keep_only(&snapshot.machines.iter().filter(|m| m.cloud.is_some()).map(|m| m.id.as_str()).collect());
            for m in &snapshot.machines {
                let Some(droplet) = m.cloud.as_ref().map(|c| c.id) else { continue };
                let busy = snapshot.sessions.iter().any(|x| x.machine_id == m.id && x.busy)
                    || m.summary.working > 0
                    || m.probe.as_ref().and_then(|p| p.cpu_pct).is_some_and(|c| c > 20.);
                let last = *state.clock(&m.id, droplet, now);
                if busy {
                    state.last_active.insert(m.id.clone(), now);
                    state.acted.remove(&m.id);
                    continue;
                }
                let idle_hours = (now - last).num_minutes() as f64 / 60.;
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

/// Why an entry with several droplets that fit can't be settled, and what
/// settles it.
fn several(asked: &pending::Asked, ids: &[u64]) -> String {
    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "DigitalOcean has {} droplets called {} that were made at that time ({list}), so Muxin can't tell which one it asked for. Rename or delete the one that isn't Muxin's in DigitalOcean's panel, and Muxin adds the other to your machines at the next machine check.",
        ids.len(),
        asked.droplet
    )
}

/// `text` as a sentence others can follow: with its full stop.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) { text.to_string() } else { format!("{text}.") }
}

/// The account a new machine's first boot makes, which ssh signs in to: this
/// computer's user's name.
fn local_user() -> String {
    setup::user_name(&std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default())
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

#[cfg(test)]
mod tests;
