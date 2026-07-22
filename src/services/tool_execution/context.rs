//! Per-turn execution context glue.
//!
//! The context **value types** ([`SessionContext`], [`RunContext`],
//! [`ToolContext`]) now live in `domain::context` — they are pure values over
//! domain traits. This module re-exports them for path stability and adds two
//! service-layer concerns: the per-turn [`ToolTurnContext`] bundle the runtime
//! hands the executor, and the ambient-session task-local.
//!
//! The `SESSION` task-local survives only as an internal compatibility seam:
//! the approvers (`ChatApprover`, `PolicyApprover`) resolve a prompt against the
//! current conversation without threading a context parameter through the
//! `Approver` trait, so the executor installs the turn's session around each
//! spawned tool task and they read [`current_session`]. Migrated tools read
//! `ctx.session` instead. The run context is purely explicit — no ambient state
//! decides whether a turn is ledgered.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub use crate::domain::context::{RunContext, SessionContext, ToolContext};

/// Everything the executor needs to know about the turn a round of tool calls
/// belongs to. Built once per turn by `AgentRuntime::run_agent_loop`.
#[derive(Clone)]
pub struct ToolTurnContext {
    pub session: SessionContext,
    /// `Some` when the turn is recorded in the run ledger (the main agent);
    /// `None` for callers without a ledger (rig's fallback path).
    pub run: Option<RunContext>,
    /// Cumulative tool-output budget for this turn. A round's tool calls share
    /// it via an `Arc`, so the total is tracked across the concurrent calls and
    /// across rounds — see [`TurnResultBudget`].
    pub budget: TurnResultBudget,
}

/// Per-turn cap on the *cumulative* bytes of tool output fed back to the model.
///
/// `max_tool_result_bytes` bounds one result; this bounds the whole turn, so a
/// long tool chain (dozens of rounds, each returning a capped result) can't
/// quietly accumulate past the context window and fail the turn only after its
/// side effects have already run. Once the running total crosses the cap, the
/// executor swaps each further result for a short note telling the model to stop
/// gathering and answer with what it has. Shared across a round's concurrent
/// calls via an `Arc<AtomicUsize>`; the counter is approximate under that
/// concurrency (a small overshoot is fine for a backstop).
#[derive(Clone)]
pub struct TurnResultBudget {
    consumed: Arc<AtomicUsize>,
    /// `0` disables the budget (unlimited).
    cap: usize,
}

impl TurnResultBudget {
    /// A budget capping cumulative tool output at `cap` bytes (`0` = unlimited).
    pub fn new(cap: usize) -> Self {
        Self {
            consumed: Arc::new(AtomicUsize::new(0)),
            cap,
        }
    }

    /// A disabled budget — for execution paths with no per-turn accounting
    /// (rig's fallback, tests).
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// Account for a tool result about to be returned. `Ok(out)` when there is
    /// still budget (the result is admitted and its size recorded); `Err(note)`
    /// once the turn is over budget — the note replaces the result. Disabled
    /// (`cap == 0`) always admits.
    pub(super) fn admit(&self, out: String) -> Result<String, String> {
        if self.cap == 0 {
            return Ok(out);
        }
        let already = self.consumed.load(Ordering::Relaxed);
        if already >= self.cap {
            return Err(format!(
                "[tool result omitted: this turn has already returned ~{} KB of tool output, \
                 over the {} KB per-turn budget. Stop calling tools and answer the user with \
                 what you already have; start a new turn if you genuinely need more.]",
                already / 1024,
                self.cap / 1024
            ));
        }
        self.consumed.fetch_add(out.len(), Ordering::Relaxed);
        Ok(out)
    }
}

tokio::task_local! {
    pub(super) static SESSION: SessionContext;
}

/// Run `future` with `ctx` as the ambient session context. Called by the turn
/// entry points (the gateway dispatcher, the api channel, `handle_input`); the
/// executor re-installs the context around each spawned tool task.
pub async fn with_session<F: std::future::Future>(ctx: SessionContext, future: F) -> F::Output {
    SESSION.scope(ctx, future).await
}

/// The ambient session context, if the current task is running inside one.
/// `None` for aux sub-agents and maintenance sweeps.
pub fn current_session() -> Option<SessionContext> {
    SESSION.try_with(|c| c.clone()).ok()
}
