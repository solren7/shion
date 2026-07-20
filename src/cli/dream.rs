//! `komo dream` — operator view over the usage-driven memory "dreaming"
//! consolidation (the OpenClaw-borrowed back-loop).
//!
//! By default this is a **dry run**: it shows which candidate memories would be
//! promoted (recalled often enough) or archived (old and gone cold), with
//! the dreaming score that drove each verdict — like OpenClaw's `rem-harness` /
//! `promote-explain`. Pass `--apply` to actually run one consolidation cycle
//! (the same `DreamSweep` the gateway runs on `dream_schedule`).
//!
//! Both preview and apply run through the operator surface — whichever
//! transport answers (a running gateway, or the store directly) is not this
//! module's business.

use crate::services::operator_control::{
    DreamItem, OperatorCommand, OperatorCommandResult, OperatorControl, OperatorQuery,
    OperatorQueryResult,
};

/// Run a dreaming cycle, or preview one. `apply = false` mutates nothing.
pub async fn run(control: &OperatorControl, apply: bool) -> anyhow::Result<()> {
    let OperatorQueryResult::DreamPreview(report) =
        control.query(OperatorQuery::DreamPreview).await?
    else {
        unreachable!("DreamPreview query answers with DreamPreview");
    };

    if report.is_empty() {
        println!("Nothing to dream about — no candidate meets the promote or archive bar.");
        return Ok(());
    }

    report_bucket(
        "promote → active (well-recalled candidates)",
        &report.promote,
    );
    report_bucket("archive (old, gone cold)", &report.archive);

    if !apply {
        println!("\n(dry run — pass --apply to execute this cycle)");
        return Ok(());
    }

    let OperatorCommandResult::DreamApplied { promoted, archived } =
        control.command(OperatorCommand::DreamApply).await?
    else {
        unreachable!("DreamApply answers with DreamApplied");
    };
    println!("\nApplied: promoted {promoted}, archived {archived}.");
    Ok(())
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
