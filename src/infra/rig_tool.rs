use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig::{
    completion::ToolDefinition,
    tool::{ToolDyn, ToolError},
};

use crate::domain::tool::Tool;

/// Adapts a shion [`Tool`] into a `rig` [`ToolDyn`] so the LLM can call it
/// directly via function calling. The same tool instance is shared (via `Arc`)
/// with the keyword-routed [`ToolRegistry`](crate::services::tool_registry::ToolRegistry).
pub struct RigTool(pub Arc<dyn Tool>);

impl ToolDyn for RigTool {
    fn name(&self) -> String {
        self.0.name().to_string()
    }

    fn definition(
        &self,
        _prompt: String,
    ) -> Pin<Box<dyn Future<Output = ToolDefinition> + Send + '_>> {
        let name = self.0.name().to_string();
        let description = self.0.description().to_string();
        let parameters = self.0.parameters_schema();
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
        let tool = self.0.clone();
        Box::pin(async move {
            // `args` is the JSON arguments object produced by the model, matching
            // the tool's `parameters_schema`. Pass it through; each tool parses
            // its own arguments (argument-less tools simply ignore it).
            crate::services::tool_registry::execute_isolated(tool, args)
                .await
                .map_err(|e| ToolError::ToolCallError(format!("{e:#}").into()))
        })
    }
}
