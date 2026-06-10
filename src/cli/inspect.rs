//! Operator subcommands (`shion cron list`, `shion session list/clean`).
//!
//! These query the database directly and print to stdout — no LLM, no agent
//! runtime. They are the operator's view into what the gateway will act on.

use crate::{
    domain::{reminder::ReminderRepository, repository::SessionRepository},
    infra::db::Db,
};

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

/// List pending reminders with their schedules (recurring ones show the cron
/// expression, one-shots are marked as such).
pub async fn cron_list(db_url: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    let mut pending = ReminderRepository::list_pending(&db).await?;
    pending.sort_by_key(|r| r.run_at);

    if pending.is_empty() {
        println!("No pending reminders.");
        return Ok(());
    }
    for r in pending {
        if r.is_recurring() {
            println!(
                "{}  [{}]  next {}  {}",
                r.id,
                r.schedule,
                local_time(r.run_at),
                r.message
            );
        } else {
            println!(
                "{}  [one-shot]  due {}  {}",
                r.id,
                local_time(r.run_at),
                r.message
            );
        }
    }
    Ok(())
}

/// List stored sessions with creation time and message counts.
pub async fn session_list(db_url: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    let sessions = SessionRepository::list(&db).await?;

    if sessions.is_empty() {
        println!("No sessions.");
        return Ok(());
    }
    for s in sessions {
        println!(
            "{}  created {}  {} messages ({} user turns)",
            s.id,
            local_time(s.created_at),
            s.messages.len(),
            s.user_turns()
        );
    }
    Ok(())
}

/// Delete every session with zero messages. An operator action — run it by
/// hand or from an external scheduler (launchd/cron), e.g. daily at 4am.
pub async fn session_clean(db_url: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    let removed = SessionRepository::delete_empty_sessions(&db).await?;
    println!("Removed {removed} empty session(s).");
    Ok(())
}
