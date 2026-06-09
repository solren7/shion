use async_trait::async_trait;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// JSON Schema describing this tool's arguments, exposed to the LLM for
    /// function calling. Defaults to "no arguments". Tools that take arguments
    /// override this and parse the matching JSON object from `execute`'s input.
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    /// Execute the tool. `input` carries the tool's arguments: a JSON object
    /// matching [`parameters_schema`](Tool::parameters_schema) when invoked by
    /// the LLM, or an empty string for argument-less tools.
    async fn execute(&self, input: String) -> anyhow::Result<String>;
}
