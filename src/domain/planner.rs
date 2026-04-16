use super::session::Session;

#[derive(Debug, Clone)]
pub enum Plan {
    RespondDirectly,
    CallTool { tool_name: String, input: String },
    MultiStep { steps: Vec<Step> },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub tool_name: String,
    pub input: String,
}

pub trait Planner: Send + Sync {
    fn plan(&self, session: &Session) -> Plan;
}
