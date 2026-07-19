//! The memory store: durable long-term memories in their **own** SQLite file
//! (`~/.komo/memory.db`), separate from the disposable session db (`state.db`)
//! and the task db (`kanban.db`).
//!
//! Memories are real personal data that must survive a `state.db` reset, so —
//! like `KanbanDb` — they live in an independent file. `MemoryDb` is the only
//! place toasty appears for memories. Markdown (`infra/md_memory.rs`) is kept
//! as an import/export format, not the canonical backend.
//!
//! Schema is laid out **schema-first**: governance/scope/usage columns land all
//! at once even before every consumer exists, because toasty's `push_schema`
//! is not idempotent (a column change means deleting the file). See
//! `docs/personal-agent-roadmap.md`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use toasty_driver_turso::Turso;
use tracing::info;

use crate::domain::memory::{
    Memory, MemoryRepository, MemoryScope, parse_memory_confidence, parse_memory_kind,
    parse_memory_status,
};
use crate::infra::memory::md_memory::MdMemoryStore;
use crate::infra::persistence::{
    DEFAULT_POOL_SIZE, prepare_turso_path, sqlite_backup_path, turso_marker_path, with_write_retry,
};

// Optional i64 fields use 0 as the "unset" sentinel (same convention as `Db`).
#[derive(Debug, toasty::Model)]
struct MemoryRecord {
    #[key]
    id: String,
    kind: String,
    content: String,
    status: String,
    confidence: String,
    importance: i64,
    pinned: bool,
    scope_type: String,
    scope_key: String,
    source: String,
    source_message_id: String,
    created_at: i64,
    updated_at: i64,
    expires_at: i64,
    last_used_at: i64,
    recall_count: i64,
    // Comma-separated distinct query fingerprints (hex, so commas never occur
    // inside a value); domain type is `Vec<String>`.
    recall_query_hashes: String,
}

/// Connection to the memory database. Holds only `MemoryRecord`.
///
/// Backed by the Turso engine with a per-operation connection pool: `inner` is a
/// plain `Arc<toasty::Db>` (no outer `Mutex`), and every method checks out a
/// pooled `Connection`, so independent reads/writes run concurrently. Writes use
/// Turso's MVCC concurrent-write mode and retry on commit conflict (see
/// `infra::persistence::with_write_retry`).
pub struct MemoryDb {
    inner: Arc<toasty::Db>,
}

impl MemoryDb {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        // `url` is `turso:<path>`. Durable data: memories must survive the engine
        // switch. `prepare_turso_path` stages a file written by the old
        // rusqlite/SQLite backend aside to `.sqlite-backup`; its rows are
        // extracted and reloaded into a fresh Turso db after the schema is pushed
        // (below), guarded by a `.turso` marker so we never re-migrate. In-memory
        // (no path) skips migration entirely.
        let (path, is_new) = prepare_turso_path(url)?;

        // Additive in-place migration for an EXISTING db: toasty's `push_schema`
        // is not idempotent and only runs for new files, so a column added to
        // `MemoryRecord` after the file was created would otherwise be missing
        // (every query referencing it would fail). Rather than force a destructive
        // "delete memory.db" reset — these are durable personal memories — we run
        // the one DDL toasty's typed API can't: `ALTER TABLE ADD COLUMN` with a
        // default, directly against the Turso file, before toasty opens it. The
        // turso handle is dropped here so it never contends with toasty's pool.
        if !is_new && let Some(p) = &path {
            ensure_columns(p).await?;
        }

        // MVCC concurrent-writes on: writers run in parallel and conflicting
        // commits are retried (see `with_write_retry`). komo's keys are all
        // UUIDs (no AUTOINCREMENT, which MVCC rejects), so this is uniform across
        // every db.
        let driver = match &path {
            Some(p) => Turso::file(p).concurrent_writes(),
            None => Turso::in_memory().concurrent_writes(),
        };
        let db = toasty::Db::builder()
            .models(toasty::models!(MemoryRecord))
            .max_pool_size(DEFAULT_POOL_SIZE)
            .build(driver)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        let me = Self {
            inner: Arc::new(db),
        };

