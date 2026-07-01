use async_trait::async_trait;

use super::{message::Message, session::Session, skill::Skill};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Find a session by id. Returns None if it does not exist.
    ///
    /// Loads the *entire* transcript — the reflective reviewer depends on
    /// seeing every message. The per-turn agent loop, which only needs a recent
    /// window, should use [`find_windowed`](Self::find_windowed) instead.
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>>;
    /// Like [`find`](Self::find) but loads only the most recent `limit`
    /// messages (by timestamp), keeping the per-turn hot path off a full-
    /// transcript read for long-lived chat sessions. `limit == 0` means no
    /// window (load everything, same as `find`). The returned messages stay in
    /// chronological order.
    async fn find_windowed(&self, id: &str, limit: usize) -> anyhow::Result<Option<Session>>;
    /// Return all sessions, ordered by creation time.
    async fn list(&self) -> anyhow::Result<Vec<Session>>;
    /// Persist a session (insert or update).
    async fn save(&self, session: &Session) -> anyhow::Result<()>;
    /// Delete every session that has zero messages. Returns the count removed.
    async fn delete_empty_sessions(&self) -> anyhow::Result<usize>;
    /// Rotate a session (hermes' `/new`): move its messages to a fresh archived
    /// id so `session_id` is left empty for a new conversation, while the old
    /// transcript is preserved (the reviewer can still see it). Returns the
    /// archived id, or `None` when there was nothing to archive.
    async fn rotate(&self, session_id: &str) -> anyhow::Result<Option<String>>;
}

#[async_trait]
pub trait MessageRepository: Send + Sync {
    /// Return all messages for a session, ordered by timestamp.
    async fn list_by_session(&self, session_id: &str) -> anyhow::Result<Vec<Message>>;
    /// Count the user-role messages in a session (i.e. the number of user
    /// turns), without materializing the transcript. Used to drive the periodic
    /// reviewer cadence cheaply now that the turn loads only a windowed session.
    async fn count_user_turns(&self, session_id: &str) -> anyhow::Result<usize>;
    /// Append a message to a session.
    async fn save(&self, session_id: &str, message: &Message) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SkillRepository: Send + Sync {
    async fn find(&self, name: &str) -> anyhow::Result<Option<Skill>>;
    async fn list(&self) -> anyhow::Result<Vec<Skill>>;
    async fn save(&self, skill: &Skill) -> anyhow::Result<()>;
}
