//! `shion run resume` — re-dispatch an interrupted turn from the run ledger.
//!
//! The ledger is an audit record, not a checkpoint, so resume runs one *fresh*
//! turn in the interrupted run's session, primed with the original input and a
//! digest of the steps that had completed (`domain::run::resume_prompt`). The
//! model judges which side effects already took hold; new side effects go
//! through approval as usual.
//!
//! Eligibility, priming, and the at-most-once `recoverable` clear live in
//! [`OperatorControl::resume_run`]. Only the local turn itself is supplied
//! here: with no gateway the run executes in-process with interactive approval
//! at the TTY, built on the very stores the operator backend already opened.

use std::sync::Arc;

use crate::{
    cli::{approver::CliApprover, wiring},
    config::ConfigSnapshot,
    domain::approval::Approver,
    services::operator_control::OperatorControl,
};

/// Resume an interrupted run in its original session. `id = None` picks the
/// most recent recoverable run.
pub async fn run(
    config: &ConfigSnapshot,
    control: &OperatorControl,
    id: Option<String>,
) -> anyhow::Result<()> {
    let outcome = control
        .resume_run(id, |db, kanban, session_id, input| async move {
            // Same construction as the chat TUI's local mode: interactive
            // approval at the TTY.
            let approver: Arc<dyn Approver> = Arc::new(CliApprover::new());
            let runtime = wiring::build(config, db, kanban, approver).await?.runtime;
            runtime.handle_input(&session_id, input).await
        })
        .await?;
    println!(
        "Resumed {} (session {}, {} completed step(s) handed to the model).\n",
        outcome.run_id, outcome.session_id, outcome.steps
    );
    println!("Agent: {}", outcome.reply);
    Ok(())
}
