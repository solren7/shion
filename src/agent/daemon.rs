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
    llm::LlmClient,
    memory::{Memory, MemoryRepository},
    message::Message,
    notify::Notifier,
    reminder::{ReminderRepository, ReminderStatus},
    repository::SessionRepository,
    reviewer::Reviewer,
    session::Session,
    task::{Task, TaskRepository},
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
    pub tasks_notified: usize,
    /// Commitments the reviewer captured into the task inbox this sweep.
    pub tasks_captured: usize,
    /// Daily briefings composed and delivered this sweep (0 or 1).
    pub briefings_sent: usize,
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
                    summary.tasks_captured += outcome.tasks_captured.len();
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

/// Sweep open tasks every minute and notify once when one comes due. Unlike a
/// reminder, the task itself stays open — only `due_notified_at` flips, which
/// is the at-most-once guard.
pub struct TaskSweep {
    pub tasks: Arc<dyn TaskRepository>,
    pub notifier: Arc<dyn Notifier>,
}

#[async_trait]
impl Maintenance for TaskSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut summary = MaintenanceSummary::default();

        for task in self.tasks.list_open().await? {
            let Some(due_at) = task.due_at else {
                continue;
            };
            if due_at > now || task.due_notified_at.is_some() {
                continue;
            }
            let title = if now - due_at > REMINDER_GRACE_SECS {
                "Shion (overdue task)"
            } else {
                "Shion task due"
            };
            let body = if task.waiting_on.is_empty() {
                task.title.clone()
            } else {
                format!("{} (waiting on: {})", task.title, task.waiting_on)
            };
            // Notify first, then mark — prefer duplicate over silent loss
            // (same ordering as recurring reminders).
            self.notifier.notify(title, &body).await.ok();
            let mut notified = task.clone();
            notified.due_notified_at = Some(now);
            if let Err(e) = self.tasks.update(&notified).await {
                warn!(error = %e, id = %task.id, "failed to mark task notified");
            } else {
                summary.tasks_notified += 1;
            }
        }
        Ok(summary)
    }
}

/// Window for "recently learned" memories surfaced in the briefing.
const BRIEFING_MEMORY_WINDOW_SECS: i64 = 7 * 86_400;
/// Cap each briefing list so a large backlog can't produce an unreadable wall;
/// truncation is disclosed in-line ("+N more") rather than hidden.
const BRIEFING_SECTION_CAP: usize = 10;

/// Daily proactive briefing: read the open tasks and recently-learned memories,
/// let the aux LLM compose a short digest, and deliver it through the notifier
/// (a channel `home_chat`, else macOS). Opt-in via `briefing_schedule`; the
/// roadmap's §4 "morning briefing". Reuses the existing scheduler and notifier —
/// no new delivery mechanism.
pub struct BriefingSweep {
    pub tasks: Arc<dyn TaskRepository>,
    pub memories: Arc<dyn MemoryRepository>,
    pub llm: Arc<dyn LlmClient>,
    pub notifier: Arc<dyn Notifier>,
}

#[async_trait]
impl Maintenance for BriefingSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let mut summary = MaintenanceSummary::default();
        let tasks = self.tasks.list_open().await?;
        let memories = self.memories.list().await?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        // Nothing on the plate → stay silent rather than ping an empty note.
        let Some(prompt) = briefing_prompt(&tasks, &memories, now) else {
            return Ok(summary);
        };

        // Same synthetic-session trick the reviewer uses: one user turn, no tools.
        let session = Session {
            id: "briefing".to_string(),
            messages: vec![Message::user(prompt)],
            created_at: now,
        };
        let text = self.llm.complete(&session).await?;
        let text = text.trim();
        if text.is_empty() {
            return Ok(summary);
        }
        self.notifier
            .notify("Shion daily briefing", text)
            .await
            .ok();
        summary.briefings_sent = 1;
        Ok(summary)
    }
}

/// Render a unix timestamp in local time at minute precision for the digest.
fn briefing_local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| unix.to_string())
}

