//! Per-turn execution context: which session a tool call belongs to, and the
//! run-ledger handle recording it.
//!
//! [`ToolTurnContext`] is the **explicit** contract the runtime hands the
//! executor. The session half also rides a task-local — but only as an
//! internal compatibility seam: rig's `ToolDyn::call` signature and each
//! `Tool::execute(String)` can't take a context parameter, so session-scoped
//! tools (`todo`, `memory`) and the approvers read [`current_session`] while
//! the executor installs the explicit context around each spawned tool task.
//! The run context is purely explicit — no ambient state decides whether a
//! turn is ledgered.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::domain::gateway::ReplySink;
use crate::domain::run::RunRepository;

/// Everything the executor needs to know about the turn a round of tool calls
/// belongs to. Built once per turn by `AgentRuntime::run_agent_loop`.
#[derive(Clone)]
pub struct ToolTurnContext {
    pub session: SessionContext,
    /// `Some` when the turn is recorded in the run ledger (the main agent);
    /// `None` for callers without a ledger (rig's fallback path).
    pub run: Option<RunContext>,
}

/// The session a tool is executing within: which conversation it belongs to
/// and how to talk back to that conversation. Set by the gateway dispatcher
/// around a turn (`agent::interaction`) and read by a chat-channel approver
/// when a tool needs mid-execution approval.
#[derive(Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub sink: Arc<dyn ReplySink>,
    /// Whether a human can answer a mid-turn approval prompt on this channel.
    /// Chat channels set this `true`; non-interactive callers (the detached
    /// context, the HTTP API) set it `false` so a `Risk::Normal` /
    /// `Risk::Dangerous` request is denied immediately instead of waiting out
    /// the approval timeout against a sink no one is reading.
    pub interactive: bool,
    /// Whether approval-needing tool calls should be auto-approved without a
    /// prompt. Set only for a **trusted** turn — a `komo chat` routed over the
    /// gateway's loopback api channel, where the CLI user *is* the host
    /// operator (see `SessionContext::trusted`). The api channel gates this to
    /// loopback callers, so a publicly-bound api can never reach it. Leave
    /// `false` everywhere else.
    pub auto_approve: bool,
}

impl SessionContext {
    /// A context that knows the session but cannot talk back mid-turn (its sink
    /// is a no-op, and it is non-interactive). Used by any caller that has a
    /// session id but no channel to prompt on — enough for session-scoped tools
    /// like `todo`, while a mid-turn approval prompt is auto-denied.
    pub fn detached(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: false,
            auto_approve: false,
        }
    }

    /// A trusted context: like `detached` (no mid-turn prompting), but
    /// approval-needing tool calls are auto-approved. Used for a `komo chat`
    /// turn routed over the gateway's **loopback** api channel — the CLI user
    /// is the host operator, so there is no separate human to prompt. The api
    /// channel only builds this for loopback callers carrying the trusted
    /// header; a publicly-bound api keeps using `detached` (auto-deny).
    pub fn trusted(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: false,
            auto_approve: true,
        }
    }
}

/// A [`ReplySink`] that drops everything — see [`SessionContext::detached`].
struct NoopSink;

#[async_trait::async_trait]
impl ReplySink for NoopSink {
    async fn send(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
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

/// The run-ledger handle for one turn (`domain/run.rs`, roadmap §7): created by
/// `AgentRuntime::run_turn` and passed **explicitly** down to the executor, so
/// ledgering and the per-turn call budget never depend on a caller having
/// established an ambient scope. Absent (`None` in [`ToolTurnContext`]) for
/// callers without a ledger, so their tool use never pollutes it.
#[derive(Clone)]
pub struct RunContext {
    pub run_id: String,
    pub repo: Arc<dyn RunRepository>,
    /// Monotonic step counter, shared across clones so steps within a run get a
    /// stable order even when tool calls run concurrently.
    seq: Arc<AtomicI64>,
}

impl RunContext {
    pub fn new(run_id: String, repo: Arc<dyn RunRepository>) -> Self {
        Self {
            run_id,
            repo,
            seq: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Claim the next step's sequence number.
    pub(super) fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// How many tool steps have been claimed so far (the post-turn count).
    pub fn steps_count(&self) -> i64 {
        self.seq.load(Ordering::Relaxed)
    }
}
