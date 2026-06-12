//! The `task` tool: durable cross-session tasks (roadmap §2).
//!
//! Four actions, deliberately minimal: `capture` collects into the inbox,
//! `list` shows open tasks, `update` retriages (status / due / waiting_on),
//! `complete` closes. No `plan_today` — daily planning is the briefing
//! sweep's job, where the model reads this list and organizes it itself.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{
    task::{Task, TaskRepository, TaskStatus, parse_task_status},
    tool::Tool,
};

#[derive(Deserialize)]
struct TaskArgs {
    action: String,
    title: Option<String>,
    note: Option<String>,
    status: Option<String>,
    waiting_on: Option<String>,
    due: Option<String>,
    id: Option<String>,
}

pub struct TaskTool {
    tasks: Arc<dyn TaskRepository>,
}

impl TaskTool {
    pub fn new(tasks: Arc<dyn TaskRepository>) -> Self {
        Self { tasks }
    }
}

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

fn parse_due(due: &str) -> anyhow::Result<i64> {
    chrono::DateTime::parse_from_rfc3339(due)
        .map(|dt| dt.timestamp())
        .map_err(|e| anyhow::anyhow!("invalid `due` time `{due}` (expected RFC3339): {e}"))
}

/// One task as a display line (shared by list responses and confirmations).
fn render(task: &Task) -> String {
    let mut line = format!("{} [{}] {}", task.id, task.status.as_str(), task.title);
    if !task.waiting_on.is_empty() {
        line.push_str(&format!(" (waiting on: {})", task.waiting_on));
    }
    if let Some(due) = task.due_at {
        line.push_str(&format!(" (due {})", local_time(due)));
    }
    if !task.note.is_empty() {
        line.push_str(&format!(" — {}", task.note));
    }
    line
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Durable task list that persists across sessions (unlike this conversation). \
         action=\"capture\" collects a task or idea into the inbox (use status=\"todo\" \
         when it is already actionable, waiting_on when it is a commitment to/from \
         someone); action=\"list\" shows open tasks; action=\"update\" changes \
         status/due/waiting_on/title/note by id; action=\"complete\" marks a task done. \
         Tasks with a due time are delivered as notifications by the gateway."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["capture", "list", "update", "complete"],
                    "description": "The task operation."
                },
                "title": {
                    "type": "string",
                    "description": "Short task title (action=capture; optional rename on update)."
                },
                "note": {
                    "type": "string",
                    "description": "Free-form details (optional)."
                },
                "status": {
                    "type": "string",
                    "enum": ["inbox", "todo", "waiting", "done", "cancelled"],
                    "description": "Task status. capture defaults to \"inbox\"; use \"waiting\" with waiting_on when blocked on someone."
                },
                "waiting_on": {
                    "type": "string",
                    "description": "Who this task waits on / was promised to (optional)."
                },
                "due": {
                    "type": "string",
                    "description": "RFC3339 due time, e.g. \"2026-06-20T18:00:00+08:00\" (optional)."
                },
                "id": {
                    "type": "string",
                    "description": "Task id (action=update/complete)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: TaskArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid task arguments: {e}"))?;

        match args.action.as_str() {
            "capture" => {
                let title = args
                    .title
                    .ok_or_else(|| anyhow::anyhow!("`title` is required for action=capture"))?;
                let mut task = Task::new(title);
                if let Some(note) = args.note {
                    task.note = note;
                }
                if let Some(status) = args.status {
                    task.status = parse_task_status(&status)?;
                }
                if let Some(waiting_on) = args.waiting_on {
                    task.waiting_on = waiting_on;
                }
                if let Some(due) = args.due {
                    task.due_at = Some(parse_due(&due)?);
                }
                self.tasks.save(&task).await?;
                Ok(format!("Captured: {}", render(&task)))
            }

            "list" => {
                let mut open = self.tasks.list_open().await?;
                if let Some(status) = args.status {
                    let wanted = parse_task_status(&status)?;
                    open.retain(|t| t.status == wanted);
                }
                if open.is_empty() {
                    return Ok("No open tasks.".to_string());
                }
                Ok(open.iter().map(render).collect::<Vec<_>>().join("\n"))
            }

            "update" => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("`id` is required for action=update"))?;
                let mut task = self
                    .tasks
                    .find(&id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no task with id `{id}`"))?;
                if let Some(title) = args.title {
                    task.title = title;
                }
                if let Some(note) = args.note {
                    task.note = note;
                }
                if let Some(status) = args.status {
                    task.status = parse_task_status(&status)?;
                    if task.status == TaskStatus::Done && task.completed_at.is_none() {
                        task.completed_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
                    }
                }
                if let Some(waiting_on) = args.waiting_on {
                    task.waiting_on = waiting_on;
                }
                if let Some(due) = args.due {
                    task.due_at = Some(parse_due(&due)?);
                    // A moved deadline should notify again.
                    task.due_notified_at = None;
                }
                self.tasks.update(&task).await?;
                Ok(format!("Updated: {}", render(&task)))
            }

            "complete" => {
                let id = args
                    .id
                    .ok_or_else(|| anyhow::anyhow!("`id` is required for action=complete"))?;
                let mut task = self
                    .tasks
                    .find(&id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no task with id `{id}`"))?;
                task.status = TaskStatus::Done;
                task.completed_at = Some(time::OffsetDateTime::now_utc().unix_timestamp());
                self.tasks.update(&task).await?;
                Ok(format!("Completed: {}", task.title))
            }

            other => Err(anyhow::anyhow!(
                "unknown action `{other}` (expected capture/list/update/complete)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory repository so tool behavior is testable without SQLite.
    #[derive(Default)]
    struct MemTasks {
        rows: Mutex<Vec<Task>>,
    }

    #[async_trait]
    impl TaskRepository for MemTasks {
        async fn save(&self, task: &Task) -> anyhow::Result<()> {
            self.rows.lock().unwrap().push(task.clone());
            Ok(())
        }
        async fn find(&self, id: &str) -> anyhow::Result<Option<Task>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == id)
                .cloned())
        }
        async fn list_open(&self) -> anyhow::Result<Vec<Task>> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status.is_open())
                .cloned()
                .collect())
        }
        async fn update(&self, task: &Task) -> anyhow::Result<()> {
            let mut rows = self.rows.lock().unwrap();
            let slot = rows
                .iter_mut()
                .find(|t| t.id == task.id)
                .ok_or_else(|| anyhow::anyhow!("not found"))?;
            *slot = task.clone();
            Ok(())
        }
    }

    fn tool() -> (TaskTool, Arc<MemTasks>) {
        let repo = Arc::new(MemTasks::default());
        (TaskTool::new(repo.clone()), repo)
    }

    #[tokio::test]
    async fn capture_defaults_to_inbox() {
        let (tool, repo) = tool();
        let reply = tool
            .execute(r#"{"action":"capture","title":"review PR"}"#.to_string())
            .await
            .unwrap();
        assert!(reply.contains("[inbox] review PR"), "{reply}");
        assert_eq!(repo.rows.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn capture_with_waiting_on_records_commitment() {
        let (tool, repo) = tool();
        tool.execute(
            r#"{"action":"capture","title":"weekly report","status":"waiting","waiting_on":"boss"}"#
                .to_string(),
        )
        .await
        .unwrap();
        let rows = repo.rows.lock().unwrap();
        assert_eq!(rows[0].status, TaskStatus::Waiting);
        assert_eq!(rows[0].waiting_on, "boss");
    }

    #[tokio::test]
    async fn complete_sets_done_and_completed_at() {
        let (tool, repo) = tool();
        tool.execute(r#"{"action":"capture","title":"x"}"#.to_string())
            .await
            .unwrap();
        let id = repo.rows.lock().unwrap()[0].id.clone();
        let reply = tool
            .execute(format!(r#"{{"action":"complete","id":"{id}"}}"#))
            .await
            .unwrap();
        assert!(reply.contains("Completed"), "{reply}");
        let rows = repo.rows.lock().unwrap();
        assert_eq!(rows[0].status, TaskStatus::Done);
        assert!(rows[0].completed_at.is_some());
    }

    #[tokio::test]
    async fn update_due_resets_notification_guard() {
        let (tool, repo) = tool();
        tool.execute(r#"{"action":"capture","title":"x"}"#.to_string())
            .await
            .unwrap();
        let id = {
            let mut rows = repo.rows.lock().unwrap();
            rows[0].due_notified_at = Some(100);
            rows[0].id.clone()
        };
        tool.execute(format!(
            r#"{{"action":"update","id":"{id}","due":"2099-01-01T09:00:00+08:00"}}"#
        ))
        .await
        .unwrap();
        let rows = repo.rows.lock().unwrap();
        assert!(rows[0].due_at.is_some());
        assert_eq!(rows[0].due_notified_at, None);
    }

    #[tokio::test]
    async fn list_filters_by_status_and_hides_closed() {
        let (tool, _repo) = tool();
        tool.execute(r#"{"action":"capture","title":"a"}"#.to_string())
            .await
            .unwrap();
        tool.execute(r#"{"action":"capture","title":"b","status":"todo"}"#.to_string())
            .await
            .unwrap();
        let reply = tool
            .execute(r#"{"action":"list","status":"todo"}"#.to_string())
            .await
            .unwrap();
        assert!(reply.contains("b"), "{reply}");
        assert!(!reply.contains("[inbox] a"), "{reply}");
    }

    #[tokio::test]
    async fn unknown_status_is_an_error() {
        let (tool, _repo) = tool();
        let err = tool
            .execute(r#"{"action":"capture","title":"x","status":"urgent"}"#.to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown task status"));
    }
}
