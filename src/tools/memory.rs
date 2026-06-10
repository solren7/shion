use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    memory::{Memory, MemoryKind, MemoryRepository, parse_memory_kind},
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
         fact (optional kind: user | feedback | project | reference); \
         action=\"search\" returns stored facts matching a query; \
         action=\"list\" returns all stored facts."
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
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "Category of the fact (action=save, default: user)."
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
                    .unwrap_or(MemoryKind::User);
                let memory = Memory::new(kind, text);
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
        .map(|m| format!("[{}] {}: {}", m.kind.as_str(), m.id, m.content))
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
