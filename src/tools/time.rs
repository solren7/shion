use async_trait::async_trait;
use time::format_description::well_known::Rfc3339;

use crate::domain::tool::Tool;

pub struct TimeTool;

#[async_trait]
impl Tool for TimeTool {
    fn name(&self) -> &'static str {
        "time"
    }

    fn description(&self) -> &'static str {
        "Returns the current UTC date and time in RFC 3339 format."
    }

    async fn execute(&self, _input: String) -> anyhow::Result<String> {
        let s = time::OffsetDateTime::now_utc().format(&Rfc3339)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn time_tool_returns_non_empty_string() {
        let result = TimeTool.execute(String::new()).await.unwrap();
        assert!(!result.is_empty());
    }
}
