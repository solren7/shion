//! Background maintenance daemon.
//!
//! Borrowed from gbrain's `autopilot` supervisor (a long-running loop that runs
//! one work "cycle" on a schedule), trimmed to shion's needs:
//!
//!   - **cron-expression scheduling** — 5-field Unix syntax (`*/5 * * * *`) via
//!     `croner`, rather than gbrain's fixed interval seconds.
//!   - **single fixed maintenance action** — a sweep that runs the reflective
//!     reviewer over stored sessions, instead of gbrain's brain-sync cycle.
//!   - **circuit breaker** — stop after N consecutive failures so a permanent
//!     error (bad config, dead LLM) can't spin forever. This mirrors gbrain's
//!     `consecutiveErrors >= 5` cap / launchd `ThrottleInterval`.
//!
//! The OS-level supervisor install (launchd / systemd / crontab) that gbrain
//! also ships is intentionally left out of v0.1: this is the in-process loop
//! only, which a later `shion daemon --install` can wrap.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use croner::Cron;
use tracing::{error, info, warn};

use crate::domain::{
    notify::Notifier,
    reminder::{ReminderRepository, ReminderStatus},
    repository::SessionRepository,
    reviewer::Reviewer,
};

/// Stop the daemon once this many maintenance cycles fail back-to-back.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// A parsed cron schedule. Wraps `croner` so the supervisor never touches the
/// cron crate directly and the "when does it next fire" math stays testable.
pub struct Schedule {
    cron: Cron,
}

impl Schedule {
    /// Parse a 5-field Unix cron expression (e.g. `0 * * * *` for hourly).
    pub fn parse(expr: &str) -> anyhow::Result<Self> {
        let cron = expr
            .parse::<Cron>()
            .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
        Ok(Self { cron })
    }

    /// Duration from `now` until the next scheduled fire (strictly after `now`).
    fn next_after(&self, now: chrono::DateTime<Utc>) -> anyhow::Result<Duration> {
        let next = self.cron.find_next_occurrence(&now, false)?;
        Ok((next - now).to_std().unwrap_or(Duration::ZERO))
    }
}

/// One scheduled unit of work. Kept behind a trait so the supervisor loop can be
/// exercised without a real reviewer or database.
#[async_trait]
pub trait Maintenance: Send + Sync {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaintenanceSummary {
    pub sessions_reviewed: usize,
    pub memories_written: usize,
    pub skills_written: usize,
    pub reminders_fired: usize,
}

/// The fixed maintenance action: review every stored session that has at least
/// one user turn, letting the reviewer distill durable memories/skills.
pub struct ReviewSweep {
    pub sessions: Arc<dyn SessionRepository>,
    pub reviewer: Arc<dyn Reviewer>,
}

#[async_trait]
impl Maintenance for ReviewSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let sessions = self.sessions.list().await?;
        let mut summary = MaintenanceSummary::default();
        for session in sessions {
            if session.user_turns() == 0 {
                continue;
            }
            // Isolate per-session failures: a single bad review must not abort
            // the whole sweep (gbrain's "a failing phase never crashes the loop").
            match self.reviewer.review(&session).await {
                Ok(outcome) => {
                    summary.sessions_reviewed += 1;
                    summary.memories_written += outcome.memories_written.len();
                    summary.skills_written += outcome.skills_written.len();
                }
                Err(error) => {
                    warn!(%error, session = %session.id, "session review failed (skipped)")
                }
            }
        }
        Ok(summary)
    }
}

/// Grace window: reminders missed by up to this many seconds are delivered late
/// (with a "missed" prefix); older ones are marked missed without re-notifying.
const REMINDER_GRACE_SECS: i64 = 600;

/// Compute the next occurrence of a cron expression strictly after `after`.
/// Timezone-generic so tests can use `FixedOffset` for determinism while
/// production uses `Local`.
pub fn next_occurrence_in<Tz>(
    expr: &str,
    after: chrono::DateTime<Tz>,
) -> anyhow::Result<chrono::DateTime<Tz>>
where
    Tz: chrono::TimeZone + Clone,
{
    let cron = expr
        .parse::<Cron>()
        .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
    Ok(cron.find_next_occurrence(&after, false)?)
}

