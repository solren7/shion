use serde::{Deserialize, Serialize};

use super::message::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    pub created_at: i64,
    /// Optional operator-set display name (empty = untitled; clients fall back
    /// to a label derived from the id). Set via `SessionRepository::set_title`.
    #[serde(default)]
    pub title: String,
    /// Lifecycle: `"active"` (default), `"archive"`, or `"deleted"`. A soft
    /// status set via `SessionRepository::set_status`; the session list hides
    /// `deleted`. See [`SESSION_STATUS_ACTIVE`] etc.
    #[serde(default = "default_status")]
    pub status: String,
}

/// Default session status when none is stored (older rows, fresh sessions).
pub const SESSION_STATUS_ACTIVE: &str = "active";
pub const SESSION_STATUS_ARCHIVE: &str = "archive";
pub const SESSION_STATUS_DELETED: &str = "deleted";

fn default_status() -> String {
    SESSION_STATUS_ACTIVE.to_string()
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            messages: Vec::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            title: String::new(),
            status: default_status(),
        }
    }

    pub fn user_turns(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.role == super::message::Role::User)
            .count()
    }
}
