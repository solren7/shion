//! The run ledger — an execution/audit record of every agent turn
//! (docs/personal-agent-roadmap.md §7). One [`Run`] per user turn, with one
//! [`RunStep`] per tool invocation (captured at the single choke point every
//! tool call funnels through, the tool executor (`services::tool_execution`)).
//!
//! Runs are execution state bound to a session, so they live in `state.db`
//! (disposable dev state) alongside sessions/messages — not in the durable
//! kanban/memory files. Every ledger write is best-effort: it must never fail a
//! turn or a tool call (same contract as memory `mark_used`).
//!
//! `recoverable` marks the resumable set (§6): set by `reconcile_interrupted`
//! when a crash leaves a run mid-flight, cleared by `mark_resumed` once a
//! resume turn has been dispatched — so `komo run resume` is at-most-once.

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
    /// The run was interrupted mid-flight (process died) and can be resumed:
    /// set by [`RunRepository::reconcile_interrupted`], cleared by
    /// [`RunRepository::mark_resumed`]. Only interruption produces a resumable
    /// run — an ordinary `Failed` has no half-done steps worth handing over.
    #[serde(default)]
    pub recoverable: bool,
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
            recoverable: false,
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

/// Per-field cap on a step's args/result inside the resume digest, and the
/// budget for the digest as a whole (a turn can have up to 100 steps — past
/// the budget the rest is elided, `run inspect` still has everything).
const RESUME_SNIPPET_CAP: usize = 200;
const RESUME_DIGEST_CAP: usize = 8000;

/// Compose the priming input for resuming an interrupted run: the original
/// request plus a digest of the tool calls that had completed, so the model can
/// judge which side effects already took hold and continue rather than restart.
///
/// The ledger is an audit record, not a checkpoint — intermediate assistant
/// turns are never persisted and step args are redacted/truncated — so resume
/// re-dispatches a *fresh* turn primed with this digest instead of pretending
/// to replay the loop from mid-flight. The result is a normal user message in
/// the session transcript, visible as such.
pub fn resume_prompt(run: &Run, steps: &[RunStep]) -> String {
    // Collapse newlines so each step stays one digest line.
    let snip = |s: &str| truncate(&s.replace('\n', " "), RESUME_SNIPPET_CAP);

    let mut out = format!(
        "[resume {id}] The previous attempt at this task was interrupted (the \
         process restarted mid-turn). Original request:\n\n{input}\n\n",
        id = run.id,
        input = run.input,
    );
    if steps.is_empty() {
        out.push_str("No tool calls had completed before the interruption.\n");
    } else {
        out.push_str(&format!(
            "Before the interruption, {} tool call(s) had already completed:\n",
            steps.len()
        ));
        for (idx, s) in steps.iter().enumerate() {
            if out.len() > RESUME_DIGEST_CAP {
                out.push_str(&format!(
                    "…and {} more step(s), elided for length (full record: \
                     `komo run inspect {}`).\n",
                    steps.len() - idx,
                    run.id
                ));
                break;
            }
            let outcome = if s.ok {
                snip(&s.result)
            } else {
                format!("error: {}", snip(&s.error))
            };
            out.push_str(&format!(
                "{}. {} {} → {}\n",
                idx + 1,
                s.tool_name,
                snip(&s.args),
                outcome
            ));
        }
    }
    out.push_str(
        "\nReview what already took effect, then continue the task from where \
         it stopped. Do not re-apply side effects that already succeeded — \
         verify first when unsure. Reply with the completed outcome.",
    );
    out
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
    /// `ended_at = now` and `recoverable = true`; return how many were
    /// reconciled. Called once at process startup: a run is `Running` only while
    /// in flight, so any left over is the residue of a crashed earlier process —
    /// leaving it would make `run list` lie. The runs it marks are the set
    /// `resume` picks from (§6).
    async fn reconcile_interrupted(&self, now: i64) -> anyhow::Result<usize>;

    /// Clear a run's `recoverable` flag once a resume turn has been dispatched
    /// for it, so the same interruption is never resumed twice.
    async fn mark_resumed(&self, id: &str) -> anyhow::Result<()>;

    /// The most recent steps of one tool across all runs (newest first, capped
    /// at `limit`). Backs derived audit views — e.g. which turns loaded a given
    /// skill (`steps_by_tool("skill", …)` + [`step_views_skill`]) — without
    /// adding usage fields to any model.
    async fn steps_by_tool(&self, tool_name: &str, limit: usize) -> anyhow::Result<Vec<RunStep>>;
}

