//! Shared construction of a fully-wired `AgentRuntime`.
//!
//! Both the chat REPL (`cli/chat.rs`) and the gateway (`cli/gateway.rs`) need
//! the same agent: identical tools, skills, LLM, and reviewer. The only thing
//! that differs is the `Approver` — interactive at a TTY vs. auto-deny in the
//! unattended gateway — so it is passed in.

use std::{path::PathBuf, sync::Arc};

use crate::{
    agent::{
        planner::KeywordPlanner, reviewer::ReflectiveReviewer, runtime::AgentRuntime,
        system_prompt::SystemPromptBuilder,
    },
    config::ModelConfig,
    domain::{
        approval::Approver,
        llm::LlmClient,
        memory::MemoryRepository,
        repository::{SessionRepository, SkillRepository},
        reviewer::Reviewer,
        workspace::Workspace,
    },
    infra::{
        db::Db,
        kanban::KanbanDb,
        llm::{PreambleFn, build_llm},
        memory_db::MemoryDb,
    },
    services::{skill_registry::SkillRegistry, tool_registry::ToolRegistry},
    tools::{
        delegate::DelegateTool, file::FileTool, memory::MemoryTool, reminder::ReminderTool,
        session::SessionTool, shell::ShellTool, skill::SkillTool, task::TaskTool, time::TimeTool,
        todo::TodoTool, web_fetch::WebFetchTool, web_search::WebSearchTool,
    },
};

/// A wired agent plus the handles background work needs (sessions for sweeping,
/// the reviewer the sweep invokes).
pub struct Wiring {
    pub runtime: AgentRuntime,
    pub sessions: Arc<dyn SessionRepository>,
    pub reviewer: Arc<dyn Reviewer>,
    /// The auxiliary (cheaper) LLM, reused by the daily briefing sweep.
    pub aux_llm: Arc<dyn LlmClient>,
    /// The markdown memory store, also read by the briefing sweep.
    pub memories: Arc<dyn MemoryRepository>,
}

/// Build the agent against `db` (sessions/messages/etc.) and `kanban` (durable
/// tasks, a separate file), gating side-effecting tools through `approver`.
pub async fn build(
    db: Arc<Db>,
    kanban: Arc<KanbanDb>,
    approver: Arc<dyn Approver>,
) -> anyhow::Result<Wiring> {
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
    tools.register(Arc::new(ReminderTool::new(db.clone())));
    tools.register(Arc::new(TaskTool::new(kanban.clone())));
    tools.register(Arc::new(TodoTool::new(db.clone())));

    // Memories live in their own SQLite file (~/.shion/memory.db), shared by the
    // `memory` tool, the reflective reviewer, the L1 pinned injection, and the
    // briefing sweep. On first run it seeds itself from any legacy markdown
    // memories under ~/.shion/memory/ (a one-time, no-op-once-populated import).
    let memory_db = MemoryDb::connect(&crate::config::default_memory_db_url()).await?;
    let imported = memory_db
        .import_legacy_markdown(&crate::config::shion_home().join("memory"))
        .await
        .unwrap_or(0);
    if imported > 0 {
        tracing::info!(imported, "migrated legacy markdown memories into memory.db");
    }
    let memory_repo: Arc<dyn MemoryRepository> = Arc::new(memory_db);
    tools.register(Arc::new(MemoryTool::new(memory_repo.clone())));

    // The delegate tool runs a separate, tool-less sub-agent on the (optionally
    // cheaper) aux model. It gets a minimal identity-only preamble — no tools,
    // skills, or project context — rebuilt per turn like the main agent.
    let aux_config = model_config.aux_variant();
    let aux_builder = Arc::new(SystemPromptBuilder::new(&aux_config));
    let aux_preamble: PreambleFn = Arc::new(move || aux_builder.build());
    // Aux/delegate sub-agents must not be fed the user's memory library.
    let aux_llm = build_llm(&aux_config, Vec::new(), aux_preamble, None)?;
    tools.register(Arc::new(DelegateTool::new(aux_llm.clone())));

    // Skills load from, in priority order (first to define a name wins):
    //   SHION_SKILLS_PATH (colon-separated), <workspace>/skills,
    //   <workspace>/.claude/skills, and the user-global ~/.claude/skills shared
    //   by general agents (Claude Agent Skills `SKILL.md` format).
    let env = crate::config::ShionEnv::load()?;
    let root = workspace.roots().first().cloned().unwrap_or_default();
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    if let Some(extra) = &env.skills_path {
        skill_dirs.extend(
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    skill_dirs.push(root.join("skills"));
    skill_dirs.push(root.join(".claude/skills"));
    if let Some(home) = dirs::home_dir() {
        skill_dirs.push(home.join(".claude/skills"));
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

    // Assemble the tiered system prompt: stable identity + tool-aware guidance
    // (gated on the tools actually loaded) + skills catalog, then the workspace
    // project-instruction file, then the day-precision volatile footer. Wrapped
    // in a factory so `complete` rebuilds it per turn (per session) rather than
    // freezing the date at process start — important for the long-lived gateway.
    let tool_names = tools.tools().iter().map(|t| t.name().to_string()).collect();
    let prompt_builder = Arc::new(
        SystemPromptBuilder::new(&model_config)
            .tools(tool_names)
            .skills_note(skills_note)
            .workspace_root(Some(root.clone())),
    );
    let preamble: PreambleFn = Arc::new(move || prompt_builder.build());

    // Hand the same tool instances to the LLM so the model can call them, plus
    // the memory store for L1 pinned injection (main agent only).
    let llm = build_llm(
        &model_config,
        tools.tools(),
        preamble,
        Some(memory_repo.clone()),
    )?;
    let skill_repo: Arc<dyn SkillRepository> = db.clone();
    let reviewer: Arc<dyn Reviewer> = Arc::new(ReflectiveReviewer::new(
        aux_llm.clone(),
        memory_repo.clone(),
        skill_repo,
        kanban.clone(),
    ));

    let runtime = AgentRuntime {
        planner: Box::new(KeywordPlanner),
        llm,
        tools,
        sessions: db.clone(),
        messages: db.clone(),
        reviewer: Some(reviewer.clone()),
        review_interval: env.review_interval.filter(|v| *v > 0).unwrap_or(10),
    };

    Ok(Wiring {
        runtime,
        sessions: db.clone(),
        reviewer,
        aux_llm,
        memories: memory_repo,
    })
}
