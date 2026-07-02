use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A long-term memory: a durable fact, preference, or note about the user, a
/// project, a person, or a decision. Memories are governed (status/confidence)
/// and scoped (where they may surface) so the agent can be injected with a
/// conservative profile (L1), recall relevant facts (L3), and let the user
/// curate the full library (L2). See `docs/personal-agent-roadmap.md`.
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
    /// Last time this memory surfaced in recall, for usage-based
    /// promotion/archival signals. `None` = never used.
    pub last_used_at: Option<i64>,
    /// How many times this memory has surfaced in L3 recall. The OpenClaw-style
    /// "dreaming" signal: importance proven by use, not guessed at write time.
    /// A candidate that earns enough recalls is auto-promoted to active by the
    /// `DreamSweep`; one that never does is eventually archived. See
    /// [`dream_verdict`].
    pub recall_count: i64,
    /// Distinct fingerprints of the queries that recalled this memory (see
    /// [`recall_query_hash`]), capped at [`RECALL_QUERY_HASHES_CAP`]. The
    /// query-diversity half of the dreaming signal (OpenClaw's
    /// `minUniqueQueries`): a raw `recall_count` can be pumped by one repeated
    /// question, but promotion also requires being recalled by
    /// [`DREAM_MIN_UNIQUE_QUERIES`] lexically-distinct queries.
    /// `#[serde(default)]` so a payload from an older gateway still parses.
    #[serde(default)]
    pub recall_query_hashes: Vec<String>,
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
            recall_count: 0,
            recall_query_hashes: Vec::new(),
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
        Self { allowed_scopes }
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
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub allowed_scopes: Vec<MemoryScope>,
    pub kinds: Vec<MemoryKind>,
    pub statuses: Vec<MemoryStatus>,
    pub limit: usize,
}

/// A memory plus its rerank score for a given query.
#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub memory: Memory,
    pub score: f64,
}

/// Explainable rerank score for a memory against a (already lowercased) query.
/// Returns `None` when a non-empty query does not lexically match the content
/// (the memory is excluded); otherwise a positive score combining lexical hit,
/// importance, confidence, and recency. No embedding — `LIKE`-style substring
/// match plus weighted signals, per the first-version plan. Scope/status/kind
/// are filtered before this is called.
pub fn rerank_score(memory: &Memory, query_lower: &str, now: i64) -> Option<f64> {
    if !query_lower.is_empty() && !memory.content.to_lowercase().contains(query_lower) {
        return None;
    }
    let mut score = 0.0;
    if !query_lower.is_empty() {
        score += 2.0; // lexical match
    }
    score += memory.importance as f64 / 100.0; // 0..~1
    score += match memory.confidence {
        MemoryConfidence::UserWritten => 0.4,
        MemoryConfidence::Confirmed => 0.3,
        MemoryConfidence::Inferred => 0.1,
        MemoryConfidence::Extracted => 0.0,
    };
    // Recency: 30-day half-life decay on the last update.
    let age_days = (now - memory.updated_at).max(0) as f64 / 86_400.0;
    score += 0.5 * (-age_days / 30.0).exp();
    Some(score)
}

// ── recall (L3) ───────────────────────────────────────────────────────────────

/// Extract lexical terms from text for L3 recall matching, language-agnostically:
/// runs of alphanumeric characters of length ≥ 2 become word terms, and adjacent
/// CJK characters become bigrams (a cheap stand-in for word segmentation, since
/// CJK has no whitespace boundaries). Everything lowercased.
///
/// This is distinct from [`rerank_score`]'s whole-query substring match: the L2
/// tool passes a focused keyword query (substring works), but L3 recall passes a
/// whole user message, where token overlap is the meaningful signal.
fn recall_terms(text: &str) -> HashSet<String> {
    let mut terms = HashSet::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    fn flush(word: &mut String, terms: &mut HashSet<String>) {
        if word.chars().count() >= 2 && !is_stopword(word) {
            terms.insert(word.clone());
        }
        word.clear();
    }
    for ch in text.chars() {
        let lc = ch.to_lowercase().next().unwrap_or(ch);
        if is_cjk(ch) {
            if let Some(p) = prev_cjk {
                terms.insert(format!("{p}{lc}"));
            }
            prev_cjk = Some(lc);
            flush(&mut word, &mut terms);
        } else if ch.is_alphanumeric() {
            word.push(lc);
            prev_cjk = None;
        } else {
            flush(&mut word, &mut terms);
            prev_cjk = None;
        }
    }
    flush(&mut word, &mut terms);
    terms
}

