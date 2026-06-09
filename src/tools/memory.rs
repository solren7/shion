use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::domain::tool::Tool;

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct MemoryEntry {
    id: usize,
    text: String,
    ts: i64,
}

/// Long-term, cross-session memory backed by a JSONL file. The model decides
/// what to remember (`save`) and recalls it later (`list` / `search`).
pub struct MemoryTool {
    path: PathBuf,
}

impl MemoryTool {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    async fn load(&self) -> Vec<MemoryEntry> {
        let Ok(content) = tokio::fs::read_to_string(&self.path).await else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<MemoryEntry>(line).ok())
            .collect()
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Persistent long-term memory across sessions. action=\"save\" stores a \
         fact; action=\"search\" returns stored facts matching a query; \
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
                let mut entries = self.load().await;
                let entry = MemoryEntry {
                    id: entries.len() + 1,
                    text,
                    ts: time::OffsetDateTime::now_utc().unix_timestamp(),
                };
                entries.push(entry);

                let mut buf = String::new();
                for e in &entries {
                    buf.push_str(&serde_json::to_string(e)?);
                    buf.push('\n');
                }
                tokio::fs::write(&self.path, buf)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to persist memory: {e}"))?;
                Ok(format!("Saved memory #{}.", entries.len()))
            }
            "list" => {
                let entries = self.load().await;
                Ok(render(&entries))
            }
            "search" => {
                let query = args
                    .query
                    .ok_or_else(|| anyhow::anyhow!("`query` is required for action=search"))?
                    .to_lowercase();
                let entries = self.load().await;
                let hits: Vec<MemoryEntry> = entries
                    .into_iter()
                    .filter(|e| e.text.to_lowercase().contains(&query))
                    .collect();
                Ok(render(&hits))
            }
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected save/search/list)"
            )),
        }
    }
}

fn render(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return "(no memories)".to_string();
    }
    entries
        .iter()
        .map(|e| format!("#{}: {}", e.id, e.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tool(name: &str) -> MemoryTool {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        MemoryTool::new(path)
    }

    #[tokio::test]
    async fn save_list_search_roundtrip() {
        let tool = temp_tool("shion_mem_test.jsonl");

        tool.execute(json!({ "action": "save", "text": "用户喜欢蓝色" }).to_string())
            .await
            .unwrap();
        tool.execute(json!({ "action": "save", "text": "项目用 Rust 写" }).to_string())
            .await
            .unwrap();

        let list = tool
            .execute(json!({ "action": "list" }).to_string())
            .await
            .unwrap();
        assert!(list.contains("蓝色"));
        assert!(list.contains("Rust"));

        let hit = tool
            .execute(json!({ "action": "search", "query": "rust" }).to_string())
            .await
            .unwrap();
        assert!(hit.contains("Rust"));
        assert!(!hit.contains("蓝色"));
    }
}
