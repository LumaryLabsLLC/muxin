//! What cloud machines cost. DigitalOcean bills a droplet by the second (at
//! least $0.01) from creation until it's destroyed, powered off too, up to
//! 672 hours a month on standard plans: never more than its monthly price.

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use serde::Serialize;

use super::digitalocean::Balance;

/// What the cloud machines cost: now, this month, and by the month's end.
#[derive(Debug, Clone, Serialize)]
pub struct CloudUsage {
    pub currency: &'static str,
    /// "2026-10"
    pub month: String,
    /// What the ones that exist cost per hour together, on or off.
    pub hourly_now: f64,
    /// This month so far, destroyed ones included.
    pub this_month: f64,
    /// This month if the ones that exist are kept until it ends.
    pub month_if_kept: f64,
    pub machines: Vec<UsageLine>,
    /// The whole DigitalOcean account's bill this month, when Muxen may read it.
    pub account: Option<Balance>,
    /// Why `account` is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_error: Option<String>,
    /// Signing in again (in the browser) would let Muxen read the bill.
    pub account_needs_sign_in: bool,
    pub billing: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageLine {
    /// None once destroyed.
    pub machine_id: Option<String>,
    pub name: String,
    pub region: String,
    pub size: String,
    /// "on" | "off" | "starting" | "destroyed" | null (not known yet)
    pub power: Option<String>,
    pub price_hourly: f64,
    /// The most it costs in a month.
    pub price_monthly: f64,
    pub created_at: Option<DateTime<Utc>>,
    pub destroyed_at: Option<DateTime<Utc>>,
    pub this_month: f64,
    pub since_created: f64,
}

/// Hours a month DigitalOcean bills at most.
pub const MONTHLY_CAP_HOURS: f64 = 672.;

/// How DigitalOcean bills, in one sentence for people.
pub const BILLING: &str = "DigitalOcean bills a droplet by the second while it exists, powered on or off, up to its monthly price. Destroying it stops the charges.";

/// The most a droplet costs in a month.
pub fn monthly_price(hourly: f64) -> f64 {
    round2(hourly * MONTHLY_CAP_HOURS)
}

/// The first moment of `t`'s month (UTC, as DigitalOcean bills).
pub fn month_start(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(t.year(), t.month(), 1, 0, 0, 0).single().unwrap_or(t)
}

fn next_month(t: DateTime<Utc>) -> DateTime<Utc> {
    month_start(t).checked_add_months(Months::new(1)).unwrap_or(t)
}

/// What it cost in the month that starts at `month`, existing from `created`
/// until `ended` (or still, at `now`).
pub fn cost_in_month(hourly: f64, created: DateTime<Utc>, ended: Option<DateTime<Utc>>, month: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let start = created.max(month);
    let end = ended.unwrap_or(now).min(now).min(next_month(month));
    if end <= start || hourly <= 0. {
        return 0.;
    }
    let hours = (end - start).num_seconds() as f64 / 3600.;
    round2((hours.min(MONTHLY_CAP_HOURS) * hourly).max(0.01))
}

/// What it has cost since it was created, month by month.
pub fn cost_since(hourly: f64, created: DateTime<Utc>, ended: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f64 {
    let last = ended.unwrap_or(now).min(now);
    let mut month = month_start(created);
    let mut total = 0.;
    while month <= last {
        total += cost_in_month(hourly, created, ended, month, now);
        month = next_month(month);
    }
    round2(total)
}

/// What it will have cost this month if it keeps existing until the month ends.
pub fn month_if_kept(hourly: f64, created: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let end = next_month(now);
    cost_in_month(hourly, created, Some(end), month_start(now), end)
}

pub fn round2(v: f64) -> f64 {
    (v * 100.).round() / 100.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn bills_by_the_second_up_to_the_monthly_price() {
        let hourly = 0.00893;
        assert_eq!(monthly_price(hourly), 6.0);
        let created = t("2026-10-09T18:53:00Z");
        let month = month_start(created);
        // Two hours in: two hours' worth, but at least a cent.
        assert_eq!(cost_in_month(hourly, created, None, month, t("2026-10-09T20:53:00Z")), 0.02);
        assert_eq!(cost_in_month(hourly, created, None, month, t("2026-10-09T18:54:00Z")), 0.01);
        // A whole month never costs more than the monthly price.
        assert_eq!(cost_in_month(hourly, t("2026-09-01T00:00:00Z"), None, month_start(t("2026-09-15T00:00:00Z")), t("2026-10-05T00:00:00Z")), 6.0);
        // Destroyed after a day.
        assert_eq!(cost_in_month(hourly, created, Some(t("2026-10-10T18:53:00Z")), month, t("2026-10-20T00:00:00Z")), 0.21);
        // Not in a month it didn't exist in.
        assert_eq!(cost_in_month(hourly, created, None, month_start(t("2026-09-15T00:00:00Z")), t("2026-10-20T00:00:00Z")), 0.0);
    }

    #[test]
    fn adds_up_months_and_projects_this_one() {
        let hourly = 0.03571; // $24/mo
        let created = t("2026-09-20T00:00:00Z");
        let now = t("2026-10-02T00:00:00Z");
        // September: 11 days = 264h; October so far: 1 day.
        assert_eq!(cost_since(hourly, created, None, now), round2(round2(264. * hourly) + round2(24. * hourly)));
        // Kept all of October: capped at the monthly price.
        assert_eq!(month_if_kept(hourly, created, now), monthly_price(hourly));
        // Created late in the month: only what's left of it.
        assert_eq!(month_if_kept(hourly, t("2026-10-30T00:00:00Z"), t("2026-10-30T00:00:00Z")), round2(48. * hourly));
    }
}
