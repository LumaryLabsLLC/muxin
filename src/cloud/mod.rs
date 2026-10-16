//! Cloud machines: droplets Muxin creates in the user's DigitalOcean account,
//! sets up, and adds to your machines (see ops::cloud for the operations).

pub mod auth;
pub mod digitalocean;
pub mod secrets;
pub mod setup;
pub mod usage;

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
    /// The browser sign-in is built into this copy of Muxin.
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

/// Basic droplets on regular CPUs, the ones Muxin offers: the premium AMD
/// and Intel twins of each size are left out.
pub fn regular_size(slug: &str) -> bool {
    slug.starts_with("s-") && !slug.ends_with("-amd") && !slug.ends_with("-intel")
}

/// The sizes on offer in a region, cheapest first (as DigitalOcean lists them).
pub fn offered<'a>(sizes: &'a [Size], region: &str) -> impl Iterator<Item = &'a Size> {
    let region = region.to_string();
    sizes.iter().filter(move |s| regular_size(&s.slug) && s.regions.contains(&region))
}

/// One region per city, to pick from: the one with the most sizes on offer
/// (DigitalOcean stops offering sizes in some datacenters, New York's older
/// ones among them), in a fixed order of cities. Cities with none are left out.
pub fn cities(regions: &[Region], sizes: &[Size]) -> Vec<(String, String)> {
    const ORDER: &[&str] = &[
        "New York", "Toronto", "Atlanta", "Richmond", "San Francisco", "Kansas City", "Memphis",
        "London", "Amsterdam", "Frankfurt",
        "Bangalore", "Singapore", "Sydney",
    ];
    let city_of = |r: &Region| r.name.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ').trim().to_string();
    let number = |slug: &str| slug.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u32>().unwrap_or(0);
    // (city, slug, sizes on offer there)
    let mut best: Vec<(String, String, usize)> = Vec::new();
    for r in regions {
        let count = offered(sizes, &r.slug).count();
        if count == 0 {
            continue;
        }
        let city = city_of(r);
        let city = if city.is_empty() { r.slug.to_uppercase() } else { city };
        match best.iter_mut().find(|(c, _, _)| *c == city) {
            Some(entry) if (count, number(&r.slug)) > (entry.2, number(&entry.1)) => {
                entry.1 = r.slug.clone();
                entry.2 = count;
            }
            Some(_) => {}
            None => best.push((city, r.slug.clone(), count)),
        }
    }
    let rank = |c: &str| ORDER.iter().position(|o| *o == c).unwrap_or(ORDER.len());
    best.sort_by_key(|(c, _, _)| rank(c));
    best.into_iter().map(|(city, slug, _)| (city, slug)).collect()
}

/// The city a region slug is in, by the same names.
pub fn city_of_slug(regions: &[Region], slug: &str) -> Option<String> {
    let r = regions.iter().find(|r| r.slug == slug)?;
    Some(r.name.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ').trim().to_string())
}

/// A region's city from its slug alone ("nyc3" → "New York"), for when the
/// region list isn't at hand.
pub fn region_city(slug: &str) -> Option<&'static str> {
    let prefix = slug.trim_end_matches(|c: char| c.is_ascii_digit());
    Some(match prefix {
        "nyc" => "New York",
        "sfo" => "San Francisco",
        "tor" => "Toronto",
        "atl" => "Atlanta",
        "ric" => "Richmond",
        "mkc" => "Kansas City",
        "mem" => "Memphis",
        "lon" => "London",
        "ams" => "Amsterdam",
        "fra" => "Frankfurt",
        "blr" => "Bangalore",
        "sgp" => "Singapore",
        "syd" => "Sydney",
        _ => return None,
    })
}

/// A size slug in words: "s-2vcpu-4gb" → "2 vCPU · 4 GB" (the slug itself
/// when it doesn't say).
pub fn size_label(slug: &str) -> String {
    let parts: Vec<&str> = slug.split('-').collect();
    let cpu = parts.iter().find_map(|p| p.strip_suffix("vcpu"));
    let mem = parts.iter().find_map(|p| p.strip_suffix("gb").filter(|n| n.chars().all(|c| c.is_ascii_digit())));
    match (cpu, mem) {
        (Some(c), Some(m)) => format!("{c} vCPU · {m} GB"),
        _ => slug.to_string(),
    }
}

/// The region nearest this computer, by its UTC offset, among `available`.
pub fn nearest_region(offset_hours: i32, available: &[String]) -> Option<String> {
    // Nearest first, then the next nearest: a region can have nothing on offer.
    let preferences: &[&str] = match offset_hours {
        ..=-9 => &["sfo3", "sfo2", "tor1", "nyc3", "nyc1", "syd1"],
        -8..=-6 => &["sfo3", "sfo2", "tor1", "nyc3", "nyc1", "atl1"],
        -5..=-3 => &["nyc3", "nyc1", "tor1", "atl1", "ric1", "sfo3", "sfo2", "lon1", "ams3"],
        -2..=2 => &["lon1", "ams3", "fra1", "nyc3", "tor1"],
        3..=4 => &["fra1", "ams3", "lon1", "blr1"],
        5..=6 => &["blr1", "sgp1", "fra1"],
        7..=9 => &["sgp1", "syd1", "blr1"],
        _ => &["syd1", "sgp1"],
    };
    preferences.iter().map(|s| s.to_string()).find(|s| available.contains(s)).or_else(|| available.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_and_sizes_in_words() {
        assert_eq!(region_city("nyc3"), Some("New York"));
        assert_eq!(region_city("mkc1"), Some("Kansas City"));
        assert_eq!(region_city("zzz1"), None);
        assert_eq!(size_label("s-1vcpu-1gb"), "1 vCPU · 1 GB");
        assert_eq!(size_label("s-2vcpu-4gb-amd"), "2 vCPU · 4 GB");
        assert_eq!(size_label("gpu-h100x1-80gb"), "gpu-h100x1-80gb");
    }

    #[test]
    fn one_choice_per_city() {
        let r = |slug: &str, name: &str| Region { slug: slug.into(), name: name.into(), sizes: Vec::new() };
        let size = |slug: &str, regions: &[&str]| Size { slug: slug.into(), regions: regions.iter().map(|r| r.to_string()).collect(), ..Default::default() };
        let regions = vec![r("nyc1", "New York 1"), r("sgp1", "Singapore 1"), r("nyc3", "New York 3"), r("lon1", "London 1"), r("nyc2", "New York 2"), r("atl1", "Atlanta 1")];
        // nyc3 has the most on offer; atl1 has only a premium size, so it isn't offered.
        let sizes = vec![
            size("s-1vcpu-1gb", &["nyc1", "nyc3", "sgp1", "lon1"]),
            size("s-2vcpu-2gb", &["nyc3", "lon1"]),
            size("s-2vcpu-2gb-intel", &["atl1"]),
        ];
        assert_eq!(
            cities(&regions, &sizes),
            vec![("New York".to_string(), "nyc3".to_string()), ("London".into(), "lon1".into()), ("Singapore".into(), "sgp1".into())]
        );
        // When the newest datacenter has nothing on offer, an older one with sizes is chosen.
        let sizes = vec![size("s-1vcpu-1gb", &["nyc1", "lon1"]), size("s-2vcpu-2gb", &["lon1"])];
        assert_eq!(cities(&regions, &sizes), vec![("New York".to_string(), "nyc1".to_string()), ("London".into(), "lon1".into())]);
        assert_eq!(city_of_slug(&regions, "nyc1").as_deref(), Some("New York"));
        assert_eq!(offered(&sizes, "lon1").count(), 2);
    }
}
