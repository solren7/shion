use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::domain::{
    approval::{ActionRef, ApprovalRequest},
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
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

/// Commands that are never run, even with user approval (hermes calls this the
/// "hardline floor"): the blast radius is the whole machine, not the workspace.
const HARDLINE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "mkfs",
    "dd if=/dev/zero of=/dev/",
    "of=/dev/sd",
    "of=/dev/disk",
    ":(){",
    "shutdown",
    "reboot",
    "halt",
];

/// True if `pattern` occurs in `haystack` (already lowercased) at a command
/// boundary, not buried inside a larger alphanumeric word. A naive `contains`
/// flags `terraform apply` and `kill -TERM 1` as the `rm ` pattern, because
/// "rm " is a substring of "terrafo*rm* " and "-te*rm* 1". We require the char
/// before the match to be a non-alphanumeric (or start), and — when the pattern
/// ends in a letter/digit — the char after it likewise, so the pattern lines up
/// with a real token rather than the middle of one.
fn matches_at_boundary(haystack: &str, pattern: &str) -> bool {
    let bytes = haystack.as_bytes();
    let pat = pattern.as_bytes();
    let pattern_ends_alnum = pat.last().is_some_and(u8::is_ascii_alphanumeric);
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(pattern) {
        let at = from + rel;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let after = at + pat.len();
        let after_ok =
            !pattern_ends_alnum || after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        // The matched pattern starts on an ASCII byte, so `at + 1` is a valid
        // char boundary to resume the scan from.
        from = at + 1;
    }
    false
}

fn dangerous_pattern(command: &str) -> Option<&'static str> {
    let lc = command.to_lowercase();
    DANGEROUS_PATTERNS
        .iter()
        .copied()
        .find(|p| matches_at_boundary(&lc, p))
}

fn hardline_pattern(command: &str) -> Option<&'static str> {
    let lc = command.to_lowercase();
    HARDLINE_PATTERNS
        .iter()
        .copied()
        .find(|p| matches_at_boundary(&lc, p))
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

/// Markers that introduce a secret value as `marker=<secret>` (case-insensitive).
const SECRET_KEY_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "api-key=",
    "token=",
    "secret=",
    "password=",
    "passwd=",
    "pwd=",
    "access_key=",
    "auth=",
];

/// Flags whose *following* token is a secret (`--password hunter2`).
const SECRET_FLAGS: &[&str] = &["--password", "--token", "--api-key", "--secret", "-p"];

/// Upper bound on how many bytes of stdout/stderr each stream is read into
/// memory. Well above the LLM result cap (which truncates the model-facing text
/// anyway), so it never clips useful output — it only stops a command that
/// spews unbounded output (`cat` a huge file, `yes`) from OOMing the gateway.
/// Reading stops at the cap and the child is killed (`kill_on_drop`).
const MAX_STREAM_BYTES: u64 = 256 * 1024;

/// A token that "looks like" an opaque credential: long and a single run of
/// url-safe-ish characters with no shell punctuation.
fn looks_like_secret(token: &str) -> bool {
    token.len() >= 24
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '='))
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

/// Best-effort scrub of secret-looking substrings from a shell command before it
/// is written to the run ledger. Heuristic, dependency-free, whitespace-tokenized:
/// covers `key=value`, `Bearer <tok>`, `--password <tok>`, and high-entropy
/// tokens. The command structure stays readable; only the secret is replaced.
fn redact_secrets(command: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut scrub_next = false;
    for raw in command.split_whitespace() {
        if scrub_next {
            out.push("***".to_string());
            scrub_next = false;
            continue;
        }
        let lower = raw.to_lowercase();
        if lower == "bearer" || SECRET_FLAGS.contains(&lower.as_str()) {
            out.push(raw.to_string());
            scrub_next = true;
            continue;
        }
        if let Some(marker) = SECRET_KEY_MARKERS.iter().find(|m| lower.starts_with(**m)) {
            // Preserve the original-case key prefix, drop the value.
            out.push(format!("{}***", &raw[..marker.len()]));
            continue;
        }
        if looks_like_secret(raw) {
            out.push("***".to_string());
            continue;
        }
        out.push(raw.to_string());
    }
    out.join(" ")
}

/// Runs a shell command via `sh -c`, gated behind an [`Approver`]. Dangerous
/// commands (deletes, `git push`, `sudo`, ...) are flagged prominently. Runs
/// with the working directory set to the workspace root.
pub struct ShellTool {
    workspace: Arc<Workspace>,
}