/// Whether a ledger step is the `skill` tool loading `skill_name`'s
/// instructions (`action=view`). The skill-invocation audit is *derived* from
/// the ledger — a skill "used" is exactly a skill viewed; no usage counters are
/// stored anywhere (roadmap §9 / "no dead fields").
pub fn step_views_skill(step: &RunStep, skill_name: &str) -> bool {
    if step.tool_name != "skill" {
        return false;
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&step.args) else {
        return false;
    };
    args.get("action").and_then(|v| v.as_str()) == Some("view")
        && args.get("name").and_then(|v| v.as_str()) == Some(skill_name)
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

    fn interrupted_run() -> Run {
        let mut run = Run::start("feishu:chat-1", "deploy the new build");
        run.status = RunStatus::Failed;
        run.error = INTERRUPTED_ERROR.to_string();
        run.recoverable = true;
        run
    }

    fn step(run: &Run, seq: i64, tool: &str, ok: bool) -> RunStep {
        RunStep {
            run_id: run.id.clone(),
            seq,
            tool_name: tool.to_string(),
            args: format!("{{\"n\":{seq}}}"),
            result: if ok { "done".into() } else { String::new() },
            error: if ok { String::new() } else { "boom".into() },
            ok,
            started_at: 100 + seq,
            ended_at: 101 + seq,
        }
    }

    #[test]
    fn resume_prompt_carries_input_and_step_digest() {
        let run = interrupted_run();
        let steps = vec![step(&run, 0, "shell", true), step(&run, 1, "file", false)];
        let prompt = resume_prompt(&run, &steps);

        assert!(prompt.contains(&run.id));
        assert!(prompt.contains("deploy the new build"));
        assert!(prompt.contains("2 tool call(s)"));
        assert!(prompt.contains("1. shell"));
        assert!(prompt.contains("2. file"));
        assert!(prompt.contains("error: boom"));
        assert!(prompt.contains("Do not re-apply side effects"));
    }

    #[test]
    fn resume_prompt_without_steps_says_so() {
        let run = interrupted_run();
        let prompt = resume_prompt(&run, &[]);
        assert!(prompt.contains("No tool calls had completed"));
    }

    #[test]
    fn resume_prompt_elides_past_the_digest_budget() {
        let run = interrupted_run();
        let steps: Vec<RunStep> = (0..100)
            .map(|seq| {
                let mut s = step(&run, seq, "web_fetch", true);
                s.result = "r".repeat(400); // each line lands near the snippet cap
                s
            })
            .collect();
        let prompt = resume_prompt(&run, &steps);
        assert!(prompt.contains("elided for length"));
        assert!(prompt.len() < RESUME_DIGEST_CAP + 2000);
    }

    #[test]
    fn step_views_skill_matches_only_view_steps_of_that_skill() {
        let run = interrupted_run();
        let mut s = step(&run, 0, "skill", true);
        s.args = r#"{"action":"view","name":"feishu-calendar"}"#.to_string();
        assert!(step_views_skill(&s, "feishu-calendar"));
        assert!(!step_views_skill(&s, "other-skill"));

        s.args = r#"{"action":"list"}"#.to_string();
        assert!(!step_views_skill(&s, "feishu-calendar"));

        let mut shell = step(&run, 1, "shell", true);
        shell.args = r#"{"action":"view","name":"feishu-calendar"}"#.to_string();
        assert!(!step_views_skill(&shell, "feishu-calendar"));

        s.args = "not json".to_string();
        assert!(!step_views_skill(&s, "feishu-calendar"));
    }

    #[test]
    fn status_roundtrips() {
        for s in [RunStatus::Running, RunStatus::Done, RunStatus::Failed] {
            assert_eq!(parse_run_status(s.as_str()).unwrap(), s);
        }
        assert!(parse_run_status("bogus").is_err());
    }
}
