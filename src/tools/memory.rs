use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    agent::system_prompt::{PINNED_MEMORY_BUDGET, render_pinned_memory_block},
    domain::{
        memory::{
            Memory, MemoryConfidence, MemoryContext, MemoryKind, MemoryQuery, MemoryRepository,
            MemoryStatus, ScoredMemory, parse_memory_kind, parse_memory_status,
        },
        tool::Tool,
    },
    services::tool_registry::current_session,
};

/// Default cap on search results.
const SEARCH_LIMIT: usize = 10;

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// Target memory id (action=update/promote/reject/archive).
    #[serde(default)]
    id: Option<String>,
    /// New status (action=update).
    #[serde(default)]
    status: Option<String>,
    /// Pin/unpin (action=update). Pinning is the only path into L1 injection.
    #[serde(default)]
    pinned: Option<bool>,
    /// New ranking weight 0–100 (action=update).
    #[serde(default)]
    importance: Option<i32>,
    /// Optional TTL in days (action=save).
    #[serde(default)]
    expiry_days: Option<i64>,
}

/// Long-term, cross-session memory with governance. The model `save`s facts,
/// `search`es them (scoped to the current chat/session), and curates the
/// library: `promote` a candidate to active, `reject`/`archive` it, or `update`
/// fields (including `pinned`, which gates L1 per-turn injection). Storage lives
/// behind [`MemoryRepository`] — the same store the reviewer writes to.
pub struct MemoryTool {
    memories: Arc<dyn MemoryRepository>,
}

impl MemoryTool {
    pub fn new(memories: Arc<dyn MemoryRepository>) -> Self {
        Self { memories }
    }

    /// A Hermes-style usage line for the L1 pinned profile — the one memory
    /// surface with a real, finite budget (it is injected verbatim every turn).
    /// Surfacing "how full is it" nudges the model to keep pinned compact and
    /// curate before adding. Returns `None` when nothing is pinned (no pressure
    /// to report). Best-effort: a load failure just omits the line.
    async fn pinned_usage_line(&self) -> Option<String> {
        let pinned = self.memories.pinned(&ctx()).await.ok()?;
        let used = render_pinned_memory_block(&pinned)
            .map(|b| b.len())
            .unwrap_or(0);
        if used == 0 {
            return None;
        }
        let pct = (used * 100) / PINNED_MEMORY_BUDGET;
        Some(format!(
            "L1 pinned profile: {used}/{PINNED_MEMORY_BUDGET} chars ({pct}%) used."
        ))
    }

    /// Load a memory by id or return a helpful error.
    async fn require(&self, id: &Option<String>) -> anyhow::Result<Memory> {
        let id = id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("`id` is required for this action"))?;
        self.memories
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no memory with id `{id}`"))
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Persistent long-term memory across sessions, with governance. \
         action=\"save\" stores a fact (optional kind: profile | preference | feedback | \
         project | person | fact | decision | reference); action=\"search\" returns facts \
         matching a query (scoped to this chat); action=\"list\" returns stored facts; \
         action=\"update\" changes a memory by id (status / pinned / importance / kind / \
         content); action=\"promote\" marks a candidate active; action=\"reject\" / \
         \"archive\" retire one. Pin a memory (update pinned=true) only when the user \
         confirms it as durable profile context. \
         Write each memory as a declarative fact, not an instruction (\"User prefers \
         concise replies\" ✓, \"Always reply concisely\" ✗), and prioritize what reduces \
         future steering. Do not save anything that will be stale within a week — task \
         progress, completed-work logs, PR/issue numbers, or commit SHAs do not belong here."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["save", "search", "list", "update", "promote", "reject", "archive"],
                    "description": "The memory operation to perform."
                },
                "text": { "type": "string", "description": "Fact to store (action=save) or new content (action=update)." },
                "kind": {
                    "type": "string",
                    "enum": ["profile", "preference", "feedback", "project", "person", "fact", "decision", "reference"],
                    "description": "Category (action=save, default profile; or action=update)."
                },
                "query": { "type": "string", "description": "Search term (action=search)." },
                "id": { "type": "string", "description": "Target memory id (action=update/promote/reject/archive)." },
                "status": { "type": "string", "enum": ["candidate", "active", "archived", "rejected"], "description": "New status (action=update)." },
                "pinned": { "type": "boolean", "description": "Pin/unpin for L1 injection (action=update). Only pin user-confirmed durable facts." },
                "importance": { "type": "integer", "description": "Ranking weight 0–100 (action=update)." },
                "expiry_days": { "type": "integer", "description": "Optional TTL in days (action=save); omit for permanent." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: MemoryArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid memory arguments: {e}"))?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

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
                // Scope to the current chat so a channel fact does not leak elsewhere.
                memory.scope = ctx().write_scope();
                if let Some(days) = args.expiry_days.filter(|d| *d > 0) {
                    memory.expires_at = Some(now + days * 86_400);
                }
                self.memories.save(&memory).await?;
                let mut out = format!("Saved memory {}.", memory.id);
                if let Some(usage) = self.pinned_usage_line().await {
                    out.push('\n');
                    out.push_str(&usage);
                }
                Ok(out)
            }
            "list" => {
                let mut memories = self.memories.list().await?;
                if let Some(status) = args.status.as_deref().map(parse_memory_status) {
                    memories.retain(|m| m.status == status);
                }
                let mut out = render(&memories);
                if let Some(usage) = self.pinned_usage_line().await {
                    out.push_str("\n\n");
                    out.push_str(&usage);
                }
                Ok(out)
            }
            "search" => {
                let text = args
                    .query
                    .ok_or_else(|| anyhow::anyhow!("`query` is required for action=search"))?;
                let query = MemoryQuery {
                    text,
                    allowed_scopes: ctx().allowed_scopes,
                    kinds: Vec::new(),
                    statuses: vec![MemoryStatus::Active],
                    limit: SEARCH_LIMIT,
                };
                let hits = self.memories.search(query).await?;
                Ok(render_scored(&hits))
            }
            "update" => {
                let mut memory = self.require(&args.id).await?;
                if let Some(text) = args.text {
                    memory.content = text;
                }
                if let Some(kind) = args.kind.as_deref() {
                    memory.kind = parse_memory_kind(kind);
                }
                if let Some(status) = args.status.as_deref() {
                    memory.status = parse_memory_status(status);
                }
                if let Some(pinned) = args.pinned {
                    memory.pinned = pinned;
                    // Pinning requires high confidence to actually surface in L1.
                    if pinned && memory.confidence == MemoryConfidence::Extracted {
                        memory.confidence = MemoryConfidence::Confirmed;
                    }
                }
                if let Some(importance) = args.importance {
                    memory.importance = importance.clamp(0, 100);
                }
                memory.updated_at = now;
                self.memories.save(&memory).await?;
                Ok(format!("Updated memory {}.", memory.id))
            }
            "promote" => {
                let mut memory = self.require(&args.id).await?;
                memory.status = MemoryStatus::Active;
                memory.confidence = MemoryConfidence::Confirmed;
                memory.updated_at = now;
                self.memories.save(&memory).await?;
                Ok(format!("Promoted memory {} to active.", memory.id))
            }
            "reject" => set_status(self, &args.id, MemoryStatus::Rejected, now).await,
            "archive" => set_status(self, &args.id, MemoryStatus::Archived, now).await,
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected save/search/list/update/promote/reject/archive)"
            )),
        }
    }
}

