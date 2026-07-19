use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;

use crate::domain::{repository::SessionRepository, tool::Tool};

#[derive(Deserialize)]
struct SessionArgs {
    action: String,
}

/// Introspection over Komo's own stored conversation sessions. Lets the
/// model answer "how many sessions do you have" from the database instead of
/// reaching for shell commands like `tmux ls` or `who`.
pub struct SessionTool {
    sessions: Arc<dyn SessionRepository>,
}

impl SessionTool {
    pub fn new(sessions: Arc<dyn SessionRepository>) -> Self {
        Self { sessions }
    }
}

#[async_trait]
impl Tool for SessionTool {
    fn name(&self) -> &'static str {
        "session"
    }

    fn description(&self) -> &'static str {
        "Inspect Komo's own stored conversation sessions (this agent's chat \
         history database, NOT system/tmux/login sessions). action=\"count\" \
         returns how many sessions exist; action=\"list\" returns each \
         session's id, creation time, and message count."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["count", "list"],
                    "description": "count = total number of stored sessions; list = one line per session."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: SessionArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid session arguments: {e}"))?;
        let sessions = self.sessions.list().await?;

        match args.action.as_str() {
            "count" => Ok(format!("{} stored sessions", sessions.len())),
            "list" => {
                if sessions.is_empty() {
                    return Ok("no stored sessions".to_string());
                }
                let lines: Vec<String> = sessions
                    .iter()
                    .map(|s| {
                        let created = time::OffsetDateTime::from_unix_timestamp(s.created_at)
                            .ok()
                            .and_then(|t| t.format(&Rfc3339).ok())
                            .unwrap_or_else(|| s.created_at.to_string());
                        format!(
                            "{} | created {} | {} messages ({} user turns)",
                            s.id,
                            created,
                            s.messages.len(),
                            s.user_turns()
                        )
                    })
                    .collect();
                Ok(format!(
                    "{} sessions:\n{}",
                    sessions.len(),
                    lines.join("\n")
                ))
            }
            other => anyhow::bail!("unknown session action `{other}` (expected: count | list)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session::Session;

    struct FakeRepo(Vec<Session>);

    #[async_trait]
    impl SessionRepository for FakeRepo {
        async fn find(&self, _id: &str) -> anyhow::Result<Option<Session>> {
            Ok(None)
        }
        async fn find_windowed(&self, _id: &str, _limit: usize) -> anyhow::Result<Option<Session>> {
            Ok(None)
        }
        async fn list(&self) -> anyhow::Result<Vec<Session>> {
            Ok(self.0.clone())
        }
        async fn save(&self, _session: &Session) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn rotate(&self, _session_id: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn count_reports_number_of_sessions() {
        let repo = Arc::new(FakeRepo(vec![Session::new("a"), Session::new("b")]));
        let out = SessionTool::new(repo)
            .execute(r#"{"action":"count"}"#.to_string())
            .await
            .unwrap();
        assert_eq!(out, "2 stored sessions");
    }

    #[tokio::test]
    async fn list_includes_session_ids() {
        let repo = Arc::new(FakeRepo(vec![Session::new("abc-123")]));
        let out = SessionTool::new(repo)
            .execute(r#"{"action":"list"}"#.to_string())
            .await
            .unwrap();
        assert!(out.contains("abc-123"));
        assert!(out.contains("0 user turns"));
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let repo = Arc::new(FakeRepo(Vec::new()));
        let err = SessionTool::new(repo)
            .execute(r#"{"action":"drop"}"#.to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown session action"));
    }
}
