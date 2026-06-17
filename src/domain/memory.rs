use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A long-term memory: a durable fact, preference, or note about the user, a
/// project, a person, or a decision. Memories are governed (status/confidence)
/// and scoped (where they may surface) so the agent can be injected with a
/// conservative profile (L1), recall relevant facts (L3), and let the user
/// curate the full library (L2). See `docs/memory-injection-plan.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,

    /// Lifecycle state. Automated extraction lands as `Candidate`; only
    /// user-confirmed/written memories become high-confidence `Active`.
    pub status: MemoryStatus,
    /// How much the memory can be trusted, by origin.
    pub confidence: MemoryConfidence,
    /// 0–100 ranking weight; ties broken by recency. Default 50.
    pub importance: i32,
    /// Eligible for L1 pinned-profile injection (every turn). Only ever set by
    /// the user / explicit confirmation, never by automated extraction.
    pub pinned: bool,

    /// Where this memory may surface. Scope is enforced at the query layer, not
    /// the render layer, so a channel-scoped memory never leaks into another
    /// chat. See [`MemoryContext`].
    pub scope: MemoryScope,

    /// Session this memory was distilled from (`telegram:{chat_id}`, a cli
    /// session uuid, …). Empty = written outside any session.
    pub source: String,
    /// Content-derived dedup key set on automated extraction (FNV-1a over the
    /// normalized content), so re-reviewing a session never duplicates it.
    pub source_message_id: String,

    pub created_at: i64,
    pub updated_at: i64,
    /// Optional governance TTL: a unix timestamp past which the memory is
    /// treated as stale and hidden from recall. `None` = never expires.
    pub expires_at: Option<i64>,
    /// Last time this memory surfaced in recall, for future usage-based
    /// promotion/archival signals. `None` = never used.
    pub last_used_at: Option<i64>,
}

/// Default ranking weight for a new memory.
pub const DEFAULT_IMPORTANCE: i32 = 50;

impl Memory {
    /// A new memory with conservative defaults: `Active` status, `Inferred`
    /// confidence, global scope, not pinned. Callers (the `memory` tool, the
    /// reviewer) override status/confidence/scope to match their trust level.
    pub fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Self {
            id: format!(
                "mem-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            kind,
            content: content.into(),
            status: MemoryStatus::Active,
            confidence: MemoryConfidence::Inferred,
            importance: DEFAULT_IMPORTANCE,
            pinned: false,
            scope: MemoryScope::Global,
            source: String::new(),
            source_message_id: String::new(),
            created_at: now,
            updated_at: now,
            expires_at: None,
            last_used_at: None,
        }
    }

    /// Whether this memory has expired as of `now` (a unix timestamp).
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }

    /// Whether this memory is eligible for L1 pinned-profile injection in the
    /// given context: pinned, active, high-confidence, an identity/preference
    /// kind, in a scope the context allows, and not expired.
    pub fn is_pinnable(&self, ctx: &MemoryContext, now: i64) -> bool {
        self.pinned
            && self.status == MemoryStatus::Active
            && matches!(
                self.confidence,
                MemoryConfidence::Confirmed | MemoryConfidence::UserWritten
            )
            && matches!(
                self.kind,
                MemoryKind::Profile | MemoryKind::Preference | MemoryKind::Feedback
            )
            && ctx.allows(&self.scope)
            && !self.is_expired(now)
    }
}

// ── kind ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    Profile,
    Preference,
    Feedback,
    Project,
    Person,
    Fact,
    Decision,
    Reference,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Preference => "preference",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Person => "person",
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Reference => "reference",
        }
    }
}

/// Parse a kind string, accepting both the current vocabulary and the legacy
/// markdown values (`user` → `Profile`). Unknown → `Fact` (the most neutral
/// bucket).
pub fn parse_memory_kind(value: &str) -> MemoryKind {
    match value.trim() {
        "profile" | "user" => MemoryKind::Profile,
        "preference" => MemoryKind::Preference,
        "feedback" => MemoryKind::Feedback,
        "project" => MemoryKind::Project,
        "person" => MemoryKind::Person,
        "decision" => MemoryKind::Decision,
        "reference" => MemoryKind::Reference,
        _ => MemoryKind::Fact,
    }
}

// ── status ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Candidate,
    Active,
    Archived,
    Rejected,
}

impl MemoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Rejected => "rejected",
        }
    }
}

pub fn parse_memory_status(value: &str) -> MemoryStatus {
    match value.trim() {
        "candidate" => MemoryStatus::Candidate,
        "archived" => MemoryStatus::Archived,
        "rejected" => MemoryStatus::Rejected,
        _ => MemoryStatus::Active,
    }
}

