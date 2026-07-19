//! Pairing operator commands (`komo pair list/approve/revoke`).
//!
//! Approval deliberately lives here and nowhere else: typing
//! `komo pair approve <code>` in a shell on this machine is the proof of
//! ownership that lets a new chat-platform sender talk to the agent. The
//! gateway reads the same SQLite db, so approval takes effect on the
//! sender's next message — no restart.

use crate::services::operator_control::{
    OperatorCommand, OperatorCommandResult, OperatorControl, OperatorQuery, OperatorQueryResult,
    PairApproveOutcome,
};

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

/// List all pairings: pending requests and approved senders. The code itself is
/// stored only as a salted hash — get it from the sender and run
/// `komo pair approve <code>` (or `/pair approve` in chat while the gateway runs).
pub async fn list(control: &OperatorControl) -> anyhow::Result<()> {
    let OperatorQueryResult::Pairings(pairings) = control.query(OperatorQuery::Pairings).await?
    else {
        unreachable!("Pairings query answers with Pairings");
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

/// Approve the pending request bearing `code`. Routes through a running gateway
/// (which holds the db lock) when one is up, else opens the db directly. (The
/// `/pair approve` chat command is the other in-gateway path.)
pub async fn approve(control: &OperatorControl, code: &str) -> anyhow::Result<()> {
    let code = code.trim().to_uppercase();
    let OperatorCommandResult::PairApproved(outcome) = control
        .command(OperatorCommand::PairApprove { code: code.clone() })
        .await?
    else {
        unreachable!("PairApprove answers with PairApproved");
    };
    match outcome {
        PairApproveOutcome::Approved { id } => {
            println!("Paired {id} — they can chat now.");
            Ok(())
        }
        PairApproveOutcome::NotFound => anyhow::bail!(
            "no approvable pairing with code {code} — unknown or expired (see `komo pair list`)"
        ),
        PairApproveOutcome::Locked { retry_after_secs } => anyhow::bail!(
            "too many failed attempts — approve is locked for {} more minutes",
            (retry_after_secs + 59) / 60
        ),
    }
}

/// Remove a pairing (`{platform}:{sender_id}`, as printed by `pair list`).
pub async fn revoke(control: &OperatorControl, id: &str) -> anyhow::Result<()> {
    let OperatorCommandResult::PairRevoked { revoked } = control
        .command(OperatorCommand::PairRevoke { id: id.to_string() })
        .await?
    else {
        unreachable!("PairRevoke answers with PairRevoked");
    };
    if revoked {
        println!("Revoked {id}.");
    } else {
        println!("No pairing {id} (see `komo pair list`).");
    }
    Ok(())
}
