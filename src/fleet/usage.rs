//! What the agents cost: tokens each machine's agent CLIs used (read from
//! their own logs by the agent there, see the agent's usage.rs), and what
//! they'd cost at the API's list prices. With a Claude or ChatGPT plan you
//! don't pay per token; the price says what the work was worth.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Engine;

/// Tokens in one hour (UTC) for one tool and model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageHour {
    /// "YYYY-MM-DDTHH", UTC.
    pub hour: String,
    /// "claude" (Claude Code) or "codex".
    pub tool: String,
    pub model: String,
    #[serde(flatten)]
    pub tokens: Tokens,
}

/// A folder's use: what was done where.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageFolder {
    pub tool: String,
    pub cwd: String,
    /// When it was last used (UTC, as the log says).
    #[serde(default)]
    pub last: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(flatten)]
    pub tokens: Tokens,
}

/// Tokens as they're priced.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    /// Cache writes kept 5 minutes, and an hour.
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_write_1h: u64,
    #[serde(default)]
    pub requests: u64,
}

impl Tokens {
    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cache_write_1h += o.cache_write_1h;
        self.requests += o.requests;
    }

    /// Every token, read or written.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write + self.cache_write_1h
    }
}

/// One machine's agents' use.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MachineUsage {
    #[serde(default)]
    pub machine_id: String,
    /// The tools whose logs were found ("claude", "codex").
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub hours: Vec<UsageHour>,
    #[serde(default)]
    pub folders: Vec<UsageFolder>,
}

/// A model's list price, per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Price {
    pub input: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// When the prices below were read off Anthropic's and OpenAI's pricing pages.
pub const PRICES_AS_OF: &str = "October 2026";

/// A model's list price, when it's known: Anthropic's for Claude, OpenAI's
/// standard tier for the rest (October 2026). Unknown models have none
/// rather than a guess.
pub fn price(model: &str) -> Option<Price> {
    let p = |input: f64, cache_write: f64, cache_write_1h: f64, cache_read: f64, output: f64| Some(Price { input, cache_write, cache_write_1h, cache_read, output });
    // OpenAI's have no cache writes: they're input.
    let o = |input: f64, cache_read: f64, output: f64| Some(Price { input, cache_write: input, cache_write_1h: input, cache_read, output });
    if let Some(rest) = model.strip_prefix("claude-") {
        // "claude-opus-5-5", "claude-sonnet-4-5-20250929", "claude-3-5-haiku-…".
        let name = match rest.rsplit_once('-') {
            Some((head, date)) if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) => head,
            _ => rest,
        };
        return match name {
            "fable-5-1" | "mythos-5-1" => p(10., 12.5, 20., 0.25, 50.),
            "fable-5" | "mythos-5" => p(10., 12.5, 20., 1., 50.),
            "opus-5-5" => p(4., 5., 8., 0.20, 20.),
            "opus-5" | "opus-4-8" | "opus-4-7" | "opus-4-6" | "opus-4-5" => p(5., 6.25, 10., 0.5, 25.),
            "opus-4-1" | "opus-4" | "opus-4-0" => p(15., 18.75, 30., 1.5, 75.),
            "sonnet-5-5" => p(2., 2.5, 4., 0.10, 10.),
            "sonnet-5" => p(2., 2.5, 4., 0.20, 10.),
            "sonnet-4-6" | "sonnet-4-5" | "sonnet-4" | "sonnet-4-0" => p(3., 3.75, 6., 0.30, 15.),
            // Prompts up to 100,000 tokens (most of them).
            "haiku-5-5" => p(0.10, 0.125, 0.20, 0.01, 0.50),
            "haiku-4-5" => p(1., 1.25, 2., 0.10, 5.),
            "3-5-haiku" | "haiku-3-5" => p(0.8, 1., 1.6, 0.08, 4.),
            _ => None,
        };
    }
    match model {
        "gpt-6-astra" => o(10., 1., 50.),
        "gpt-6.1-sol" => o(2., 0.10, 10.),
        "gpt-6-sol" => o(2., 0.20, 10.),
        "gpt-6-luna" => o(0.10, 0.01, 0.50),
        "gpt-5.6-sol" => o(4., 0.40, 20.),
        "gpt-5.6-terra" => o(2., 0.20, 12.),
        "gpt-5.6-luna" => o(0.20, 0.02, 1.20),
        "gpt-5.5" => o(5., 0.5, 30.),
        "gpt-5.4" => o(2.5, 0.25, 15.),
        "gpt-5.4-mini" => o(0.75, 0.075, 4.5),
        "gpt-5.4-nano" => o(0.20, 0.02, 1.25),
        "gpt-5.3-codex" => o(1.75, 0.175, 14.),
        "gpt-5.2" => o(1.75, 0.175, 14.),
        "gpt-5.1" | "gpt-5" => o(1.25, 0.125, 10.),
        "gpt-5-mini" => o(0.25, 0.025, 2.),
        "gpt-5-nano" => o(0.05, 0.005, 0.40),
        "gpt-4.1" => o(2., 0.5, 8.),
        "o3" => o(2., 0.5, 8.),
        "o4-mini" => o(1.1, 0.275, 4.4),
        _ => None,
    }
}

/// What tokens of a model cost at its list price (None: price unknown).
pub fn cost(model: &str, t: &Tokens) -> Option<f64> {
    let p = price(model)?;
    let m = 1_000_000.;
    Some((t.input as f64 * p.input + t.output as f64 * p.output + t.cache_read as f64 * p.cache_read + t.cache_write as f64 * p.cache_write + t.cache_write_1h as f64 * p.cache_write_1h) / m)
}

