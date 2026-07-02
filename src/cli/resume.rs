//! `shion run resume` — re-dispatch an interrupted turn from the run ledger.
//!
//! The ledger is an audit record, not a checkpoint, so resume runs one *fresh*
//! turn in the interrupted run's session, primed with the original input and a
//! digest of the steps that had completed (`domain::run::resume_prompt`). The
//! model judges which side effects already took hold; new side effects go
//! through approval as usual.

use std::sync::Arc;

use crate::{
    cli::{approver::CliApprover, wiring},
    domain::{
        approval::Approver,
        run::{Run, RunRepository, resume_prompt},
    },
    infra::persistence::{db::Db, kanban::KanbanDb},
};

/// How many recent runs to scan when picking the latest recoverable one.
const RESUME_SCAN_LIMIT: usize = 100;

/// Resume an interrupted run in its original session. `id = None` picks the
/// most recent recoverable run.
pub async fn run(db_url: &str, kanban_url: &str, id: Option<String>) -> anyhow::Result<()> {
    crate::cli::gateway_client::refuse_if_gateway_running("run resume").await?;

    let db = Arc::new(Db::connect(db_url).await?);
    let target = resolve_target(&*db, id).await?;
    let steps = RunRepository::steps(&*db, &target.id).await?;
    let input = resume_prompt(&target, &steps);
    println!(
        "Resuming {} (session {}, {} completed step(s))…\n",
        target.id,
        target.session_id,
        steps.len()
    );

    // Same construction as the chat REPL: interactive approval at the TTY.
    let kanban = Arc::new(KanbanDb::connect(kanban_url).await?);
    let approver: Arc<dyn Approver> = Arc::new(CliApprover::new());
    let runtime = wiring::build(db.clone(), kanban, approver).await?.runtime;

    let reply = runtime.handle_input(&target.session_id, input).await?;
    // Clear the flag only after a turn was actually dispatched; best-effort,
    // like every other ledger write.
    if let Err(error) = RunRepository::mark_resumed(&*db, &target.id).await {
        eprintln!("warning: failed to clear the recoverable flag: {error:#}");
    }
    println!("Agent: {reply}");
    Ok(())
}

/// Resolve the run to resume: an explicit id must exist and be recoverable; no
/// id means the most recent recoverable run.
async fn resolve_target(runs: &dyn RunRepository, id: Option<String>) -> anyhow::Result<Run> {
    match id {
        Some(id) => {
            let Some(run) = runs.get(&id).await? else {
                anyhow::bail!("no run with id `{id}`");
            };
            if !run.recoverable {
                anyhow::bail!(
                    "run `{id}` is not recoverable (status: {} — it finished normally, \
                     failed without interruption, or was already resumed)",
                    run.status.as_str()
                );
            }
            Ok(run)
        }
        None => runs
            .list(RESUME_SCAN_LIMIT)
            .await?
            .into_iter()
            .find(|r| r.recoverable)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no recoverable runs — nothing was interrupted, or it was already resumed"
                )
            }),
    }
}