        // Load the rows extracted from a legacy SQLite file (if any) into the
        // fresh Turso db, then drop the marker so this only ever happens once.
        if let Some(p) = &path {
            let pending = sqlite_backup_path(p);
            let marker = turso_marker_path(p);
            if pending.exists() && !marker.exists() {
                let rows = extract_sqlite_rows(&pending).await?;
                let count = rows.len();
                for memory in &rows {
                    me.save(memory).await?;
                }
                std::fs::write(&marker, b"migrated from sqlite\n").ok();
                info!(count, backup = %pending.display(), "migrated memory.db sqlite → turso");
            } else if is_new {
                // Brand-new Turso db with no legacy file: still mark it
                // Turso-native so a future run never mistakes it for SQLite.
                std::fs::write(&marker, b"turso-native\n").ok();
            }
        }

        Ok(me)
    }

    /// One-time migration: import every memory from a legacy markdown directory
    /// into a freshly-created db. No-op when the directory is absent or the db
    /// already holds memories (so it is safe to call on every startup). Returns
    /// the number imported.
    pub async fn import_legacy_markdown(&self, dir: &Path) -> anyhow::Result<usize> {
        // Only seed an empty db — never double-import or fight live writes.
        if !self.list().await?.is_empty() {
            return Ok(0);
        }
        let legacy = MdMemoryStore::new(dir.to_path_buf());
        let memories = legacy.read_all().await?;
        let count = memories.len();
        for memory in &memories {
            self.save(memory).await?;
        }
        Ok(count)
    }
}

fn record_from_memory(memory: &Memory) -> MemoryRecord {
    MemoryRecord {
        id: memory.id.clone(),
        kind: memory.kind.as_str().to_string(),
        content: memory.content.clone(),
        status: memory.status.as_str().to_string(),
        confidence: memory.confidence.as_str().to_string(),
        importance: memory.importance as i64,
        pinned: memory.pinned,
        scope_type: memory.scope.type_str().to_string(),
        scope_key: memory.scope.key(),
        source: memory.source.clone(),
        source_message_id: memory.source_message_id.clone(),
        created_at: memory.created_at,
        updated_at: memory.updated_at,
        expires_at: memory.expires_at.unwrap_or(0),
        last_used_at: memory.last_used_at.unwrap_or(0),
        recall_count: memory.recall_count,
        recall_query_hashes: memory.recall_query_hashes.join(","),
    }
}

