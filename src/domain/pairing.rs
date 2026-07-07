//! Channel pairing: an unknown sender on a message platform must be approved
//! from the shion host before the agent talks to them.
//!
//! The flow is a device-pairing handshake: the first message from an unpaired
//! sender gets a short code as the only reply; someone with shell access to
//! the host runs `shion pair approve <code>`, which proves they control the
//! machine; from the next message on, the sender is admitted. Senders in a
//! channel's `allow_from` list are pre-trusted and skip pairing.
//!
//! Hardened after hermes-agent's `pairing.py`:
//!   - the code is never stored in plaintext — only a salted SHA-256 hash
//!     (`code_hash` + `salt`); the plaintext exists just long enough to send it
//!     to the sender once;
//!   - a sender can only be issued a fresh code once per [`PAIRING_RATE_LIMIT_SECS`];
//!   - at most [`MAX_PENDING_PER_PLATFORM`] distinct senders may await approval
//!     on a platform at a time;
//!   - after [`APPROVE_MAX_FAILURES`] wrong `shion pair approve` codes the
//!     approve command locks for [`APPROVE_LOCKOUT_SECS`].

use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// A pending code is only approvable for this long; after that the next
/// message from the sender (past the rate-limit window) mints a fresh code.
pub const PAIRING_CODE_TTL_SECS: i64 = 3600;

/// A sender is issued at most one fresh code per this window (anti-spam).
pub const PAIRING_RATE_LIMIT_SECS: i64 = 600;

/// Cap on distinct senders awaiting approval on one platform at once.
pub const MAX_PENDING_PER_PLATFORM: usize = 3;

/// Wrong `shion pair approve` codes tolerated before the approve path locks.
pub const APPROVE_MAX_FAILURES: i64 = 5;

/// How long the approve path stays locked after too many failures.
pub const APPROVE_LOCKOUT_SECS: i64 = 3600;

/// Code length and alphabet — uppercase, no easily-confused glyphs (0/O, 1/I/L).
const CODE_LEN: usize = 8;
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

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
    /// Salted SHA-256 of the code (hex). The plaintext is never persisted.
    pub code_hash: String,
    /// Per-row salt (hex) mixed into `code_hash`.
    pub salt: String,
    pub status: PairingStatus,
    pub created_at: i64,
}

impl PairingRequest {
    /// Mint a fresh pending request. Returns the row to persist (hash + salt
    /// only) and the plaintext code to send to the sender — the only moment the
    /// plaintext exists.
    pub fn mint(platform: &str, sender_id: &str, chat_id: &str) -> (Self, String) {
        let code = new_code();
        let salt = new_salt();
        let code_hash = hash_code(&salt, &code);
        let request = Self {
            id: format!("{platform}:{sender_id}"),
            platform: platform.to_string(),
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            code_hash,
            salt,
            status: PairingStatus::Pending,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        };
        (request, code)
    }

    /// A pending request past the code TTL. Approved pairings never expire.
    pub fn is_expired(&self, now: i64) -> bool {
        self.status == PairingStatus::Pending && now - self.created_at > PAIRING_CODE_TTL_SECS
    }
}

/// Whether `code` hashes (with `salt`) to `code_hash`, compared in constant time.
pub fn verify_code(salt: &str, code_hash: &str, code: &str) -> bool {
    ct_eq(&hash_code(salt, code), code_hash)
}

/// Salted SHA-256 of `code`, hex-encoded.
fn hash_code(salt: &str, code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(code.as_bytes());
    hex(&hasher.finalize())
}

/// Short uppercase pairing code from the unambiguous alphabet. Random enough to
/// resist guessing within the 1h TTL; approval is host-only regardless.
fn new_code() -> String {
    uuid::Uuid::new_v4()
        .into_bytes()
        .iter()
        .take(CODE_LEN)
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect()
}

/// 16-byte random salt, hex-encoded.
fn new_salt() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time equality for byte strings — the one shared comparison
/// primitive for secret material (pairing-code hashes here, the api channel's
/// bearer digests). A plain `==` short-circuits on the first differing byte,
/// letting a timing side-channel probe a secret byte by byte; keep every
/// secret comparison on this fold so a "simplification" of one copy can't
/// silently reintroduce the leak elsewhere.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Outcome of approving a code from the host (`shion pair approve`).
pub enum ApproveOutcome {
    Approved(PairingRequest),
    /// No pending, unexpired request matched the code.
    NotFound,
    /// Too many recent failures; the approve path is locked.
    Locked {
        retry_after_secs: i64,
    },
}

#[async_trait]
pub trait PairingRepository: Send + Sync {
    /// Insert or replace the row for `request.id`.
    async fn upsert(&self, request: &PairingRequest) -> anyhow::Result<()>;
    async fn find(&self, platform: &str, sender_id: &str)
    -> anyhow::Result<Option<PairingRequest>>;
    /// Count distinct senders with an active (pending, unexpired) request on
    /// `platform` — backs the per-platform pending cap.
    async fn count_active_pending(&self, platform: &str) -> anyhow::Result<usize>;
    /// Approve the pending, unexpired request bearing `code`, enforcing the
    /// failure lockout. See [`ApproveOutcome`].
    async fn approve_code(&self, code: &str) -> anyhow::Result<ApproveOutcome>;
    async fn list(&self) -> anyhow::Result<Vec<PairingRequest>>;
    /// Remove a pairing by row id (`{platform}:{sender_id}`); returns whether
    /// a row was removed. Un-pairs an approved sender or discards a pending code.
    async fn revoke(&self, id: &str) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_code_is_unambiguous_and_verifies() {
        let (request, code) = PairingRequest::mint("telegram", "42", "42");
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        // The plaintext is not stored; only the salted hash, which verifies.
        assert_ne!(request.code_hash, code);
        assert!(verify_code(&request.salt, &request.code_hash, &code));
        assert!(!verify_code(&request.salt, &request.code_hash, "WRONGCOD"));
    }

    #[test]
    fn salt_makes_identical_codes_hash_differently() {
        // Two mints almost surely differ; verifying one against the other fails.
        let (a, code_a) = PairingRequest::mint("telegram", "1", "1");
        let (b, _) = PairingRequest::mint("telegram", "2", "2");
        assert!(!verify_code(&b.salt, &b.code_hash, &code_a) || a.code_hash != b.code_hash);
    }

    #[test]
    fn pending_request_expires_after_ttl() {
        let (request, _) = PairingRequest::mint("telegram", "42", "42");
        assert!(!request.is_expired(request.created_at + 60));
        assert!(request.is_expired(request.created_at + PAIRING_CODE_TTL_SECS + 1));
    }

    #[test]
    fn approved_request_never_expires() {
        let (mut request, _) = PairingRequest::mint("telegram", "42", "42");
        request.status = PairingStatus::Approved;
        assert!(!request.is_expired(request.created_at + PAIRING_CODE_TTL_SECS * 100));
    }
}
