use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    memory::{Memory, MemoryConfidence, MemoryKind, MemoryRepository, parse_memory_kind},
    tool::Tool,
};

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// Optional TTL in days (action=save): the memory is hidden from recall
    /// once it elapses. Omit for a memory that never expires.
    #[serde(default)]
    expiry_days: Option<i64>,
}

/// Long-term, cross-session memory. The model decides what to remember
/// (`save`) and recalls it later (`list` / `search`). Storage lives behind
/// [`MemoryRepository`] — the same store the reflective reviewer writes to.
pub struct MemoryTool {
    memories: Arc<dyn MemoryRepository>,
}

impl MemoryTool {
    pub fn new(memories: Arc<dyn MemoryRepository>) -> Self {
        Self { memories }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Persistent long-term memory across sessions. action=\"save\" stores a \
         fact (optional kind: profile | preference | feedback | project | person | \
         fact | decision | reference); action=\"search\" returns stored facts \
         matching a query; action=\"list\" returns all stored facts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["save", "search", "list"],
                    "description": "The memory operation to perform."
                },
                "text": { "type": "string", "description": "Fact to store (action=save)." },
                "kind": {
                    "type": "string",
                    "enum": ["profile", "preference", "feedback", "project", "person", "fact", "decision", "reference"],
                    "description": "Category of the fact (action=save, default: profile)."
                },
                "expiry_days": {
                    "type": "integer",
                    "description": "Optional TTL in days (action=save); the fact is forgotten after this many days. Omit for a permanent memory."
                },
                "query": { "type": "string", "description": "Search term (action=search)." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: MemoryArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid memory arguments: {e}"))?;

        match args.action.as_str() {
            "save" => {
                let text = args
                    .text
                    .ok_or_else(|| anyhow::anyhow!("`text` is required for action=save"))?;
                let kind = args
                    .kind
                    .as_deref()
                    .map(parse_memory_kind)
                    .unwrap_or(MemoryKind::Profile);
                let mut memory = Memory::new(kind, text);
                // An explicit user save is the highest trust tier.
                memory.confidence = MemoryConfidence::UserWritten;
                if let Some(days) = args.expiry_days.filter(|d| *d > 0) {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    memory.expires_at = Some(now + days * 86_400);
                }
                self.memories.save(&memory).await?;
                Ok(format!("Saved memory {}.", memory.id))
            }
            "list" => {
                let memories = self.memories.list().await?;
                Ok(render(&memories))
            }
            "search" => {
                let query = args
                    .query
                    .ok_or_else(|| anyhow::anyhow!("`query` is required for action=search"))?
                    .to_lowercase();
                let hits: Vec<Memory> = self
                    .memories
                    .list()
                    .await?
                    .into_iter()
                    .filter(|m| m.content.to_lowercase().contains(&query))
                    .collect();
                Ok(render(&hits))
            }
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected save/search/list)"
            )),
        }
    }
}

fn render(memories: &[Memory]) -> String {
    if memories.is_empty() {
        return "(no memories)".to_string();
    }
    memories
        .iter()
        .map(|m| {
            let mut line = format!("[{}] {}: {}", m.kind.as_str(), m.id, m.content);
            if !m.source.is_empty() {
                line.push_str(&format!(" (from {})", m.source));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::md_memory::MdMemoryStore;

    fn temp_tool(name: &str) -> MemoryTool {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        MemoryTool::new(Arc::new(MdMemoryStore::new(dir)))
    }

    #[tokio::test]
    async fn save_list_search_roundtrip() {
        let tool = temp_tool("shion_mem_tool_test");

        tool.execute(json!({ "action": "save", "text": "用户喜欢蓝色" }).to_string())
            .await
            .unwrap();
        tool.execute(
            json!({ "action": "save", "text": "项目用 Rust 写", "kind": "project" }).to_string(),
        )
        .await
        .unwrap();

        let list = tool
            .execute(json!({ "action": "list" }).to_string())
            .await
            .unwrap();
        assert!(list.contains("蓝色"));
        assert!(list.contains("Rust"));
        assert!(list.contains("[project]"));

        let hit = tool
            .execute(json!({ "action": "search", "query": "rust" }).to_string())
            .await
            .unwrap();
        assert!(hit.contains("Rust"));
        assert!(!hit.contains("蓝色"));
    }
}
