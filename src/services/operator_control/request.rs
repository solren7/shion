//! Typed operator requests and replies.
//!
//! These are the view types the operator surface exchanges — serialized by the
//! gateway's HTTP endpoints, deserialized by the CLI's gateway adapter, and
//! produced directly by the in-process adapter — so they live here as the
//! single source of truth, not in either transport.

use crate::domain::{
    memory::Memory,
    reminder::Reminder,
    run::{Run, RunStep},
    task::Task,
};

// The pure view DTOs (no domain dependency) live in `komo-core` so HTTP clients
// — the CLI gateway adapter and the Dioxus GUI — share one definition. Re-export
// them here so `operator_control::{SessionSummary, …}` paths are unchanged.
pub use komo_core::operator_view::{
    DreamItem, DreamReport, PairingView, ResumeOutcome, SessionSummary, SkillInvocation,
};

/// A read-only operator request. One `query` call per CLI render — the CLI
/// never knows which transport answers it.
#[derive(Debug)]
pub enum OperatorQuery {
    /// Pending reminders, soonest first.
    Reminders,
    /// Open tasks (inbox/todo/waiting).
    Tasks,
    /// Recent runs, newest first.
    Runs { limit: usize },
    /// One run with its tool steps (`None` = no such run).
    Run { id: String },
    /// Session summaries (never full transcripts).
    Sessions,
    /// The whole memory library (operator view — no scope enforcement).
    Memories,
    /// Hash-free pairing rows.
    Pairings,
    /// The dreaming dry-run classification.
    DreamPreview,
    /// Which turns loaded a skill (derived from the run ledger).
    SkillAudit { name: String },
    /// The `/sethome` runtime override (`None` when unset).
    HomeOverride,
}

/// The reply to an [`OperatorQuery`], variant-for-variant. Callers match
/// exhaustively — transport JSON shapes never become the caller interface.
#[derive(Debug)]
pub enum OperatorQueryResult {
    Reminders(Vec<Reminder>),
    Tasks(Vec<Task>),
    Runs(Vec<Run>),
    Run(Option<(Run, Vec<RunStep>)>),
    Sessions(Vec<SessionSummary>),
    Memories(Vec<Memory>),
    Pairings(Vec<PairingView>),
    DreamPreview(DreamReport),
    SkillAudit(Vec<SkillInvocation>),
    HomeOverride(Option<String>),
}

/// A state-changing operator action (host-operator writes; the gateway serves
/// these only to loopback callers).
#[derive(Debug)]
pub enum OperatorCommand {
    /// Apply one memory governance transition.
    MemoryTransition {
        id: String,
        action: MemoryTransitionAction,
    },
    /// Drop runs (and their steps) started before `cutoff`.
    PruneRuns { cutoff: i64 },
    /// Delete every session with no messages.
    CleanSessions,
    /// Approve the pending pairing bearing `code`.
    PairApprove { code: String },
    /// Remove a pairing by id.
    PairRevoke { id: String },
    /// Run one dreaming consolidation cycle.
    DreamApply,
}

/// The reply to an [`OperatorCommand`], variant-for-variant.
#[derive(Debug)]
pub enum OperatorCommandResult {
    /// The transition applied (an unknown id is an `Err`, identical on both
    /// transports).
    MemoryTransitioned,
    RunsPruned {
        removed: usize,
    },
    SessionsCleaned {
        removed: usize,
    },
    PairApproved(PairApproveOutcome),
    PairRevoked {
        revoked: bool,
    },
    DreamApplied {
        promoted: usize,
        archived: usize,
    },
}

/// A memory governance transition. The domain owns the semantics
/// (`Memory::promote/reject/pin`); this only names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTransitionAction {
    Promote,
    Reject,
    Pin,
}

impl MemoryTransitionAction {
    /// The api route leg (`/api/memories/{id}/<route>`).
    pub fn route(self) -> &'static str {
        match self {
            MemoryTransitionAction::Promote => "promote",
            MemoryTransitionAction::Reject => "reject",
            MemoryTransitionAction::Pin => "pin",
        }
    }

    /// The domain method this action names.
    pub fn apply(self) -> fn(&mut Memory, i64) {
        match self {
            MemoryTransitionAction::Promote => Memory::promote,
            MemoryTransitionAction::Reject => Memory::reject,
            MemoryTransitionAction::Pin => Memory::pin,
        }
    }
}

/// The outcome of a pairing approval, identical on both transports.
#[derive(Debug)]
pub enum PairApproveOutcome {
    Approved { id: String },
    NotFound,
    Locked { retry_after_secs: i64 },
}
