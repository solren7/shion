//! Direct persistence adapter: operator actions against directly-opened
//! stores, used when no gateway is running (nothing holds the Turso lock).
//!
//! Stores open **lazily, per request family, once per command**: `run list`
//! never touches memory.db or kanban.db, and a batch of memory transitions
//! reuses one connection instead of reconnecting per id.

use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::domain::{
    home::HomeRepository,
    memory::MemoryRepository,
    pairing::{ApproveOutcome, PairingRepository},
    reminder::ReminderRepository,
    repository::SessionRepository,
    run::RunRepository,
    task::TaskRepository,
};
use crate::infra::{
    memory::memory_db::MemoryDb,
    persistence::{db::Db, kanban::KanbanDb},
};

use super::actions;
use super::request::{
    OperatorCommand, OperatorCommandResult, OperatorQuery, OperatorQueryResult, PairApproveOutcome,
};
use super::{StoreUrls, now};

pub(super) struct DirectOperatorAdapter {
    urls: StoreUrls,
    db: OnceCell<Arc<Db>>,
    kanban: OnceCell<Arc<KanbanDb>>,
    memory: OnceCell<Arc<MemoryDb>>,
}

impl DirectOperatorAdapter {
    pub(super) fn new(urls: StoreUrls) -> Self {
        Self {
            urls,
            db: OnceCell::new(),
            kanban: OnceCell::new(),
            memory: OnceCell::new(),
        }
    }

    /// The session/run/pairing store (`state.db`), opened on first use.
    pub(super) async fn db(&self) -> anyhow::Result<&Arc<Db>> {
        self.db
            .get_or_try_init(|| async { Ok(Arc::new(Db::connect(&self.urls.db).await?)) })
            .await
    }

    /// The durable task store (`kanban.db`), opened on first use.
    pub(super) async fn kanban(&self) -> anyhow::Result<&Arc<KanbanDb>> {
        self.kanban
            .get_or_try_init(|| async { Ok(Arc::new(KanbanDb::connect(&self.urls.kanban).await?)) })
            .await
    }

    /// The durable memory store (`memory.db`), opened on first use.
    pub(super) async fn memory(&self) -> anyhow::Result<&Arc<MemoryDb>> {
        self.memory
            .get_or_try_init(|| async { Ok(Arc::new(MemoryDb::connect(&self.urls.memory).await?)) })
            .await
    }

    pub(super) async fn query(&self, query: OperatorQuery) -> anyhow::Result<OperatorQueryResult> {
        Ok(match query {
            OperatorQuery::Reminders => {
                let mut pending =
                    ReminderRepository::list_pending(self.db().await?.as_ref()).await?;
                pending.sort_by_key(|r| r.run_at);
                OperatorQueryResult::Reminders(pending)
            }
            OperatorQuery::Tasks => OperatorQueryResult::Tasks(
                TaskRepository::list_open(self.kanban().await?.as_ref()).await?,
            ),
            OperatorQuery::Runs { limit } => OperatorQueryResult::Runs(
                RunRepository::list(self.db().await?.as_ref(), limit).await?,
            ),
            OperatorQuery::Run { id } => {
                let db = self.db().await?;
                let fetched = match RunRepository::get(db.as_ref(), &id).await? {
                    Some(run) => {
                        let steps = RunRepository::steps(db.as_ref(), &run.id).await?;
                        Some((run, steps))
                    }
                    None => None,
                };
                OperatorQueryResult::Run(fetched)
            }
            OperatorQuery::Sessions => OperatorQueryResult::Sessions(actions::session_summaries(
                SessionRepository::list(self.db().await?.as_ref()).await?,
            )),
            OperatorQuery::Memories => {
                OperatorQueryResult::Memories(self.memory().await?.list().await?)
            }
            OperatorQuery::Pairings => OperatorQueryResult::Pairings(actions::pairing_views(
                PairingRepository::list(self.db().await?.as_ref()).await?,
                now(),
            )),
            OperatorQuery::DreamPreview => OperatorQueryResult::DreamPreview(
                actions::dream_classify(&self.memory().await?.list().await?, now()),
            ),
            OperatorQuery::SkillAudit { name } => {
                let steps = RunRepository::steps_by_tool(
                    self.db().await?.as_ref(),
                    "skill",
                    actions::AUDIT_SCAN_LIMIT,
                )
                .await?;
                OperatorQueryResult::SkillAudit(actions::skill_invocations(
                    steps,
                    &name,
                    actions::AUDIT_RESULT_CAP,
                ))
            }
            OperatorQuery::HomeOverride => OperatorQueryResult::HomeOverride(
                HomeRepository::get(self.db().await?.as_ref()).await?,
            ),
        })
    }

    pub(super) async fn command(
        &self,
        command: OperatorCommand,
    ) -> anyhow::Result<OperatorCommandResult> {
        Ok(match command {
            OperatorCommand::MemoryTransition { id, action } => {
                match actions::apply_memory_transition(
                    self.memory().await?.as_ref(),
                    &id,
                    action,
                    now(),
                )
                .await?
                {
                    actions::TransitionOutcome::Applied(_) => {
                        OperatorCommandResult::MemoryTransitioned
                    }
                    actions::TransitionOutcome::NotFound => {
                        anyhow::bail!("no memory with id `{id}`")
                    }
                }
            }
            OperatorCommand::PruneRuns { cutoff } => OperatorCommandResult::RunsPruned {
                removed: RunRepository::prune(self.db().await?.as_ref(), cutoff).await?,
            },
            OperatorCommand::CleanSessions => OperatorCommandResult::SessionsCleaned {
                removed: SessionRepository::delete_empty_sessions(self.db().await?.as_ref())
                    .await?,
            },
            OperatorCommand::PairApprove { code } => {
                let outcome = match PairingRepository::approve_code(
                    self.db().await?.as_ref(),
                    &code,
                )
                .await?
                {
                    ApproveOutcome::Approved(request) => {
                        PairApproveOutcome::Approved { id: request.id }
                    }
                    ApproveOutcome::NotFound => PairApproveOutcome::NotFound,
                    ApproveOutcome::Locked { retry_after_secs } => {
                        PairApproveOutcome::Locked { retry_after_secs }
                    }
                };
                OperatorCommandResult::PairApproved(outcome)
            }
            OperatorCommand::PairRevoke { id } => OperatorCommandResult::PairRevoked {
                revoked: PairingRepository::revoke(self.db().await?.as_ref(), &id).await?,
            },
            OperatorCommand::DreamApply => {
                let summary = crate::agent::daemon::DreamSweep {
                    memories: self.memory().await?.clone() as Arc<dyn MemoryRepository>,
                }
                .apply()
                .await?;
                OperatorCommandResult::DreamApplied {
                    promoted: summary.memories_promoted,
                    archived: summary.memories_archived,
                }
            }
        })
    }
}
