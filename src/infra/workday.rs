//! `HolidayCalendar`: a `WorkdayCalendar` backed by a free Chinese
//! holiday/调休 API, cached on disk one year at a time.
//!
//! Data source is `jiejiariapi.com` (`GET /v1/holidays/{year}`), which returns
//! a map keyed by `YYYY-MM-DD` → `{ "isOffDay": bool }` listing only the
//! *special* days of the year: statutory holidays (`isOffDay: true`) and 调休
//! makeup workdays (`isOffDay: false`, a weekend you must work). Any date not in
//! the map follows the ordinary Monday–Friday rule.
//!
//! Caching is deliberately coarse — "fetch a year the first time any date in it
//! is asked, then never again" — because the user only needs a yearly refresh:
//! the State Council publishes the next year's arrangement once, late in the
//! prior year. Each year is cached to `<komo_home>/workdays/{year}.json`; a new
//! year rolls in automatically the first time it is queried. Delete a year's
//! file (or the whole `workdays/` dir) to force a re-fetch.
//!
//! Every failure path degrades to `is_weekday` rather than erroring, so a
//! network blip or a year the API hasn't published yet just falls back to
//! Mon–Fri instead of breaking the gate.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Datelike;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::domain::workday::{WorkdayCalendar, is_weekday};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const API_BASE: &str = "https://api.jiejiariapi.com/v1/holidays";

/// One day in the API response. Only `isOffDay` matters: `true` = a rest day
/// (statutory holiday), `false` = a makeup workday (调休) on a weekend.
#[derive(Deserialize)]
struct ApiEntry {
    #[serde(rename = "isOffDay")]
    is_off_day: bool,
}

/// A loaded year: the special days only (`date → isOffDay`). Absent dates use
/// the Mon–Fri default.
type YearMap = HashMap<chrono::NaiveDate, bool>;

pub struct HolidayCalendar {
    http: reqwest::Client,
    cache_dir: PathBuf,
    /// In-memory cache of loaded years. A year is inserted only once it has been
    /// resolved from disk or the network, so a failed fetch is retried on the
    /// next call rather than poisoning the year.
    loaded: Mutex<HashMap<i32, Arc<YearMap>>>,
}

impl HolidayCalendar {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("failed to build reqwest client"),
            cache_dir,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    fn year_file(&self, year: i32) -> PathBuf {
        self.cache_dir.join(format!("{year}.json"))
    }

    /// Resolve a year's map: memory → disk → network. Returns `None` only when
    /// the year is nowhere to be found (no cache and the fetch failed), so the
    /// caller can fall back to the weekday rule. The lock is held across the
    /// fetch so two concurrent first-lookups don't both hit the network — fine
    /// here because lookups happen at most once per maintenance tick.
    async fn year_map(&self, year: i32) -> Option<Arc<YearMap>> {
        let mut loaded = self.loaded.lock().await;
        if let Some(map) = loaded.get(&year) {
            return Some(map.clone());
        }

        // Disk cache (written by a prior run or a prior year-rollover fetch).
        if let Some(map) = self.read_disk(year) {
            let map = Arc::new(map);
            loaded.insert(year, map.clone());
            return Some(map);
        }

        // First time we've needed this year: fetch and persist.
        match self.fetch(year).await {
            Ok(map) => {
                self.write_disk(year, &map);
                let map = Arc::new(map);
                loaded.insert(year, map.clone());
                Some(map)
            }
            Err(error) => {
                warn!(%error, year, "workday calendar fetch failed; using Mon–Fri fallback");
                None
            }
        }
    }

    fn read_disk(&self, year: i32) -> Option<YearMap> {
        let path = self.year_file(year);
        let bytes = std::fs::read(&path).ok()?;
        // On-disk form mirrors the API: a `date-string → isOffDay` map.
        let raw: HashMap<String, bool> = serde_json::from_slice(&bytes)
            .map_err(|e| warn!(error = %e, ?path, "corrupt workday cache; ignoring"))
            .ok()?;
        debug!(year, days = raw.len(), "workday calendar loaded from disk");
        Some(parse_dates(raw))
    }

    fn write_disk(&self, year: i32, map: &YearMap) {
        if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
            warn!(error = %e, dir = ?self.cache_dir, "could not create workday cache dir");
            return;
        }
        let raw: HashMap<String, bool> = map
            .iter()
            .map(|(d, off)| (d.format("%Y-%m-%d").to_string(), *off))
            .collect();
        match serde_json::to_vec(&raw) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(self.year_file(year), bytes) {
                    warn!(error = %e, year, "could not write workday cache");
                }
            }
            Err(e) => warn!(error = %e, year, "could not serialize workday cache"),
        }
    }

    async fn fetch(&self, year: i32) -> anyhow::Result<YearMap> {
        let url = format!("{API_BASE}/{year}");
        let raw: HashMap<String, ApiEntry> = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        debug!(year, days = raw.len(), "workday calendar fetched");
        Ok(parse_dates(
            raw.into_iter().map(|(k, v)| (k, v.is_off_day)).collect(),
        ))
    }
}

/// Parse `YYYY-MM-DD` keys into `NaiveDate`, dropping any the source got wrong
/// rather than failing the whole year.
fn parse_dates(raw: HashMap<String, bool>) -> YearMap {
    raw.into_iter()
        .filter_map(|(k, off)| {
            chrono::NaiveDate::parse_from_str(&k, "%Y-%m-%d")
                .ok()
                .map(|d| (d, off))
        })
        .collect()
}

#[async_trait]
impl WorkdayCalendar for HolidayCalendar {
    async fn is_workday(&self, date: chrono::NaiveDate) -> bool {
        match self.year_map(date.year()).await {
            // A listed special day overrides the weekday rule: holiday → off,
            // 调休 makeup day → work.
            Some(map) => match map.get(&date) {
                Some(&is_off) => !is_off,
                None => is_weekday(date),
            },
            None => is_weekday(date),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parse_dates_drops_bad_keys() {
        let raw = HashMap::from([
            ("2026-01-01".to_string(), true),
            ("not-a-date".to_string(), false),
        ]);
        let map = parse_dates(raw);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&d("2026-01-01")), Some(&true));
    }

    #[tokio::test]
    async fn listed_holiday_is_not_a_workday_and_makeup_day_is() {
        // Seed the disk cache so no network is touched: New Year's Day off, and
        // a Saturday (2026-02-14) flipped to a makeup workday by 调休.
        let dir = std::env::temp_dir().join(format!("komo-workday-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = HashMap::from([
            ("2026-01-01".to_string(), true),
            ("2026-02-14".to_string(), false),
        ]);
        std::fs::write(dir.join("2026.json"), serde_json::to_vec(&raw).unwrap()).unwrap();

        let cal = HolidayCalendar::new(dir.clone());
        assert!(!cal.is_workday(d("2026-01-01")).await, "holiday → off");
        assert!(
            cal.is_workday(d("2026-02-14")).await,
            "Saturday makeup day → work"
        );
        // A Saturday with no override stays off; a plain weekday stays on.
        assert!(!cal.is_workday(d("2026-02-07")).await, "ordinary Saturday");
        assert!(cal.is_workday(d("2026-02-06")).await, "ordinary Friday");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn weekday_rule_is_the_fallback() {
        // The "no data" default, exercised without any network: weekdays work,
        // weekends rest. (2026-06-19 is a Friday; 2026-06-20 a Saturday.)
        assert!(is_weekday(d("2026-06-19")));
        assert!(!is_weekday(d("2026-06-20")));
    }
}
