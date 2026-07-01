use std::path::PathBuf;

/// Risk level of an action, used to decide how prominently to warn the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Read-only or otherwise harmless action. An interactive approver may
    /// allow these without prompting; non-interactive approvers still deny.
    Safe,
    Normal,
    Dangerous,
}

/// A structured, machine-matchable description of the action being approved.
///
/// Distinct from [`ApprovalRequest::summary`] (which is for humans) and
/// [`ApprovalRequest::scope_key`] (a coarse "remember this kind" cache key):
/// `ActionRef` carries the *resource* — the command, path, URL, or service — so
/// the configurable permission policy (`domain::policy`) can match on directory
/// prefixes, command prefixes, and domains rather than parsing the summary
/// string. Optional: a request without one degrades to risk/scope-only matching.
#[derive(Debug, Clone)]
pub enum ActionRef {
    /// A shell command (`shell` tool). Matched against the full command line.
    Shell { command: String },
    /// A filesystem access (`file` tool). Matched against the path.
    File { path: PathBuf, write: bool },
    /// An outbound network fetch (`web_fetch`). Matched against the URL's host.
    ///
    /// Constructed by no tool yet — `web_fetch` runs un-gated — but it is *not*
    /// dead: the policy layer matches it (`domain::policy`) and config exposes a
    /// `network` rule category, so the capability is wired end-to-end and waiting
    /// only for a tool to route network access through the approver.
    #[allow(dead_code)]
    Network { url: String },
    /// A Home Assistant service call, matched as `domain.service`.
    Service { domain: String, service: String },
}

/// A request for the user to approve a side-effecting action.
pub struct ApprovalRequest {
    /// Human-readable description of the action, e.g. `run shell command: ls`.
    pub summary: String,
    pub risk: Risk,
    /// Optional extra context, e.g. why a command was flagged dangerous.
    pub detail: Option<String>,
    /// Stable key identifying the *kind* of action (e.g. the matched dangerous
    /// pattern, or `file:write`). An approver can cache an "allow for this
    /// session" answer under this key so repeats don't prompt again.
    pub scope_key: Option<String>,
    /// Structured resource the permission policy matches on (see [`ActionRef`]).
    pub action: Option<ActionRef>,
}

impl ApprovalRequest {
    pub fn safe(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Safe,
            detail: None,
            scope_key: None,
            action: None,
        }
    }

    pub fn normal(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Normal,
            detail: None,
            scope_key: None,
            action: None,
        }
    }

    pub fn dangerous(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Dangerous,
            detail: Some(detail.into()),
            scope_key: None,
            action: None,
        }
    }

    /// Attach a session-scope key (see [`ApprovalRequest::scope_key`]).
    pub fn with_scope_key(mut self, key: impl Into<String>) -> Self {
        self.scope_key = Some(key.into());
        self
    }

    /// Attach the structured resource the policy matches on (see [`ActionRef`]).
    pub fn with_action(mut self, action: ActionRef) -> Self {
        self.action = Some(action);
        self
    }
}

/// Gate for sensitive, side-effecting actions (e.g. running a shell command or
/// writing a file).
///
/// The domain layer only knows this trait; the interface layer provides a
/// concrete implementation that prompts the user. Tools that perform risky
/// actions depend on an `Arc<dyn Approver>` rather than on any I/O directly.
///
/// `approve` is async: an interactive approver reads a TTY, but a chat-channel
/// approver sends an approval prompt to the conversation and awaits the user's
/// reply on a later turn (see `agent::interaction::ChatApprover`).
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// Ask the user to approve `request`. Returns `true` if it may proceed.
    async fn approve(&self, request: &ApprovalRequest) -> bool;
}
