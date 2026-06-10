//! Shared construction of a fully-wired `AgentRuntime`.
//!
//! Both the chat REPL (`cli/chat.rs`) and the gateway (`cli/gateway.rs`) need
//! the same agent: identical tools, skills, LLM, and reviewer. The only thing
//! that differs is the `Approver` — interactive at a TTY vs. auto-deny in the
//! unattended gateway — so it is passed in.

use std::{path::PathBuf, sync::Arc};

use crate::{
    agent::{planner::KeywordPlanner, reviewer::ReflectiveReviewer, runtime::AgentRuntime},
    config::ModelConfig,
    domain::{
        approval::Approver,
        memory::MemoryRepository,
        repository::{SessionRepository, SkillRepository},
        reviewer::Reviewer,
        workspace::Workspace,
    },
    infra::{db::Db, llm::build_llm},
    services::{skill_registry::SkillRegistry, tool_registry::ToolRegistry},
    tools::{
        delegate::DelegateTool, file::FileTool, memory::MemoryTool, session::SessionTool,
        shell::ShellTool, skill::SkillTool, time::TimeTool, web_fetch::WebFetchTool,
        web_search::WebSearchTool,
    },
};

/// A wired agent plus the handles background work needs (sessions for sweeping,
/// the reviewer the sweep invokes).
pub struct Wiring {
    pub runtime: AgentRuntime,
    pub sessions: Arc<dyn SessionRepository>,
    pub reviewer: Arc<dyn Reviewer>,
}

/// Build the agent against `db`, gating side-effecting tools through `approver`.
pub async fn build(db: Arc<Db>, approver: Arc<dyn Approver>) -> anyhow::Result<Wiring> {
    let model_config = ModelConfig::from_env()?;
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
    tools.register(Arc::new(SessionTool::new(db.clone())));

    let memory_path = workspace
        .roots()
        .first()
        .cloned()
        .unwrap_or_default()
        .join(".shion_memory.jsonl");
    tools.register(Arc::new(MemoryTool::new(memory_path)));

    // The delegate tool runs a separate, tool-less sub-agent on the (optionally
    // cheaper) aux model.
    let aux_llm = build_llm(&model_config.aux_variant(), Vec::new(), None)?;
    tools.register(Arc::new(DelegateTool::new(aux_llm.clone())));

    // Skills load from, in priority order (first to define a name wins):
    //   SHION_SKILLS_PATH (colon-separated), <workspace>/skills,
    //   <workspace>/.claude/skills, and the user-global ~/.claude/skills shared
    //   by general agents (Claude Agent Skills `SKILL.md` format).
    let root = workspace.roots().first().cloned().unwrap_or_default();
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(extra) = std::env::var("SHION_SKILLS_PATH") {
        skill_dirs.extend(
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    skill_dirs.push(root.join("skills"));
    skill_dirs.push(root.join(".claude/skills"));
    if let Ok(home) = std::env::var("HOME") {
        skill_dirs.push(PathBuf::from(home).join(".claude/skills"));
    }
    let skills = Arc::new(SkillRegistry::load_from_dirs(&skill_dirs));

    // Keep the always-on preamble small: list a bounded catalog, the rest is
    // discoverable on demand via the `skill` tool.
    const SKILL_CATALOG_CAP: usize = 30;
    let skills_note = (!skills.is_empty()).then(|| {
        format!(
            "You have skills (instruction playbooks) available. To use one, call the \
             `skill` tool with action=view and the skill name to load its instructions, \
             then follow them. Available skills:\n{}",
            skills.catalog_capped(SKILL_CATALOG_CAP)
        )
    });
    tools.register(Arc::new(SkillTool::new(skills.clone())));

    // Hand the same tool instances to the LLM so the model can call them.
    let llm = build_llm(&model_config, tools.tools(), skills_note)?;
    let memory_repo: Arc<dyn MemoryRepository> = db.clone();
    let skill_repo: Arc<dyn SkillRepository> = db.clone();
    let reviewer: Arc<dyn Reviewer> =
        Arc::new(ReflectiveReviewer::new(aux_llm, memory_repo, skill_repo));

    let runtime = AgentRuntime {
        planner: Box::new(KeywordPlanner),
        llm,
        tools,
        sessions: db.clone(),
        messages: db.clone(),
        reviewer: Some(reviewer.clone()),
        review_interval: review_interval_from_env(),
    };

    Ok(Wiring {
        runtime,
        sessions: db.clone(),
        reviewer,
    })
}

pub fn review_interval_from_env() -> usize {
    std::env::var("SHION_REVIEW_INTERVAL")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(10)
}
