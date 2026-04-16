use std::{
    io::{self, BufRead, Write},
    sync::Arc,
};

use crate::{
    agent::{planner::KeywordPlanner, runtime::AgentRuntime},
    infra::db::Db,
    services::tool_registry::ToolRegistry,
    tools::time::TimeTool,
};

pub async fn run(db_url: &str, session_id: &str) -> anyhow::Result<()> {
    let db = Arc::new(Db::connect(db_url).await?);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(TimeTool));

    let runtime = AgentRuntime {
        planner: Box::new(KeywordPlanner),
        tools,
        sessions: db.clone(),
        messages: db.clone(),
    };

    println!("Shion v0.1 — type your message, Ctrl-D to quit.\n");

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let input = line?;
        if input.trim().is_empty() {
            continue;
        }
        println!("You: {}", input);
        let reply = runtime.handle_input(session_id, input).await?;
        println!("Agent: {}\n", reply);
        io::stdout().flush()?;
    }

    Ok(())
}
