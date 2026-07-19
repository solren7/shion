//! `komo workday [date]` — operator view of the Chinese working-day calendar.
//!
//! Reports whether a date is a workday (and triggers the once-a-year fetch +
//! cache for its year if not already cached), so the gate can be verified
//! without waiting for a scheduled sweep. Defaults to today.

use crate::domain::workday::WorkdayCalendar;
use crate::infra::workday::HolidayCalendar;

pub async fn check(date: Option<String>) -> anyhow::Result<()> {
    let date = match date {
        Some(s) => chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .map_err(|e| anyhow::anyhow!("invalid date `{s}` (expected YYYY-MM-DD): {e}"))?,
        None => chrono::Local::now().date_naive(),
    };

    let calendar = HolidayCalendar::new(crate::config::workday_cache_dir());
    let workday = calendar.is_workday(date).await;
    let weekday = date.format("%A");
    println!(
        "{date} ({weekday}): {}",
        if workday {
            "workday — 上班"
        } else {
            "off — 休息"
        }
    );
    Ok(())
}
