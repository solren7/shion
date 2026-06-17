use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::info;

use crate::domain::{
    home::HomeRepository,
    message::{Message, Role},
    pairing::{
        APPROVE_LOCKOUT_SECS, APPROVE_MAX_FAILURES, ApproveOutcome, PAIRING_CODE_TTL_SECS,
        PairingRepository, PairingRequest, PairingStatus, parse_pairing_status, verify_code,
    },
    reminder::{Reminder, ReminderRepository, ReminderStatus, parse_reminder_status},
    repository::{MessageRepository, SessionRepository, SkillRepository},
    session::Session,
    skill::Skill,
    todo::{SessionTodoRepository, TodoItem},
};

// ── toasty models (infra-internal) ───────────────────────────────────────────

#[derive(Debug, toasty::Model)]
struct SessionRecord {
    #[key]
    id: String,
    created_at: i64,

    #[has_many]
    messages: toasty::Deferred<Vec<MessageRecord>>,
}

#[derive(Debug, toasty::Model)]
struct MessageRecord {
    #[key]
    #[auto]
    id: u64,

    #[index]
    session_id: String,

    #[belongs_to(key = session_id, references = id)]
    session_record: toasty::Deferred<SessionRecord>,

    role: String,
    content: String,
    timestamp: i64,
}

#[derive(Debug, toasty::Model)]
struct SkillRecord {
    #[key]
    name: String,
    description: String,
    instructions: String,
    protected: bool,
}

#[derive(Debug, toasty::Model)]
struct ReminderRecord {
    #[key]
    id: String,
    message: String,
    run_at: i64,
    status: String,   // "pending" | "fired" | "missed" | "cancelled"
    schedule: String, // reserved for v2 cron expressions; always "" in v1
    created_at: i64,
}

/// Session-scoped working todo list (`domain/todo.rs`). One row per session;
/// `items` is the JSON-serialized `Vec<TodoItem>`. Disposable working state —
/// cleared on `/new` rotate.
#[derive(Debug, toasty::Model)]
struct SessionTodoRecord {
    #[key]
    session_id: String,
    items: String, // JSON array of TodoItem
    updated_at: i64,
}

#[derive(Debug, toasty::Model)]
struct PairingRecord {
    /// One row per sender: `{platform}:{sender_id}`.
    #[key]
    id: String,
    platform: String,
    sender_id: String,
    chat_id: String,
    code_hash: String, // salted SHA-256 of the code; plaintext never stored
    salt: String,
    status: String, // "pending" | "approved"
    created_at: i64,
}

/// Failure-lockout counter for the `shion pair approve` path. A singleton row
/// (`id = "approve"`); mirrors hermes' per-platform approve lockout.
#[derive(Debug, toasty::Model)]
struct LockoutRecord {
    #[key]
    id: String,
    failed_count: i64,
    locked_until: i64,
}

/// Generic key/value settings. One row per setting (`id` is the key); the home
/// channel set via `/sethome` lives under `id = "home_chat"`.
#[derive(Debug, toasty::Model)]
struct SettingRecord {
    #[key]
    id: String,
    value: String,
}

/// Setting key for the runtime home channel (`/sethome`).
const HOME_SETTING_KEY: &str = "home_chat";

// ── Db ───────────────────────────────────────────────────────────────────────

pub struct Db {
    inner: Arc<Mutex<toasty::Db>>,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let is_new = url
            .strip_prefix("sqlite:")
            .map(|path| !std::path::Path::new(path).exists())
            .unwrap_or(true);

        let db = toasty::Db::builder()
            .models(toasty::models!(
                SessionRecord,
                MessageRecord,
                SkillRecord,
                ReminderRecord,
                SessionTodoRecord,
                PairingRecord,
                LockoutRecord,
                SettingRecord
            ))
            .connect(url)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(db)),
        })
    }
}

// ── SkillRepository ───────────────────────────────────────────────────────────

#[async_trait]
impl SkillRepository for Db {
    async fn find(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        let mut db = self.inner.lock().await;
        match SkillRecord::get_by_name(&mut *db, name).await {
            Ok(record) => Ok(Some(skill_from_record(record))),
            Err(_) => Ok(None),
        }
    }

