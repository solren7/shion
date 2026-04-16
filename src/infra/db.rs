use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::domain::{
    message::{Message, Role},
    repository::{MessageRepository, SessionRepository},
    session::Session,
};

// ── toasty models (infra-internal) ───────────────────────────────────────────

#[derive(Debug, toasty::Model)]
struct SessionRecord {
    #[key]
    id: String,
    created_at: i64,

    #[has_many]
    messages: toasty::HasMany<MessageRecord>,
}

#[derive(Debug, toasty::Model)]
struct MessageRecord {
    #[key]
    #[auto]
    id: u64,

    #[index]
    session_id: String,

    #[belongs_to(key = session_id, references = id)]
    session_record: toasty::BelongsTo<SessionRecord>,

    role: String,
    content: String,
    timestamp: i64,
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
            .models(toasty::models!(SessionRecord, MessageRecord))
            .connect(url)
            .await?;

        if is_new {
            db.push_schema().await?;
        }

        Ok(Self { inner: Arc::new(Mutex::new(db)) })
    }
}

// ── SessionRepository ─────────────────────────────────────────────────────────

#[async_trait]
impl SessionRepository for Db {
    async fn find(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let mut db = self.inner.lock().await;
        match SessionRecord::get_by_id(&mut *db, id).await {
            Ok(record) => {
                let created_at = record.created_at;
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
                Ok(Some(Session { id: id.to_string(), messages, created_at }))
            }
            Err(_) => Ok(None),
        }
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_role(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}