/// Production wrapper: compute the next local-time occurrence after `after_unix`
/// and return it as a Unix timestamp. Computes from the given time (usually
/// `now`) so a resting daemon always jumps to the next future slot.
pub fn next_occurrence_local(expr: &str, after_unix: i64) -> anyhow::Result<i64> {
    let after_utc = chrono::DateTime::from_timestamp(after_unix, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid unix timestamp: {after_unix}"))?;
    let after_local = after_utc.with_timezone(&chrono::Local);
    let next = next_occurrence_in(expr, after_local)?;
    Ok(next.timestamp())
}

/// Sweep due reminders every minute and deliver them as desktop notifications.
pub struct ReminderSweep {
    pub reminders: Arc<dyn ReminderRepository>,
    pub notifier: Arc<dyn Notifier>,
}

#[async_trait]
impl Maintenance for ReminderSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut summary = MaintenanceSummary::default();

        for r in self.reminders.list_pending().await? {
            if r.run_at > now {
                continue;
            }
            let late = now - r.run_at;

            if r.is_recurring() {
                // Notify first, then reschedule — prefer duplicate over silent loss.
                let title = if late > REMINDER_GRACE_SECS {
                    "Shion (missed reminder)"
                } else {
                    "Shion reminder"
                };
                self.notifier.notify(title, &r.message).await.ok();
                // Compute next occurrence from now (not run_at) so a resting daemon
                // always jumps to a future slot without replaying missed ticks.
                match next_occurrence_local(&r.schedule, now) {
                    Ok(next) => {
                        if let Err(e) = self.reminders.reschedule(&r.id, next).await {
                            warn!(error = %e, id = %r.id, "failed to reschedule recurring reminder");
                        } else {
                            summary.reminders_fired += 1;
                        }
                    }
                    Err(e) => {
                        // Broken expression (bypassed tool validation): degrade to
                        // missed so we don't spam errors on every tick.
                        warn!(error = %e, id = %r.id, "broken schedule; marking missed");
                        if let Err(e) = self
                            .reminders
                            .set_status(&r.id, ReminderStatus::Missed)
                            .await
                        {
                            warn!(error = %e, id = %r.id, "failed to mark reminder missed");
                        }
                    }
                }
            } else {
                // One-shot path — original v1 logic unchanged.
                if late > REMINDER_GRACE_SECS {
                    self.notifier
                        .notify("Shion (missed reminder)", &r.message)
                        .await
                        .ok();
                    if let Err(e) = self
                        .reminders
                        .set_status(&r.id, ReminderStatus::Missed)
                        .await
                    {
                        warn!(error = %e, id = %r.id, "failed to mark reminder missed");
                    }
                } else {
                    self.notifier
                        .notify("Shion reminder", &r.message)
                        .await
                        .ok();
                    if let Err(e) = self
                        .reminders
                        .set_status(&r.id, ReminderStatus::Fired)
                        .await
                    {
                        warn!(error = %e, id = %r.id, "failed to mark reminder fired");
                    } else {
                        summary.reminders_fired += 1;
                    }
                }
            }
        }
        Ok(summary)
    }
}

/// Update the consecutive-failure counter and report whether the circuit breaker
/// has tripped. Pulled out as a pure function so the breaker is unit-testable
/// without driving the real clock.
fn breaker_tripped(consecutive_failures: &mut u32, cycle_ok: bool) -> bool {
    if cycle_ok {
        *consecutive_failures = 0;
        false
    } else {
        *consecutive_failures += 1;
        *consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }
}