    async fn list(&self) -> anyhow::Result<Vec<Skill>> {
        let mut db = self.inner.lock().await;
        let mut rows = toasty::query!(SkillRecord).exec(&mut *db).await?;
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows.into_iter().map(skill_from_record).collect())
    }

    async fn save(&self, skill: &Skill) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        match SkillRecord::get_by_name(&mut *db, &skill.name).await {
            Ok(mut record) => {
                record
                    .update()
                    .description(skill.description.clone())
                    .instructions(skill.instructions.clone())
                    .protected(skill.protected)
                    .exec(&mut *db)
                    .await?;
            }
            Err(_) => {
                toasty::create!(SkillRecord {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    instructions: skill.instructions.clone(),
                    protected: skill.protected,
                })
                .exec(&mut *db)
                .await?;
            }
        }
        Ok(())
    }
}

// ── SessionRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl SessionRepository for Db {
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let mut db = self.inner.lock().await;
        match SessionRecord::get_by_id(&mut *db, id).await {
            Ok(record) => Ok(Some(session_from_record(&mut db, record).await?)),
            Err(_) => Ok(None),
        }
    }

    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        let mut db = self.inner.lock().await;
        let mut rows = toasty::query!(SessionRecord).exec(&mut *db).await?;
        rows.sort_by_key(|r| r.created_at);

        let mut sessions = Vec::with_capacity(rows.len());
        for record in rows {
            sessions.push(session_from_record(&mut db, record).await?);
        }
        Ok(sessions)
    }

    async fn save(&self, session: &Session) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        // INSERT OR IGNORE semantics via error handling on duplicate key.
        let _ = toasty::create!(SessionRecord {
            id: session.id.clone(),
            created_at: session.created_at,
        })
        .exec(&mut *db)
        .await;
        Ok(())
    }

    async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
        let mut db = self.inner.lock().await;
        let rows = toasty::query!(SessionRecord).exec(&mut *db).await?;

        let mut removed = 0usize;
        for record in rows {
            let msgs = record.messages().exec(&mut *db).await?;
            if msgs.is_empty() {
                record.delete().exec(&mut *db).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!(removed, "pruned empty sessions");
        }
        Ok(removed)
    }

    async fn rotate(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let mut db = self.inner.lock().await;
        // Nothing to archive if the session is absent or already empty.
        let Ok(live) = SessionRecord::get_by_id(&mut *db, session_id).await else {
            return Ok(None);
        };
        let msgs = live.messages().exec(&mut *db).await?;
        if msgs.is_empty() {
            return Ok(None);
        }

        // Move the transcript to a fresh archive session, preserving its start
        // time; the live row stays and is now empty for the next conversation.
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let archived_id = format!("{session_id}#{now}");
        toasty::create!(SessionRecord {
            id: archived_id.clone(),
            created_at: live.created_at,
        })
        .exec(&mut *db)
        .await?;
        let archive = SessionRecord::get_by_id(&mut *db, &archived_id).await?;
        for msg in msgs {
            toasty::create!(in archive.messages() {
                role: msg.role.clone(),
                content: msg.content.clone(),
                timestamp: msg.timestamp,
            })
            .exec(&mut *db)
            .await?;
            msg.delete().exec(&mut *db).await?;
        }
        Ok(Some(archived_id))
    }
}

// ── MessageRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl MessageRepository for Db {
    async fn list_by_session(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        let mut db = self.inner.lock().await;
        let record = SessionRecord::get_by_id(&mut *db, session_id).await?;
        let rows = record.messages().exec(&mut *db).await?;
        let mut messages: Vec<Message> = rows
            .into_iter()
            .map(|r| Message {
                role: parse_role(&r.role),
                content: r.content,
                timestamp: r.timestamp,
            })
            .collect();
        messages.sort_by_key(|m| m.timestamp);
        Ok(messages)
    }

    async fn save(&self, session_id: &str, message: &Message) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        let session = SessionRecord::get_by_id(&mut *db, session_id).await?;
        let role = format!("{:?}", message.role).to_lowercase();
        toasty::create!(in session.messages() {
            role: role,
            content: message.content.clone(),
            timestamp: message.timestamp,
        })
        .exec(&mut *db)
        .await?;
        Ok(())
    }
}

