use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("tool not found: {0}")]
    ToolNotFound(String),
}
