use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::{
    domain::{
        llm::LlmClient,
        message::Message,
        planner::{Plan, Planner},
        repository::{MessageRepository, SessionRepository},
        reviewer::Reviewer,
        session::Session,
    },
    services::tool_registry::ToolRegistry,
};

pub struct AgentRuntime {
    pub planner: Box<dyn Planner>,
    pub llm: Arc<dyn LlmClient>,
    pub tools: ToolRegistry,
    pub sessions: Arc<dyn SessionRepository>,
    pub messages: Arc<dyn MessageRepository>,
    pub reviewer: Option<Arc<dyn Reviewer>>,
    pub review_interval: usize,
}

impl AgentRuntime {
    pub async fn handle_input(
        &self,
        session_id: &str,
        user_input: String,
    ) -> anyhow::Result<String> {
        // Load or create session.
        let mut session = match self.sessions.find(session_id).await? {
            Some(s) => s,
            None => {
                let s = Session::new(session_id);
                self.sessions.save(&s).await?;
                s
            }
        };

        let user_msg = Message::user(&user_input);
        self.messages.save(session_id, &user_msg).await?;
        session.messages.push(user_msg);

        let plan = self.planner.plan(&session);
        debug!(?plan, "planner decision");

        let reply = match plan {
            Plan::RespondDirectly => self.llm.complete(&session).await?,
            Plan::CallTool { tool_name, input } => {
                info!(tool = %tool_name, "executing tool");
                let output = self.tools.execute(&tool_name, input).await?;
                let tool_msg = Message::tool(&output);
                self.messages.save(session_id, &tool_msg).await?;
                session.messages.push(tool_msg);
                output
            }
            Plan::MultiStep { steps } => {
                let mut last = String::new();
                for step in steps {
                    info!(tool = %step.tool_name, "executing step");
                    let output = self.tools.execute(&step.tool_name, step.input).await?;
                    let tool_msg = Message::tool(&output);
                    self.messages.save(session_id, &tool_msg).await?;
                    session.messages.push(tool_msg);
                    last = output;
                }
                last
            }
        };

        let assistant_msg = Message::assistant(&reply);
        self.messages.save(session_id, &assistant_msg).await?;
        session.messages.push(assistant_msg);

        if let Some(reviewer) = &self.reviewer {
            let interval = self.review_interval.max(1);
            if session.user_turns() % interval == 0 {
                let reviewer = reviewer.clone();
                let snapshot = session.clone();
                tokio::spawn(async move {
                    match reviewer.review(&snapshot).await {
                        Ok(outcome) if !outcome.is_empty() => {
                            info!(?outcome, "self-improvement review")
                        }
                        Ok(_) => {}
                        Err(error) => warn!(%error, "review failed (non-fatal)"),
                    }
                });
            }
        }

        Ok(reply)
    }
}
