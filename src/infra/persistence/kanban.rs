//! The kanban store: durable cross-session tasks, in their **own** SQLite file
//! (`~/.shion/kanban.db`), separate from the session/message db (`shion.db`).
//!
//! Sessions, messages, and the session-scoped todo are disposable developer
//! state — `shion.db` is documented as deletable to reset, and a toasty schema
//! change forces deleting it. Kanban tasks are real personal data that must
//! survive that reset, so they live in a separate file with an independent
//! lifecycle. `KanbanDb` is the only place toasty appears for tasks (mirroring
//! `Db`'s role for everything else).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use toasty_driver_turso::Turso;
use tracing::info;

use crate::domain::task::{Task, TaskRepository, parse_task_status};
use crate::infra::persistence::{
    DEFAULT_POOL_SIZE, prepare_turso_path, sqlite_backup_path, turso_marker_path, with_write_retry,
};

// Optional i64 fields use 0 as the "unset" sentinel (same convention as `Db`).
#[derive(Debug, toasty::Model)]
struct TaskRecord {
    #[key]
    id: String,
    title: String,
    note: String,
    status: String, // "inbox" | "todo" | "waiting" | "done" | "cancelled"
    waiting_on: String,
    due_at: i64,
    source: String,
    source_message_id: String,
    board: String,
    due_notified_at: i64,
    created_at: i64,
    completed_at: i64,
}

/// Connection to the kanban database. Holds only `TaskRecord`; everything else
/// lives in `Db`.
///
/// Backed by the Turso engine with a per-operation connection pool: `inner` is a
/// plain `Arc<toasty::Db>` (no outer `Mutex`), every method checks out a pooled
/// `Connection`, and writes retry on an MVCC commit conflict. Tasks are durable
/// data, so a legacy SQLite file is migrated row-by-row (see `connect`).
pub struct KanbanDb {
    inner: Arc<toasty::Db>,
}

impl KanbanDb {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        // Durable tasks must survive the engine switch: `prepare_turso_path`
        // stages a legacy SQLite file aside (kept as `.sqlite-backup`), and its
        // rows are extracted and reloaded into a fresh Turso db below, guarded by
        // a `.turso` marker. Same shape as `MemoryDb::connect`.
        let (path, is_new) = prepare_turso_path(url)?;

        let driver = match &path {
            Some(p) => Turso::file(p).concurrent_writes(),
            None => Turso::in_memory().concurrent_writes(),
        };
        let db = toasty::Db::builder()
            .models(toasty::models!(TaskRecord))
            .max_pool_size(DEFAULT_POOL_SIZE)
            .build(driver)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        let me = Self {
            inner: Arc::new(db),
        };

        if let Some(p) = &path {
            let pending = sqlite_backup_path(p);
            let marker = turso_marker_path(p);
            if pending.exists() && !marker.exists() {
                let rows = extract_sqlite_rows(&pending).await?;
                let count = rows.len();
                for task in &rows {
                    me.save(task).await?;
                }
                std::fs::write(&marker, b"migrated from sqlite\n").ok();
                info!(count, backup = %pending.display(), "migrated kanban.db sqlite → turso");
            } else if is_new {
                std::fs::write(&marker, b"turso-native\n").ok();
            }
        }

        Ok(me)
    }
}

/// Read every task row from a legacy SQLite db file (toasty's SQLite driver),
/// faithfully — including closed tasks — so the migration preserves the full
/// board, not just the open subset.
async fn extract_sqlite_rows(backup: &Path) -> anyhow::Result<Vec<Task>> {
    let url = format!("sqlite:{}", backup.display());
    let db = toasty::Db::builder()
        .models(toasty::models!(TaskRecord))
        .connect(&url)
        .await
        .with_context(|| format!("opening legacy sqlite db at {}", backup.display()))?;
    let mut conn = db.connection().await?;
    let rows = toasty::query!(TaskRecord).exec(&mut conn).await?;
    rows.into_iter().map(task_from_record).collect()
    // `db` drops here, releasing the backup file.
}

