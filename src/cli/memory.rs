//! `shion memory` — operator-facing governance over the memory library.
//!
//! Unlike the in-chat `memory` tool (scoped to the current chat), the CLI is a
//! host-side operator view: it lists and searches across *all* scopes, so you
//! can triage candidates the reviewer captured and promote/pin the durable ones.

use crate::cli::gateway_client::{GatewayClient, refuse_if_gateway_running};
use crate::domain::memory::{Memory, MemoryConfidence, MemoryRepository, MemoryStatus};
use crate::infra::memory::memory_db::MemoryDb;

async fn store(url: &str) -> anyhow::Result<MemoryDb> {
    MemoryDb::connect(url).await
}

/// Load every memory — from a running gateway (which holds the db lock) if one
/// is up, else straight from the db. The CLI's list/search/report all filter
/// this set client-side, so one loader serves all three.
async fn load_all(url: &str) -> anyhow::Result<Vec<Memory>> {
    match GatewayClient::try_connect().await {
        Some(gw) => gw.memories().await,
        None => store(url).await?.list().await,
    }
}

/// List stored memories, optionally filtered by status.
pub async fn list(url: &str, status: Option<String>) -> anyhow::Result<()> {
    let filter = status
        .as_deref()
        .map(crate::domain::memory::parse_memory_status);
    let mut memories = load_all(url).await?;
    if let Some(status) = filter {
        memories.retain(|m| m.status == status);
    }
    if memories.is_empty() {
        println!("(no memories)");
        return Ok(());
    }
    // Group by status so candidates needing triage stand out.
    memories.sort_by(|a, b| {
        a.status
            .as_str()
            .cmp(b.status.as_str())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    for m in &memories {
        println!("{}", line(m));
    }
    Ok(())
}

/// Substring search across all scopes (operator view — no scope enforcement).
pub async fn search(url: &str, query: &str) -> anyhow::Result<()> {
    let needle = query.to_lowercase();
    let hits: Vec<Memory> = load_all(url)
        .await?
        .into_iter()
        .filter(|m| m.content.to_lowercase().contains(&needle))
        .collect();
    if hits.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    for m in &hits {
        println!("{}", line(m));
    }
    Ok(())
}

/// Promote a candidate to an active, confirmed memory.
pub async fn promote(url: &str, id: &str) -> anyhow::Result<()> {
    refuse_if_gateway_running("memory promote").await?;
    mutate(url, id, |m| {
        m.status = MemoryStatus::Active;
        m.confidence = MemoryConfidence::Confirmed;
    })
    .await?;
    println!("Promoted {id} to active.");
    Ok(())
}

/// Reject a candidate (won't surface in recall or injection).
pub async fn reject(url: &str, id: &str) -> anyhow::Result<()> {
    refuse_if_gateway_running("memory reject").await?;
    mutate(url, id, |m| m.status = MemoryStatus::Rejected).await?;
    println!("Rejected {id}.");
    Ok(())
}

/// Pin a memory into the L1 per-turn profile (the manual, explicit path —
/// automated extraction never pins). Raises confidence so it actually surfaces.
pub async fn pin(url: &str, id: &str) -> anyhow::Result<()> {
    refuse_if_gateway_running("memory pin").await?;
    mutate(url, id, |m| {
        m.pinned = true;
        m.status = MemoryStatus::Active;
        if m.confidence == MemoryConfidence::Extracted {
            m.confidence = MemoryConfidence::Confirmed;
        }
    })
    .await?;
    println!("Pinned {id} into the L1 profile.");
    Ok(())
}

/// Memory quality report (roadmap §9): bucket the whole library by status and
/// confidence, then surface the piles that need attention — candidates awaiting
/// triage, the pinned L1 set, low-confidence actives, long-unused actives, and
/// expired memories. Read-only; suggests `promote`/`reject`/`archive`/`pin`.
///
/// Recall counts (the dreaming usage signal) are shown per line; `shion dream`
/// previews which candidates that signal would promote or archive.
pub async fn report(url: &str) -> anyhow::Result<()> {
    let memories = load_all(url).await?;
    if memories.is_empty() {
        println!("(no memories)");
        return Ok(());
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let total = memories.len();

    // Counts by status, in lifecycle order.
    println!("total: {total}");
    println!("\nby status:");
    for status in [
        MemoryStatus::Candidate,
        MemoryStatus::Active,
        MemoryStatus::Archived,
        MemoryStatus::Rejected,
    ] {
        let n = memories.iter().filter(|m| m.status == status).count();
        if n > 0 {
            println!("  {:<10} {n}", status.as_str());
        }
    }

    println!("\nby confidence:");
    for confidence in [
        MemoryConfidence::UserWritten,
        MemoryConfidence::Confirmed,
        MemoryConfidence::Inferred,
        MemoryConfidence::Extracted,
    ] {
        let n = memories
            .iter()
            .filter(|m| m.confidence == confidence)
            .count();
        if n > 0 {
            println!("  {:<12} {n}", confidence.as_str());
        }
    }

    // The piles that need an operator's eye.
    let active = |m: &&Memory| m.status == MemoryStatus::Active;
    let candidates: Vec<_> = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Candidate)
        .collect();
    let pinned: Vec<_> = memories.iter().filter(|m| m.pinned).collect();
    let low_conf: Vec<_> = memories
        .iter()
        .filter(active)
        .filter(|m| {
            matches!(
                m.confidence,
                MemoryConfidence::Extracted | MemoryConfidence::Inferred
            )
        })
        .collect();
    // Active but never surfaced, or not surfaced in 90+ days — archival candidates.
    const STALE_SECS: i64 = 90 * 24 * 60 * 60;
    let mut unused: Vec<_> = memories
        .iter()
        .filter(active)
        .filter(|m| m.last_used_at.is_none_or(|t| now - t > STALE_SECS))
        .collect();
    let expired: Vec<_> = memories.iter().filter(|m| m.is_expired(now)).collect();

    report_bucket("candidates awaiting triage (→ promote/reject)", &candidates);
    report_bucket("pinned into L1 profile", &pinned);
    report_bucket("low-confidence active (extracted/inferred)", &low_conf);
    unused.sort_by_key(|m| m.last_used_at.unwrap_or(0));
    report_bucket("active, long unused (90d+ → consider archive)", &unused);
    report_bucket("expired (past expires_at)", &expired);
    Ok(())
}

/// Print a named bucket: a header with the count, then up to 10 sample lines.
fn report_bucket(label: &str, items: &[&Memory]) {
    if items.is_empty() {
        return;
    }
    println!("\n{label}: {}", items.len());
    for m in items.iter().take(10) {
        println!("  {}", line(m));
    }
    if items.len() > 10 {
        println!("  … and {} more", items.len() - 10);
    }
}

async fn mutate(url: &str, id: &str, apply: impl FnOnce(&mut Memory)) -> anyhow::Result<()> {
    let db = store(url).await?;
    let mut memory = db
        .get(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no memory with id `{id}`"))?;
    apply(&mut memory);
    memory.updated_at = time::OffsetDateTime::now_utc().unix_timestamp();
    db.save(&memory).await?;
    Ok(())
}

fn line(m: &Memory) -> String {
    let pin = if m.pinned { " 📌" } else { "" };
    let mut s = format!(
        "{}  [{}/{}/{}{}]  {}",
        m.id,
        m.status.as_str(),
        m.kind.as_str(),
        m.scope.type_str(),
        pin,
        m.content
    );
    if m.recall_count > 0 {
        s.push_str(&format!("  (recalls={})", m.recall_count));
    }
    if !m.source.is_empty() {
        s.push_str(&format!("  (from {})", m.source));
    }
    s
}