// ── confidence ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConfidence {
    Extracted,
    Inferred,
    Confirmed,
    UserWritten,
}

impl MemoryConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::Inferred => "inferred",
            Self::Confirmed => "confirmed",
            Self::UserWritten => "user_written",
        }
    }
}

pub fn parse_memory_confidence(value: &str) -> MemoryConfidence {
    match value.trim() {
        "inferred" => MemoryConfidence::Inferred,
        "confirmed" => MemoryConfidence::Confirmed,
        "user_written" => MemoryConfidence::UserWritten,
        _ => MemoryConfidence::Extracted,
    }
}

// ── scope ─────────────────────────────────────────────────────────────────────

/// Where a memory may surface. Serialized to the DB as a `(scope_type,
/// scope_key)` pair so it can be filtered in queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryScope {
    /// Visible everywhere.
    Global,
    /// Tied to a project (CLI workspace key).
    Project(String),
    /// Tied to a chat channel (`{platform}:{chat_id}`).
    Channel { platform: String, chat_id: String },
    /// Tied to a single session id.
    Session(String),
}

impl MemoryScope {
    pub fn type_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project(_) => "project",
            Self::Channel { .. } => "channel",
            Self::Session(_) => "session",
        }
    }

    /// The opaque key stored alongside `type_str`. Empty for `Global`.
    pub fn key(&self) -> String {
        match self {
            Self::Global => String::new(),
            Self::Project(p) => p.clone(),
            Self::Channel { platform, chat_id } => format!("{platform}:{chat_id}"),
            Self::Session(id) => id.clone(),
        }
    }

    /// Rebuild a scope from its serialized `(type, key)` pair. Unknown type or a
    /// malformed channel key degrades to `Global` (fail safe — never widen).
    pub fn from_parts(scope_type: &str, scope_key: &str) -> Self {
        match scope_type.trim() {
            "project" if !scope_key.is_empty() => Self::Project(scope_key.to_string()),
            "channel" => match scope_key.split_once(':') {
                Some((platform, chat_id)) => Self::Channel {
                    platform: platform.to_string(),
                    chat_id: chat_id.to_string(),
                },
                None => Self::Global,
            },
            "session" if !scope_key.is_empty() => Self::Session(scope_key.to_string()),
            _ => Self::Global,
        }
    }
}

/// The scopes a memory may be drawn from for the current turn, derived from the
/// session id. `Global` is always allowed; chat sessions add their `Channel`
/// and `Session` scopes. Scope is decided here, before any query, so a query
/// can never widen beyond what the context permits.
#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub session_id: String,
    pub allowed_scopes: Vec<MemoryScope>,
}

impl MemoryContext {
    /// Derive the allowed scopes from a session id. A chat session id is
    /// `{platform}:{chat_id}`; a CLI session is an opaque uuid. (Project scope
    /// for CLI sessions is wired separately once the workspace key is known.)
    pub fn from_session(session_id: &str) -> Self {
        let mut allowed_scopes = vec![MemoryScope::Global];
        if let Some((platform, chat_id)) = session_id.split_once(':') {
            allowed_scopes.push(MemoryScope::Channel {
                platform: platform.to_string(),
                chat_id: chat_id.to_string(),
            });
        }
        allowed_scopes.push(MemoryScope::Session(session_id.to_string()));
        Self {
            session_id: session_id.to_string(),
            allowed_scopes,
        }
    }

    /// The scope an automated write from this context should carry: the channel
    /// for a chat session, else global. (Never `Session`, which would make a
    /// memory unrecallable outside the exact session.)
    pub fn write_scope(&self) -> MemoryScope {
        self.allowed_scopes
            .iter()
            .find(|s| matches!(s, MemoryScope::Channel { .. }))
            .cloned()
            .unwrap_or(MemoryScope::Global)
    }

    /// Whether a memory's scope is permitted in this context.
    pub fn allows(&self, scope: &MemoryScope) -> bool {
        self.allowed_scopes.contains(scope)
    }
}

// ── query / scored result ─────────────────────────────────────────────────────

/// A scope-bounded search over the memory library. `allowed_scopes` and
/// `statuses` must be filled before the store is hit — the repository enforces
/// them, callers cannot widen them downstream.
// Consumed by L2 search / L3 recall (next increment); defined now so the
// repository contract is stable. See `docs/memory-injection-plan.md`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub allowed_scopes: Vec<MemoryScope>,
    pub kinds: Vec<MemoryKind>,
    pub statuses: Vec<MemoryStatus>,
    pub limit: usize,
}

/// A memory plus its rerank score for a given query.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub memory: Memory,
    pub score: f64,
}