// ── ReminderRepository ────────────────────────────────────────────────────────

#[async_trait]
impl ReminderRepository for Db {
    async fn save(&self, reminder: &Reminder) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        toasty::create!(ReminderRecord {
            id: reminder.id.clone(),
            message: reminder.message.clone(),
            run_at: reminder.run_at,
            status: reminder.status.as_str().to_string(),
            schedule: reminder.schedule.clone(),
            created_at: reminder.created_at,
        })
        .exec(&mut *db)
        .await?;
        Ok(())
    }

    async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>> {
        let mut db = self.inner.lock().await;
        let rows = toasty::query!(ReminderRecord).exec(&mut *db).await?;
        let pending = rows
            .into_iter()
            .filter(|r| r.status == "pending")
            .map(reminder_from_record)
            .collect();
        Ok(pending)
    }

    async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        let mut record = ReminderRecord::get_by_id(&mut *db, id).await?;
        record
            .update()
            .status(status.as_str().to_string())
            .exec(&mut *db)
            .await?;
        Ok(())
    }

    async fn reschedule(&self, id: &str, next_run_at: i64) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        let mut record = ReminderRecord::get_by_id(&mut *db, id).await?;
        record.update().run_at(next_run_at).exec(&mut *db).await?;
        Ok(())
    }
}

// ── SessionTodoRepository ─────────────────────────────────────────────────────