impl ShellTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a shell command on the local machine via `sh -c` and return its \
         combined stdout/stderr. Safe (read-only) commands run without a \
         prompt; destructive commands require an explicit dangerous-action \
         confirmation, and a few catastrophic ones are always refused."
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

    /// Scrub secret-looking substrings from the command before it lands in the
    /// run ledger (the command itself is kept for audit; only secrets go).
    fn redact_args(&self, args: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(args) {
            Ok(mut v) => {
                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                    v["command"] = serde_json::json!(redact_secrets(cmd));
                }
                v.to_string()
            }
            Err(_) => "<shell args redacted>".to_string(),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs = parse_args(&input)?;

        // Hardline floor: catastrophic commands are refused outright — no
        // approval can unlock them.
        if let Some(pattern) = hardline_pattern(&args.command) {
            return Ok(ToolOutput::text(format!(
                "Command refused: matched hardline pattern `{pattern}`. \
                 This command is never run, even with approval. Do not retry it."
            )));
        }

        // Approval gate (hermes-style): commands matching a dangerous pattern
        // prompt the user; everything else is `Risk::Safe` and an interactive
        // approver lets it through without asking.
        let summary = format!("run shell command: {}", args.command);
        let action = ActionRef::Shell {
            command: args.command.clone(),
        };
        let request = match dangerous_pattern(&args.command) {
            Some(pattern) => ApprovalRequest::dangerous(
                summary,
                format!("matched dangerous pattern `{pattern}`"),
            )
            .with_scope_key(format!("shell:{pattern}"))
            .with_action(action),
            None => ApprovalRequest::safe(summary).with_action(action),
        };
        if !ctx.approve(&request).await {
            return Ok(ToolOutput::text(
                "Command rejected by user; nothing was run.",
            ));
        }

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&args.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the executor's wall-clock timeout aborts the task awaiting this
            // command, dropping the `Child` must kill the process — otherwise
            // `sh` (and its children) would be orphaned and keep running.
            .kill_on_drop(true);
        if let Some(root) = self.workspace.roots().first() {
            cmd.current_dir(root);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("failed to spawn command: {e}")))?;

        // Read both streams concurrently, each bounded to MAX_STREAM_BYTES, so a
        // command emitting unbounded output can't buffer the whole thing into
        // memory and OOM the gateway. `stdin(null)` above means a command that
        // reads stdin sees EOF instead of blocking forever waiting for input.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let read_out = async {
            let mut buf = Vec::new();
            if let Some(p) = out_pipe.as_mut() {
                let _ = p.take(MAX_STREAM_BYTES).read_to_end(&mut buf).await;
            }
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            if let Some(p) = err_pipe.as_mut() {
                let _ = p.take(MAX_STREAM_BYTES).read_to_end(&mut buf).await;
            }
            buf
        };
        let (out_bytes, err_bytes) = tokio::join!(read_out, read_err);
        let status = child
            .wait()
            .await
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("failed to await command: {e}")))?;

        let stdout = String::from_utf8_lossy(&out_bytes);
        let stderr = String::from_utf8_lossy(&err_bytes);
        let status = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());

        let clipped = |raw: &[u8]| (raw.len() as u64) >= MAX_STREAM_BYTES;
        let mut result = format!("exit status: {status}");
        if !stdout.trim().is_empty() {
            result.push_str(&format!("\n--- stdout ---\n{}", stdout.trim_end()));
            if clipped(&out_bytes) {
                result.push_str("\n…[stdout truncated at the output limit]");
            }
        }
        if !stderr.trim().is_empty() {
            result.push_str(&format!("\n--- stderr ---\n{}", stderr.trim_end()));
            if clipped(&err_bytes) {
                result.push_str("\n…[stderr truncated at the output limit]");
            }
        }
        Ok(ToolOutput::text(result).with_title(format!("shell: {}", args.command)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::{Approver, Risk};
    use crate::domain::context::{SessionContext, ToolContext};
    use std::sync::Mutex;

    fn ctx_with(approver: Arc<dyn Approver>) -> ToolContext {
        ToolContext::new(SessionContext::detached("cli:test"), None, approver)
    }

    struct AlwaysApprove;
    #[async_trait::async_trait]
    impl Approver for AlwaysApprove {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            true
        }
    }

    struct AlwaysReject;
    #[async_trait::async_trait]
    impl Approver for AlwaysReject {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            false
        }
    }

    /// Records the risk level of the last request it saw.
    struct Recording {
        risk: Mutex<Option<Risk>>,
        approve: bool,
    }

    #[async_trait::async_trait]
    impl Approver for Recording {
        async fn approve(&self, request: &ApprovalRequest) -> bool {
            *self.risk.lock().unwrap() = Some(request.risk);
            self.approve
        }
    }

    fn workspace() -> Arc<Workspace> {
        Arc::new(Workspace::new(vec![std::env::temp_dir()]))
    }

    #[tokio::test]
    async fn approved_command_runs() {
        let tool = ShellTool::new(workspace());
        let out = tool
            .call(
                json!({ "command": "echo hello" }),
                &ctx_with(Arc::new(AlwaysApprove)),
            )
            .await
            .unwrap();
        assert!(out.text.contains("hello"));
        assert!(out.text.contains("exit status: 0"));
    }

    #[tokio::test]
    async fn rejected_command_does_not_run() {
        let tool = ShellTool::new(workspace());
        let out = tool
            .call(
                json!({ "command": "rm -r should_not_appear" }),
                &ctx_with(Arc::new(AlwaysReject)),
            )
            .await
            .unwrap();
        assert!(out.text.contains("rejected"));
        assert!(!out.text.contains("--- stdout ---"));
    }

    #[tokio::test]
    async fn hardline_command_is_refused_without_consulting_approver() {
        let rec = Arc::new(Recording {
            risk: Mutex::new(None),
            approve: true,
        });
        let tool = ShellTool::new(workspace());
        let out = tool
            .call(
                json!({ "command": "sudo rm -rf / --no-preserve-root" }),
                &ctx_with(rec.clone()),
            )
            .await
            .unwrap();
        assert!(out.text.contains("refused"));
        // The approver was never asked: hardline sits above the approval gate.
        assert_eq!(*rec.risk.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn dangerous_commands_are_flagged() {
        for cmd in ["rm -rf foo", "git push origin main"] {
            let rec = Arc::new(Recording {
                risk: Mutex::new(None),
                approve: false,
            });
            let tool = ShellTool::new(workspace());
            let _ = tool
                .call(json!({ "command": cmd }), &ctx_with(rec.clone()))
                .await;
            assert_eq!(
                *rec.risk.lock().unwrap(),
                Some(Risk::Dangerous),
                "cmd: {cmd}"
            );
        }
    }

    #[test]
    fn dangerous_pattern_matches_at_command_boundary() {
        // Real dangerous commands still match.
        assert_eq!(dangerous_pattern("rm -rf foo"), Some("rm "));
        assert_eq!(dangerous_pattern("git push origin main"), Some("git push"));
        // ...including when chained after a shell separator.
        assert_eq!(dangerous_pattern("cd /tmp && rm -rf x"), Some("rm "));

        // `kill -TERM 1` is dangerous because of `kill `, NOT a stray `rm ` buried
        // in "-te*rm* 1" (the bug that mislabeled the prompt as `rm`).
        assert_eq!(dangerous_pattern("kill -TERM 1"), Some("kill "));

        // Innocuous commands that merely contain a pattern as a substring inside
        // a word must not be flagged.
        assert_eq!(dangerous_pattern("terraform apply"), None);
        assert_eq!(dangerous_pattern("echo perform task"), None);
    }

    #[test]
    fn redact_secrets_scrubs_common_shapes() {
        let cmd = "curl -H 'Authorization: Bearer sk-abc123def456ghi789' https://api.example.com";
        let r = redact_secrets(cmd);
        assert!(!r.contains("sk-abc123def456ghi789"));
        assert!(r.contains("Bearer"));

        let kv = redact_secrets("deploy --env api_key=AKIA1234567890SECRET token=zzz");
        assert!(!kv.contains("AKIA1234567890SECRET"));
        assert!(kv.contains("api_key=***"));

        let flag = redact_secrets("login --password hunter2longenoughxx");
        assert!(!flag.contains("hunter2longenoughxx"));
        assert!(flag.contains("--password ***"));

        let entropy = redact_secrets("echo ABCD1234efgh5678ijkl9012mnop");
        assert!(entropy.contains("***"));

        // Ordinary commands pass through untouched.
        assert_eq!(redact_secrets("ls -la /tmp"), "ls -la /tmp");
    }

    #[test]
    fn redact_args_scrubs_command_value() {
        let tool = ShellTool::new(workspace());
        let args = json!({ "command": "x token=supersecretvalue123456" }).to_string();
        let redacted = tool.redact_args(&args);
        assert!(!redacted.contains("supersecretvalue123456"));
    }

    #[tokio::test]
    async fn output_is_bounded_at_the_stream_limit() {
        // A command that emits more than the stream cap must be truncated, not
        // buffered whole into memory.
        let tool = ShellTool::new(workspace());
        let bytes = MAX_STREAM_BYTES + 50_000;
        let out = tool
            .call(
                json!({ "command": format!("yes a | head -c {bytes}") }),
                &ctx_with(Arc::new(AlwaysApprove)),
            )
            .await
            .unwrap();
        assert!(
            out.text.contains("stdout truncated"),
            "expected a truncation marker, got {} bytes",
            out.text.len()
        );
    }

    #[tokio::test]
    async fn command_reading_stdin_sees_eof_instead_of_hanging() {
        // stdin is wired to /dev/null, so a command that reads stdin gets EOF
        // and exits promptly rather than blocking forever waiting for input.
        let tool = ShellTool::new(workspace());
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tool.call(
                json!({ "command": "cat" }),
                &ctx_with(Arc::new(AlwaysApprove)),
            ),
        )
        .await
        .expect("cat must not hang on stdin")
        .unwrap();
        assert!(out.text.contains("exit status: 0"));
    }

    #[tokio::test]
    async fn safe_commands_are_safe_risk() {
        let rec = Arc::new(Recording {
            risk: Mutex::new(None),
            approve: true,
        });
        let tool = ShellTool::new(workspace());
        let _ = tool
            .call(json!({ "command": "echo hi" }), &ctx_with(rec.clone()))
            .await;
        assert_eq!(*rec.risk.lock().unwrap(), Some(Risk::Safe));
    }
}
