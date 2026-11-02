//! iPads paired from this computer: Muxin for iPad reaches the machines
//! directly with a key of its own, which each machine's agent trusts. This
//! app vouches for it, over the connections it already has (`share::pair`
//! is the iPad's side of that). `ipads.json` in Muxin's config folder keeps
//! each iPad's name, key and the machines that trust it, so removing one
//! takes its key off them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

use super::sessions::agent;
use super::{Engine, direct, link};
use crate::share::pair;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ipad {
    pub name: String,
    /// Its key's fingerprint, as the agents know it.
    pub fingerprint: String,
    pub paired_at: DateTime<Utc>,
    /// The machines that trust it, by id.
    pub machines: Vec<String>,
}

fn path(dir: &Path) -> PathBuf {
    dir.join("ipads.json")
}

pub fn list(dir: &Path) -> Vec<Ipad> {
    std::fs::read_to_string(path(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write(dir: &Path, all: &[Ipad]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(all).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&path(dir), text.as_bytes()).map_err(|e| format!("Couldn't save the iPads: {e:#}"))
}

/// Notes an iPad that paired: pairing one again adds its new machines.
pub fn record(dir: &Path, name: &str, fingerprint: &str, machines: &[String]) -> Result<(), String> {
    let mut all = list(dir);
    match all.iter_mut().find(|i| i.fingerprint == fingerprint) {
        Some(i) => {
            i.name = name.to_string();
            i.paired_at = Utc::now();
            for m in machines {
                if !i.machines.contains(m) {
                    i.machines.push(m.clone());
                }
            }
        }
        None => all.push(Ipad { name: name.to_string(), fingerprint: fingerprint.to_string(), paired_at: Utc::now(), machines: machines.to_vec() }),
    }
    write(dir, &all)
}

/// The iPad named (or with that key) among `all`.
pub fn find<'a>(all: &'a [Ipad], name_or_key: &str) -> Result<&'a Ipad, String> {
    let want = name_or_key.trim();
    if let Some(i) = all.iter().find(|i| i.fingerprint == want.to_lowercase()) {
        return Ok(i);
    }
    let named: Vec<&Ipad> = all.iter().filter(|i| i.name.eq_ignore_ascii_case(want)).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("No iPad called {want} is paired.")),
        _ => Err(format!("More than one iPad is called {want}: use its key (muxin ipad ls --json).")),
    }
}

impl Engine {
    /// The iPads paired from here.
    pub fn ipads(&self) -> Vec<Ipad> {
        list(&self.paths().dir)
    }

    /// Starts pairing an iPad with these machines (ids): the link to show
    /// as a QR code, and how it goes.
    pub async fn pair_ipad(self: &Arc<Self>, machine_ids: &[String]) -> Result<watch::Receiver<pair::View>, String> {
        if self.demo_world().is_some() {
            return Err("The demo's machines are made up, so there's nothing to pair an iPad with.".into());
        }
        let mut machines = Vec::new();
        for id in machine_ids {
            let (spec, _) = self.machine(id).ok_or_else(|| format!("No machine called {id}."))?;
            machines.push((spec.id, spec.name));
        }
        let (trusting, dir) = (self.clone(), self.paths().dir.clone());
        let hooks = pair::Hooks {
            trust: Box::new(move |id, key| {
                let engine = trusting.clone();
                async move { engine.trust_app(&id, &key).await }.boxed()
            }),
            paired: Box::new(move |name, key, ids| {
                if let Err(e) = record(&dir, &name, &key, &ids) {
                    tracing::warn!("{e}");
                }
            }),
        };
        pair::start(&self.settings().share_server(), machines, hooks).await
    }

