use async_trait::async_trait;

use super::{message::Message, session::Session, skill::Skill};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Find a session by id. Returns None if it does not exist.
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>>;
    /// Return all sessions, ordered by creation time.
    async fn list(&self) -> anyhow::Result<Vec<Session>>;
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

#[async_trait]
pub trait SkillRepository: Send + Sync {
    async fn find(&self, name: &str) -> anyhow::Result<Option<Skill>>;
    async fn list(&self) -> anyhow::Result<Vec<Skill>>;
    async fn save(&self, skill: &Skill) -> anyhow::Result<()>;
}