#[async_trait]
impl TaskRepository for KanbanDb {
    async fn save(&self, task: &Task) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            toasty::create!(TaskRecord {
                id: task.id.clone(),
                title: task.title.clone(),
                note: task.note.clone(),
                status: task.status.as_str().to_string(),
                waiting_on: task.waiting_on.clone(),
                due_at: task.due_at.unwrap_or(0),
                source: task.source.clone(),
                source_message_id: task.source_message_id.clone(),
                board: task.board.clone(),
                due_notified_at: task.due_notified_at.unwrap_or(0),
                created_at: task.created_at,
                completed_at: task.completed_at.unwrap_or(0),
            })
            .exec(&mut conn)
            .await?;
            Ok(())
        })
        .await
    }

    async fn find(&self, id: &str) -> anyhow::Result<Option<Task>> {
        let mut conn = self.inner.connection().await?;
        match TaskRecord::get_by_id(&mut conn, id).await {
            Ok(record) => Ok(Some(task_from_record(record)?)),
            Err(_) => Ok(None),
        }
    }

    async fn list_open(&self) -> anyhow::Result<Vec<Task>> {
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(TaskRecord).exec(&mut conn).await?;
        let mut open: Vec<Task> = rows
            .into_iter()
            .map(task_from_record)
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .filter(|t| t.status.is_open())
            .collect();
        open.sort_by_key(|t| t.created_at);
        Ok(open)
    }

    async fn update(&self, task: &Task) -> anyhow::Result<()> {
        with_write_retry(|| async {
            let mut conn = self.inner.connection().await?;
            let mut record = TaskRecord::get_by_id(&mut conn, &task.id).await?;
            record
                .update()
                .title(task.title.clone())
                .note(task.note.clone())
                .status(task.status.as_str().to_string())
                .waiting_on(task.waiting_on.clone())
                .due_at(task.due_at.unwrap_or(0))
                .board(task.board.clone())
                .due_notified_at(task.due_notified_at.unwrap_or(0))
                .completed_at(task.completed_at.unwrap_or(0))
                .exec(&mut conn)
                .await?;
            Ok(())
        })
        .await
    }

    async fn find_by_source_message_id(
        &self,
        source: &str,
        source_message_id: &str,
    ) -> anyhow::Result<Option<Task>> {
        // Dedup keys are only set on automated captures, so an empty key can
        // never match a real extraction — bail before scanning.
        if source_message_id.is_empty() {
            return Ok(None);
        }
        let mut conn = self.inner.connection().await?;
        let rows = toasty::query!(TaskRecord).exec(&mut conn).await?;
        for record in rows {
            if record.source == source && record.source_message_id == source_message_id {
                return Ok(Some(task_from_record(record)?));
            }
        }
        Ok(None)
    }
}

fn task_from_record(record: TaskRecord) -> anyhow::Result<Task> {
    let nonzero = |v: i64| (v != 0).then_some(v);
    Ok(Task {
        id: record.id,
        title: record.title,
        note: record.note,
        status: parse_task_status(&record.status)?,
        waiting_on: record.waiting_on,
        due_at: nonzero(record.due_at),
        source: record.source,
        source_message_id: record.source_message_id,
        board: record.board,
        due_notified_at: nonzero(record.due_notified_at),
        created_at: record.created_at,
        completed_at: nonzero(record.completed_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::TaskStatus;

    fn sqlite_url(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        crate::infra::persistence::reset_test_db(&path);
        format!("turso:{}", path.display())
    }

    #[tokio::test]
    async fn task_roundtrip_and_update() {
        let db = KanbanDb::connect(&sqlite_url("shion_kanban_repo_test.db"))
            .await
            .unwrap();
        let mut task = Task::new("send weekly report".to_string());
        task.due_at = Some(9999999999);
        task.waiting_on = "boss".to_string();
        task.board = "work".to_string();

        db.save(&task).await.unwrap();
        let open = db.list_open().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].title, "send weekly report");
        assert_eq!(open[0].status, TaskStatus::Inbox);
        assert_eq!(open[0].due_at, Some(9999999999));
        assert_eq!(open[0].waiting_on, "boss");
        assert_eq!(open[0].board, "work");
        assert_eq!(open[0].due_notified_at, None);

        let mut updated = open[0].clone();
        updated.status = TaskStatus::Done;
        updated.completed_at = Some(123);
        db.update(&updated).await.unwrap();

        assert!(db.list_open().await.unwrap().is_empty());
        let found = db.find(&task.id).await.unwrap().unwrap();
        assert_eq!(found.status, TaskStatus::Done);
        assert_eq!(found.completed_at, Some(123));
    }

    #[tokio::test]
    async fn find_returns_none_for_unknown_id() {
        let db = KanbanDb::connect(&sqlite_url("shion_kanban_find_test.db"))
            .await
            .unwrap();
        assert!(db.find("task-nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_source_message_id_matches_source_and_key() {
        let db = KanbanDb::connect(&sqlite_url("shion_kanban_dedup_test.db"))
            .await
            .unwrap();
        let mut task = Task::new("call Bob".to_string());
        task.source = "telegram:1".to_string();
        task.source_message_id = "commit-abc".to_string();
        db.save(&task).await.unwrap();

        // Match on source + key.
        assert!(
            db.find_by_source_message_id("telegram:1", "commit-abc")
                .await
                .unwrap()
                .is_some()
        );
        // Same key, different source → no match.
        assert!(
            db.find_by_source_message_id("telegram:2", "commit-abc")
                .await
                .unwrap()
                .is_none()
        );
        // Empty key never matches.
        assert!(
            db.find_by_source_message_id("telegram:1", "")
                .await
                .unwrap()
                .is_none()
        );
    }
}