    /// Has a machine's agent trust another app's key (an iPad's) as it
    /// trusts this one: how that app reaches the machine directly. Where
    /// this app already connects directly, the agent only adds the key;
    /// otherwise it's paired the way `set_direct` pairs, over ssh.
    pub async fn trust_app(self: &Arc<Self>, machine_id: &str, client: &str) -> Result<pair::Machine, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if spec.is_local() {
            return Err("This computer isn't a machine the iPad reaches.".into());
        }
        spec.refuse_if_disconnected()?;
        agent::ensure(&spec, probe.as_ref()).await?;
        let asked = json!({ "op": "trust", "client": client });
        let quick = match link::get(&spec).await {
            Some(l) if l.transport() == "direct" => l.request(asked, Duration::from_secs(30)).await.ok().filter(|r| r["ok"] == true),
            _ => None,
        };
        let reply = match quick {
            Some(r) => r,
            // Pairing (re)starts the agent's listener, which would cut a
            // direct link off mid-request: over ssh, with this app's own
            // direct connection put back after.
            None => {
                let mine = direct::paired(&spec.id);
                direct::save(&spec.id, None)?;
                link::reset(&spec.id);
                let paired = async {
                    let l = link::get(&spec).await.ok_or_else(|| format!("Couldn't reach the agent on {}.", spec.name))?;
                    l.request(json!({ "op": "pair", "client": client }), Duration::from_secs(60)).await
                }
                .await;
                direct::save(&spec.id, mine)?;
                link::reset(&spec.id);
                paired?
            }
        };
        if reply["ok"] != true {
            return Err(reply["error"].as_str().unwrap_or("The agent couldn't set up direct connections.").to_string());
        }
        let list = |k: &str| -> Vec<String> { reply[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default() };
        let host = direct::resolve_host(&spec).await.unwrap_or_else(|| spec.host.rsplit('@').next().unwrap_or(&spec.host).to_string());
        let paired = direct::Paired {
            fingerprint: reply["fingerprint"].as_str().unwrap_or("").to_string(),
            host,
            port: reply["port"].as_u64().unwrap_or(0) as u16,
            lan: list("addresses"),
            tailnet: list("tailnet"),
        };
        if paired.fingerprint.is_empty() || paired.port == 0 {
            return Err(format!("{}'s agent didn't say how to reach it.", spec.name));
        }
        Ok(pair::Machine { id: spec.id, name: spec.name, host: spec.host, direct: paired })
    }

    /// Unpairs an iPad (by name or key): each machine that trusted it forgets
    /// its key. One that can't be reached now keeps it: the machines' names.
    pub async fn remove_ipad(&self, name_or_key: &str) -> Result<(Ipad, Vec<String>), String> {
        let dir = self.paths().dir.clone();
        let mut all = list(&dir);
        let ipad = find(&all, name_or_key)?.clone();
        let tries = ipad.machines.iter().filter_map(|id| self.machine(id).map(|(spec, _)| spec)).map(|spec| {
            let key = ipad.fingerprint.clone();
            async move {
                let ask = async {
                    let l = link::get(&spec).await?;
                    l.request(json!({ "op": "unpair", "client": key }), Duration::from_secs(15)).await.ok().filter(|r| r["ok"] == true)
                };
                let done = tokio::time::timeout(Duration::from_secs(20), ask).await.ok().flatten().is_some();
                (!done).then_some(spec.name)
            }
        });
        let kept: Vec<String> = futures::future::join_all(tries).await.into_iter().flatten().collect();
        all.retain(|i| i.fingerprint != ipad.fingerprint);
        write(&dir, &all)?;
        Ok((ipad, kept))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_again_adds_machines_and_finds_by_name_or_key() {
        let dir = std::env::temp_dir().join(format!("muxin-ipads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = "ab".repeat(32);
        record(&dir, "Studio iPad", &key, &["beelink".into()]).unwrap();
        record(&dir, "Studio iPad", &key, &["beelink".into(), "forge".into()]).unwrap();
        let all = list(&dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].machines, vec!["beelink".to_string(), "forge".to_string()]);
        assert_eq!(find(&all, "studio ipad").unwrap().fingerprint, key);
        assert_eq!(find(&all, &key.to_uppercase()).unwrap().name, "Studio iPad");
        assert!(find(&all, "kitchen").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
