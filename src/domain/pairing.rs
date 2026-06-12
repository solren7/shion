//! Channel pairing: an unknown sender on a message platform must be approved
//! from the shion host before the agent talks to them.
//!
//! The flow is a device-pairing handshake: the first message from an unpaired
//! sender gets a short code as the only reply; someone with shell access to
//! the host runs `shion pair approve <code>`, which proves they control the
//! machine; from the next message on, the sender is admitted. Senders in a
//! channel's `allow_from` list are pre-trusted and skip pairing.

use async_trait::async_trait;

/// A pending code is only approvable for this long; after that the next
/// message from the sender mints a fresh code.
pub const PAIRING_CODE_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    Pending,
    Approved,
}

impl PairingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
        }
    }
}

pub fn parse_pairing_status(s: &str) -> PairingStatus {
    match s {
        "approved" => PairingStatus::Approved,
        _ => PairingStatus::Pending,
    }
}

#[derive(Debug, Clone)]
pub struct PairingRequest {
    /// One row per sender: `{platform}:{sender_id}`.
    pub id: String,
    pub platform: String,
    pub sender_id: String,
    /// Chat the request came from (where the code was sent).
    pub chat_id: String,
    pub code: String,
    pub status: PairingStatus,
    pub created_at: i64,
}

impl PairingRequest {
    pub fn new(platform: &str, sender_id: &str, chat_id: &str) -> Self {
        Self {
            id: format!("{platform}:{sender_id}"),
            platform: platform.to_string(),
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            code: new_code(),
            status: PairingStatus::Pending,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }

    /// A pending request past the code TTL. Approved pairings never expire.
    pub fn is_expired(&self, now: i64) -> bool {
        self.status == PairingStatus::Pending && now - self.created_at > PAIRING_CODE_TTL_SECS
    }
}

/// Short uppercase pairing code. Unguessability is not load-bearing — approval
/// only ever happens from a shell on the host — the code just has to be easy
/// to read out of a chat message and type into a terminal.
fn new_code() -> String {
    let id = uuid::Uuid::now_v7().simple().to_string();
    // The tail of a v7 uuid is the random part; the head is a timestamp.
    id[id.len() - 8..].to_uppercase()
}

#[async_trait]
pub trait PairingRepository: Send + Sync {
    /// Insert or replace the row for `request.id`.
    async fn upsert(&self, request: &PairingRequest) -> anyhow::Result<()>;
    async fn find(&self, platform: &str, sender_id: &str)
    -> anyhow::Result<Option<PairingRequest>>;
    /// Approve the pending, unexpired request bearing `code`. Returns the
    /// approved request, or `None` when nothing approvable matches.
    async fn approve_code(&self, code: &str) -> anyhow::Result<Option<PairingRequest>>;
    async fn list(&self) -> anyhow::Result<Vec<PairingRequest>>;
    /// Remove a pairing by row id (`{platform}:{sender_id}`); returns whether
    /// a row was removed. Un-pairs an approved sender or discards a pending code.
    async fn revoke(&self, id: &str) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_code_is_short_uppercase_hex() {
        let code = new_code();
        assert_eq!(code.len(), 8);
        assert!(
            code.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn pending_request_expires_after_ttl() {
        let request = PairingRequest::new("telegram", "42", "42");
        assert!(!request.is_expired(request.created_at + 60));
        assert!(request.is_expired(request.created_at + PAIRING_CODE_TTL_SECS + 1));
    }

    #[test]
    fn approved_request_never_expires() {
        let mut request = PairingRequest::new("telegram", "42", "42");
        request.status = PairingStatus::Approved;
        assert!(!request.is_expired(request.created_at + PAIRING_CODE_TTL_SECS * 100));
    }
}