/// Run maintenance on `schedule` until `shutdown` resolves or the circuit
/// breaker trips. Returns `Ok` on a clean shutdown, `Err` when the breaker stops
/// the loop.
pub async fn supervise<S>(
    schedule: &Schedule,
    maintenance: Arc<dyn Maintenance>,
    shutdown: S,
) -> anyhow::Result<()>
where
    S: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut consecutive_failures = 0u32;

    loop {
        let wait = schedule.next_after(Utc::now())?;
        info!(seconds = wait.as_secs(), "next maintenance cycle scheduled");

        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received; stopping daemon");
                return Ok(());
            }
            _ = tokio::time::sleep(wait) => {}
        }

        let started = std::time::Instant::now();
        let cycle_ok = match maintenance.run().await {
            Ok(summary) => {
                info!(
                    sessions = summary.sessions_reviewed,
                    memories = summary.memories_written,
                    skills = summary.skills_written,
                    reminders = summary.reminders_fired,
                    elapsed_s = started.elapsed().as_secs(),
                    "maintenance cycle complete"
                );
                true
            }
            Err(error) => {
                error!(%error, "maintenance cycle failed");
                false
            }
        };

        if breaker_tripped(&mut consecutive_failures, cycle_ok) {
            anyhow::bail!(
                "{MAX_CONSECUTIVE_FAILURES} consecutive maintenance failures; stopping daemon"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reminder::{Reminder, ReminderStatus};
    use chrono::{Datelike, TimeZone, Timelike};
    use std::sync::Mutex;

    // ── FakeReminderRepository ────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeRepo {
        reminders: Mutex<Vec<Reminder>>,
    }

    #[async_trait]
    impl ReminderRepository for FakeRepo {
        async fn save(&self, reminder: &Reminder) -> anyhow::Result<()> {
            self.reminders.lock().unwrap().push(reminder.clone());
            Ok(())
        }

        async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>> {
            Ok(self
                .reminders
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.status == ReminderStatus::Pending)
                .cloned()
                .collect())
        }

        async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()> {
            if let Some(r) = self
                .reminders
                .lock()
                .unwrap()
                .iter_mut()
                .find(|r| r.id == id)
            {
                r.status = status;
            }
            Ok(())
        }

        async fn reschedule(&self, id: &str, next_run_at: i64) -> anyhow::Result<()> {
            if let Some(r) = self
                .reminders
                .lock()
                .unwrap()
                .iter_mut()
                .find(|r| r.id == id)
            {
                r.run_at = next_run_at;
            }
            Ok(())
        }
    }

    // ── FakeNotifier ──────────────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeNotifier {
        calls: Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    #[async_trait]
    impl Notifier for FakeNotifier {
        async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
            if self.fail {
                return Err(anyhow::anyhow!("notification failed"));
            }
            self.calls
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    fn sweep_with(
        reminders: Vec<Reminder>,
        notifier_fail: bool,
    ) -> (ReminderSweep, Arc<FakeRepo>, Arc<FakeNotifier>) {
        let repo = Arc::new(FakeRepo {
            reminders: Mutex::new(reminders),
        });
        let notifier = Arc::new(FakeNotifier {
            fail: notifier_fail,
            ..Default::default()
        });
        let sweep = ReminderSweep {
            reminders: repo.clone() as Arc<dyn ReminderRepository>,
            notifier: notifier.clone() as Arc<dyn Notifier>,
        };
        (sweep, repo, notifier)
    }

    fn past_reminder(secs_ago: i64) -> Reminder {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Reminder::new("test".to_string(), now - secs_ago)
    }

    fn future_reminder() -> Reminder {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Reminder::new("future".to_string(), now + 3600)
    }

    fn recurring_reminder(secs_ago: i64, schedule: &str) -> Reminder {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Reminder::recurring("test".to_string(), now - secs_ago, schedule.to_string())
    }

    #[tokio::test]
    async fn sweep_fires_due_reminder() {
        let r = past_reminder(30);
        let id = r.id.clone();
        let (sweep, repo, notifier) = sweep_with(vec![r], false);
        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.reminders_fired, 1);
        assert_eq!(notifier.calls.lock().unwrap().len(), 1);
        let status = repo
            .reminders
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, ReminderStatus::Fired);
    }

    #[tokio::test]
    async fn sweep_skips_future_reminder() {
        let (sweep, _, notifier) = sweep_with(vec![future_reminder()], false);
        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.reminders_fired, 0);
        assert!(notifier.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_marks_long_overdue_as_missed() {
        let r = past_reminder(REMINDER_GRACE_SECS + 60);
        let id = r.id.clone();
        let (sweep, repo, notifier) = sweep_with(vec![r], false);
        sweep.run().await.unwrap();
        let status = repo
            .reminders
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, ReminderStatus::Missed);
        let title = &notifier.calls.lock().unwrap()[0].0;
        assert!(title.contains("missed"));
    }

    #[tokio::test]
    async fn notifier_failure_does_not_abort_sweep() {
        let r1 = past_reminder(10);
        let r2 = past_reminder(20);
        let (sweep, repo, _) = sweep_with(vec![r1, r2], true);
        // Should not error even though notifier always fails.
        sweep.run().await.unwrap();
        // Both reminders attempted set_status despite notify failures.
        let statuses: Vec<_> = repo
            .reminders
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.status.clone())
            .collect();
        // set_status is called after notify — with fail=true, notify returns
        // Err but sweep uses .ok(), so set_status still runs.
        assert!(
            statuses
                .iter()
                .all(|s| *s == ReminderStatus::Fired || *s == ReminderStatus::Pending)
        );
    }

    // ── cron helpers ─────────────────────────────────────────────────────────

    #[test]
    fn next_occurrence_in_computes_strictly_future_fire() {
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let expr = "0 9 * * *"; // 9 AM daily

        // 8 AM local → next occurrence is 9 AM the same day
        let at_8am = tz.with_ymd_and_hms(2024, 1, 1, 8, 0, 0).unwrap();
        let next = next_occurrence_in(expr, at_8am).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 1);

        // exactly 9 AM local → next is 9 AM the following day (strictly future)
        let at_9am = tz.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        let next = next_occurrence_in(expr, at_9am).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 2);
    }

    #[test]
    fn next_occurrence_in_rejects_invalid_expr() {
        let result = next_occurrence_in("not a cron", chrono::Utc::now());
        assert!(result.is_err());
    }

    // ── recurring sweep ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn sweep_advances_recurring_reminder() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let r = recurring_reminder(30, "* * * * *");
        let id = r.id.clone();
        let (sweep, repo, notifier) = sweep_with(vec![r], false);
        sweep.run().await.unwrap();

        assert_eq!(notifier.calls.lock().unwrap().len(), 1);
        assert_eq!(notifier.calls.lock().unwrap()[0].0, "Shion reminder");

        let rems = repo.reminders.lock().unwrap();
        let updated = rems.iter().find(|r| r.id == id).unwrap();
        assert_eq!(updated.status, ReminderStatus::Pending);
        assert!(updated.run_at > now);
    }

    #[tokio::test]
    async fn sweep_recurring_overdue_fires_once_and_skips_catchup() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let r = recurring_reminder(3 * 86400, "0 9 * * *");
        let id = r.id.clone();
        let (sweep, repo, notifier) = sweep_with(vec![r], false);
        sweep.run().await.unwrap();

        // Only one notification (missed)
        assert_eq!(notifier.calls.lock().unwrap().len(), 1);
        assert!(notifier.calls.lock().unwrap()[0].0.contains("missed"));

        let rems = repo.reminders.lock().unwrap();
        let updated = rems.iter().find(|r| r.id == id).unwrap();
        assert_eq!(updated.status, ReminderStatus::Pending);
        assert!(updated.run_at > now);
    }

    #[tokio::test]
    async fn sweep_marks_recurring_with_broken_schedule_missed() {
        let r = recurring_reminder(30, "not a valid cron");
        let id = r.id.clone();
        let (sweep, repo, _) = sweep_with(vec![r], false);
        sweep.run().await.unwrap();

        let rems = repo.reminders.lock().unwrap();
        let updated = rems.iter().find(|r| r.id == id).unwrap();
        assert_eq!(updated.status, ReminderStatus::Missed);
    }

    #[test]
    fn rejects_invalid_cron() {
        assert!(Schedule::parse("not a cron").is_err());
    }

    #[test]
    fn next_fire_of_every_minute_is_within_a_minute() {
        let schedule = Schedule::parse("* * * * *").unwrap();
        let wait = schedule.next_after(Utc::now()).unwrap();
        assert!(wait <= Duration::from_secs(60));
    }

    #[test]
    fn breaker_trips_only_after_max_consecutive_failures() {
        let mut failures = 0u32;
        // The first MAX-1 straight failures do not trip the breaker.
        for _ in 0..MAX_CONSECUTIVE_FAILURES - 1 {
            assert!(!breaker_tripped(&mut failures, false));
        }
        // The MAX-th straight failure trips it.
        assert!(breaker_tripped(&mut failures, false));
    }

    #[test]
    fn breaker_resets_on_success() {
        let mut failures = 0u32;
        breaker_tripped(&mut failures, false);
        breaker_tripped(&mut failures, false);
        // A success clears the count so the next failure starts from one.
        breaker_tripped(&mut failures, true);
        assert_eq!(failures, 0);
        assert!(!breaker_tripped(&mut failures, false));
        assert_eq!(failures, 1);
    }
}
