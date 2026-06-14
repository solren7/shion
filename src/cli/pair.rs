//! Pairing operator commands (`shion pair list/approve/revoke`).
//!
//! Approval deliberately lives here and nowhere else: typing
//! `shion pair approve <code>` in a shell on this machine is the proof of
//! ownership that lets a new chat-platform sender talk to the agent. The
//! gateway reads the same SQLite db, so approval takes effect on the
//! sender's next message — no restart.

use crate::{
    domain::pairing::{ApproveOutcome, PairingRepository, PairingStatus},
    infra::db::Db,
};

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

/// List all pairings: pending requests (with their codes) and approved senders.
pub async fn list(db_url: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    let pairings = PairingRepository::list(&db).await?;

    if pairings.is_empty() {
        println!("No pairings. Unknown senders get a code on first contact.");
        return Ok(());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    for p in pairings {
        match p.status {
            PairingStatus::Pending => {
                let state = if p.is_expired(now) {
                    "expired"
                } else {
                    "pending"
                };
                // The code is stored only as a salted hash; the sender has it —
                // get it from them and approve with `shion pair approve <code>`.
                println!(
                    "{}  [{}]  requested {}",
                    p.id,
                    state,
                    local_time(p.created_at)
                );
            }
            PairingStatus::Approved => {
                println!("{}  [approved]  since {}", p.id, local_time(p.created_at));
            }
        }
    }
    Ok(())
}

/// Approve the pending request bearing `code`.
pub async fn approve(db_url: &str, code: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    let code = code.trim().to_uppercase();
    match PairingRepository::approve_code(&db, &code).await? {
        ApproveOutcome::Approved(request) => {
            println!("Paired {} — they can chat now.", request.id);
            Ok(())
        }
        ApproveOutcome::NotFound => anyhow::bail!(
            "no approvable pairing with code {code} — unknown or expired (see `shion pair list`)"
        ),
        ApproveOutcome::Locked { retry_after_secs } => anyhow::bail!(
            "too many failed attempts — approve is locked for {} more minutes",
            (retry_after_secs + 59) / 60
        ),
    }
}

/// Remove a pairing (`{platform}:{sender_id}`, as printed by `pair list`).
pub async fn revoke(db_url: &str, id: &str) -> anyhow::Result<()> {
    let db = Db::connect(db_url).await?;
    if PairingRepository::revoke(&db, id).await? {
        println!("Revoked {id}.");
    } else {
        println!("No pairing {id} (see `shion pair list`).");
    }
    Ok(())
}
