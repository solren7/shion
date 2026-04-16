use async_trait::async_trait;

use super::{message::Message, session::Session};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Find a session by id. Returns None if it does not exist.
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>>;
    /// Persist a session (insert or update).
    async fn save(&self, session: &Session) -> anyhow::Result<()>;
}

#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Return all messages for a session, ordered by timestamp.
    async fn list_by_session(&self, session_id: &str) -> anyhow::Result<Vec<Message>>;
    /// Append a message to a session.
    async fn save(&self, session_id: &str, message: &Message) -> anyhow::Result<()>;
}
