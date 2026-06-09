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

use crate::domain::{repository::SessionRepository, reviewer::Reviewer};

/// Stop the daemon once this many maintenance cycles fail back-to-back.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// A parsed cron schedule. Wraps `croner` so the supervisor never touches the
/// cron crate directly and the "when does it next fire" math stays testable.
pub struct Schedule {
    cron: Cron,
    expr: String,
}

impl Schedule {
    /// Parse a 5-field Unix cron expression (e.g. `0 * * * *` for hourly).
    pub fn parse(expr: &str) -> anyhow::Result<Self> {
        let cron = expr
            .parse::<Cron>()
            .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
        Ok(Self {
            cron,
            expr: expr.to_string(),
        })
    }

    pub fn expr(&self) -> &str {
        &self.expr
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
