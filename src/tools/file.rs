use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    approval::{ApprovalRequest, Approver},
    tool::Tool,
    workspace::Workspace,
};

/// Maximum number of bytes returned by a single read, to keep tool output bounded.
const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
struct FileArgs {
    action: String,
    path: String,
    #[serde(default)]
    content: Option<String>,
}

/// Reads and writes local files, confined to a [`Workspace`]. Writes require
/// user approval.
pub struct FileTool {
    workspace: Arc<Workspace>,
    approver: Arc<dyn Approver>,
}

impl FileTool {
    pub fn new(workspace: Arc<Workspace>, approver: Arc<dyn Approver>) -> Self {
        Self {
            workspace,
            approver,
        }
    }
}

#[async_trait]
impl Tool for FileTool {
    fn name(&self) -> &'static str {
        "file"
    }

    fn description(&self) -> &'static str {
        "Read or write a local file within the workspace. action=\"read\" returns \
         the file's contents; action=\"write\" creates or overwrites the file with \
         the given content (requires user approval)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["read", "write"],
                    "description": "Whether to read or write the file."
                },
                "path": {
                    "type": "string",
                    "description": "Path to the file, inside the workspace."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write. Required when action=\"write\"."
                }
            },
            "required": ["action", "path"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: FileArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid file arguments: {e}"))?;

        // Workspace whitelist: reject any path outside the allowed roots.
        if !self.workspace.contains(Path::new(&args.path)) {
            return Err(anyhow::anyhow!(
                "path `{}` is outside the workspace and was blocked",
                args.path
            ));
        }

        match args.action.as_str() {
            "read" => {
                let mut text = tokio::fs::read_to_string(&args.path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", args.path))?;
                if text.len() > MAX_READ_BYTES {
                    text.truncate(MAX_READ_BYTES);
                    text.push_str("\n…[truncated]");
                }
                Ok(text)
            }
            "write" => {
                let content = args
                    .content
                    .ok_or_else(|| anyhow::anyhow!("`content` is required for action=write"))?;

                // Approval gate: writing mutates the filesystem.
                let request = ApprovalRequest::normal(format!(
                    "write {} bytes to {}",
                    content.len(),
                    args.path
                ));
                if !self.approver.approve(&request) {
                    return Ok("Write rejected by user; nothing was changed.".to_string());
                }

                tokio::fs::write(&args.path, &content)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", args.path))?;
                Ok(format!("Wrote {} bytes to {}", content.len(), args.path))
            }
            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected \"read\" or \"write\")"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::ApprovalRequest;
    use std::path::PathBuf;

    struct AlwaysApprove;
    impl Approver for AlwaysApprove {
        fn approve(&self, _request: &ApprovalRequest) -> bool {
            true
        }
    }

    fn tool_rooted_at(dir: PathBuf) -> FileTool {
        FileTool::new(Arc::new(Workspace::new(vec![dir])), Arc::new(AlwaysApprove))
    }

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let dir = std::env::temp_dir();
        let path = dir.join("shion_file_tool_test.txt");
        let path_str = path.to_string_lossy().to_string();
        let tool = tool_rooted_at(dir);

        let write_args = json!({ "action": "write", "path": path_str, "content": "hello" });
        let out = tool.execute(write_args.to_string()).await.unwrap();
        assert!(out.contains("Wrote 5 bytes"));

        let read_args = json!({ "action": "read", "path": path_str });
        let content = tool.execute(read_args.to_string()).await.unwrap();
        assert_eq!(content, "hello");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn write_without_content_errors() {
        let dir = std::env::temp_dir();
        let path = dir
            .join("shion_no_content.txt")
            .to_string_lossy()
            .to_string();
        let tool = tool_rooted_at(dir);
        let args = json!({ "action": "write", "path": path });
        assert!(tool.execute(args.to_string()).await.is_err());
    }

    #[tokio::test]
    async fn path_outside_workspace_is_blocked() {
        let tool = tool_rooted_at(PathBuf::from("/home/user/project"));
        let args = json!({ "action": "read", "path": "/etc/passwd" });
        let err = tool.execute(args.to_string()).await.unwrap_err();
        assert!(err.to_string().contains("outside the workspace"));
    }
}