/// High-frequency English function words that carry no recall signal — dropping
/// them keeps a memory like "the user likes coffee" from matching any query that
/// merely contains "the". Not exhaustive; just the worst offenders.
fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "and"
            | "are"
            | "for"
            | "you"
            | "your"
            | "with"
            | "was"
            | "were"
            | "this"
            | "that"
            | "what"
            | "how"
            | "why"
            | "when"
            | "where"
            | "who"
            | "does"
            | "did"
            | "can"
            | "will"
            | "would"
            | "should"
            | "has"
            | "have"
            | "had"
            | "not"
            | "but"
            | "from"
            | "into"
            | "out"
            | "off"
            | "all"
            | "any"
            | "some"
            | "than"
            | "then"
            | "them"
            | "they"
            | "其中"
            | "可以"
            | "如何"
    )
}

/// CJK ranges where per-character (bigram) matching beats whitespace tokens:
/// CJK ideographs (+ Ext A), Hiragana/Katakana, Hangul syllables.
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

/// Fingerprint a recall query for the dreaming query-diversity signal: the
/// message's [`recall_terms`] (already normalized: lowercased, stopwords
/// dropped) sorted and hashed, first 16 hex chars of the SHA-256. Reordering or
/// re-punctuating the same question yields the same fingerprint; only a change
/// in substantive terms counts as a new query. Lexical, not semantic — a full
/// rephrasing with different words counts as distinct, which slightly
/// overstates diversity; acceptable until the embedding phase.
///
/// Returns an empty string when the text yields no terms (no signal — callers
/// must not record it).
pub fn recall_query_hash(text: &str) -> String {
    let mut terms: Vec<String> = recall_terms(text).into_iter().collect();
    if terms.is_empty() {
        return String::new();
    }
    terms.sort();
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for term in &terms {
        hasher.update(term.as_bytes());
        hasher.update([0x1f]); // separator so ["ab","c"] != ["a","bc"]
    }
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Score a memory for L3 recall against the query's extracted terms. Returns
/// `None` when there is no lexical overlap (the memory is excluded); otherwise a
/// positive score: shared-term count plus the same importance/confidence/recency
/// signals as [`rerank_score`]. Scope/status are filtered before this is called.
pub fn recall_score(memory: &Memory, query_terms: &HashSet<String>, now: i64) -> Option<f64> {
    if query_terms.is_empty() {
        return None;
    }
    let mem_terms = recall_terms(&memory.content);
    let overlap = query_terms
        .iter()
        .filter(|t| mem_terms.contains(*t))
        .count();
    if overlap == 0 {
        return None;
    }
    let mut score = overlap as f64; // each shared term = 1.0
    score += memory.importance as f64 / 100.0;
    score += match memory.confidence {
        MemoryConfidence::UserWritten => 0.4,
        MemoryConfidence::Confirmed => 0.3,
        MemoryConfidence::Inferred => 0.1,
        MemoryConfidence::Extracted => 0.0,
    };
    let age_days = (now - memory.updated_at).max(0) as f64 / 86_400.0;
    score += 0.5 * (-age_days / 30.0).exp();
    Some(score)
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

    /// Fetch a single memory by id. Default scans [`list`](MemoryRepository::list)
    /// (so it does not see expired memories); a store may override to fetch
    /// directly. Used by governance actions (promote/reject/archive/update).
    async fn get(&self, id: &str) -> anyhow::Result<Option<Memory>> {
        Ok(self.list().await?.into_iter().find(|m| m.id == id))
    }

    /// Scope-bounded L2/L3 search. Filters by `allowed_scopes` / `statuses` /
    /// `kinds`, scores the rest with [`rerank_score`], and returns the top
    /// `limit` by score. Default runs over [`list`](MemoryRepository::list); a
    /// store may override the candidate fetch (e.g. an FTS prefilter) later
    /// without changing the rerank.
    async fn search(&self, query: MemoryQuery) -> anyhow::Result<Vec<ScoredMemory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let needle = query.text.to_lowercase();
        let mut scored: Vec<ScoredMemory> = self
            .list()
            .await?
            .into_iter()
            .filter(|m| query.allowed_scopes.contains(&m.scope))
            .filter(|m| query.statuses.is_empty() || query.statuses.contains(&m.status))
            .filter(|m| query.kinds.is_empty() || query.kinds.contains(&m.kind))
            .filter_map(|m| {
                rerank_score(&m, &needle, now).map(|score| ScoredMemory { memory: m, score })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if query.limit > 0 {
            scored.truncate(query.limit);
        }
        Ok(scored)
    }

    /// Find a memory by its origin + content-derived dedup key, for reviewer
    /// re-extraction guarding (mirrors `TaskRepository`). An empty key never
    /// matches a real extraction. Default scans [`list`](MemoryRepository::list).
    async fn find_by_source_message_id(
        &self,
        source: &str,
        source_message_id: &str,
    ) -> anyhow::Result<Option<Memory>> {
        if source_message_id.is_empty() {
            return Ok(None);
        }
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|m| m.source == source && m.source_message_id == source_message_id))
    }

    /// L3 active recall: the in-scope memories most relevant to `text` (the
    /// current user message), ranked by [`recall_score`], top `limit`. Unlike
    /// [`search`](MemoryRepository::search) — which substring-matches a focused
    /// query — recall does token-overlap matching against a whole message.
    /// Scope/status are enforced here (design principle 3: never widen in the
    /// render layer). Default runs over [`list`](MemoryRepository::list); a
    /// store may override the candidate fetch later without changing scoring.
    ///
    /// **Candidates are recallable.** Both `Active` and `Candidate` memories
    /// surface (only `Archived`/`Rejected` are excluded). This is what makes the
    /// OpenClaw-style dreaming loop possible: a reviewer-extracted candidate must
    /// be visible to recall to *earn* its usage signal (`recall_count`), which
    /// the `DreamSweep` then uses to auto-promote it. Candidates score lower
    /// (their `Extracted` confidence adds nothing) and the rendered block tags
    /// each line with confidence, so the model treats them cautiously.
    async fn recall(
        &self,
        ctx: &MemoryContext,
        text: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ScoredMemory>> {
        let query_terms = recall_terms(text);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut scored: Vec<ScoredMemory> = self
            .list()
            .await?
            .into_iter()
            .filter(|m| matches!(m.status, MemoryStatus::Active | MemoryStatus::Candidate))
            .filter(|m| ctx.allows(&m.scope))
            .filter_map(|m| {
                recall_score(&m, &query_terms, now).map(|score| ScoredMemory { memory: m, score })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if limit > 0 {
            scored.truncate(limit);
        }
        Ok(scored)
    }

    /// Record that memories surfaced in recall: bump `recall_count`, stamp
    /// `last_used_at`, and accumulate the query's fingerprint (see
    /// [`recall_query_hash`]) into `recall_query_hashes` — never touching
    /// `updated_at` so the recency-decay signal stays tied to real edits.
    /// Count + distinct-query fingerprints are the dreaming system's usage
    /// signal (see [`dream_verdict`]). An empty `query_hash` (a query with no
    /// terms) still counts the recall but records no fingerprint; a fingerprint
    /// already present, or one past [`RECALL_QUERY_HASHES_CAP`], is dropped.
    /// Best-effort: ids that no longer resolve are skipped.
    async fn mark_used(&self, ids: &[String], now: i64, query_hash: &str) -> anyhow::Result<()> {
        for id in ids {
            if let Some(mut memory) = self.get(id).await? {
                memory.recall_count += 1;
                memory.last_used_at = Some(now);
                if !query_hash.is_empty()
                    && memory.recall_query_hashes.len() < RECALL_QUERY_HASHES_CAP
                    && !memory.recall_query_hashes.iter().any(|h| h == query_hash)
                {
                    memory.recall_query_hashes.push(query_hash.to_string());
                }
                self.save(&memory).await?;
            }
        }
        Ok(())
    }
}

// ── dreaming (usage-driven consolidation) ──────────────────────────────────────

/// What the nightly `DreamSweep` should do with a candidate memory, decided
/// purely from its accumulated usage. Borrowed from OpenClaw's dreaming system,
/// adapted to shion's governance ladder: instead of promoting daily-journal
/// fragments into `MEMORY.md`, we promote reviewer-extracted **candidates** into
/// the active, recallable set — and archive the ones that never earn their keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamVerdict {
    /// Recalled often enough, recently enough → promote to active.
    Promote,
    /// Old and never recalled → archive (reversible; not rejected).
    Archive,
    /// Leave as-is this cycle.
    Keep,
}

/// Minimum recalls a candidate must have earned to be promoted (OpenClaw's
/// `minRecallCount`, default 3).
pub const DREAM_MIN_RECALL_COUNT: i64 = 3;
/// Minimum distinct query fingerprints a candidate must have been recalled by
/// to be promoted (OpenClaw's `minUniqueQueries`) — so one repeated question
/// cannot pump a candidate to active on count alone.
pub const DREAM_MIN_UNIQUE_QUERIES: usize = 2;
/// Cap on stored distinct query fingerprints per memory. Diversity is proven
/// well before this; appending forever would only widen the row.
pub const RECALL_QUERY_HASHES_CAP: usize = 8;
/// A candidate older than this (days) with no recalls is archived — the
/// "forget the flotsam" half of dreaming.
pub const DREAM_FORGET_AGE_DAYS: i64 = 30;
/// Score a candidate must clear to be promoted (alongside the recall-count gate),
/// so a barely-relevant fact recalled by coincidence does not auto-promote.
pub const DREAM_PROMOTE_MIN_SCORE: f64 = 1.0;

/// Dreaming score for a candidate: dominated by recall frequency, nudged by
/// recency and importance. Explainable and embedding-free, matching the rest of
/// shion's ranking. Higher = stronger case for promotion. (Query-diversity is
/// a separate hard gate in [`dream_verdict`], not a score component — a
/// diversity-failing candidate must not promote no matter how high it scores.)
pub fn dream_score(memory: &Memory, now: i64) -> f64 {
    let mut score = memory.recall_count as f64; // each recall = 1.0, the core signal
    score += memory.importance as f64 / 100.0; // 0..~1
    // Recency of last use: a 30-day half-life decay, 0 when never used.
    if let Some(last) = memory.last_used_at {
        let age_days = (now - last).max(0) as f64 / 86_400.0;
        score += 0.5 * (-age_days / 30.0).exp();
    }
    score
}

/// Decide a candidate's fate for this dream cycle. Only `Candidate` memories are
/// ever acted on — active memories (user-saved or already promoted) are left to
/// the operator (`shion memory report` flags long-unused ones), so dreaming can
/// never silently retire something the user deliberately kept.
pub fn dream_verdict(memory: &Memory, now: i64) -> DreamVerdict {
    if memory.status != MemoryStatus::Candidate {
        return DreamVerdict::Keep;
    }
    let age_days = (now - memory.created_at).max(0) as f64 / 86_400.0;
    if memory.recall_count >= DREAM_MIN_RECALL_COUNT
        && memory.recall_query_hashes.len() >= DREAM_MIN_UNIQUE_QUERIES
        && dream_score(memory, now) >= DREAM_PROMOTE_MIN_SCORE
    {
        DreamVerdict::Promote
    } else if memory.recall_count == 0 && age_days as i64 > DREAM_FORGET_AGE_DAYS {
        DreamVerdict::Archive
    } else {
        DreamVerdict::Keep
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
    fn recall_terms_splits_ascii_words_and_cjk_bigrams() {
        let terms = recall_terms("Uses Rust 项目");
        assert!(terms.contains("uses"));
        assert!(terms.contains("rust"));
        assert!(terms.contains("项目")); // CJK bigram
    }

    #[test]
    fn recall_score_requires_term_overlap() {
        let now = 1_000;
        let m = Memory::new(MemoryKind::Project, "the project is written in Rust");
        // Overlapping term "rust" → scored.
        let hit = recall_terms("what language is the rust project in");
        assert!(recall_score(&m, &hit, now).is_some());
        // No overlap → excluded.
        let miss = recall_terms("当前天气如何");
        assert!(recall_score(&m, &miss, now).is_none());
        // Empty query → excluded.
        assert!(recall_score(&m, &HashSet::new(), now).is_none());
    }

    #[test]
    fn recall_score_orders_by_overlap_then_signals() {
        let now = 1_000;
        let mut more = Memory::new(MemoryKind::Fact, "rust async tokio runtime");
        more.updated_at = now;
        let mut fewer = Memory::new(MemoryKind::Fact, "rust crate");
        fewer.updated_at = now;
        let q = recall_terms("rust async tokio");
        let s_more = recall_score(&more, &q, now).unwrap();
        let s_fewer = recall_score(&fewer, &q, now).unwrap();
        assert!(s_more > s_fewer, "more overlapping terms must score higher");
    }

    fn candidate(recall_count: i64, age_days: i64, now: i64) -> Memory {
        let mut m = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned");
        m.status = MemoryStatus::Candidate;
        m.confidence = MemoryConfidence::Extracted;
        m.created_at = now - age_days * 86_400;
        m.recall_count = recall_count;
        if recall_count > 0 {
            m.last_used_at = Some(now - 86_400); // used yesterday
            // Recalled by as many distinct queries as recalls (the diverse,
            // promotion-friendly shape); diversity-gate tests override this.
            m.recall_query_hashes = (0..recall_count.min(RECALL_QUERY_HASHES_CAP as i64))
                .map(|i| format!("hash-{i}"))
                .collect();
        }
        m
    }

    #[test]
    fn dream_promotes_well_recalled_recent_candidate() {
        let now = 10_000 * 86_400;
        let m = candidate(DREAM_MIN_RECALL_COUNT, 5, now);
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);
    }

    #[test]
    fn dream_keeps_candidate_pumped_by_one_repeated_query() {
        let now = 10_000 * 86_400;
        // Plenty of recalls, but all from the same question: count alone must
        // not promote (the minUniqueQueries gate).
        let mut m = candidate(10, 5, now);
        m.recall_query_hashes = vec!["same-hash".into()];
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
        // Pre-upgrade rows (counts accumulated before fingerprints existed)
        // look the same — empty hashes — and stay put until diversity accrues.
        m.recall_query_hashes.clear();
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
        // A second distinct query unlocks promotion.
        m.recall_query_hashes = vec!["hash-a".into(), "hash-b".into()];
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Promote);
    }

    #[test]
    fn recall_query_hash_is_order_and_punctuation_invariant() {
        let a = recall_query_hash("rust project language");
        let b = recall_query_hash("Language — PROJECT... rust?!");
        assert_eq!(a.len(), 16);
        assert_eq!(a, b, "same terms, any order/punctuation → same fingerprint");
        let c = recall_query_hash("does shion support telegram");
        assert_ne!(a, c, "different substantive terms → different fingerprint");
        assert!(
            recall_query_hash("— …!").is_empty(),
            "no extractable terms → empty (never recorded)"
        );
    }

    #[test]
    fn dream_keeps_under_recalled_candidate() {
        let now = 10_000 * 86_400;
        // Two recalls — below the threshold of three — and still young.
        let m = candidate(2, 5, now);
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
    }

    #[test]
    fn dream_archives_old_never_recalled_candidate() {
        let now = 10_000 * 86_400;
        let m = candidate(0, DREAM_FORGET_AGE_DAYS + 1, now);
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Archive);
    }

    #[test]
    fn dream_keeps_young_never_recalled_candidate() {
        let now = 10_000 * 86_400;
        // Never recalled but still within the forget window — give it time.
        let m = candidate(0, DREAM_FORGET_AGE_DAYS - 1, now);
        assert_eq!(dream_verdict(&m, now), DreamVerdict::Keep);
    }

    #[test]
    fn dream_never_touches_active_memories() {
        let now = 10_000 * 86_400;
        // An active memory recalled a lot is still left alone (no auto-archive of
        // user-kept memories), and an old unused active is not archived either.
        let mut hot = candidate(99, 1, now);
        hot.status = MemoryStatus::Active;
        assert_eq!(dream_verdict(&hot, now), DreamVerdict::Keep);
        let mut cold = candidate(0, DREAM_FORGET_AGE_DAYS + 100, now);
        cold.status = MemoryStatus::Active;
        assert_eq!(dream_verdict(&cold, now), DreamVerdict::Keep);
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
