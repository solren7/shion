use std::sync::Arc;

use crate::{
    agent::{
        daemon::{Maintenance, ReviewSweep, Schedule, supervise},
        reviewer::ReflectiveReviewer,
    },
    domain::{
        memory::MemoryRepository, repository::SessionRepository, repository::SkillRepository,
        reviewer::Reviewer,
    },
    infra::{config::ModelConfig, db::Db, llm::build_llm},
};

/// Run the background maintenance daemon: on each cron tick, sweep stored
/// sessions through the reflective reviewer. Runs until Ctrl-C.
pub async fn run(db_url: &str, schedule_expr: &str) -> anyhow::Result<()> {
    let schedule = Schedule::parse(schedule_expr)?;

    let db = Arc::new(Db::connect(db_url).await?);
    let model_config = ModelConfig::from_env()?;

    // The reviewer runs on the (optionally cheaper) aux model and needs no
    // tools — it only reads a transcript and emits memory/skill suggestions.
    let aux_llm = build_llm(&model_config.aux_variant(), Vec::new(), None)?;
    let memory_repo: Arc<dyn MemoryRepository> = db.clone();
    let skill_repo: Arc<dyn SkillRepository> = db.clone();
    let reviewer: Arc<dyn Reviewer> =
        Arc::new(ReflectiveReviewer::new(aux_llm, memory_repo, skill_repo));
    let sessions: Arc<dyn SessionRepository> = db.clone();

    let maintenance: Arc<dyn Maintenance> = Arc::new(ReviewSweep { sessions, reviewer });

    println!(
        "Shion daemon — maintenance sweep on schedule `{}`. Ctrl-C to stop.\n",
        schedule.expr()
    );

    supervise(&schedule, maintenance, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
