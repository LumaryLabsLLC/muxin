//! Cloud machines: droplets Muxen creates in the user's DigitalOcean account,
//! sets up, and adds to the fleet (see ops::cloud for the operations).

pub mod auth;
pub mod digitalocean;
pub mod secrets;
pub mod setup;

use serde::{Deserialize, Serialize};

pub use digitalocean::{Account, Region, Size};

/// The secret name of the Tailscale auth key new machines join with.
pub const TAILSCALE_KEY: &str = "tailscale-authkey";

/// What a new machine can be: the account, regions and sizes, and the defaults.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CloudOptions {
    pub provider: String,
    /// "token" | "oauth" | "doctl"
    pub method: String,
    pub signed_in: bool,
    /// The browser sign-in is built into this copy of Muxen.
    pub oauth_available: bool,
    pub account: Option<Account>,
    pub regions: Vec<Region>,
    pub sizes: Vec<Size>,
    pub default_region: String,
    pub default_size: String,
    /// The public key new machines get, "~/.ssh/id_ed25519.pub".
    pub ssh_key: Option<String>,
    /// Why the regions and sizes couldn't be read.
    pub error: Option<String>,
}

/// A new cloud machine.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudCreate {
    pub name: String,
    /// Region slug ("nyc3"); default: Settings, else the nearest.
    #[serde(default)]
    pub region: Option<String>,
    /// Size slug ("s-2vcpu-4gb"); default: Settings.
    #[serde(default)]
    pub size: Option<String>,
    /// What it comes with, by id (see setup::EXTRAS); default: Settings.
    #[serde(default)]
    pub install: Option<Vec<String>>,
}

/// One region per city, as people know them ("New York", not NYC1/NYC2/NYC3):
/// (city, region slug), the city's newest datacenter, Americas first, then
/// Europe, then Asia and Oceania.
pub fn cities(regions: &[Region]) -> Vec<(String, String)> {
    const ORDER: &[&str] = &[
        "New York", "Toronto", "Atlanta", "Richmond", "San Francisco", "Kansas City", "Memphis",
        "London", "Amsterdam", "Frankfurt",
        "Bangalore", "Singapore", "Sydney",
    ];
    let city_of = |r: &Region| r.name.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ').trim().to_string();
    let number = |slug: &str| slug.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u32>().unwrap_or(0);
    let mut best: Vec<(String, String)> = Vec::new();
    for r in regions {
        let city = city_of(r);
        let city = if city.is_empty() { r.slug.to_uppercase() } else { city };
        match best.iter_mut().find(|(c, _)| *c == city) {
            Some(entry) if number(&r.slug) > number(&entry.1) => entry.1 = r.slug.clone(),
            Some(_) => {}
            None => best.push((city, r.slug.clone())),
        }
    }
    let rank = |c: &str| ORDER.iter().position(|o| *o == c).unwrap_or(ORDER.len());
    best.sort_by_key(|(c, _)| rank(c));
    best
}

/// The city a region slug is in, by the same names.
pub fn city_of_slug(regions: &[Region], slug: &str) -> Option<String> {
    let r = regions.iter().find(|r| r.slug == slug)?;
    Some(r.name.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ').trim().to_string())
}

/// The region nearest this computer, by its UTC offset, among `available`.
pub fn nearest_region(offset_hours: i32, available: &[String]) -> Option<String> {
    let preferences: &[&str] = match offset_hours {
        ..=-9 => &["sfo3", "sfo2", "nyc3"],
        -8..=-6 => &["sfo3", "tor1", "nyc3"],
        -5..=-3 => &["nyc3", "nyc1", "tor1"],
        -2..=2 => &["lon1", "ams3", "fra1"],
        3..=4 => &["fra1", "ams3", "blr1"],
        5..=6 => &["blr1", "sgp1"],
        7..=9 => &["sgp1", "syd1"],
        _ => &["syd1", "sgp1"],
    };
    preferences.iter().map(|s| s.to_string()).find(|s| available.contains(s)).or_else(|| available.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_choice_per_city() {
        let r = |slug: &str, name: &str| Region { slug: slug.into(), name: name.into(), sizes: Vec::new() };
        let regions = vec![r("nyc1", "New York 1"), r("sgp1", "Singapore 1"), r("nyc3", "New York 3"), r("lon1", "London 1"), r("nyc2", "New York 2")];
        assert_eq!(
            cities(&regions),
            vec![("New York".to_string(), "nyc3".to_string()), ("London".into(), "lon1".into()), ("Singapore".into(), "sgp1".into())]
        );
        assert_eq!(city_of_slug(&regions, "nyc1").as_deref(), Some("New York"));
    }
}
