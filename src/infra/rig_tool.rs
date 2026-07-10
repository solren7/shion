use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig::{
    completion::ToolDefinition,
    tool::{ToolDyn, ToolError},
};

use crate::domain::tool::Tool;
use crate::services::tool_execution::ToolExecutionCore;

/// Adapts a shion [`Tool`] into a `rig` [`ToolDyn`] so the provider sees the
/// tool's schema (`name` + `definition`). Execution is driven by shion's own
/// loop (`AgentRuntime::run_agent_loop` → `ToolExecutor::execute_round`), not
/// by rig: the main agent runs one completion per round and dispatches the
/// requested tools itself. `call` below stays as the trait-required fallback
/// for any rig-driven completion (the tool-less aux `complete()` path registers
/// no tools, so it is currently never invoked) — it delegates to the **same**
/// execution core the runtime's executor uses, so there is exactly one
/// execution semantics (retry/ledger/cap) no matter which path runs a tool.
pub struct RigTool {
    tool: Arc<dyn Tool>,
    core: Arc<ToolExecutionCore>,
}

impl RigTool {
    pub fn new(tool: Arc<dyn Tool>, core: Arc<ToolExecutionCore>) -> Self {
        Self { tool, core }
    }
}

impl ToolDyn for RigTool {
    fn name(&self) -> String {
        self.tool.name().to_string()
    }

    fn definition(
        &self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = ToolDefinition> + Send + '_>> {
        let name = self.tool.name().to_string();
        let description = self.tool.description().to_string();
        let parameters = self.tool.parameters_schema();
        Box::pin(async move {
            ToolDefinition {
                name,
                description,
                parameters,
            }
        })
    }

    fn call(
        &self,
        args: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let tool = self.tool.clone();
        let core = self.core.clone();
        Box::pin(async move {
            // Trait-required, but not on shion's hot path: `run_agent_loop` owns
            // the loop and dispatches tools itself, so rig only reaches here if it
            // drives a completion that has tools attached (none today). Kept
            // functional rather than `unreachable!` so that path stays correct.
            // `args` is the JSON arguments object produced by the model, matching
            // the tool's `parameters_schema`.
            core.execute_fallback(tool, args)
                .await
                .map_err(|e| ToolError::ToolCallError(format!("{e:#}").into()))
        })
    }
}
