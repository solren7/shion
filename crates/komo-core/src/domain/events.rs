//! Live turn events for clients that want to watch the agent work in real time
//! (the desktop GUI's tool-call activity feed).
//!
//! komo's rig tool loop has no token-level streaming, so this streams the
//! *tool-call process* — each tool starting and finishing — not the assistant
//! text token-by-token. Mirrors the [`ReplySink`](crate::domain::gateway::ReplySink)
//! pattern: a domain trait with no I/O and no tokio, so komo-core stays
//! dependency-light. The infra layer (the api channel) provides an mpsc-backed
//! impl; every non-streaming caller leaves the sink absent.

use serde::Serialize;

/// One event emitted during a turn. Serialized to JSON for the SSE stream
/// (`{"type":"tool_started", ...}`).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    /// A tool call is about to run.
    ToolStarted {
        /// The turn's ledger sequence for this call (`-1` when un-ledgered).
        seq: i64,
        name: String,
        /// Redacted arguments (secrets scrubbed, same as the ledger stores).
        args: String,
    },
    /// A tool call finished (after any transient-error retries collapse).
    ToolFinished {
        seq: i64,
        name: String,
        ok: bool,
        /// Short result preview (on success) or error message (on failure).
        summary: String,
    },
}

/// Sink for [`TurnEvent`]s. Sync + fire-and-forget so it can be called from deep
/// inside the tool executor (including spawned per-tool tasks) without an
/// `async` hop. Absent (`None` on the session context) for every turn that has
/// no live watcher.
pub trait ToolEventSink: Send + Sync {
    fn emit(&self, event: TurnEvent);
}