/// Build the briefing prompt from open tasks and recent memories. Returns
/// `None` when there is nothing worth a proactive ping (no open tasks and no
/// recent memories), so the sweep can skip delivery. Pure and clock-injected
/// (`now`) so the digest is unit-testable without a real LLM or notifier.
fn briefing_prompt(tasks: &[Task], memories: &[Memory], now: i64) -> Option<String> {
    let recent: Vec<&Memory> = memories
        .iter()
        .filter(|m| now - m.created_at <= BRIEFING_MEMORY_WINDOW_SECS)
        .collect();
    if tasks.is_empty() && recent.is_empty() {
        return None;
    }

    // A `with_overflow` helper: list up to the cap, then disclose how many were
    // dropped instead of silently truncating.
    let render_lines = |out: &mut String, lines: Vec<String>| {
        for line in lines.iter().take(BRIEFING_SECTION_CAP) {
            out.push_str(line);
            out.push('\n');
        }
        if lines.len() > BRIEFING_SECTION_CAP {
            out.push_str(&format!(
                "- (+{} more)\n",
                lines.len() - BRIEFING_SECTION_CAP
            ));
        }
    };

    let mut digest = String::new();
    if !tasks.is_empty() {
        // Oldest-first within the listing; the model is told to surface the
        // urgent ones, so we keep the raw data ordered by due date then age.
        let mut ordered: Vec<&Task> = tasks.iter().collect();
        ordered.sort_by_key(|t| (t.due_at.unwrap_or(i64::MAX), t.created_at));
        let lines: Vec<String> = ordered
            .iter()
            .map(|t| {
                let mut line = format!("- [{}] {}", t.status.as_str(), t.title);
                if let Some(due) = t.due_at {
                    let tag = if due < now { "OVERDUE" } else { "due" };
                    line.push_str(&format!(" ({tag} {})", briefing_local_time(due)));
                }
                if !t.waiting_on.is_empty() {
                    line.push_str(&format!(" (waiting on: {})", t.waiting_on));
                }
                line
            })
            .collect();
        digest.push_str(&format!("Open tasks ({}):\n", tasks.len()));
        render_lines(&mut digest, lines);
    }
    if !recent.is_empty() {
        let lines: Vec<String> = recent
            .iter()
            .map(|m| format!("- [{}] {}", m.kind.as_str(), m.content))
            .collect();
        digest.push_str(&format!("\nRecently learned ({}):\n", recent.len()));
        render_lines(&mut digest, lines);
    }

    Some(format!(
        "Compose a short, friendly daily briefing for the user from the items below. \
         Lead with anything overdue or due today, then commitments waiting on others, \
         then a brief note of what's newly learned. Be concise and warm; never invent \
         anything not listed, and if nothing is urgent, say so plainly. Reply with the \
         briefing text only — no preamble.\n\n{}",
        digest.trim_end()
    ))
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
                    tasks_captured = summary.tasks_captured,
                    briefings = summary.briefings_sent,
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
    use crate::domain::task::{Task, TaskStatus};
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

    // ── TaskSweep ─────────────────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeTasks {
        tasks: Mutex<Vec<Task>>,
    }

    #[async_trait]
    impl crate::domain::task::TaskRepository for FakeTasks {
        async fn save(&self, task: &Task) -> anyhow::Result<()> {
            self.tasks.lock().unwrap().push(task.clone());
            Ok(())
        }
        async fn find(&self, id: &str) -> anyhow::Result<Option<Task>> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == id)
                .cloned())
        }
        async fn list_open(&self) -> anyhow::Result<Vec<Task>> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status.is_open())
                .cloned()
                .collect())
        }
        async fn update(&self, task: &Task) -> anyhow::Result<()> {
            let mut tasks = self.tasks.lock().unwrap();
            let slot = tasks
                .iter_mut()
                .find(|t| t.id == task.id)
                .ok_or_else(|| anyhow::anyhow!("not found"))?;
            *slot = task.clone();
            Ok(())
        }
        async fn find_by_source_message_id(
            &self,
            source: &str,
            source_message_id: &str,
        ) -> anyhow::Result<Option<Task>> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.source == source && t.source_message_id == source_message_id)
                .cloned())
        }
    }

    fn task_sweep_with(tasks: Vec<Task>) -> (TaskSweep, Arc<FakeTasks>, Arc<FakeNotifier>) {
        let repo = Arc::new(FakeTasks {
            tasks: Mutex::new(tasks),
        });
        let notifier = Arc::new(FakeNotifier::default());
        let sweep = TaskSweep {
            tasks: repo.clone() as Arc<dyn crate::domain::task::TaskRepository>,
            notifier: notifier.clone() as Arc<dyn Notifier>,
        };
        (sweep, repo, notifier)
    }

    fn due_task(offset_secs: i64) -> Task {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut task = Task::new("send report".to_string());
        task.status = TaskStatus::Todo;
        task.due_at = Some(now + offset_secs);
        task
    }

    #[tokio::test]
    async fn task_sweep_notifies_due_task_once() {
        let (sweep, repo, notifier) = task_sweep_with(vec![due_task(-30)]);

        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.tasks_notified, 1);
        assert_eq!(notifier.calls.lock().unwrap().len(), 1);
        assert_eq!(notifier.calls.lock().unwrap()[0].0, "Shion task due");
        // Task stays open; only the guard flips.
        let tasks = repo.tasks.lock().unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Todo);
        assert!(tasks[0].due_notified_at.is_some());
        drop(tasks);

        // Second sweep: nothing new.
        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.tasks_notified, 0);
        assert_eq!(notifier.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn task_sweep_skips_future_and_undated_tasks() {
        let mut undated = Task::new("someday".to_string());
        undated.status = TaskStatus::Todo;
        let (sweep, _repo, notifier) = task_sweep_with(vec![due_task(3600), undated]);

        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.tasks_notified, 0);
        assert!(notifier.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn task_sweep_marks_overdue_past_grace() {
        let (sweep, _repo, notifier) = task_sweep_with(vec![due_task(-(REMINDER_GRACE_SECS + 60))]);

        sweep.run().await.unwrap();
        let calls = notifier.calls.lock().unwrap();
        assert_eq!(calls[0].0, "Shion (overdue task)");
    }

    #[tokio::test]
    async fn task_sweep_includes_waiting_on_in_body() {
        let mut task = due_task(-30);
        task.waiting_on = "alice".to_string();
        let (sweep, _repo, notifier) = task_sweep_with(vec![task]);

        sweep.run().await.unwrap();
        let calls = notifier.calls.lock().unwrap();
        assert!(calls[0].1.contains("waiting on: alice"), "{}", calls[0].1);
    }

    // ── BriefingSweep ─────────────────────────────────────────────────────────

    use crate::domain::memory::{Memory, MemoryKind, MemoryRepository};

    struct FixedLlm(String);

    #[async_trait]
    impl LlmClient for FixedLlm {
        async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeMemories(Mutex<Vec<Memory>>);

    #[async_trait]
    impl MemoryRepository for FakeMemories {
        async fn list(&self) -> anyhow::Result<Vec<Memory>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(memory.clone());
            Ok(())
        }
    }

    fn briefing_with(
        tasks: Vec<Task>,
        memories: Vec<Memory>,
        reply: &str,
    ) -> (BriefingSweep, Arc<FakeNotifier>) {
        let notifier = Arc::new(FakeNotifier::default());
        let sweep = BriefingSweep {
            tasks: Arc::new(FakeTasks {
                tasks: Mutex::new(tasks),
            }),
            memories: Arc::new(FakeMemories(Mutex::new(memories))),
            llm: Arc::new(FixedLlm(reply.to_string())),
            notifier: notifier.clone(),
        };
        (sweep, notifier)
    }

    #[test]
    fn briefing_prompt_is_none_when_nothing_to_say() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        assert!(briefing_prompt(&[], &[], now).is_none());
    }

    #[test]
    fn briefing_prompt_skips_stale_memories() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut old = Memory::new(MemoryKind::Profile, "ancient");
        old.created_at = now - BRIEFING_MEMORY_WINDOW_SECS - 1;
        // Only a stale memory, no tasks → nothing recent → no briefing.
        assert!(briefing_prompt(&[], std::slice::from_ref(&old), now).is_none());
    }

    #[test]
    fn briefing_prompt_marks_overdue_tasks() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut task = Task::new("file taxes".to_string());
        task.status = TaskStatus::Todo;
        task.due_at = Some(now - 3600);
        let prompt = briefing_prompt(std::slice::from_ref(&task), &[], now).unwrap();
        assert!(prompt.contains("file taxes"));
        assert!(prompt.contains("OVERDUE"), "{prompt}");
    }

    #[tokio::test]
    async fn briefing_sweep_sends_when_tasks_present() {
        let mut task = Task::new("ship release".to_string());
        task.status = TaskStatus::Todo;
        let (sweep, notifier) = briefing_with(vec![task], vec![], "Good morning! One task today.");

        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.briefings_sent, 1);
        let calls = notifier.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Shion daily briefing");
        assert!(calls[0].1.contains("Good morning"));
    }

    #[tokio::test]
    async fn briefing_sweep_stays_silent_when_nothing_open() {
        let (sweep, notifier) = briefing_with(vec![], vec![], "should never be sent");

        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.briefings_sent, 0);
        assert!(notifier.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn briefing_sweep_silent_on_empty_llm_reply() {
        let mut task = Task::new("review PR".to_string());
        task.status = TaskStatus::Todo;
        let (sweep, notifier) = briefing_with(vec![task], vec![], "   ");

        let summary = sweep.run().await.unwrap();
        assert_eq!(summary.briefings_sent, 0);
        assert!(notifier.calls.lock().unwrap().is_empty());
    }
}
