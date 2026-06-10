/// Risk level of an action, used to decide how prominently to warn the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Read-only or otherwise harmless action. An interactive approver may
    /// allow these without prompting; non-interactive approvers still deny.
    Safe,
    Normal,
    Dangerous,
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
}

impl ApprovalRequest {
    pub fn safe(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Safe,
            detail: None,
            scope_key: None,
        }
    }

    pub fn normal(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Normal,
            detail: None,
            scope_key: None,
        }
    }

    pub fn dangerous(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Dangerous,
            detail: Some(detail.into()),
            scope_key: None,
        }
    }

    /// Attach a session-scope key (see [`ApprovalRequest::scope_key`]).
    pub fn with_scope_key(mut self, key: impl Into<String>) -> Self {
        self.scope_key = Some(key.into());
        self
    }
}

/// Gate for sensitive, side-effecting actions (e.g. running a shell command or
/// writing a file).
///
/// The domain layer only knows this trait; the interface layer (CLI) provides a
/// concrete implementation that prompts the user. Tools that perform risky
/// actions depend on an `Arc<dyn Approver>` rather than on any I/O directly.
pub trait Approver: Send + Sync {
    /// Ask the user to approve `request`. Returns `true` if it may proceed.
    fn approve(&self, request: &ApprovalRequest) -> bool;
}
