use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::info;

use crate::domain::{
    message::{Message, Role},
    reminder::{Reminder, ReminderRepository, ReminderStatus, parse_reminder_status},
    repository::{MessageRepository, SessionRepository, SkillRepository},
    session::Session,
    skill::Skill,
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
                ReminderRecord
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
