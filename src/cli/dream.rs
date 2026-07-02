//! `shion dream` — operator view over the usage-driven memory "dreaming"
//! consolidation (the OpenClaw-borrowed back-loop).
//!
//! By default this is a **dry run**: it shows which candidate memories would be
//! promoted (recalled often enough) or archived (old and never recalled), with
//! the dreaming score that drove each verdict — like OpenClaw's `rem-harness` /
//! `promote-explain`. Pass `--apply` to actually run one consolidation cycle
//! (the same `DreamSweep` the gateway runs on `dream_schedule`).
//!
//! The dry-run routes through a running gateway (which holds the db lock) when
//! one is up; `--apply` mutates the db, so it requires the gateway stopped.

use crate::agent::daemon::DreamSweep;
use crate::cli::gateway_client::{GatewayClient, refuse_if_gateway_running};
use crate::domain::memory::{DreamVerdict, MemoryRepository, dream_score, dream_verdict};
use crate::infra::memory::memory_db::MemoryDb;
use crate::infra::messaging::api::DreamItem;
use std::sync::Arc;

/// Run a dreaming cycle, or preview one. `apply = false` mutates nothing.
pub async fn run(url: &str, apply: bool) -> anyhow::Result<()> {
    // Applying mutates the db — refuse while the gateway owns the lock.
    if apply {
        refuse_if_gateway_running("dream --apply").await?;
    }
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    // The preview can route to a running gateway; the apply path is only reached
    // with the gateway down (guarded above), so it always classifies locally.
    let (promote, archive) = if !apply {
        match GatewayClient::try_connect().await {
            Some(gw) => gw.dream_preview().await?,
            None => classify_local(url, now).await?,
        }
    } else {
        classify_local(url, now).await?
    };

    if promote.is_empty() && archive.is_empty() {
        println!("Nothing to dream about — no candidate meets the promote or archive bar.");
        return Ok(());
    }

    report_bucket("promote → active (well-recalled candidates)", &promote);
    report_bucket("archive (old, never recalled)", &archive);

    if !apply {
        println!("\n(dry run — pass --apply to execute this cycle)");
        return Ok(());
    }

    let db = Arc::new(MemoryDb::connect(url).await?);
    let summary = DreamSweep { memories: db }.apply().await?;
    println!(
        "\nApplied: promoted {}, archived {}.",
        summary.memories_promoted, summary.memories_archived
    );
    Ok(())
}

/// Classify the whole library into (promote, archive) candidate lists, strongest
/// promote case first — the same verdict the gateway's `/api/dream` computes.
async fn classify_local(url: &str, now: i64) -> anyhow::Result<(Vec<DreamItem>, Vec<DreamItem>)> {
    let db = MemoryDb::connect(url).await?;
    let mut promote: Vec<DreamItem> = Vec::new();
    let mut archive: Vec<DreamItem> = Vec::new();
    for m in &db.list().await? {
        let item = DreamItem {
            id: m.id.clone(),
            recall_count: m.recall_count,
            unique_queries: m.recall_query_hashes.len(),
            score: dream_score(m, now),
            content: m.content.clone(),
        };
        match dream_verdict(m, now) {
            DreamVerdict::Promote => promote.push(item),
            DreamVerdict::Archive => archive.push(item),
            DreamVerdict::Keep => {}
        }
    }
    promote.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((promote, archive))
}

fn report_bucket(label: &str, items: &[DreamItem]) {
    if items.is_empty() {
        return;
    }
    println!("\n{label}: {}", items.len());
    for m in items.iter().take(20) {
        println!(
            "  {}  [recalls={} queries={} score={:.2}]  {}",
            m.id, m.recall_count, m.unique_queries, m.score, m.content
        );
    }
    if items.len() > 20 {
        println!("  … and {} more", items.len() - 20);
    }
}
