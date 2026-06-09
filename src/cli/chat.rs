use std::{
    io::{self, Write},
    sync::Arc,
};

use crate::{
    agent::{planner::KeywordPlanner, runtime::AgentRuntime},
    cli::approver::CliApprover,
    domain::{approval::Approver, workspace::Workspace},
    infra::{config::ModelConfig, db::Db, llm::build_llm},
    services::{skill_registry::SkillRegistry, tool_registry::ToolRegistry},
    tools::{
        delegate::DelegateTool, file::FileTool, memory::MemoryTool, shell::ShellTool,
        skill::SkillTool, time::TimeTool, web_fetch::WebFetchTool, web_search::WebSearchTool,
    },
};

pub async fn run(db_url: &str, session_id: &str) -> anyhow::Result<()> {
    let db = Arc::new(Db::connect(db_url).await?);
    let model_config = ModelConfig::from_env()?;
    let approver: Arc<dyn Approver> = Arc::new(CliApprover);
    // File operations are confined to the current working directory.
    let workspace = Arc::new(Workspace::current_dir()?);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TimeTool));
    tools.register(Arc::new(FileTool::new(workspace.clone(), approver.clone())));
    tools.register(Arc::new(ShellTool::new(
        workspace.clone(),
        approver.clone(),
    )));
    tools.register(Arc::new(WebFetchTool::new()));
    tools.register(Arc::new(WebSearchTool::new()));

    let memory_path = workspace
        .roots()
        .first()
        .cloned()
        .unwrap_or_default()
        .join(".shion_memory.jsonl");
    tools.register(Arc::new(MemoryTool::new(memory_path)));

    // The delegate tool runs a separate, tool-less sub-agent on the (optionally
    // cheaper) aux model.
    let sub_llm = build_llm(&model_config.aux_variant(), Vec::new(), None)?;
    tools.register(Arc::new(DelegateTool::new(sub_llm)));

    // Skills: instruction playbooks discovered from <workspace>/skills.
    let skills_dir = workspace
        .roots()
        .first()
        .cloned()
        .unwrap_or_default()
        .join("skills");
    let skills = Arc::new(SkillRegistry::load_from_dir(&skills_dir));
    let skills_note = (!skills.is_empty()).then(|| {
        format!(
            "You have skills (instruction playbooks) available. To use one, call the \
             `skill` tool with action=view and the skill name to load its instructions, \
             then follow them. Available skills:\n{}",
            skills.catalog()
        )
    });
    tools.register(Arc::new(SkillTool::new(skills.clone())));

    // Hand the same tool instances to the LLM so the model can call them.
    let llm = build_llm(&model_config, tools.tools(), skills_note)?;

    let runtime = AgentRuntime {
        planner: Box::new(KeywordPlanner),
        llm,
        tools,
        sessions: db.clone(),
        messages: db.clone(),
    };

    println!("Shion v0.1 — type your message, Ctrl-D to quit.\n");

    // Read one line at a time without holding the stdin lock across a turn, so
    // tools (e.g. the shell approval gate) can read stdin while a turn is in
    // flight.
    loop {
        let mut input = String::new();
        let bytes = io::stdin().read_line(&mut input)?;
        if bytes == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }
        println!("You: {}", input);
        let reply = runtime.handle_input(session_id, input).await?;
        println!("Agent: {}\n", reply);
        io::stdout().flush()?;
    }

    Ok(())
}
