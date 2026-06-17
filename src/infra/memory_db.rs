//! The memory store: durable long-term memories in their **own** SQLite file
//! (`~/.shion/memory.db`), separate from the disposable session db (`shion.db`)
//! and the task db (`kanban.db`).
//!
//! Memories are real personal data that must survive a `shion.db` reset, so —
//! like `KanbanDb` — they live in an independent file. `MemoryDb` is the only
//! place toasty appears for memories. Markdown (`infra/md_memory.rs`) is kept
//! as an import/export format, not the canonical backend.
//!
//! Schema is laid out **schema-first**: governance/scope/usage columns land all
//! at once even before every consumer exists, because toasty's `push_schema`
//! is not idempotent (a column change means deleting the file). See
//! `docs/memory-injection-plan.md`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::domain::memory::{
    Memory, MemoryRepository, MemoryScope, parse_memory_confidence, parse_memory_kind,
    parse_memory_status,
};
use crate::infra::md_memory::MdMemoryStore;

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
}

/// Connection to the memory database. Holds only `MemoryRecord`.
pub struct MemoryDb {
    inner: Arc<Mutex<toasty::Db>>,
}

impl MemoryDb {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let is_new = url
            .strip_prefix("sqlite:")
            .map(|path| !Path::new(path).exists())
            .unwrap_or(true);

        let db = toasty::Db::builder()
            .models(toasty::models!(MemoryRecord))
            .connect(url)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(db)),
        })
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
    }
}

#[async_trait]
impl MemoryRepository for MemoryDb {
    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        let r = record_from_memory(memory);
        // Overwrite on id collision (save is create-or-replace), mirroring the
        // markdown store's filename-keyed overwrite.
        if let Ok(mut existing) = MemoryRecord::get_by_id(&mut *db, &r.id).await {
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
                .exec(&mut *db)
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
        })
        .exec(&mut *db)
        .await?;
        Ok(())
    }

    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut db = self.inner.lock().await;
        let rows = toasty::query!(MemoryRecord).exec(&mut *db).await?;
        let mut memories: Vec<Memory> = rows
            .into_iter()
            .map(memory_from_record)
            .filter(|m| !m.is_expired(now))
            .collect();
        memories.sort_by_key(|m| m.created_at);
        Ok(memories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryConfidence, MemoryContext, MemoryKind, MemoryStatus};

    fn sqlite_url(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        format!("sqlite:{}", path.display())
    }

    #[tokio::test]
    async fn save_list_roundtrip_and_overwrite() {
        let db = MemoryDb::connect(&sqlite_url("shion_memory_db_roundtrip.db"))
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
        let db = MemoryDb::connect(&sqlite_url("shion_memory_db_expired.db"))
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
        let db = MemoryDb::connect(&sqlite_url("shion_memory_db_pinned.db"))
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
    async fn import_legacy_seeds_empty_db_only_once() {
        let dir = std::env::temp_dir().join("shion_memory_db_import_src");
        let _ = std::fs::remove_dir_all(&dir);
        let legacy = MdMemoryStore::new(dir.clone());
        legacy
            .save(&Memory::new(MemoryKind::Project, "uses Rust"))
            .await
            .unwrap();

        let db = MemoryDb::connect(&sqlite_url("shion_memory_db_import.db"))
            .await
            .unwrap();
        assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 1);
        assert_eq!(db.list().await.unwrap().len(), 1);
        // Second call is a no-op (db non-empty).
        assert_eq!(db.import_legacy_markdown(&dir).await.unwrap(), 0);
        assert_eq!(db.list().await.unwrap().len(), 1);
    }
}
