//! Shared construction of a fully-wired `AgentRuntime`.
//!
//! Both the chat REPL (`cli/chat.rs`) and the gateway (`cli/gateway.rs`) need
//! the same agent: identical tools, skills, LLM, and reviewer. The only thing
//! that differs is the `Approver` — interactive at a TTY vs. auto-deny in the
//! unattended gateway — so it is passed in.

use std::{path::PathBuf, sync::Arc};

use crate::{
    agent::{
        review_coordinator::ReviewCoordinator, reviewer::ReflectiveReviewer, runtime::AgentRuntime,
        system_prompt::SystemPromptBuilder,
    },
    config::ConfigSnapshot,
    domain::{
        approval::Approver, llm::LlmClient, memory::MemoryRepository, repository::SkillRepository,
        reviewer::Reviewer, workspace::Workspace,
    },
    infra::{
        llm::{PreambleFn, build_llm},
        memory::memory_db::MemoryDb,
        persistence::{db::Db, kanban::KanbanDb},
        skills::FsSkillStore,
    },
    services::{
        clarify::ClarifyState,
        memory_enrichment::MemoryEnricher,
        skill_registry::SkillRegistry,
        tool_execution::{ToolExecutionConfig, ToolExecutor},
    },
    tools::{
        ask_user::AskUserTool, delegate::DelegateTool, file::FileTool,
        homeassistant::HomeAssistantTool, memory::MemoryTool, reminder::ReminderTool,
        session::SessionTool, shell::ShellTool, skill::SkillTool, task::TaskTool, time::TimeTool,
        todo::TodoTool, web_fetch::WebFetchTool, web_search::WebSearchTool,
    },
};

/// A wired agent plus the handles background work needs (sessions for sweeping,
/// the reviewer the sweep invokes).
pub struct Wiring {
    pub runtime: AgentRuntime,
    /// The shared review coordinator (post-turn + scheduled), for the
    /// gateway's `ReviewSweep`.
    pub review: Arc<ReviewCoordinator>,
    /// The auxiliary (cheaper) LLM, reused by the daily briefing sweep.
    pub aux_llm: Arc<dyn LlmClient>,
    /// The markdown memory store, also read by the briefing sweep.
    pub memories: Arc<dyn MemoryRepository>,
    /// The governed skill store (`~/.shion/skills`, files — roadmap §9), shared
    /// with the gateway's api channel.
    pub skills: Arc<FsSkillStore>,
    /// Mid-turn clarify state: the `ask_user` tool waits on it; the gateway
    /// dispatcher (and the TUI) resolve an inbound message into it.
    pub clarify: Arc<ClarifyState>,
}

/// Build the agent against `db` (sessions/messages/etc.) and `kanban` (durable
/// tasks, a separate file), gating side-effecting tools through `approver`.
/// Every setting comes from the caller's one resolved `config` snapshot —
/// wiring never re-reads config.toml, the env, or `.env`.
pub async fn build(
    config: &ConfigSnapshot,
    db: Arc<Db>,
    kanban: Arc<KanbanDb>,
    approver: Arc<dyn Approver>,
) -> anyhow::Result<Wiring> {
    // An unusable model selection (bad SHION_* value, unknown provider,
    // missing API key) can't produce a working agent — fail here like the old
    // strict resolver did.
    config.validate_agent()?;
    let model_config = &config.runtime.model;

    // Wrap the interactive approver in the configurable permission policy
    // (roadmap §3): the policy auto-allows / hard-denies per `[policy]` rules and
    // only escalates to `approver` when it says "ask". With no `[policy]` table
    // this is the empty policy — identical to the bare interactive approver.
    let approver = crate::agent::policy_approver::PolicyApprover::wrap(
        config.runtime.policy.policy.clone(),
        approver,
    );

    // File operations are confined to the current working directory.
    let workspace = Arc::new(Workspace::current_dir()?);

    // The executor owns execution policy (result cap, per-turn call budget) as
    // instance config — no process globals.
    let mut tools = ToolExecutor::new(ToolExecutionConfig::with_result_cap(
        model_config.max_tool_result_bytes,
    ));
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

    // Mid-turn clarification (roadmap §7): the sentinel tool suspends the turn
    // on a question; whoever routes inbound messages (gateway dispatcher, TUI)
    // resolves the answer through this shared state.
    let clarify = Arc::new(ClarifyState::new());
    tools.register(Arc::new(AskUserTool::new(clarify.clone())));

    // Home Assistant tool, only when configured (HASS_TOKEN set; HASS_URL
    // optional, defaults to homeassistant.local:8123).
    if let Some(ha) = &config.runtime.homeassistant_tool {
        tools.register(Arc::new(HomeAssistantTool::new(
            ha.base_url.clone(),
            ha.token.clone(),
            approver.clone(),
        )));
    }

    // Memories live in their own SQLite file (~/.shion/memory.db), shared by the
    // `memory` tool, the reflective reviewer, the L1 pinned injection, and the
    // briefing sweep. On first run it seeds itself from any legacy markdown
    // memories under ~/.shion/memory/ (a one-time, no-op-once-populated import).
    let memory_db = MemoryDb::connect(&config.runtime.memory_db_url).await?;
    let imported = memory_db
        .import_legacy_markdown(&config.runtime.home.join("memory"))
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
    let aux_llm = build_llm(&aux_config, None, aux_preamble, None)?;
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
    let root = workspace.roots().first().cloned().unwrap_or_default();
    let mut skill_dirs: Vec<PathBuf> = config.runtime.skills_path.clone();
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
    let tool_names = tools
        .definitions()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let prompt_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(tool_names)
            .skills_note(skills_note)
            .workspace_root(Some(root.clone())),
    );
    let preamble: PreambleFn = Arc::new(move || prompt_builder.build());

    // Hand the same tool instances to the LLM so the model can call them, plus
    // the memory enricher (main agent only): the memory store for pinned/recall
    // selection and the aux agent for recall screening, behind one interface.
    let enricher = Arc::new(MemoryEnricher::new(
        memory_repo.clone(),
        Some(aux_llm.clone()),
    ));
    let llm = build_llm(model_config, Some(&tools), preamble, Some(enricher))?;
    let skill_repo: Arc<dyn SkillRepository> = skill_store.clone();
    let reviewer: Arc<dyn Reviewer> = Arc::new(ReflectiveReviewer::new(
        aux_llm.clone(),
        memory_repo.clone(),
        skill_repo,
        kanban.clone(),
    ));
    // One coordinator instance shared by the runtime's post-turn trigger and
    // the gateway's scheduled sweep — that sharing is what makes its
    // per-session in-flight guard effective across the two paths.
    let review = Arc::new(ReviewCoordinator::new(
        db.clone(),
        db.clone(),
        reviewer,
        config.runtime.review_interval,
    ));

    let runtime = AgentRuntime {
        llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        // The in-house agent loop hands each round to this executor; the LLM
        // was handed RigTool adapters over the same core above.
        tool_executor: tools,
        max_turns: model_config.max_turns,
        // Mirror the LLM's history window so the turn loads exactly what the
        // model will replay (no full-transcript read on long chat sessions).
        history_window: model_config.max_history_messages,
        review: Some(review.clone()),
    };

    Ok(Wiring {
        runtime,
        review,
        aux_llm,
        memories: memory_repo,
        skills: skill_store,
        clarify,
    })
}
