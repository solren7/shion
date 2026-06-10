use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    reminder::{Reminder, ReminderRepository, ReminderStatus},
    tool::Tool,
};

#[derive(Deserialize)]
struct ReminderArgs {
    action: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

pub struct ReminderTool {
    reminders: Arc<dyn ReminderRepository>,
}

impl ReminderTool {
    pub fn new(reminders: Arc<dyn ReminderRepository>) -> Self {
        Self { reminders }
    }
}

#[async_trait]
impl Tool for ReminderTool {
    fn name(&self) -> &'static str {
        "reminder"
    }

    fn description(&self) -> &'static str {
        "Schedule one-shot reminders delivered as desktop notifications by the \
         gateway process. action=\"create\" schedules a new reminder (requires \
         message + either after or at); action=\"list\" returns pending reminders; \
         action=\"cancel\" cancels a reminder by id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "cancel"],
                    "description": "The reminder operation."
                },
                "message": {
                    "type": "string",
                    "description": "Reminder text to deliver (action=create)."
                },
                "after": {
                    "type": "string",
                    "description": "Relative delay: \"45s\", \"5m\", \"2h\", \"1d\" (action=create, pick one of after/at)."
                },
                "at": {
                    "type": "string",
                    "description": "Absolute RFC3339 fire time, e.g. \"2025-01-01T09:00:00+08:00\" (action=create, pick one of after/at)."
                },
                "id": {
                    "type": "string",
                    "description": "Reminder id to cancel (action=cancel)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: ReminderArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid reminder arguments: {e}"))?;

        match args.action.as_str() {
            "create" => {
                let message = args
                    .message
                    .ok_or_else(|| anyhow::anyhow!("`message` is required for action=create"))?;

                let run_at = match (args.after, args.at) {
                    (Some(after), _) => {
                        let delay = parse_after(&after)?;
                        time::OffsetDateTime::now_utc().unix_timestamp() + delay.as_secs() as i64
                    }
                    (None, Some(at)) => {
                        let dt = chrono::DateTime::parse_from_rfc3339(&at)
                            .map_err(|e| anyhow::anyhow!("invalid `at` time `{at}`: {e}"))?;
                        dt.timestamp()
                    }
                    (None, None) => {
                        return Err(anyhow::anyhow!(
                            "either `after` or `at` is required for action=create"
                        ));
                    }
                };

                let reminder = Reminder::new(message, run_at);
                let id = reminder.id.clone();
                let fire_time = chrono::DateTime::from_timestamp(run_at, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| run_at.to_string());

                self.reminders.save(&reminder).await?;
                Ok(format!(
                    "Reminder {id} set for {fire_time}. \
                     Delivered by the gateway process — make sure `shion gateway` is running."
                ))
            }

            "list" => {
                let pending = self.reminders.list_pending().await?;
                if pending.is_empty() {
                    return Ok("No pending reminders.".to_string());
                }
                let lines: Vec<String> = pending
                    .iter()
                    .map(|r| {
                        let fire_time = chrono::DateTime::from_timestamp(r.run_at, 0)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_else(|| r.run_at.to_string());
                        format!("{}: {} (due {})", r.id, r.message, fire_time)
                    })
                    .collect();
                Ok(lines.join("\n"))
            }

            "cancel" => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("`id` is required for action=cancel"))?;
                self.reminders
                    .set_status(&id, ReminderStatus::Cancelled)
                    .await?;
                Ok(format!("Reminder {id} cancelled."))
            }

            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected create/list/cancel)"
            )),
        }
    }
}

/// Parse a relative duration string: `<number><unit>` where unit is s/m/h/d.
pub fn parse_after(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow::anyhow!("empty duration string"));
    }
    let (digits, unit) = s.split_at(s.len() - 1);
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration `{s}`: expected format like `5m`"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        other => {
            return Err(anyhow::anyhow!(
                "unknown unit `{other}` in `{s}` (expected s/m/h/d)"
            ));
        }
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── parse_after ───────────────────────────────────────────────────────────

    #[test]
    fn parse_after_supports_s_m_h_d() {
        assert_eq!(parse_after("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_after("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_after("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_after("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn parse_after_rejects_invalid() {
        assert!(parse_after("abc").is_err());
        assert!(parse_after("5x").is_err());
        assert!(parse_after("").is_err());
    }

    // ── FakeReminderRepository ────────────────────────────────────────────────

    #[derive(Default)]
    struct FakeRepo {
        reminders: Mutex<Vec<Reminder>>,
    }

    #[async_trait]
    impl ReminderRepository for FakeRepo {
        async fn save(&self, reminder: &Reminder) -> anyhow::Result<()> {
            self.reminders.lock().unwrap().push(reminder.clone());
            Ok(())
        }

        async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>> {
            Ok(self
                .reminders
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.status == ReminderStatus::Pending)
                .cloned()
                .collect())
        }

        async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()> {
            if let Some(r) = self
                .reminders
                .lock()
                .unwrap()
                .iter_mut()
                .find(|r| r.id == id)
            {
                r.status = status;
            }
            Ok(())
        }
    }

    fn tool() -> (ReminderTool, Arc<FakeRepo>) {
        let repo = Arc::new(FakeRepo::default());
        let t = ReminderTool::new(repo.clone() as Arc<dyn ReminderRepository>);
        (t, repo)
    }

    #[tokio::test]
    async fn reminder_tool_create_persists_pending() {
        let (t, repo) = tool();
        let result = t
            .execute(
                json!({"action": "create", "message": "drink water", "after": "1m"}).to_string(),
            )
            .await
            .unwrap();
        assert!(result.contains("set for"));
        assert!(result.contains("gateway"));
        let pending = repo.reminders.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, ReminderStatus::Pending);
        assert_eq!(pending[0].message, "drink water");
    }

    #[tokio::test]
    async fn reminder_tool_cancel_sets_status() {
        let (t, repo) = tool();
        t.execute(json!({"action": "create", "message": "foo", "after": "5m"}).to_string())
            .await
            .unwrap();
        let id = repo.reminders.lock().unwrap()[0].id.clone();
        t.execute(json!({"action": "cancel", "id": id}).to_string())
            .await
            .unwrap();
        assert_eq!(
            repo.reminders.lock().unwrap()[0].status,
            ReminderStatus::Cancelled
        );
    }
}
