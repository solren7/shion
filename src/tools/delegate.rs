use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::{llm::LlmClient, message::Message, session::Session, tool::Tool};

#[derive(Deserialize)]
struct DelegateArgs {
    task: String,
}

/// Delegates a self-contained subtask to a fresh sub-agent (its own LLM, with
/// no tools) and returns the sub-agent's answer. Useful for focused side
/// questions without polluting the main conversation.
pub struct DelegateTool {
    llm: Arc<dyn LlmClient>,
}

impl DelegateTool {
    pub fn new(llm: Arc<dyn LlmClient>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &'static str {
        "Delegate a focused, self-contained subtask to a sub-agent and return \
         its result. Provide all needed context in `task`; the sub-agent does \
         not see the main conversation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Fully self-contained instruction for the sub-agent."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: DelegateArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid delegate arguments: {e}"))?;

        let mut session = Session::new("delegate");
        session.messages.push(Message::user(&args.task));
        self.llm.complete(&session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoLlm;
    #[async_trait]
    impl LlmClient for EchoLlm {
        async fn complete(&self, session: &Session) -> anyhow::Result<String> {
            let last = session.messages.last().unwrap();
            Ok(format!("sub-agent handled: {}", last.content))
        }
    }

    #[tokio::test]
    async fn delegates_task_to_sub_agent() {
        let tool = DelegateTool::new(Arc::new(EchoLlm));
        let out = tool
            .execute(json!({ "task": "summarize X" }).to_string())
            .await
            .unwrap();
        assert_eq!(out, "sub-agent handled: summarize X");
    }
}
