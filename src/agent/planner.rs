use crate::domain::{
    planner::{Plan, Planner},
    session::Session,
};

/// Keyword-based router for v0.1.
/// Checks the last user message for known tool trigger words.
pub struct KeywordPlanner;

impl Planner for KeywordPlanner {
    fn plan(&self, session: &Session) -> Plan {
        let last_user_msg = session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == crate::domain::message::Role::User)
            .map(|m| m.content.to_lowercase());

        match last_user_msg.as_deref() {
            Some(text)
                if text.contains("time") || text.contains("时间") || text.contains("now") =>
            {
                Plan::CallTool {
                    tool_name: "time".to_string(),
                    input: String::new(),
                }
            }
            _ => Plan::RespondDirectly,
        }
    }
}
