//! Markdown-file memory store: one `.md` file per memory under a directory
//! (normally `~/.shion/memory/`), with a small frontmatter block for metadata.
//!
//! Chosen over a database table or JSONL so memories stay human-readable and
//! hand-editable: open the file, fix the fact, done. The filename is the
//! memory id.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::domain::memory::{Memory, MemoryRepository, parse_memory_kind};

pub struct MdMemoryStore {
    dir: PathBuf,
}

impl MdMemoryStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

fn render_md(memory: &Memory) -> String {
    format!(
        "---\nkind: {}\ncreated_at: {}\n---\n\n{}\n",
        memory.kind.as_str(),
        memory.created_at,
        memory.content
    )
}

/// Parse a memory file written by [`render_md`]. Returns `None` when the file
/// doesn't follow the frontmatter format (e.g. a stray hand-created note);
/// such files are skipped rather than failing the whole listing.
fn parse_md(id: &str, text: &str) -> Option<Memory> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;

    let mut kind = None;
    let mut created_at = None;
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("kind:") {
            kind = Some(parse_memory_kind(v.trim()));
        } else if let Some(v) = line.strip_prefix("created_at:") {
            created_at = v.trim().parse::<i64>().ok();
        }
    }

    Some(Memory {
        id: id.to_string(),
        kind: kind?,
        content: body.trim().to_string(),
        created_at: created_at?,
    })
}

#[async_trait]
impl MemoryRepository for MdMemoryStore {
    async fn list(&self) -> anyhow::Result<Vec<Memory>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            // No directory yet means no memories yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::anyhow!("failed to read {:?}: {e}", self.dir)),
        };

        let mut memories = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = tokio::fs::read_to_string(&path).await?;
            if let Some(memory) = parse_md(id, &text) {
                memories.push(memory);
            }
        }
        memories.sort_by_key(|m| m.created_at);
        Ok(memories)
    }

    async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| anyhow::anyhow!("failed to create {:?}: {e}", self.dir))?;
        let path = self.dir.join(format!("{}.md", memory.id));
        tokio::fs::write(&path, render_md(memory))
            .await
            .map_err(|e| anyhow::anyhow!("failed to write {path:?}: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::MemoryKind;

    fn temp_store(name: &str) -> MdMemoryStore {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        MdMemoryStore::new(dir)
    }

    #[tokio::test]
    async fn save_then_list_roundtrips() {
        let store = temp_store("shion_md_memory_roundtrip");
        let memory = Memory::new(MemoryKind::Project, "项目用 Rust 写");
        store.save(&memory).await.unwrap();

        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, memory.id);
        assert_eq!(rows[0].kind, MemoryKind::Project);
        assert_eq!(rows[0].content, "项目用 Rust 写");
    }

    #[tokio::test]
    async fn list_on_missing_dir_is_empty() {
        let store = temp_store("shion_md_memory_missing");
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_files_are_skipped() {
        let store = temp_store("shion_md_memory_malformed");
        store
            .save(&Memory::new(MemoryKind::User, "valid"))
            .await
            .unwrap();
        std::fs::write(
            std::env::temp_dir().join("shion_md_memory_malformed/notes.md"),
            "just some text, no frontmatter",
        )
        .unwrap();

        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "valid");
    }

    #[tokio::test]
    async fn save_overwrites_same_id() {
        let store = temp_store("shion_md_memory_overwrite");
        let mut memory = Memory::new(MemoryKind::User, "v1");
        store.save(&memory).await.unwrap();
        memory.content = "v2".to_string();
        store.save(&memory).await.unwrap();

        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "v2");
    }
}
