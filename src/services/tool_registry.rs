use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::{error::DomainError, gateway::ReplySink, tool::Tool};

/// Ambient context for the turn a tool is executing within: which session it
/// belongs to and how to talk back to that conversation. Set by the gateway
/// dispatcher around a turn (`agent::interaction`) and read by a chat-channel
/// approver when a tool needs mid-execution approval.
///
/// It rides a task-local rather than the tool's argument string because rig's
/// `ToolDyn::call` signature is fixed — we can't thread it through the LLM
/// tool-call path. `execute_isolated` re-establishes it across its `spawn`.
#[derive(Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub sink: Arc<dyn ReplySink>,
}

tokio::task_local! {
    static SESSION: SessionContext;
}

/// Run `future` with `ctx` as the ambient session context.
pub async fn with_session<F: std::future::Future>(ctx: SessionContext, future: F) -> F::Output {
    SESSION.scope(ctx, future).await
}

/// The ambient session context, if the current task is running inside one.
/// `None` for the REPL, aux sub-agents, and maintenance sweeps.
pub fn current_session() -> Option<SessionContext> {
    SESSION.try_with(|c| c.clone()).ok()
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// All registered tools, shared via `Arc` (e.g. to hand to the LLM agent).
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().cloned().collect()
    }

    pub async fn execute(&self, name: &str, input: String) -> anyhow::Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| DomainError::ToolNotFound(name.to_string()))?
            .clone();
        execute_isolated(tool, input).await
    }

    pub fn tool_descriptions(&self) -> Vec<(&str, &str)> {
        self.tools
            .values()
            .map(|t| (t.name(), t.description()))
            .collect()
    }
}

/// Runs a tool on its own tokio task, isolated from the caller. This keeps
/// tool work off the chat task's thread and — because `JoinHandle` catches
/// panics — turns a panicking tool into an error reply instead of a process
/// exit. Used by both invocation paths: the keyword-routed registry above and
/// the LLM function-calling adapter (`infra::rig_tool::RigTool`).
pub async fn execute_isolated(tool: Arc<dyn Tool>, input: String) -> anyhow::Result<String> {
    let name = tool.name();
    // Carry the turn's session context into the spawned task; `tokio::spawn`
    // starts a fresh task that wouldn't otherwise inherit the task-local.
    let join = match current_session() {
        Some(ctx) => tokio::spawn(SESSION.scope(ctx, async move { tool.execute(input).await })),
        None => tokio::spawn(async move { tool.execute(input).await }),
    };
    match join.await {
        Ok(result) => result,
        Err(join_err) if join_err.is_panic() => {
            let panic = join_err.into_panic();
            let msg = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            Err(anyhow::anyhow!("tool `{name}` panicked: {msg}"))
        }
        Err(join_err) => Err(anyhow::anyhow!("tool `{name}` was cancelled: {join_err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn name(&self) -> &'static str {
            "boom"
        }
        fn description(&self) -> &'static str {
            "always panics"
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            panic!("kaboom");
        }
    }

    #[tokio::test]
    async fn panicking_tool_returns_error_instead_of_crashing() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(PanickingTool));
        let err = registry
            .execute("boom", String::new())
            .await
            .expect_err("panic should surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("panicked"), "unexpected error: {msg}");
        assert!(msg.contains("kaboom"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error() {
        let registry = ToolRegistry::new();
        assert!(registry.execute("nope", String::new()).await.is_err());
    }
}