/// The memory context for the current turn, derived from the ambient session.
/// Falls back to a global-only context when there is no session (aux sub-agents
/// never reach here, but be safe).
fn ctx() -> MemoryContext {
    match current_session() {
        Some(s) => MemoryContext::from_session(&s.session_id),
        None => MemoryContext::from_session(""),
    }
}

async fn set_status(
    tool: &MemoryTool,
    id: &Option<String>,
    status: MemoryStatus,
    now: i64,
) -> anyhow::Result<String> {
    let mut memory = tool.require(id).await?;
    memory.status = status;
    memory.updated_at = now;
    tool.memories.save(&memory).await?;
    Ok(format!("Set memory {} to {}.", memory.id, status.as_str()))
}

fn render(memories: &[Memory]) -> String {
    if memories.is_empty() {
        return "(no memories)".to_string();
    }
    memories
        .iter()
        .map(render_one)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_one(m: &Memory) -> String {
    let pin = if m.pinned { " 📌" } else { "" };
    let mut line = format!(
        "[{}/{}/{}{}] {}: {}",
        m.kind.as_str(),
        m.status.as_str(),
        m.scope.type_str(),
        pin,
        m.id,
        m.content
    );
    if !m.source.is_empty() {
        line.push_str(&format!(" (from {})", m.source));
    }
    line
}

fn render_scored(hits: &[ScoredMemory]) -> String {
    if hits.is_empty() {
        return "(no matches)".to_string();
    }
    hits.iter()
        .map(|h| render_one(&h.memory))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::memory::md_memory::MdMemoryStore;

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
        assert!(list.contains("[project/"));

        let hit = tool
            .execute(json!({ "action": "search", "query": "rust" }).to_string())
            .await
            .unwrap();
        assert!(hit.contains("Rust"));
        assert!(!hit.contains("蓝色"));
    }

    #[tokio::test]
    async fn promote_then_pin_via_update() {
        let tool = temp_tool("shion_mem_tool_promote");
        // A candidate (simulating a reviewer extraction).
        let mut cand = Memory::new(MemoryKind::Preference, "prefers concise answers");
        cand.status = MemoryStatus::Candidate;
        cand.confidence = MemoryConfidence::Extracted;
        tool.memories.save(&cand).await.unwrap();

        tool.execute(json!({ "action": "promote", "id": cand.id }).to_string())
            .await
            .unwrap();
        let after = tool.memories.get(&cand.id).await.unwrap().unwrap();
        assert_eq!(after.status, MemoryStatus::Active);
        assert_eq!(after.confidence, MemoryConfidence::Confirmed);

        tool.execute(json!({ "action": "update", "id": cand.id, "pinned": true }).to_string())
            .await
            .unwrap();
        let pinned = tool.memories.get(&cand.id).await.unwrap().unwrap();
        assert!(pinned.pinned);
    }

    #[tokio::test]
    async fn reject_and_archive_set_status() {
        let tool = temp_tool("shion_mem_tool_reject");
        let m = Memory::new(MemoryKind::Fact, "ephemeral");
        tool.memories.save(&m).await.unwrap();

        tool.execute(json!({ "action": "reject", "id": m.id }).to_string())
            .await
            .unwrap();
        assert_eq!(
            tool.memories.get(&m.id).await.unwrap().unwrap().status,
            MemoryStatus::Rejected
        );
    }

    #[tokio::test]
    async fn update_unknown_id_errors() {
        let tool = temp_tool("shion_mem_tool_unknown");
        let err = tool
            .execute(json!({ "action": "promote", "id": "nope" }).to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no memory with id"));
    }
}