#[async_trait]
impl SessionTodoRepository for Db {
    async fn get(&self, session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
        let mut db = self.inner.lock().await;
        match SessionTodoRecord::get_by_session_id(&mut *db, session_id).await {
            Ok(record) => Ok(serde_json::from_str(&record.items).unwrap_or_default()),
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn set(&self, session_id: &str, items: &[TodoItem]) -> anyhow::Result<()> {
        let json = serde_json::to_string(items)?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut db = self.inner.lock().await;
        match SessionTodoRecord::get_by_session_id(&mut *db, session_id).await {
            Ok(mut record) => {
                record
                    .update()
                    .items(json)
                    .updated_at(now)
                    .exec(&mut *db)
                    .await?;
            }
            Err(_) => {
                toasty::create!(SessionTodoRecord {
                    session_id: session_id.to_string(),
                    items: json,
                    updated_at: now,
                })
                .exec(&mut *db)
                .await?;
            }
        }
        Ok(())
    }

    async fn clear(&self, session_id: &str) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        if let Ok(record) = SessionTodoRecord::get_by_session_id(&mut *db, session_id).await {
            record.delete().exec(&mut *db).await?;
        }
        Ok(())
    }
}

// ── PairingRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl PairingRepository for Db {
    async fn upsert(&self, request: &PairingRequest) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        if let Ok(record) = PairingRecord::get_by_id(&mut *db, &request.id).await {
            record.delete().exec(&mut *db).await?;
        }
        toasty::create!(PairingRecord {
            id: request.id.clone(),
            platform: request.platform.clone(),
            sender_id: request.sender_id.clone(),
            chat_id: request.chat_id.clone(),
            code_hash: request.code_hash.clone(),
            salt: request.salt.clone(),
            status: request.status.as_str().to_string(),
            created_at: request.created_at,
        })
        .exec(&mut *db)
        .await?;
        Ok(())
    }

    async fn find(
        &self,
        platform: &str,
        sender_id: &str,
    ) -> anyhow::Result<Option<PairingRequest>> {
        let mut db = self.inner.lock().await;
        let id = format!("{platform}:{sender_id}");
        match PairingRecord::get_by_id(&mut *db, &id).await {
            Ok(record) => Ok(Some(pairing_from_record(record))),
            Err(_) => Ok(None),
        }
    }

    async fn count_active_pending(&self, platform: &str) -> anyhow::Result<usize> {
        let mut db = self.inner.lock().await;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let rows = toasty::query!(PairingRecord).exec(&mut *db).await?;
        Ok(rows
            .iter()
            .filter(|r| {
                r.platform == platform
                    && r.status == "pending"
                    && now - r.created_at <= PAIRING_CODE_TTL_SECS
            })
            .count())
    }

    async fn approve_code(&self, code: &str) -> anyhow::Result<ApproveOutcome> {
        const LOCK_ID: &str = "approve";
        let mut db = self.inner.lock().await;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        // Honor an active lockout before testing the code.
        let lock = LockoutRecord::get_by_id(&mut *db, LOCK_ID).await.ok();
        if let Some(l) = &lock
            && l.locked_until > now
        {
            return Ok(ApproveOutcome::Locked {
                retry_after_secs: l.locked_until - now,
            });
        }

        let rows = toasty::query!(PairingRecord).exec(&mut *db).await?;
        let matched = rows.into_iter().find(|r| {
            r.status == "pending"
                && now - r.created_at <= PAIRING_CODE_TTL_SECS
                && verify_code(&r.salt, &r.code_hash, code)
        });

        match matched {
            Some(mut record) => {
                record
                    .update()
                    .status(PairingStatus::Approved.as_str().to_string())
                    .exec(&mut *db)
                    .await?;
                // Success clears the failure counter.
                if let Some(mut l) = lock {
                    l.update()
                        .failed_count(0)
                        .locked_until(0)
                        .exec(&mut *db)
                        .await?;
                }
                Ok(ApproveOutcome::Approved(pairing_from_record(record)))
            }
            None => {
                let mut count = lock.as_ref().map(|l| l.failed_count).unwrap_or(0) + 1;
                let mut locked_until = 0;
                if count >= APPROVE_MAX_FAILURES {
                    locked_until = now + APPROVE_LOCKOUT_SECS;
                    count = 0; // reset the counter once locked
                }
                match lock {
                    Some(mut l) => {
                        l.update()
                            .failed_count(count)
                            .locked_until(locked_until)
                            .exec(&mut *db)
                            .await?;
                    }
                    None => {
                        toasty::create!(LockoutRecord {
                            id: LOCK_ID.to_string(),
                            failed_count: count,
                            locked_until,
                        })
                        .exec(&mut *db)
                        .await?;
                    }
                }
                if locked_until > now {
                    Ok(ApproveOutcome::Locked {
                        retry_after_secs: locked_until - now,
                    })
                } else {
                    Ok(ApproveOutcome::NotFound)
                }
            }
        }
    }

    async fn list(&self) -> anyhow::Result<Vec<PairingRequest>> {
        let mut db = self.inner.lock().await;
        let mut rows = toasty::query!(PairingRecord).exec(&mut *db).await?;
        rows.sort_by_key(|r| r.created_at);
        Ok(rows.into_iter().map(pairing_from_record).collect())
    }

    async fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut db = self.inner.lock().await;
        match PairingRecord::get_by_id(&mut *db, id).await {
            Ok(record) => {
                record.delete().exec(&mut *db).await?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

// ── HomeRepository ────────────────────────────────────────────────────────────

#[async_trait]
impl HomeRepository for Db {
    async fn get(&self) -> anyhow::Result<Option<String>> {
        let mut db = self.inner.lock().await;
        match SettingRecord::get_by_id(&mut *db, HOME_SETTING_KEY).await {
            Ok(record) => Ok(Some(record.value).filter(|v| !v.is_empty())),
            Err(_) => Ok(None),
        }
    }

    async fn set(&self, session_id: &str) -> anyhow::Result<()> {
        let mut db = self.inner.lock().await;
        match SettingRecord::get_by_id(&mut *db, HOME_SETTING_KEY).await {
            Ok(mut record) => {
                record
                    .update()
                    .value(session_id.to_string())
                    .exec(&mut *db)
                    .await?;
            }
            Err(_) => {
                toasty::create!(SettingRecord {
                    id: HOME_SETTING_KEY.to_string(),
                    value: session_id.to_string(),
                })
                .exec(&mut *db)
                .await?;
            }
        }
        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

async fn session_from_record(
    db: &mut toasty::Db,
    record: SessionRecord,
) -> anyhow::Result<Session> {
    let id = record.id.clone();
    let created_at = record.created_at;
    let rows = record.messages().exec(db).await?;
    let mut messages: Vec<Message> = rows
        .into_iter()
        .map(|r| Message {
            role: parse_role(&r.role),
            content: r.content,
            timestamp: r.timestamp,
        })
        .collect();
    messages.sort_by_key(|m| m.timestamp);
    Ok(Session {
        id,
        messages,
        created_at,
    })
}

fn skill_from_record(record: SkillRecord) -> Skill {
    Skill {
        name: record.name,
        description: record.description,
        instructions: record.instructions,
        protected: record.protected,
    }
}

fn pairing_from_record(record: PairingRecord) -> PairingRequest {
    PairingRequest {
        id: record.id,
        platform: record.platform,
        sender_id: record.sender_id,
        chat_id: record.chat_id,
        code_hash: record.code_hash,
        salt: record.salt,
        status: parse_pairing_status(&record.status),
        created_at: record.created_at,
    }
}

fn reminder_from_record(record: ReminderRecord) -> Reminder {
    Reminder {
        id: record.id,
        message: record.message,
        run_at: record.run_at,
        status: parse_reminder_status(&record.status),
        schedule: record.schedule,
        created_at: record.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reminder::ReminderStatus;

    fn sqlite_url(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        format!("sqlite:{}", path.display())
    }

    #[tokio::test]
    async fn session_repository_lists_sessions() {
        let db = Db::connect(&sqlite_url("shion_session_repo_test.db"))
            .await
            .unwrap();
        let first = Session::new("first");
        let second = Session::new("second");

        SessionRepository::save(&db, &first).await.unwrap();
        MessageRepository::save(&db, "first", &Message::user("hello"))
            .await
            .unwrap();
        SessionRepository::save(&db, &second).await.unwrap();

        let rows = SessionRepository::list(&db).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "first");
        assert_eq!(rows[0].user_turns(), 1);
        assert_eq!(rows[1].id, "second");
    }

    #[tokio::test]
    async fn delete_empty_sessions_prunes_only_sessions_without_messages() {
        let db = Db::connect(&sqlite_url("shion_delete_empty_test.db"))
            .await
            .unwrap();

        // Session with messages — must survive.
        let keep = Session::new("keep");
        SessionRepository::save(&db, &keep).await.unwrap();
        MessageRepository::save(&db, "keep", &Message::user("hello"))
            .await
            .unwrap();

        // Empty session — must be pruned.
        let drop = Session::new("drop");
        SessionRepository::save(&db, &drop).await.unwrap();

        // Another empty session.
        let drop2 = Session::new("drop2");
        SessionRepository::save(&db, &drop2).await.unwrap();

        let removed = SessionRepository::delete_empty_sessions(&db).await.unwrap();
        assert_eq!(removed, 2);

        let survivors = SessionRepository::list(&db).await.unwrap();
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].id, "keep");
    }

    #[tokio::test]
    async fn delete_empty_sessions_returns_zero_when_none_empty() {
        let db = Db::connect(&sqlite_url("shion_delete_none_test.db"))
            .await
            .unwrap();

        let s = Session::new("only");
        SessionRepository::save(&db, &s).await.unwrap();
        MessageRepository::save(&db, "only", &Message::user("hi"))
            .await
            .unwrap();

        let removed = SessionRepository::delete_empty_sessions(&db).await.unwrap();
        assert_eq!(removed, 0);
        assert_eq!(SessionRepository::list(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn db_reminder_schedule_roundtrip() {
        let db = Db::connect(&sqlite_url("shion_reminder_schedule_test.db"))
            .await
            .unwrap();
        let now_unix = chrono::Utc::now().timestamp();
        let reminder = crate::domain::reminder::Reminder::recurring(
            "take medication".to_string(),
            now_unix + 3600,
            "0 9 * * *".to_string(),
        );

        ReminderRepository::save(&db, &reminder).await.unwrap();
        let pending = ReminderRepository::list_pending(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].schedule, "0 9 * * *");
        assert_eq!(pending[0].status, ReminderStatus::Pending);

        let new_run_at = now_unix + 90_000;
        ReminderRepository::reschedule(&db, &reminder.id, new_run_at)
            .await
            .unwrap();

        let pending = ReminderRepository::list_pending(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].run_at, new_run_at);
        assert_eq!(pending[0].status, ReminderStatus::Pending);
    }

    #[tokio::test]
    async fn db_reminder_roundtrip() {
        let db = Db::connect(&sqlite_url("shion_reminder_repo_test.db"))
            .await
            .unwrap();
        let reminder = Reminder::new("drink water".to_string(), 9999999999);

        ReminderRepository::save(&db, &reminder).await.unwrap();
        let pending = ReminderRepository::list_pending(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message, "drink water");
        assert_eq!(pending[0].status, ReminderStatus::Pending);

        ReminderRepository::set_status(&db, &reminder.id, ReminderStatus::Fired)
            .await
            .unwrap();
        let pending = ReminderRepository::list_pending(&db).await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn db_session_todo_set_get_clear() {
        use crate::domain::todo::{TodoItem, TodoStatus};
        let db = Db::connect(&sqlite_url("shion_session_todo_test.db"))
            .await
            .unwrap();

        // Absent session reads as empty.
        assert!(
            SessionTodoRepository::get(&db, "s1")
                .await
                .unwrap()
                .is_empty()
        );

        let items = vec![
            TodoItem {
                content: "step one".to_string(),
                status: TodoStatus::InProgress,
                active_form: "doing step one".to_string(),
            },
            TodoItem {
                content: "step two".to_string(),
                status: TodoStatus::Pending,
                active_form: String::new(),
            },
        ];
        SessionTodoRepository::set(&db, "s1", &items).await.unwrap();
        let got = SessionTodoRepository::get(&db, "s1").await.unwrap();
        assert_eq!(got, items);

        // set replaces the whole list (upsert, not append).
        let replaced = vec![TodoItem {
            content: "only step".to_string(),
            status: TodoStatus::Completed,
            active_form: String::new(),
        }];
        SessionTodoRepository::set(&db, "s1", &replaced)
            .await
            .unwrap();
        assert_eq!(
            SessionTodoRepository::get(&db, "s1").await.unwrap(),
            replaced
        );

        // Scoped per session.
        assert!(
            SessionTodoRepository::get(&db, "s2")
                .await
                .unwrap()
                .is_empty()
        );

        SessionTodoRepository::clear(&db, "s1").await.unwrap();
        assert!(
            SessionTodoRepository::get(&db, "s1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn db_pairing_upsert_approve_revoke_roundtrip() {
        use crate::domain::pairing::ApproveOutcome;

        let db = Db::connect(&sqlite_url("shion_pairing_repo_test.db"))
            .await
            .unwrap();
        let (request, code) = PairingRequest::mint("telegram", "777", "777");

        PairingRepository::upsert(&db, &request).await.unwrap();
        let found = PairingRepository::find(&db, "telegram", "777")
            .await
            .unwrap()
            .unwrap();
        // The plaintext code is never persisted — only the salted hash.
        assert_eq!(found.code_hash, request.code_hash);
        assert_ne!(found.code_hash, code);
        assert_eq!(found.status, crate::domain::pairing::PairingStatus::Pending);
        assert_eq!(
            PairingRepository::count_active_pending(&db, "telegram")
                .await
                .unwrap(),
            1
        );

        // Upsert with a fresh code replaces the row (one row per sender).
        let (refreshed, refreshed_code) = PairingRequest::mint("telegram", "777", "777");
        PairingRepository::upsert(&db, &refreshed).await.unwrap();
        assert_eq!(PairingRepository::list(&db).await.unwrap().len(), 1);

        assert!(matches!(
            PairingRepository::approve_code(&db, "NOSUCHCD")
                .await
                .unwrap(),
            ApproveOutcome::NotFound
        ));
        let ApproveOutcome::Approved(approved) =
            PairingRepository::approve_code(&db, &refreshed_code)
                .await
                .unwrap()
        else {
            panic!("expected approval");
        };
        assert_eq!(approved.sender_id, "777");
        let found = PairingRepository::find(&db, "telegram", "777")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.status,
            crate::domain::pairing::PairingStatus::Approved
        );

        assert!(
            PairingRepository::revoke(&db, "telegram:777")
                .await
                .unwrap()
        );
        assert!(
            !PairingRepository::revoke(&db, "telegram:777")
                .await
                .unwrap()
        );
        assert!(
            PairingRepository::find(&db, "telegram", "777")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn db_pairing_locks_out_after_repeated_bad_codes() {
        use crate::domain::pairing::{APPROVE_MAX_FAILURES, ApproveOutcome};

        let db = Db::connect(&sqlite_url("shion_pairing_lockout_test.db"))
            .await
            .unwrap();

        // The first APPROVE_MAX_FAILURES - 1 wrong codes are NotFound; the
        // attempt that reaches the limit locks out.
        for _ in 0..APPROVE_MAX_FAILURES - 1 {
            assert!(matches!(
                PairingRepository::approve_code(&db, "BADCODE1")
                    .await
                    .unwrap(),
                ApproveOutcome::NotFound
            ));
        }
        assert!(matches!(
            PairingRepository::approve_code(&db, "BADCODE1")
                .await
                .unwrap(),
            ApproveOutcome::Locked { .. }
        ));
    }

    #[tokio::test]
    async fn rotate_archives_transcript_and_empties_live_session() {
        let db = Db::connect(&sqlite_url("shion_rotate_test.db"))
            .await
            .unwrap();
        let sid = "telegram:rot";
        SessionRepository::save(&db, &Session::new(sid))
            .await
            .unwrap();
        MessageRepository::save(&db, sid, &Message::user("hi"))
            .await
            .unwrap();
        MessageRepository::save(&db, sid, &Message::assistant("hello"))
            .await
            .unwrap();

        let archived = SessionRepository::rotate(&db, sid)
            .await
            .unwrap()
            .expect("a non-empty session rotates");
        assert_ne!(archived, sid);

        // Live session is now empty; the archive holds the transcript.
        assert!(
            MessageRepository::list_by_session(&db, sid)
                .await
                .unwrap()
                .is_empty()
        );
        let archived_msgs = MessageRepository::list_by_session(&db, &archived)
            .await
            .unwrap();
        assert_eq!(archived_msgs.len(), 2);
        assert_eq!(archived_msgs[0].content, "hi");

        // Rotating an empty session is a no-op.
        assert!(SessionRepository::rotate(&db, sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn home_repository_roundtrips_and_overwrites() {
        let db = Db::connect(&sqlite_url("shion_home_repo_test.db"))
            .await
            .unwrap();

        assert!(HomeRepository::get(&db).await.unwrap().is_none());

        HomeRepository::set(&db, "telegram:123456").await.unwrap();
        assert_eq!(
            HomeRepository::get(&db).await.unwrap().as_deref(),
            Some("telegram:123456")
        );

        // /sethome from another chat replaces the home (one row per key).
        HomeRepository::set(&db, "feishu:oc_home").await.unwrap();
        assert_eq!(
            HomeRepository::get(&db).await.unwrap().as_deref(),
            Some("feishu:oc_home")
        );
    }

    #[tokio::test]
    async fn skill_repository_upserts_by_name() {
        let db = Db::connect(&sqlite_url("shion_skill_repo_test.db"))
            .await
            .unwrap();
        let skill = Skill {
            name: "debug-builds".to_string(),
            description: "Debug build failures".to_string(),
            instructions: "Check compiler errors first.".to_string(),
            protected: true,
        };

        SkillRepository::save(&db, &skill).await.unwrap();
        SkillRepository::save(
            &db,
            &Skill {
                instructions: "Check compiler errors, then run focused tests.".to_string(),
                ..skill.clone()
            },
        )
        .await
        .unwrap();

        let found = SkillRepository::find(&db, "debug-builds")
            .await
            .unwrap()
            .unwrap();
        assert!(found.protected);
        assert!(found.instructions.contains("focused tests"));

        let rows = SkillRepository::list(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
    }
}
