//! The `todo` tool: the agent's working focus list for the *current session*
//! (roadmap §2/§8). Distinct from `task` (durable, cross-session): a todo dies
//! with the conversation. Shaped after hermes' `todo_tool` / Claude Code's
//! `TodoWrite` — full-list replace on write, list order is priority, at most one
//! item `in_progress`.
//!
//! The session is read from the ambient turn context (`current_session`), the
//! same task-local the chat approver uses. With no session in context (aux
//! sub-agents, maintenance sweeps) the tool is inert.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    domain::{
        todo::{SessionTodoRepository, TodoItem, TodoStatus, parse_todo_status},
        tool::Tool,
    },
    services::tool_registry::current_session,
};

#[derive(Deserialize)]
struct TodoArgs {
    /// Present → write (replace the whole list). Absent → read.
    todos: Option<Vec<TodoInput>>,
}

#[derive(Deserialize)]
struct TodoInput {
    content: String,
    status: Option<String>,
    #[serde(default)]
    active_form: String,
}

pub struct TodoTool {
    todos: Arc<dyn SessionTodoRepository>,
}

impl TodoTool {
    pub fn new(todos: Arc<dyn SessionTodoRepository>) -> Self {
        Self { todos }
    }
}

/// Render the list plus a one-line summary, the model's view after any op.
fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Todo list is empty.".to_string();
    }
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        let mark = match item.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
            TodoStatus::Cancelled => "[-]",
        };
        out.push_str(&format!("{}. {} {}\n", i + 1, mark, item.content));
    }
    let active = items.iter().filter(|t| t.status.is_active()).count();
    let in_progress = items
        .iter()
        .filter(|t| t.status == TodoStatus::InProgress)
        .count();
    out.push_str(&format!(
        "({} items, {active} active, {in_progress} in progress)",
        items.len()
    ));
    out
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Working task list for THIS conversation only (use `task` for things that \
         must outlive the session). Call with no arguments to read the current list. \
         Pass `todos` to replace the whole list — send every item each time with its \
         latest status. List order is priority. Keep at most ONE item in_progress; \
         mark an item completed as soon as it is done, and cancel one that no longer \
         applies. Use it for multi-step work (3+ steps) so the user can see progress."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full todo list (replaces the previous one). Omit to read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative step description, e.g. \"Write the parser\"."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Defaults to pending."
                            },
                            "active_form": {
                                "type": "string",
                                "description": "Present-continuous form shown while running, e.g. \"Writing the parser\" (optional)."
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let Some(ctx) = current_session() else {
            return Ok(
                "The todo tool is only available inside a conversation; nothing to track here."
                    .to_string(),
            );
        };
        let session_id = ctx.session_id;

        // Empty input (argument-less call) and `{}` both mean read.
        let args: TodoArgs = if input.trim().is_empty() {
            TodoArgs { todos: None }
        } else {
            serde_json::from_str(&input)
                .map_err(|e| anyhow::anyhow!("invalid todo arguments: {e}"))?
        };

        let Some(inputs) = args.todos else {
            // Read.
            let items = self.todos.get(&session_id).await?;
            return Ok(render(&items));
        };

        // Write: build the new list, validating as we go.
        let mut items = Vec::with_capacity(inputs.len());
        for input in inputs {
            let status = match input.status {
                Some(s) => parse_todo_status(&s)?,
                None => TodoStatus::Pending,
            };
            items.push(TodoItem {
                content: input.content,
                status,
                active_form: input.active_form,
            });
        }

        let in_progress = items
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(anyhow::anyhow!(
                "only one todo item can be in_progress at a time (got {in_progress})"
            ));
        }

        self.todos.set(&session_id, &items).await?;
        Ok(render(&items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tool_registry::{SessionContext, with_session};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemTodos(Mutex<std::collections::HashMap<String, Vec<TodoItem>>>);

    #[async_trait]
    impl SessionTodoRepository for MemTodos {
        async fn get(&self, session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default())
        }
        async fn set(&self, session_id: &str, items: &[TodoItem]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(session_id.to_string(), items.to_vec());
            Ok(())
        }
        async fn clear(&self, session_id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(session_id);
            Ok(())
        }
    }

    fn ctx(session: &str) -> SessionContext {
        SessionContext::detached(session)
    }

    #[tokio::test]
    async fn write_then_read_roundtrips_in_session() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo.clone());
        with_session(ctx("s1"), async {
            let out = tool
                .execute(
                    r#"{"todos":[{"content":"step one","status":"in_progress"},{"content":"step two"}]}"#
                        .to_string(),
                )
                .await
                .unwrap();
            assert!(out.contains("step one"), "{out}");
            let read = tool.execute(String::new()).await.unwrap();
            assert!(read.contains("step two"), "{read}");
            assert!(read.contains("1 in progress"), "{read}");
        })
        .await;
    }

    #[tokio::test]
    async fn rejects_two_in_progress() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo);
        with_session(ctx("s1"), async {
            let err = tool
                .execute(
                    r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#
                        .to_string(),
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("one todo item"), "{err}");
        })
        .await;
    }

    #[tokio::test]
    async fn inert_without_session_context() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo);
        // No `with_session` wrapper → current_session() is None.
        let out = tool.execute(String::new()).await.unwrap();
        assert!(
            out.contains("only available inside a conversation"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn write_replaces_whole_list() {
        let repo = Arc::new(MemTodos::default());
        let tool = TodoTool::new(repo.clone());
        with_session(ctx("s1"), async {
            tool.execute(r#"{"todos":[{"content":"a"},{"content":"b"}]}"#.to_string())
                .await
                .unwrap();
            tool.execute(r#"{"todos":[{"content":"c","status":"completed"}]}"#.to_string())
                .await
                .unwrap();
            let items = repo.get("s1").await.unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].content, "c");
        })
        .await;
    }
}
