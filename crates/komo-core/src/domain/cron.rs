//! Scheduled cron jobs: deterministic commands the gateway executes unattended
//! on a cron schedule (hermes' `no_agent` cron jobs analog).
//!
//! Jobs live in their own durable store (`~/.komo/cron.db`) — not in
//! `config.toml`, because an operator can accumulate many of them, and not in
//! the disposable `state.db`, because a job silently vanishing on a state reset
//! means its work silently stops happening. The command is **operator-authored**
//! (added via `komo cron add` or the loopback-gated api) — the same trust
//! boundary as running `komo gateway` itself — so execution is direct: no shell
//! tool, no approver, no `[policy]` involvement. Anything needing LLM judgment
//! belongs in an agent sweep (like the briefing), not here.

use async_trait::async_trait;

/// Default wall-clock budget for a job command — hermes' cron-job budget
/// (15 min), generous enough for a script that clones a repo and pushes an MR.
pub const DEFAULT_CRON_JOB_TIMEOUT_SECS: u64 = 900;

/// Outcome of a job's most recent execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronRunStatus {
    Ok,
    Failed,
}

impl CronRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// `""` (never ran) → `None`; anything not `ok` parses as failed.
pub fn parse_cron_run_status(s: &str) -> Option<CronRunStatus> {
    match s {
        "" => None,
        "ok" => Some(CronRunStatus::Ok),
        _ => Some(CronRunStatus::Failed),
    }
}

/// What a job does when it fires. Internally tagged (`kind`) so the HTTP path
/// and the db both round-trip it without a separate discriminator column having
/// to be threaded by hand.
///
/// - `Command` — run a fixed program and deliver its stdout verbatim (hermes'
///   `no_agent` mode). Deterministic, no LLM. The reliable default for scripts.
/// - `Agent` — run a prompt through an **unattended, tool-capable agent turn**
///   and deliver the reply. Optional `skills` are loaded first. The agent runs
///   with the full tool set but side effects are gated by the permission
///   policy: with no human to prompt, a `Risk::Normal` action passes only
///   through an `unattended = true` `[policy]` rule (identical model to the
///   daily briefing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronAction {
    Command {
        /// Program to execute (an absolute path; run directly, not via a shell).
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Working directory. `None` = the gateway's cwd.
        #[serde(default)]
        workdir: Option<String>,
        /// Wall-clock budget in seconds; the process is killed past it.
        timeout_secs: u64,
    },
    Agent {
        /// The instruction the agent turn runs.
        prompt: String,
        /// Skills to load before running the prompt (progressive disclosure —
        /// the turn is told to `skill` view each one first).
        #[serde(default)]
        skills: Vec<String>,
    },
}

impl CronAction {
    /// Short label for listings/logs.
    pub fn kind(&self) -> &'static str {
        match self {
            CronAction::Command { .. } => "command",
            CronAction::Agent { .. } => "agent",
        }
    }
}

/// One scheduled job. `name` is the operator-facing key (unique); `id` is the
/// storage key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// 5-field cron expression (local timezone).
    pub schedule: String,
    /// What the job does when it fires (command vs agent turn).
    pub action: CronAction,
    /// Disabled jobs stay listed/inspectable but never fire.
    pub enabled: bool,
    /// Next scheduled fire (unix seconds). The sweep runs a job once its
    /// `next_run_at` is due, then advances it — set to "now" to trigger an
    /// off-schedule run on the next sweep tick.
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<CronRunStatus>,
    /// Failure detail from the most recent run (empty on success / never ran).
    pub last_error: String,
    pub created_at: i64,
}

impl CronJob {
    /// A new enabled job with the given action. The caller (the shared operator
    /// action) validates the schedule and computes the initial `next_run_at` —
    /// this stays parse-free so komo-core needs no cron dependency.
    pub fn new(name: &str, schedule: &str, action: CronAction, next_run_at: i64) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            schedule: schedule.to_string(),
            action,
            enabled: true,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: String::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        }
    }

    /// Convenience constructor for a command-mode job with default timeout.
    pub fn new_command(name: &str, schedule: &str, command: &str, next_run_at: i64) -> Self {
        Self::new(
            name,
            schedule,
            CronAction::Command {
                command: command.to_string(),
                args: Vec::new(),
                workdir: None,
                timeout_secs: DEFAULT_CRON_JOB_TIMEOUT_SECS,
            },
            next_run_at,
        )
    }

    /// Due = enabled and the scheduled fire time has arrived.
    pub fn is_due(&self, now: i64) -> bool {
        self.enabled && self.next_run_at <= now
    }
}

/// The operator's request to create a job (`komo cron add` / `POST
/// /api/cron/add`). Validation and `next_run_at` computation happen in the
/// shared operator action, not here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CronJobSpec {
    pub name: String,
    pub schedule: String,
    pub action: CronAction,
}

#[async_trait]
pub trait CronJobRepository: Send + Sync {
    async fn save(&self, job: &CronJob) -> anyhow::Result<()>;
    /// Every job, enabled or not, ordered by name.
    async fn list(&self) -> anyhow::Result<Vec<CronJob>>;
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<CronJob>>;
    /// Update every mutable field of an existing job (matched by `id`).
    async fn update(&self, job: &CronJob) -> anyhow::Result<()>;
    /// Remove a job by name; `false` = no such job.
    async fn delete(&self, name: &str) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_command_job_is_enabled_with_default_timeout() {
        let job = CronJob::new_command("weekly", "0 14 * * 5", "/opt/rotate.py", 1000);
        assert!(job.enabled);
        assert_eq!(job.action.kind(), "command");
        let CronAction::Command { timeout_secs, .. } = &job.action else {
            panic!("command job");
        };
        assert_eq!(*timeout_secs, DEFAULT_CRON_JOB_TIMEOUT_SECS);
        assert_eq!(job.next_run_at, 1000);
        assert!(job.last_status.is_none());
        assert!(!job.id.is_empty());
    }

    #[test]
    fn agent_action_roundtrips_through_json() {
        let action = CronAction::Agent {
            prompt: "summarize my day".into(),
            skills: vec!["calendar".into()],
        };
        let job = CronJob::new("brief", "0 8 * * *", action, 0);
        let json = serde_json::to_string(&job).unwrap();
        assert!(json.contains("\"kind\":\"agent\""));
        let back: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action.kind(), "agent");
        let CronAction::Agent { prompt, skills } = &back.action else {
            panic!("agent job");
        };
        assert_eq!(prompt, "summarize my day");
        assert_eq!(skills, &vec!["calendar".to_string()]);
    }

    #[test]
    fn due_requires_enabled_and_elapsed() {
        let mut job = CronJob::new_command("j", "* * * * *", "/bin/true", 100);
        assert!(job.is_due(100));
        assert!(job.is_due(101));
        assert!(!job.is_due(99));
        job.enabled = false;
        assert!(!job.is_due(200), "a disabled job is never due");
    }

    #[test]
    fn run_status_roundtrip() {
        assert_eq!(parse_cron_run_status(""), None);
        assert_eq!(parse_cron_run_status("ok"), Some(CronRunStatus::Ok));
        assert_eq!(parse_cron_run_status("failed"), Some(CronRunStatus::Failed));
        assert_eq!(
            parse_cron_run_status("garbage"),
            Some(CronRunStatus::Failed)
        );
    }

    #[test]
    fn ids_are_unique_across_rapid_creation() {
        let a = CronJob::new_command("a", "* * * * *", "/bin/true", 0);
        let b = CronJob::new_command("b", "* * * * *", "/bin/true", 0);
        assert_ne!(a.id, b.id);
    }
}
