use async_trait::async_trait;

use super::session::Session;

/// Abstraction over a large-language-model backend.
///
/// The domain layer only knows this trait; concrete providers (DeepSeek,
/// OpenAI, an internal gateway, ...) live in `infra/`.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Produce an assistant reply for the current conversation state.
    async fn complete(&self, session: &Session) -> anyhow::Result<String>;
}
