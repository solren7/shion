use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::{error::DomainError, tool::Tool};

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
    match tokio::spawn(async move { tool.execute(input).await }).await {
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
