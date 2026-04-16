use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("invalid plan: {0}")]
    InvalidPlan(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),
}