// ── repository ────────────────────────────────────────────────────────────────

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    /// Persist a memory (create or overwrite by id).
    async fn save(&self, memory: &Memory) -> anyhow::Result<()>;

    /// All non-expired memories, any status. Callers filter further. (Kept
    /// no-arg for the briefing sweep and the `memory` tool; richer scope/status
    /// queries go through [`MemoryRepository::pinned`] / `search`.)
    async fn list(&self) -> anyhow::Result<Vec<Memory>>;

    /// L1 pinned profile: the small, stable set eligible for per-turn injection
    /// in `ctx`. Defaults to filtering [`list`](MemoryRepository::list) by
    /// [`Memory::is_pinnable`]; a store may override for efficiency.
    async fn pinned(&self, ctx: &MemoryContext) -> anyhow::Result<Vec<Memory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut pinned: Vec<Memory> = self
            .list()
            .await?
            .into_iter()
            .filter(|m| m.is_pinnable(ctx, now))
            .collect();
        // Most important first; ties broken by most-recently-updated.
        pinned.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        Ok(pinned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_accepts_legacy_and_new() {
        assert_eq!(parse_memory_kind("user"), MemoryKind::Profile);
        assert_eq!(parse_memory_kind("preference"), MemoryKind::Preference);
        assert_eq!(parse_memory_kind("decision"), MemoryKind::Decision);
        assert_eq!(parse_memory_kind("nonsense"), MemoryKind::Fact);
    }

    #[test]
    fn scope_roundtrips_through_parts() {
        let scopes = [
            MemoryScope::Global,
            MemoryScope::Project("shion".into()),
            MemoryScope::Channel {
                platform: "telegram".into(),
                chat_id: "42".into(),
            },
            MemoryScope::Session("feishu:oc_x".into()),
        ];
        for scope in scopes {
            let rebuilt = MemoryScope::from_parts(&scope.type_str(), &scope.key());
            assert_eq!(rebuilt, scope);
        }
    }

    #[test]
    fn channel_scope_with_malformed_key_degrades_to_global() {
        assert_eq!(
            MemoryScope::from_parts("channel", "no-colon"),
            MemoryScope::Global
        );
    }

    #[test]
    fn context_from_chat_session_allows_global_channel_session() {
        let ctx = MemoryContext::from_session("telegram:42");
        assert!(ctx.allows(&MemoryScope::Global));
        assert!(ctx.allows(&MemoryScope::Channel {
            platform: "telegram".into(),
            chat_id: "42".into()
        }));
        assert!(ctx.allows(&MemoryScope::Session("telegram:42".into())));
        // A different channel is not allowed.
        assert!(!ctx.allows(&MemoryScope::Channel {
            platform: "feishu".into(),
            chat_id: "oc_x".into()
        }));
        assert_eq!(
            ctx.write_scope(),
            MemoryScope::Channel {
                platform: "telegram".into(),
                chat_id: "42".into()
            }
        );
    }

    #[test]
    fn cli_session_context_writes_global() {
        let ctx = MemoryContext::from_session("0192-uuid");
        assert_eq!(ctx.write_scope(), MemoryScope::Global);
    }

    fn pinnable_memory() -> Memory {
        let mut m = Memory::new(MemoryKind::Preference, "prefers concise answers");
        m.pinned = true;
        m.confidence = MemoryConfidence::UserWritten;
        m
    }

    #[test]
    fn is_pinnable_requires_pinned_active_confident_identity_kind() {
        let ctx = MemoryContext::from_session("cli");
        let now = 1_000;
        assert!(pinnable_memory().is_pinnable(&ctx, now));

        let mut not_pinned = pinnable_memory();
        not_pinned.pinned = false;
        assert!(!not_pinned.is_pinnable(&ctx, now));

        let mut low_conf = pinnable_memory();
        low_conf.confidence = MemoryConfidence::Extracted;
        assert!(!low_conf.is_pinnable(&ctx, now));

        let mut wrong_kind = pinnable_memory();
        wrong_kind.kind = MemoryKind::Reference;
        assert!(!wrong_kind.is_pinnable(&ctx, now));

        let mut expired = pinnable_memory();
        expired.expires_at = Some(now - 1);
        assert!(!expired.is_pinnable(&ctx, now));
    }

    #[test]
    fn pinnable_excludes_out_of_scope() {
        let ctx = MemoryContext::from_session("telegram:42");
        let mut other_channel = pinnable_memory();
        other_channel.scope = MemoryScope::Channel {
            platform: "feishu".into(),
            chat_id: "oc_x".into(),
        };
        assert!(!other_channel.is_pinnable(&ctx, 1_000));
    }
}
