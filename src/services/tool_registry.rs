use std::collections::HashMap;

use crate::domain::{error::DomainError, tool::Tool};

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub async fn execute(&self, name: &str, input: String) -> anyhow::Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| DomainError::ToolNotFound(name.to_string()))?;
        tool.execute(input).await
    }

    pub fn tool_descriptions(&self) -> Vec<(&str, &str)> {
        self.tools.values().map(|t| (t.name(), t.description())).collect()
    }
}