fn memory_from_record(record: MemoryRecord) -> Memory {
    let nonzero = |v: i64| (v != 0).then_some(v);
    Memory {
        id: record.id,
        kind: parse_memory_kind(&record.kind),
        content: record.content,
        status: parse_memory_status(&record.status),
        confidence: parse_memory_confidence(&record.confidence),
        importance: record.importance as i32,
        pinned: record.pinned,
        scope: MemoryScope::from_parts(&record.scope_type, &record.scope_key),
        source: record.source,
        source_message_id: record.source_message_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
        expires_at: nonzero(record.expires_at),
        last_used_at: nonzero(record.last_used_at),
        recall_count: record.recall_count,
        recall_query_hashes: record
            .recall_query_hashes
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

#[async_trait]
impl MemoryRepository for MemoryDb {
    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        // MVCC: retry the whole transaction on a commit conflict. Each attempt
        // re-checks out its own pooled connection.
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let r = record_from_memory(memory);
            // Overwrite on id collision (save is create-or-replace), mirroring
            // the markdown store's filename-keyed overwrite.
            if let Ok(mut existing) = MemoryRecord::get_by_id(&mut conn, &r.id).await {
                existing
                    .update()
                    .kind(r.kind)
                    .content(r.content)
                    .status(r.status)
                    .confidence(r.confidence)
                    .importance(r.importance)
                    .pinned(r.pinned)
                    .scope_type(r.scope_type)
                    .scope_key(r.scope_key)
                    .source(r.source)
                    .source_message_id(r.source_message_id)
                    .updated_at(r.updated_at)
                    .expires_at(r.expires_at)
                    .last_used_at(r.last_used_at)
                    .recall_count(r.recall_count)
                    .recall_query_hashes(r.recall_query_hashes)
                    .exec(&mut conn)
                    .await?;
                return Ok(());
            }
            toasty::create!(MemoryRecord {
                id: r.id,
                kind: r.kind,
                content: r.content,
                status: r.status,
                confidence: r.confidence,
                importance: r.importance,
                pinned: r.pinned,
                scope_type: r.scope_type,
                scope_key: r.scope_key,
                source: r.source,
                source_message_id: r.source_message_id,
                created_at: r.created_at,
                updated_at: r.updated_at,
                expires_at: r.expires_at,
                last_used_at: r.last_used_at,
                recall_count: r.recall_count,
                recall_query_hashes: r.recall_query_hashes,
            })
            .exec(&mut conn)
            .await?;
            Ok(())
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(MemoryRecord).exec(&mut conn).await?;
        let mut memories: Vec<Memory> = rows
            .into_iter()
            .map(memory_from_record)
            .filter(|m| !m.is_expired(now))
            .collect();
        memories.sort_by_key(|m| m.created_at);
        Ok(memories)
    }

    /// Fetch by id directly — unlike the default (which scans `list`), this
    /// sees expired and any-status rows, so governance can still operate on
    /// them.
    async fn get(&self, id: &str) -> anyhow::Result<Option<Memory>> {
        let mut conn = self.inner.connection().await?;
        Ok(MemoryRecord::get_by_id(&mut conn, id)
            .await
            .ok()
            .map(memory_from_record))
    }
}

/// Bring an existing `memory_records` table up to the current `MemoryRecord`
/// shape by adding any columns it lacks, in place (no data loss, idempotent) —
/// the shared additive migration in `infra/persistence/mod.rs`. When adding a
/// `MemoryRecord` field, extend this list (NOT NULL with a DEFAULT, or nullable).
async fn ensure_columns(path: &Path) -> anyhow::Result<()> {
    const EXPECTED: &[(&str, &str)] = &[
        (
            "recall_count",
            "\"recall_count\" integer NOT NULL DEFAULT 0",
        ),
        (
            "recall_query_hashes",
            "\"recall_query_hashes\" text NOT NULL DEFAULT ''",
        ),
    ];
    crate::infra::persistence::ensure_columns(path, "memory_records", EXPECTED).await
}

/// Read every memory row from a legacy SQLite db file (opened with toasty's
/// SQLite driver), faithfully — including expired/any-status rows — so the
/// migration preserves the full store, not just what `list` would surface.
async fn extract_sqlite_rows(backup: &Path) -> anyhow::Result<Vec<Memory>> {
    let url = format!("sqlite:{}", backup.display());
    let db = toasty::Db::builder()
        .models(toasty::models!(MemoryRecord))
        .connect(&url)
        .await
        .with_context(|| format!("opening legacy sqlite db at {}", backup.display()))?;
    let mut conn = db.connection().await?;
    let rows = toasty::query!(MemoryRecord).exec(&mut conn).await?;
    Ok(rows.into_iter().map(memory_from_record).collect())
    // `db` drops here, releasing the backup file.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryConfidence, MemoryContext, MemoryKind, MemoryStatus};

    fn turso_url(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        crate::infra::persistence::reset_test_db(&path);
        format!("turso:{}", path.display())
    }

    /// A legacy SQLite `memory.db` written by the old rusqlite backend (same
    /// `MemoryRecord` schema) must be migrated into Turso on first connect,
    /// preserving its rows, leaving a `.sqlite-backup`, and never re-migrating.
    #[tokio::test]
    async fn migrates_legacy_sqlite_file_into_turso() {
        let path = std::env::temp_dir().join("komo_memory_db_migrate.db");
        crate::infra::persistence::reset_test_db(&path);

        // 1. Seed a legacy SQLite file with two memories via the SQLite driver.
        {
            let sdb = toasty::Db::builder()
                .models(toasty::models!(MemoryRecord))
                .connect(&format!("sqlite:{}", path.display()))
                .await
                .unwrap();
            sdb.push_schema().await.unwrap();
            let mut conn = sdb.connection().await.unwrap();
            for r in [
                record_from_memory(&Memory::new(MemoryKind::Project, "written in Rust")),
                record_from_memory(&Memory::new(MemoryKind::Fact, "likes coffee")),
            ] {
                toasty::create!(MemoryRecord {
                    id: r.id,
                    kind: r.kind,
                    content: r.content,
                    status: r.status,
                    confidence: r.confidence,
                    importance: r.importance,
                    pinned: r.pinned,
                    scope_type: r.scope_type,
                    scope_key: r.scope_key,
                    source: r.source,
                    source_message_id: r.source_message_id,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    expires_at: r.expires_at,
                    last_used_at: r.last_used_at,
                    recall_count: r.recall_count,
                    recall_query_hashes: r.recall_query_hashes,
                })
                .exec(&mut conn)
                .await
                .unwrap();
            }
        }

        // 2. Connect over Turso: the rows migrate, backup + marker appear.
        let db = MemoryDb::connect(&format!("turso:{}", path.display()))
            .await
            .unwrap();
        let mut contents: Vec<String> = db
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        contents.sort();
        assert_eq!(contents, vec!["likes coffee", "written in Rust"]);
        assert!(sqlite_backup_path(&path).exists(), "sqlite backup kept");
        assert!(turso_marker_path(&path).exists(), "turso marker written");

        // 3. Add a row, reconnect: no re-migration (still 3, not 5).
        db.save(&Memory::new(MemoryKind::Fact, "third"))
            .await
            .unwrap();
        drop(db);
        let db2 = MemoryDb::connect(&format!("turso:{}", path.display()))
            .await
            .unwrap();
        assert_eq!(db2.list().await.unwrap().len(), 3, "must not re-migrate");
    }

    /// An existing memory.db created before `recall_count` /
    /// `recall_query_hashes` existed must gain the columns **in place** on
    /// connect — additive ALTER, no data loss — rather than force a
    /// destructive reset.
    #[tokio::test]
    async fn adds_missing_recall_columns_in_place() {
        let path = std::env::temp_dir().join("komo_memory_db_addcol.db");
        crate::infra::persistence::reset_test_db(&path);

        // 1. Seed a turso file with the OLD 15-column schema (no recall_count)
        //    and one row, then drop the handle.
        {
            let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            conn.pragma_update("journal_mode", "'mvcc'").await.ok();
            conn.execute(
                "CREATE TABLE \"memory_records\" (\
                 \"id\" TEXT NOT NULL, \"kind\" TEXT NOT NULL, \"content\" TEXT NOT NULL, \
                 \"status\" TEXT NOT NULL, \"confidence\" TEXT NOT NULL, \"importance\" BIGINT NOT NULL, \
                 \"pinned\" BOOLEAN NOT NULL, \"scope_type\" TEXT NOT NULL, \"scope_key\" TEXT NOT NULL, \
                 \"source\" TEXT NOT NULL, \"source_message_id\" TEXT NOT NULL, \"created_at\" BIGINT NOT NULL, \
                 \"updated_at\" BIGINT NOT NULL, \"expires_at\" BIGINT NOT NULL, \"last_used_at\" BIGINT NOT NULL, \
                 PRIMARY KEY (\"id\"))",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO \"memory_records\" VALUES \
                 ('mem-old', 'fact', 'a pre-migration memory', 'active', 'confirmed', 50, 0, \
                 'global', '', '', '', 100, 100, 0, 0)",
                (),
            )
            .await
            .unwrap();
        }
        // Mark it turso-native so connect() does not stage it as a sqlite backup.
        std::fs::write(turso_marker_path(&path), b"turso-native\n").unwrap();

        // 2. Connect via MemoryDb: ensure_columns adds both columns in place.
        let db = MemoryDb::connect(&format!("turso:{}", path.display()))
            .await
            .unwrap();
        let rows = db.list().await.unwrap();
        assert_eq!(rows.len(), 1, "the pre-migration row survives");
        assert_eq!(rows[0].content, "a pre-migration memory");
        assert_eq!(rows[0].recall_count, 0, "new column defaults to 0");
        assert!(rows[0].recall_query_hashes.is_empty(), "defaults to empty");

        // 3. The added columns are fully usable: a recall bump persists.
        db.mark_used(&[rows[0].id.clone()], 9_000, "q-hash")
            .await
            .unwrap();
        let after = db.get("mem-old").await.unwrap().unwrap();
        assert_eq!(after.recall_count, 1);
        assert_eq!(after.recall_query_hashes, vec!["q-hash".to_string()]);
    }

    #[tokio::test]
    async fn save_list_roundtrip_and_overwrite() {
        let db = MemoryDb::connect(&turso_url("komo_memory_db_roundtrip.db"))
            .await
            .unwrap();
        let mut m = Memory::new(MemoryKind::Preference, "prefers concise answers");
        m.pinned = true;
        m.confidence = MemoryConfidence::UserWritten;
        m.scope = MemoryScope::Channel {
            platform: "telegram".into(),
            chat_id: "42".into(),
        };
        db.save(&m).await.unwrap();

        let rows = db.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "prefers concise answers");
        assert!(rows[0].pinned);
        assert_eq!(rows[0].confidence, MemoryConfidence::UserWritten);

        // Overwrite same id.
        let mut updated = m.clone();
        updated.content = "prefers terse answers".into();
        db.save(&updated).await.unwrap();
        let rows = db.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "prefers terse answers");
    }

    #[tokio::test]
    async fn expired_hidden_from_list() {
        let db = MemoryDb::connect(&turso_url("komo_memory_db_expired.db"))
            .await
            .unwrap();
        db.save(&Memory::new(MemoryKind::Fact, "live"))
            .await
            .unwrap();
        let mut stale = Memory::new(MemoryKind::Fact, "stale");
        stale.expires_at = Some(1);
        db.save(&stale).await.unwrap();

        let rows = db.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "live");
    }

    #[tokio::test]
    async fn pinned_filters_by_eligibility_and_scope() {
        let db = MemoryDb::connect(&turso_url("komo_memory_db_pinned.db"))
            .await
            .unwrap();

        // Eligible: pinned, active, user_written, preference, global.
        let mut good = Memory::new(MemoryKind::Preference, "concise answers");
        good.pinned = true;
        good.confidence = MemoryConfidence::UserWritten;
        db.save(&good).await.unwrap();

        // Not pinned.
        db.save(&Memory::new(MemoryKind::Preference, "not pinned"))
            .await
            .unwrap();

        // Pinned but candidate → excluded.
        let mut cand = Memory::new(MemoryKind::Profile, "candidate");
        cand.pinned = true;
        cand.confidence = MemoryConfidence::UserWritten;
        cand.status = MemoryStatus::Candidate;
        db.save(&cand).await.unwrap();

        let ctx = MemoryContext::from_session("cli");
        let pinned = db.pinned(&ctx).await.unwrap();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].content, "concise answers");
    }

    #[tokio::test]
    async fn recall_returns_in_scope_active_and_candidate_matches() {
        let db = MemoryDb::connect(&turso_url("komo_memory_db_recall.db"))
            .await
            .unwrap();

        // Relevant, active, global → recalled.
        db.save(&Memory::new(
            MemoryKind::Project,
            "the komo project is written in Rust",
        ))
        .await
        .unwrap();
        // Irrelevant → excluded by term overlap.
        db.save(&Memory::new(MemoryKind::Fact, "the user likes coffee"))
            .await
            .unwrap();
        // Relevant candidate → INCLUDED (so it can earn its recall signal for
        // the dreaming loop), though it ranks below the active hit.
        let mut cand = Memory::new(MemoryKind::Fact, "the rust toolchain is pinned to nightly");
        cand.status = MemoryStatus::Candidate;
        db.save(&cand).await.unwrap();
        // Relevant but rejected → excluded by status.
        let mut rejected = Memory::new(MemoryKind::Fact, "rust borrow checker notes");
        rejected.status = MemoryStatus::Rejected;
        db.save(&rejected).await.unwrap();
        // Relevant but scoped to another channel → excluded by scope.
        let mut other = Memory::new(MemoryKind::Fact, "rust edition is 2021");
        other.scope = MemoryScope::Channel {
            platform: "feishu".into(),
            chat_id: "oc_other".into(),
        };
        db.save(&other).await.unwrap();

        let ctx = MemoryContext::from_session("cli");
        let hits = db
            .recall(&ctx, "what language is the rust project in", 5)
            .await
            .unwrap();
        // Active + candidate both recalled; rejected and out-of-scope excluded.
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|h| h.memory.content.contains("written in Rust"))
        );
        assert!(
            hits.iter()
                .any(|h| h.memory.status == MemoryStatus::Candidate)
        );
        assert!(
            !hits
                .iter()
                .any(|h| h.memory.status == MemoryStatus::Rejected)
        );
    }

    #[tokio::test]
    async fn mark_used_sets_last_used_without_touching_updated_at() {
        let db = MemoryDb::connect(&turso_url("komo_memory_db_mark_used.db"))
            .await
            .unwrap();
        let mut m = Memory::new(MemoryKind::Fact, "recalled at least once");
        m.updated_at = 500;
        db.save(&m).await.unwrap();

        db.mark_used(&[m.id.clone()], 9_000, "hash-a")
            .await
            .unwrap();
        db.mark_used(&[m.id.clone()], 9_100, "hash-a")
            .await
            .unwrap();
        db.mark_used(&[m.id.clone()], 9_200, "hash-b")
            .await
            .unwrap();
        db.mark_used(&[m.id.clone()], 9_300, "").await.unwrap();

        let after = db.get(&m.id).await.unwrap().unwrap();
        assert_eq!(after.last_used_at, Some(9_300));
        assert_eq!(after.recall_count, 4, "each recall bumps the count");
        assert_eq!(after.updated_at, 500, "recall must not bump updated_at");
        assert_eq!(
            after.recall_query_hashes,
            vec!["hash-a".to_string(), "hash-b".to_string()],
            "fingerprints dedup; an empty hash is never recorded"
        );
    }

    #[tokio::test]
    async fn import_legacy_seeds_empty_db_only_once() {
        let dir = std::env::temp_dir().join("komo_memory_db_import_src");
        let _ = std::fs::remove_dir_all(&dir);
        let legacy = MdMemoryStore::new(dir.clone());
        legacy
            .save(&Memory::new(MemoryKind::Project, "uses Rust"))
            .await
            .unwrap();

        let db = MemoryDb::connect(&turso_url("komo_memory_db_import.db"))
            .await
            .unwrap();
        assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 1);
        assert_eq!(db.list().await.unwrap().len(), 1);
        // Second call is a no-op (db non-empty).
        assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 0);
        assert_eq!(db.list().await.unwrap().len(), 1);
    }
}
