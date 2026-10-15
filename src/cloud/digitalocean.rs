//! DigitalOcean's API (v2), for the few things Muxin does there: who's signed
//! in, which regions and sizes exist, the account's SSH keys, and creating,
//! reading, powering and destroying droplets.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const API: &str = "https://api.digitalocean.com/v2";

/// The image new machines run.
pub const IMAGE: &str = "ubuntu-24-04-x64";

pub struct Client {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Account {
    pub email: String,
    pub team: Option<String>,
    pub droplet_limit: u32,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Region {
    pub slug: String,
    pub name: String,
    pub sizes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Size {
    pub slug: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub disk_gb: u32,
    pub price_hourly: f64,
    pub price_monthly: f64,
    pub regions: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Droplet {
    pub id: u64,
    pub name: String,
    /// "new" | "active" | "off" | "archive"
    pub status: String,
    pub ipv4: Option<String>,
    pub region: String,
    pub size: String,
    pub price_hourly: f64,
    pub created_at: String,
}

/// The account's bill so far this month, as DigitalOcean reports it (USD).
#[derive(Debug, Clone, Serialize)]
pub struct Balance {
    /// Usage this month, before credits and payments.
    pub month_to_date_usage: f64,
    /// What's owed for this month so far (usage less credits).
    pub month_to_date_balance: f64,
    /// Balance carried on the account (negative is credit).
    pub account_balance: f64,
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SshKey {
    pub id: u64,
    pub public_key: String,
}

impl Client {
    pub fn new(token: String) -> Client {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Client { http, token }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut req = self
            .http
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.token)
            .header("User-Agent", concat!("muxen/", env!("CARGO_PKG_VERSION")));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                "DigitalOcean didn't answer within 30 seconds. Try again in a moment.".to_string()
            } else {
                format!("Couldn't reach DigitalOcean: {}", root_cause(&e))
            }
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) });
        }
        let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_string)).unwrap_or_default();
        Err(match status {
            StatusCode::UNAUTHORIZED => "DigitalOcean didn't accept the sign-in. Connect again in Settings › Cloud.".into(),
            StatusCode::FORBIDDEN => format!("DigitalOcean refused: {message} (the token may be read-only)"),
            StatusCode::TOO_MANY_REQUESTS => "DigitalOcean is rate-limiting requests; try again in a minute.".into(),
            _ if message.is_empty() => format!("DigitalOcean answered {status}"),
            _ => format!("DigitalOcean: {message}"),
        })
    }

    pub async fn account(&self) -> Result<Account, String> {
        let v = self.call(Method::GET, "/account", None).await?;
        let a = &v["account"];
        Ok(Account {
            email: a["email"].as_str().unwrap_or_default().to_string(),
            team: a["team"]["name"].as_str().map(str::to_string),
            droplet_limit: a["droplet_limit"].as_u64().unwrap_or(0) as u32,
            status: a["status"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Needs the billing:read permission.
    pub async fn balance(&self) -> Result<Balance, String> {
        let v = self.call(Method::GET, "/customers/my/balance", None).await?;
        // Amounts come as strings ("11.21").
        let amount = |k: &str| v[k].as_str().and_then(|s| s.parse().ok()).or_else(|| v[k].as_f64()).unwrap_or(0.);
        Ok(Balance {
            month_to_date_usage: amount("month_to_date_usage"),
            month_to_date_balance: amount("month_to_date_balance"),
            account_balance: amount("account_balance"),
            generated_at: v["generated_at"].as_str().map(str::to_string),
        })
    }

    pub async fn regions(&self) -> Result<Vec<Region>, String> {
        let v = self.call(Method::GET, "/regions?per_page=200", None).await?;
        Ok(v["regions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["available"].as_bool().unwrap_or(false))
            .map(|r| Region {
                slug: r["slug"].as_str().unwrap_or_default().to_string(),
                name: r["name"].as_str().unwrap_or_default().to_string(),
                sizes: strings(&r["sizes"]),
            })
            .collect())
    }

    pub async fn sizes(&self) -> Result<Vec<Size>, String> {
        let v = self.call(Method::GET, "/sizes?per_page=200", None).await?;
        let mut sizes: Vec<Size> = v["sizes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["available"].as_bool().unwrap_or(false))
            .map(|s| Size {
                slug: s["slug"].as_str().unwrap_or_default().to_string(),
                vcpus: s["vcpus"].as_u64().unwrap_or(0) as u32,
                memory_mb: s["memory"].as_u64().unwrap_or(0) as u32,
                disk_gb: s["disk"].as_u64().unwrap_or(0) as u32,
                price_hourly: s["price_hourly"].as_f64().unwrap_or(0.),
                price_monthly: s["price_monthly"].as_f64().unwrap_or(0.),
                regions: strings(&s["regions"]),
                description: s["description"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        sizes.sort_by(|a, b| a.price_monthly.total_cmp(&b.price_monthly));
        Ok(sizes)
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, String> {
        let v = self.call(Method::GET, "/account/keys?per_page=200", None).await?;
        Ok(v["ssh_keys"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|k| SshKey { id: k["id"].as_u64().unwrap_or(0), public_key: k["public_key"].as_str().unwrap_or_default().to_string() })
            .collect())
    }

    pub async fn add_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKey, String> {
        let v = self.call(Method::POST, "/account/keys", Some(json!({ "name": name, "public_key": public_key }))).await?;
        let k = &v["ssh_key"];
        Ok(SshKey { id: k["id"].as_u64().unwrap_or(0), public_key: k["public_key"].as_str().unwrap_or_default().to_string() })
    }

    pub async fn create_droplet(&self, name: &str, region: &str, size: &str, ssh_key: u64, user_data: &str) -> Result<Droplet, String> {
        let body = json!({
            "name": name,
            "region": region,
            "size": size,
            "image": IMAGE,
            "ssh_keys": [ssh_key],
            "user_data": user_data,
            "monitoring": true,
            "ipv6": true,
        });
        let v = self.call(Method::POST, "/droplets", Some(body)).await?;
        Ok(droplet(&v["droplet"]))
    }

    pub async fn droplet(&self, id: u64) -> Result<Droplet, String> {
        let v = self.call(Method::GET, &format!("/droplets/{id}"), None).await?;
        Ok(droplet(&v["droplet"]))
    }

    pub async fn delete_droplet(&self, id: u64) -> Result<(), String> {
        match self.call(Method::DELETE, &format!("/droplets/{id}"), None).await {
            // Already gone is what was wanted.
            Err(e) if e.contains("404") || e.to_lowercase().contains("not be found") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// "power_on", "power_off" (hard) or "shutdown" (clean).
    pub async fn droplet_action(&self, id: u64, action: &str) -> Result<(), String> {
        self.call(Method::POST, &format!("/droplets/{id}/actions"), Some(json!({ "type": action }))).await.map(|_| ())
    }
}

/// The innermost error: reqwest's own message only says the request failed.
fn root_cause(e: &dyn std::error::Error) -> String {
    let mut e = e;
    while let Some(next) = e.source() {
        e = next;
    }
    e.to_string()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

fn droplet(d: &Value) -> Droplet {
    let ipv4 = d["networks"]["v4"].as_array().into_iter().flatten().find(|n| n["type"] == "public").and_then(|n| n["ip_address"].as_str()).map(str::to_string);
    Droplet {
        id: d["id"].as_u64().unwrap_or(0),
        name: d["name"].as_str().unwrap_or_default().to_string(),
        status: d["status"].as_str().unwrap_or_default().to_string(),
        ipv4,
        region: d["region"]["slug"].as_str().unwrap_or_default().to_string(),
        size: d["size_slug"].as_str().or(d["size"]["slug"].as_str()).unwrap_or_default().to_string(),
        price_hourly: d["size"]["price_hourly"].as_f64().unwrap_or(0.),
        created_at: d["created_at"].as_str().unwrap_or_default().to_string(),
    }
}

/// Whether two public keys are the same key (type and body; the comment doesn't matter).
pub fn same_key(a: &str, b: &str) -> bool {
    let core = |k: &str| k.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    !core(a).is_empty() && core(a) == core(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_droplet() {
        let v = json!({
            "id": 7, "name": "dev", "status": "active", "created_at": "2026-10-09T12:00:00Z",
            "region": { "slug": "nyc3" }, "size_slug": "s-2vcpu-4gb", "size": { "price_hourly": 0.03571 },
            "networks": { "v4": [ { "ip_address": "10.0.0.2", "type": "private" }, { "ip_address": "203.0.113.7", "type": "public" } ] }
        });
        let d = droplet(&v);
        assert_eq!((d.id, d.ipv4.as_deref(), d.region.as_str(), d.size.as_str()), (7, Some("203.0.113.7"), "nyc3", "s-2vcpu-4gb"));
        assert!(same_key("ssh-ed25519 AAAA me@mac", "ssh-ed25519 AAAA other comment"));
        assert!(!same_key("ssh-ed25519 AAAA", "ssh-ed25519 BBBB"));
    }
}
