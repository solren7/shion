//! The run ledger — an execution/audit record of every agent turn
//! (docs/personal-agent-roadmap.md §7). One [`Run`] per user turn, with one
//! [`RunStep`] per tool invocation (captured at the single choke point every
//! tool call funnels through, `services::tool_registry::execute_isolated`).
//!
//! Runs are execution state bound to a session, so they live in `shion.db`
//! (disposable dev state) alongside sessions/messages — not in the durable
//! kanban/memory files. Every ledger write is best-effort: it must never fail a
//! turn or a tool call (same contract as memory `mark_used`).
//!
//! Deliberately omitted in v1: a `recoverable` flag. `resume` is deferred, and
//! the roadmap's governance principle is "no fields without a consumer" (§6).

use async_trait::async_trait;

/// Verbatim caps so a row can't grow unbounded. `input`/`final_output` may be a
/// whole message; tool args/results are usually smaller but a `file`/`shell`
/// payload can be large.
pub const RUN_FIELD_CAP: usize = 4000;
pub const STEP_FIELD_CAP: usize = 2000;

/// Error stamped on a run reconciled at startup. A run left in `Running` is the
/// residue of a process that died mid-turn (a run is `Running` only while in
/// flight), so on the next start it is flipped to `Failed` with this reason.
pub const INTERRUPTED_ERROR: &str = "interrupted (process restarted)";

/// Truncate `s` to at most `cap` chars (char-boundary safe), appending an
/// ellipsis marker when cut so the reader knows the row is not the whole story.
pub fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push_str(" …[truncated]");
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The turn is in flight (set at start; an in-flight crash leaves it here).
    Running,
    Done,
    Failed,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

pub fn parse_run_status(s: &str) -> anyhow::Result<RunStatus> {
    match s {
        "running" => Ok(RunStatus::Running),
        "done" => Ok(RunStatus::Done),
        "failed" => Ok(RunStatus::Failed),
        other => Err(anyhow::anyhow!(
            "unknown run status `{other}` (expected running/done/failed)"
        )),
    }
}

/// One agent turn: the user input, a short outcome summary, the final reply,
/// and the status. Steps (tool calls) hang off it by `run_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    /// The user message that started the turn (truncated to [`RUN_FIELD_CAP`]).
    pub input: String,
    /// Post-turn summary: "respond" (no tools) or "<n> tool call(s)". The LLM
    /// owns tool dispatch, so this is derived from the recorded step count, not
    /// a planner decision.
    pub plan: String,
    pub status: RunStatus,
    /// The assistant reply (truncated). Empty until the turn finishes / on failure.
    pub final_output: String,
    /// Failure reason. Empty unless `status == Failed`.
    pub error: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl Run {
    /// Open a new run for `session_id`, started now.
    pub fn start(session_id: &str, input: &str) -> Self {
        Self {
            id: format!(
                "run-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            session_id: session_id.to_string(),
            input: truncate(input, RUN_FIELD_CAP),
            plan: String::new(),
            status: RunStatus::Running,
            final_output: String::new(),
            error: String::new(),
            started_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            ended_at: None,
        }
    }
}

/// One tool invocation within a run. `args`/`result` are stored verbatim
/// (truncated), except that each tool may redact its own args before they reach
/// the ledger (see [`crate::domain::tool::Tool::redact_args`]) — `shell` scrubs
/// secret-looking substrings, `file` drops write bodies.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunStep {
    pub run_id: String,
    /// Monotonic order within the run (assigned by the run's shared counter).
    pub seq: i64,
    pub tool_name: String,
    /// Redacted + truncated JSON args the model passed.
    pub args: String,
    /// Truncated result. Empty on failure.
    pub result: String,
    /// Tool error. Empty unless `!ok`.
    pub error: String,
    pub ok: bool,
    pub started_at: i64,
    pub ended_at: i64,
}

#[async_trait]
pub trait RunRepository: Send + Sync {
    /// Persist a freshly-opened run (status = running).
    async fn start(&self, run: &Run) -> anyhow::Result<()>;
    /// Append a tool step to a run.
    async fn append_step(&self, step: &RunStep) -> anyhow::Result<()>;
    /// Update the run's outcome (status / final_output / error / ended_at).
    async fn finish(&self, run: &Run) -> anyhow::Result<()>;
    /// Most-recent runs first, capped at `limit`.
    async fn list(&self, limit: usize) -> anyhow::Result<Vec<Run>>;
    /// Fetch a single run by id.
    async fn get(&self, id: &str) -> anyhow::Result<Option<Run>>;
    /// Steps for a run, ordered by `seq`.
    async fn steps(&self, run_id: &str) -> anyhow::Result<Vec<RunStep>>;
    /// Delete every run started before `cutoff` (unix seconds) and its steps.
    /// Returns the number of runs removed. The ledger accumulates like messages,
    /// so this is the operator's manual prune (roadmap §9) — no automatic policy.
    async fn prune(&self, cutoff: i64) -> anyhow::Result<usize>;

    /// Flip every run still `Running` to `Failed`/[`INTERRUPTED_ERROR`], stamping
    /// `ended_at = now`; return how many were reconciled. Called once at process
    /// startup: a run is `Running` only while in flight, so any left over is the
    /// residue of a crashed earlier process — leaving it would make `run list`
    /// lie. Also the first building block toward resume (§6): it names the set of
    /// interrupted runs a future `resume` could pick up.
    async fn reconcile_interrupted(&self, now: i64) -> anyhow::Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings_and_cuts_long_ones() {
        assert_eq!(truncate("hi", 10), "hi");
        let long = "x".repeat(50);
        let cut = truncate(&long, 10);
        assert!(cut.starts_with(&"x".repeat(10)));
        assert!(cut.contains("truncated"));
    }

    #[test]
    fn status_roundtrips() {
        for s in [RunStatus::Running, RunStatus::Done, RunStatus::Failed] {
            assert_eq!(parse_run_status(s.as_str()).unwrap(), s);
        }
        assert!(parse_run_status("bogus").is_err());
    }
}
