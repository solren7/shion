use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    approval::{ApprovalRequest, Approver},
    tool::Tool,
    workspace::Workspace,
};

/// Command substrings treated as high-risk. Matching commands are flagged as
/// dangerous in the approval prompt.
const DANGEROUS_PATTERNS: &[&str] = &[
    "rm ",
    "rm -",
    "rmdir",
    "unlink",
    "git push",
    "git reset --hard",
    "git clean",
    "git branch -d",
    "git checkout --",
    "dd ",
    "mkfs",
    "sudo ",
    "shutdown",
    "reboot",
    "kill ",
    "killall",
    "chmod ",
    "chown ",
    "truncate",
    "> /dev/",
    "mv ",
    ":(){",
];

fn dangerous_pattern(command: &str) -> Option<&'static str> {
    let lc = command.to_lowercase();
    DANGEROUS_PATTERNS.iter().copied().find(|p| lc.contains(p))
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

/// Runs a shell command via `sh -c`, gated behind an [`Approver`]. Dangerous
/// commands (deletes, `git push`, `sudo`, ...) are flagged prominently. Runs
/// with the working directory set to the workspace root.
pub struct ShellTool {
    workspace: Arc<Workspace>,
    approver: Arc<dyn Approver>,
}

impl ShellTool {
    pub fn new(workspace: Arc<Workspace>, approver: Arc<dyn Approver>) -> Self {
        Self {
            workspace,
            approver,
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command on the local machine via `sh -c` and return its \
         combined stdout/stderr. Every invocation requires user approval; \
         destructive commands require an explicit dangerous-action confirmation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run, e.g. `ls -la`."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: ShellArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid shell arguments: {e}"))?;

        // Approval gate, escalated for destructive commands.
        let summary = format!("run shell command: {}", args.command);
        let request = match dangerous_pattern(&args.command) {
            Some(pattern) => ApprovalRequest::dangerous(
                summary,
                format!("matched dangerous pattern `{pattern}`"),
            ),
            None => ApprovalRequest::normal(summary),
        };
        if !self.approver.approve(&request) {
            return Ok("Command rejected by user; nothing was run.".to_string());
        }

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&args.command);
        if let Some(root) = self.workspace.roots().first() {
            cmd.current_dir(root);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to spawn command: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());

        let mut result = format!("exit status: {status}");
        if !stdout.trim().is_empty() {
            result.push_str(&format!("\n--- stdout ---\n{}", stdout.trim_end()));
        }
        if !stderr.trim().is_empty() {
            result.push_str(&format!("\n--- stderr ---\n{}", stderr.trim_end()));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::Risk;
    use std::sync::Mutex;

    struct AlwaysApprove;
    impl Approver for AlwaysApprove {
        fn approve(&self, _request: &ApprovalRequest) -> bool {
            true
        }
    }

    struct AlwaysReject;
    impl Approver for AlwaysReject {
        fn approve(&self, _request: &ApprovalRequest) -> bool {
            false
        }
    }

    /// Records the risk level of the last request it saw.
    struct Recording(Mutex<Option<Risk>>);
    impl Approver for Recording {
        fn approve(&self, request: &ApprovalRequest) -> bool {
            *self.0.lock().unwrap() = Some(request.risk);
            true
        }
    }

    fn workspace() -> Arc<Workspace> {
        Arc::new(Workspace::new(vec![std::env::temp_dir()]))
    }

    #[tokio::test]
    async fn approved_command_runs() {
        let tool = ShellTool::new(workspace(), Arc::new(AlwaysApprove));
        let out = tool
            .execute(json!({ "command": "echo hello" }).to_string())
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("exit status: 0"));
    }

    #[tokio::test]
    async fn rejected_command_does_not_run() {
        let tool = ShellTool::new(workspace(), Arc::new(AlwaysReject));
        let out = tool
            .execute(json!({ "command": "echo should_not_appear" }).to_string())
            .await
            .unwrap();
        assert!(out.contains("rejected"));
        assert!(!out.contains("should_not_appear"));
    }

    #[tokio::test]
    async fn dangerous_commands_are_flagged() {
        for cmd in ["rm -rf foo", "git push origin main", "sudo reboot"] {
            let rec = Arc::new(Recording(Mutex::new(None)));
            let tool = ShellTool::new(workspace(), rec.clone());
            let _ = tool.execute(json!({ "command": cmd }).to_string()).await;
            assert_eq!(*rec.0.lock().unwrap(), Some(Risk::Dangerous), "cmd: {cmd}");
        }
    }

    #[tokio::test]
    async fn safe_commands_are_normal_risk() {
        let rec = Arc::new(Recording(Mutex::new(None)));
        let tool = ShellTool::new(workspace(), rec.clone());
        let _ = tool
            .execute(json!({ "command": "echo hi" }).to_string())
            .await;
        assert_eq!(*rec.0.lock().unwrap(), Some(Risk::Normal));
    }
}
