use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,
    pub created_at: i64,
}

impl Memory {
    pub fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        Self {
            id: format!(
                "mem-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            kind,
            content: content.into(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<Memory>>;
    async fn save(&self, memory: &Memory) -> anyhow::Result<()>;
}

pub fn parse_memory_kind(value: &str) -> MemoryKind {
    match value {
        "feedback" => MemoryKind::Feedback,
        "project" => MemoryKind::Project,
        "reference" => MemoryKind::Reference,
        _ => MemoryKind::User,
    }
}