/// An hour bucket's day, in this computer's time zone.
pub fn local_day(hour: &str) -> Option<NaiveDate> {
    let t = NaiveDateTime::parse_from_str(&format!("{hour}:00:00"), "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&t).with_timezone(&Local).date_naive())
}

/// "Claude Code" for "claude".
pub fn tool_name(tool: &str) -> &str {
    match tool {
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

impl Engine {
    /// A machine's agents' tokens over the last `days`.
    pub async fn usage(self: &Arc<Self>, machine_id: &str, days: u32) -> Result<MachineUsage, String> {
        let (spec, probe) = self.machine(machine_id).ok_or_else(|| format!("No machine called {machine_id}."))?;
        if self.demo_world().is_some() {
            return Ok(demo(machine_id, days));
        }
        super::sessions::agent::ensure(&spec, probe.as_ref()).await?;
        let reply = super::sessions::agent::ctl(&spec, &json!({ "op": "usage", "days": days })).await?;
        let mut u: MachineUsage = serde_json::from_value(reply).map_err(|_| format!("{} told its usage in a way Muxin can't read.", spec.name))?;
        u.machine_id = spec.id.clone();
        Ok(u)
    }

    /// Every reachable machine's, side by side (a machine that can't say is left out, with why).
    pub async fn usage_all(self: &Arc<Self>, days: u32) -> Vec<(String, Result<MachineUsage, String>)> {
        // Not just the ones checked already: a machine not reached yet says why.
        let ids: Vec<String> = self.current().machines.iter().filter(|m| !m.is_down() && !m.host.is_empty()).map(|m| m.id.clone()).collect();
        let results = futures::future::join_all(ids.iter().map(|id| self.usage(id, days))).await;
        ids.into_iter().zip(results).collect()
    }
}

/// Totals by model across machines (with each one's cost when known).
pub fn by_model(all: &[MachineUsage]) -> Vec<(String, String, Tokens, Option<f64>)> {
    let mut m: HashMap<(String, String), Tokens> = HashMap::new();
    for u in all {
        for h in &u.hours {
            m.entry((h.tool.clone(), h.model.clone())).or_default().add(&h.tokens);
        }
    }
    let mut out: Vec<(String, String, Tokens, Option<f64>)> = m.into_iter().map(|((tool, model), t)| {
        let c = cost(&model, &t);
        (tool, model, t, c)
    }).collect();
    out.sort_by(|a, b| b.3.unwrap_or(0.).total_cmp(&a.3.unwrap_or(0.)).then(b.2.total().cmp(&a.2.total())));
    out
}

fn demo(machine_id: &str, days: u32) -> MachineUsage {
    let (tool, model, scale) = match machine_id {
        "forge" => ("claude", "claude-opus-5-5", 1.0),
        "orbit" => ("codex", "gpt-6.1-sol", 0.6),
        "beacon" => ("claude", "claude-sonnet-5-5", 0.8),
        _ => return MachineUsage { machine_id: machine_id.into(), ..Default::default() },
    };
    let now = Utc::now();
    let hours = (0..(days.min(30) * 24))
        .filter(|h| h % 24 > 6 && h % 24 < 20)
        .map(|h| {
            let at = now - chrono::Duration::hours(h as i64);
            let f = (((h * 7919) % 13) as f64 / 13. + 0.3) * scale;
            UsageHour {
                hour: at.format("%Y-%m-%dT%H").to_string(),
                tool: tool.into(),
                model: model.into(),
                tokens: Tokens { input: (2_000. * f) as u64, output: (60_000. * f) as u64, cache_read: (3_000_000. * f) as u64, cache_write: 0, cache_write_1h: (90_000. * f) as u64, requests: (40. * f) as u64 },
            }
        })
        .collect();
    let folder = |cwd: &str| UsageFolder { tool: tool.into(), cwd: cwd.into(), last: now.to_rfc3339(), models: vec![model.into()], tokens: Tokens { output: 900_000, cache_read: 40_000_000, requests: 600, ..Default::default() } };
    MachineUsage { machine_id: machine_id.into(), tools: vec![tool.into()], hours, folders: vec![folder("~/code/checkout"), folder("~/code/api")] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_known_models_only() {
        assert_eq!(price("claude-opus-5-5").map(|p| p.output), Some(20.));
        assert_eq!(price("claude-sonnet-4-5-20250929").map(|p| p.input), Some(3.));
        assert_eq!(price("claude-3-5-haiku-20241022").map(|p| p.output), Some(4.));
        assert_eq!(price("gpt-6.1-sol").map(|p| p.cache_read), Some(0.10));
        assert_eq!(price("claude-opus-9"), None);
        assert_eq!(price("gpt-5.1-codex-max"), None);
        // 1M output and 10M cache reads on Opus 5.5: $20 + $2.
        let t = Tokens { output: 1_000_000, cache_read: 10_000_000, ..Default::default() };
        assert!((cost("claude-opus-5-5", &t).unwrap() - 22.).abs() < 1e-9);
    }

    #[test]
    fn totals_by_model() {
        let h = |model: &str, out: u64| UsageHour { hour: "2026-10-12T17".into(), tool: "claude".into(), model: model.into(), tokens: Tokens { output: out, requests: 1, ..Default::default() } };
        let a = MachineUsage { hours: vec![h("claude-opus-5-5", 1_000_000), h("claude-sonnet-5-5", 1_000_000)], ..Default::default() };
        let b = MachineUsage { hours: vec![h("claude-opus-5-5", 1_000_000)], ..Default::default() };
        let m = by_model(&[a, b]);
        assert_eq!(m[0].1, "claude-opus-5-5");
        assert_eq!(m[0].2.output, 2_000_000);
        assert_eq!(m[0].3, Some(40.));
        assert_eq!(m[1].3, Some(10.));
    }
}
