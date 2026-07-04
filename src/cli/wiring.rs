//! Shared construction of a fully-wired `AgentRuntime`.
//!
//! Both the chat REPL (`cli/chat.rs`) and the gateway (`cli/gateway.rs`) need
//! the same agent: identical tools, skills, LLM, and reviewer. The only thing
//! that differs is the `Approver` — interactive at a TTY vs. auto-deny in the
//! unattended gateway — so it is passed in.

use std::{path::PathBuf, sync::Arc};

use crate::{
    agent::{
        reviewer::ReflectiveReviewer, runtime::AgentRuntime, system_prompt::SystemPromptBuilder,
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
        llm::{PreambleFn, build_llm},
        memory::memory_db::MemoryDb,
        persistence::{db::Db, kanban::KanbanDb},
        skills::FsSkillStore,
    },
    services::{skill_registry::SkillRegistry, tool_registry::ToolRegistry},
    tools::{
        delegate::DelegateTool, file::FileTool, homeassistant::HomeAssistantTool,
        memory::MemoryTool, reminder::ReminderTool, session::SessionTool, shell::ShellTool,
        skill::SkillTool, task::TaskTool, time::TimeTool, todo::TodoTool, web_fetch::WebFetchTool,
        web_search::WebSearchTool,
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
    /// The governed skill store (`~/.shion/skills`, files — roadmap §9), shared
    /// with the gateway's api channel.
    pub skills: Arc<FsSkillStore>,
}

/// Build the agent against `db` (sessions/messages/etc.) and `kanban` (durable
/// tasks, a separate file), gating side-effecting tools through `approver`.
pub async fn build(
    db: Arc<Db>,
    kanban: Arc<KanbanDb>,
    approver: Arc<dyn Approver>,
) -> anyhow::Result<Wiring> {
    let model_config = ModelConfig::from_env()?;
    // Install the process-wide tool-result byte cap (the global backstop in
    // `execute_isolated`). Resolved like every other setting; first call wins,
    // which is fine — chat and gateway each build once.
    crate::services::tool_registry::set_tool_result_cap(model_config.max_tool_result_bytes);

    // Wrap the interactive approver in the configurable permission policy
    // (roadmap §3): the policy auto-allows / hard-denies per `[policy]` rules and
    // only escalates to `approver` when it says "ask". With no `[policy]` table
    // this is the empty policy — identical to the bare interactive approver.
    let approver = crate::agent::policy_approver::PolicyApprover::wrap(
        crate::config::policy_config(),
        approver,
    );

    // File operations are confined to the current working directory.
    let workspace = Arc::new(Workspace::current_dir()?);

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TimeTool));
    tools.register(Arc::new(FileTool::new(workspace.clone(), approver.clone())));
    tools.register(Arc::new(ShellTool::new(
        workspace.clone(),
        approver.clone(),
    )));
    tools.register(Arc::new(WebFetchTool::new(approver.clone())));
    tools.register(Arc::new(WebSearchTool::new()));
    tools.register(Arc::new(SessionTool::new(db.clone())));
    tools.register(Arc::new(ReminderTool::new(db.clone())));
    tools.register(Arc::new(TaskTool::new(kanban.clone())));
    tools.register(Arc::new(TodoTool::new(db.clone())));

    // Home Assistant tool, only when configured (HASS_TOKEN set; HASS_URL
    // optional, defaults to homeassistant.local:8123).
    if let Some(ha) = crate::config::homeassistant_config() {
        tools.register(Arc::new(HomeAssistantTool::new(
            ha.base_url,
            ha.token,
            approver.clone(),
        )));
    }

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
    // Aux/delegate sub-agents must not be fed the user's memory library — and
    // the aux agent never gets an aux of its own (no recursion).
    let aux_llm = build_llm(&aux_config, Vec::new(), aux_preamble, None, None)?;
    tools.register(Arc::new(DelegateTool::new(aux_llm.clone())));

    // The governed skill store: `~/.shion/skills` is the shion-owned home for
    // durable skills (files, not db — roadmap §9). Reviewer proposals land in
    // its `.candidates/` for triage; a one-time import moves any skills a
    // pre-filesystem shion accumulated in shion.db into that triage pile.
    let skill_store = Arc::new(FsSkillStore::new(FsSkillStore::default_root()));
    match db.export_legacy_skills().await {
        Ok(rows) if !rows.is_empty() => match skill_store.import_legacy_db(rows) {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "imported legacy shion.db skills as candidates"),
            Err(error) => tracing::warn!(%error, "legacy skill import failed"),
        },
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "failed to read legacy db skills"),
    }

    // Skills load from, in priority order (first to define a name wins):
    //   SHION_SKILLS_PATH (colon-separated), <workspace>/skills,
    //   <workspace>/.claude/skills, the governed ~/.shion/skills store, and the
    //   user-global ~/.claude/skills shared by general agents (Claude Agent
    //   Skills `SKILL.md` format).
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
    skill_dirs.push(skill_store.root().to_path_buf());
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
    tools.register(Arc::new(SkillTool::new(
        skills.clone(),
        skill_store.clone(),
        approver.clone(),
    )));

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
    // the memory store for L1 pinned injection and the aux agent for recall
    // screening (main agent only).
    let llm = build_llm(
        &model_config,
        tools.tools(),
        preamble,
        Some(memory_repo.clone()),
        Some(aux_llm.clone()),
    )?;
    let skill_repo: Arc<dyn SkillRepository> = skill_store.clone();
    let reviewer: Arc<dyn Reviewer> = Arc::new(ReflectiveReviewer::new(
        aux_llm.clone(),
        memory_repo.clone(),
        skill_repo,
        kanban.clone(),
    ));

    let runtime = AgentRuntime {
        llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        // The in-house agent loop dispatches model-requested tools against this
        // same catalog (the LLM was handed clones of the same instances above).
        tools: Arc::new(tools),
        max_turns: model_config.max_turns,
        // Mirror the LLM's history window so the turn loads exactly what the
        // model will replay (no full-transcript read on long chat sessions).
        history_window: model_config.max_history_messages,
        reviewer: Some(reviewer.clone()),
        review_interval: env.review_interval.filter(|v| *v > 0).unwrap_or(10),
    };

    Ok(Wiring {
        runtime,
        sessions: db.clone(),
        reviewer,
        aux_llm,
        memories: memory_repo,
        skills: skill_store,
    })
}
