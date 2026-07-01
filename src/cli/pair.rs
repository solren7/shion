//! Pairing operator commands (`shion pair list/approve/revoke`).
//!
//! Approval deliberately lives here and nowhere else: typing
//! `shion pair approve <code>` in a shell on this machine is the proof of
//! ownership that lets a new chat-platform sender talk to the agent. The
//! gateway reads the same SQLite db, so approval takes effect on the
//! sender's next message — no restart.

use crate::{
    cli::gateway_client::{GatewayClient, refuse_if_gateway_running},
    domain::pairing::{ApproveOutcome, PairingRepository, PairingStatus},
    infra::{messaging::api::PairingView, persistence::db::Db},
};

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

/// List all pairings: pending requests and approved senders. The code itself is
/// stored only as a salted hash — get it from the sender and run
/// `shion pair approve <code>` (or `/pair approve` in chat while the gateway runs).
pub async fn list(db_url: &str) -> anyhow::Result<()> {
    let pairings: Vec<PairingView> = match GatewayClient::try_connect().await {
        Some(gw) => gw.pairings().await?,
        None => {
            let db = Db::connect(db_url).await?;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            PairingRepository::list(&db)
                .await?
                .into_iter()
                .map(|p| {
                    let status = match p.status {
                        PairingStatus::Approved => "approved",
                        PairingStatus::Pending if p.is_expired(now) => "expired",
                        PairingStatus::Pending => "pending",
                    };
                    PairingView {
                        id: p.id,
                        status: status.to_string(),
                        created_at: p.created_at,
                    }
                })
                .collect()
        }
    };

    if pairings.is_empty() {
        println!("No pairings. Unknown senders get a code on first contact.");
        return Ok(());
    }
    for p in pairings {
        if p.status == "approved" {
            println!("{}  [approved]  since {}", p.id, local_time(p.created_at));
        } else {
            println!(
                "{}  [{}]  requested {}",
                p.id,
                p.status,
                local_time(p.created_at)
            );
        }
    }
    Ok(())
}

/// Approve the pending request bearing `code`.
pub async fn approve(db_url: &str, code: &str) -> anyhow::Result<()> {
    refuse_if_gateway_running("pair approve").await?;
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
    refuse_if_gateway_running("pair revoke").await?;
    let db = Db::connect(db_url).await?;
    if PairingRepository::revoke(&db, id).await? {
        println!("Revoked {id}.");
    } else {
        println!("No pairing {id} (see `shion pair list`).");
    }
    Ok(())
}
