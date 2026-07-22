//! Execution-context value types shared by the tool loop and the tools.
//!
//! These are pure values over domain traits (`ReplySink`, `RunRepository`,
//! `Approver`) — no I/O — so they live in `domain`. The tool-execution service
//! re-exports [`SessionContext`] and [`RunContext`] for path stability, adds the
//! per-turn [`ToolTurnContext`] bundle, and owns the ambient-session task-local
//! (a compatibility seam for the approvers). [`ToolContext`] is the **explicit**
//! per-call context handed to `Tool::call` (roadmap: tool trait v2) so a tool
//! reads its session and requests approval through `ctx`, not an ambient scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::domain::approval::{ApprovalRequest, Approver};
use crate::domain::events::ToolEventSink;
use crate::domain::gateway::ReplySink;
use crate::domain::run::RunRepository;

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
    /// operator (see [`SessionContext::trusted`]). The api channel gates this to
    /// loopback callers, so a publicly-bound api never reaches it. Leave `false`
    /// everywhere else.
    pub auto_approve: bool,
    /// Optional live event sink. When set (a streaming client is watching this
    /// turn), the tool executor emits [`TurnEvent`](crate::domain::events::TurnEvent)s
    /// as each tool starts and finishes. `None` for every ordinary turn — no
    /// watcher, no emission. Attached via [`with_event_sink`](Self::with_event_sink).
    pub event_sink: Option<Arc<dyn ToolEventSink>>,
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
            event_sink: None,
        }
    }

    /// A trusted context: like [`detached`](SessionContext::detached) (no
    /// mid-turn prompting), but approval-needing tool calls are auto-approved.
    /// Used for a `komo chat` turn routed over the gateway's **loopback** api
    /// channel — the CLI user is the host operator, so there is no separate
    /// human to prompt. The api channel only builds this for loopback callers
    /// carrying the trusted header; a publicly-bound api keeps using `detached`.
    pub fn trusted(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: false,
            auto_approve: true,
            event_sink: None,
        }
    }

    /// An interactive HTTP context: `interactive` so approval / clarify prompts
    /// suspend the turn (rather than auto-denying), but the sink is a no-op — the
    /// prompt is surfaced out-of-band. Used for the GUI's turns over the
    /// gateway's **loopback** api channel (`X-Komo-Interactive`): it polls
    /// `GET /api/interactions/{session}` for the pending prompt and resolves it
    /// with a `POST`, so no reply sink is read. The api channel builds this only
    /// for loopback callers; a publicly-bound api keeps using `detached`.
    pub fn interactive_http(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: true,
            auto_approve: false,
            event_sink: None,
        }
    }

    /// Attach a live [`ToolEventSink`] so the tool executor emits `TurnEvent`s
    /// for this turn (the streaming api path uses this to feed the SSE stream).
    pub fn with_event_sink(mut self, sink: Arc<dyn ToolEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
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

/// The run-ledger handle for one turn (`domain/run.rs`, roadmap §7): created by
/// `AgentRuntime::run_turn` and passed **explicitly** down to the executor, so
/// ledgering and the per-turn call budget never depend on a caller having
/// established an ambient scope. Absent (`None` in [`ToolContext`]) for callers
/// without a ledger, so their tool use never pollutes it.
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
    pub fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// How many tool steps have been claimed so far (the post-turn count).
    pub fn steps_count(&self) -> i64 {
        self.seq.load(Ordering::Relaxed)
    }
}

/// The **explicit** per-call context handed to [`Tool::call`](crate::domain::tool::Tool::call).
///
/// A tool reads its session (`ctx.session`) and requests approval
/// (`ctx.approve(..)`) through this value rather than an ambient task-local or a
/// constructor-injected `Arc<dyn Approver>`. Owned (all fields cheap-clone) so
/// the executor can move it into the spawned tool task.
pub struct ToolContext {
    pub session: SessionContext,
    pub run: Option<RunContext>,
    approver: Arc<dyn Approver>,
}

impl ToolContext {
    pub fn new(
        session: SessionContext,
        run: Option<RunContext>,
        approver: Arc<dyn Approver>,
    ) -> Self {
        Self {
            session,
            run,
            approver,
        }
    }

    /// Ask the wired approver to allow `request`. The executor installs the
    /// ambient session scope around the tool task, so the concrete approver
    /// (chat/CLI) still resolves the prompt against the right conversation.
    pub async fn approve(&self, request: &ApprovalRequest) -> bool {
        self.approver.approve(request).await
    }
}
