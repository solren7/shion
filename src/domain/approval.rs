/// Risk level of an action, used to decide how prominently to warn the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
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
}

impl ApprovalRequest {
    pub fn normal(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Normal,
            detail: None,
        }
    }

    pub fn dangerous(summary: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            risk: Risk::Dangerous,
            detail: Some(detail.into()),
        }
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
