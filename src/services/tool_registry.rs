use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use tracing::{Instrument, info, info_span, warn};

use crate::domain::{
    gateway::ReplySink,
    run::{RunRepository, RunStep, STEP_FIELD_CAP, truncate},
    tool::{RetryHint, Tool},
};

/// Ambient context for the turn a tool is executing within: which session it
/// belongs to and how to talk back to that conversation. Set by the gateway
/// dispatcher around a turn (`agent::interaction`) and read by a chat-channel
/// approver when a tool needs mid-execution approval.
///
/// It rides a task-local rather than the tool's argument string because rig's
/// `ToolDyn::call` signature is fixed — we can't thread it through the LLM
/// tool-call path. `execute_isolated` re-establishes it across its `spawn`.
#[derive(Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub sink: Arc<dyn ReplySink>,
    /// Whether a human can answer a mid-turn approval prompt on this channel.
    /// Chat channels set this `true`; non-interactive callers (the REPL's
    /// detached context, the HTTP API) set it `false` so a `Risk::Normal` /
    /// `Risk::Dangerous` request is denied immediately instead of waiting out
    /// the approval timeout against a sink no one is reading.
    pub interactive: bool,
    /// Whether approval-needing tool calls should be auto-approved without a
    /// prompt. Set only for a **trusted** turn — a `shion chat` routed over the
    /// gateway's loopback api channel, where the CLI user *is* the host
    /// operator (see `SessionContext::trusted`). The api channel gates this to
    /// loopback callers, so a publicly-bound api can never reach it. Leave
    /// `false` everywhere else.
    pub auto_approve: bool,
}

impl SessionContext {
    /// A context that knows the session but cannot talk back mid-turn (its sink
    /// is a no-op, and it is non-interactive). Used by the REPL and any caller
    /// that has a session id but no channel to prompt on — enough for
    /// session-scoped tools like `todo`, while a mid-turn approval prompt is
    /// auto-denied (the REPL gates approvals at the TTY, not through this sink).
    pub fn detached(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: false,
            auto_approve: false,
        }
    }

    /// A trusted context: like `detached` (no mid-turn prompting), but
    /// approval-needing tool calls are auto-approved. Used for a `shion chat`
    /// turn routed over the gateway's **loopback** api channel — the CLI user
    /// is the host operator, so there is no separate human to prompt. The api
    /// channel only builds this for loopback callers carrying the trusted
    /// header; a publicly-bound api keeps using `detached` (auto-deny).
    pub fn trusted(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            sink: Arc::new(NoopSink),
            interactive: false,
            auto_approve: true,
        }
    }
}

/// A [`ReplySink`] that drops everything — see [`SessionContext::detached`].
struct NoopSink;

#[async_trait::async_trait]
impl ReplySink for NoopSink {
    async fn send(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

tokio::task_local! {
    static SESSION: SessionContext;
}

/// Run `future` with `ctx` as the ambient session context.
pub async fn with_session<F: std::future::Future>(ctx: SessionContext, future: F) -> F::Output {
    SESSION.scope(ctx, future).await
}

/// The ambient session context, if the current task is running inside one.
/// `None` for the REPL, aux sub-agents, and maintenance sweeps.
pub fn current_session() -> Option<SessionContext> {
    SESSION.try_with(|c| c.clone()).ok()
}

/// Ambient run-ledger context for the turn (`domain/run.rs`, roadmap §7). Set by
/// `AgentRuntime::run_turn` around the turn body so `execute_isolated` — the one
/// choke point every tool call funnels through (`run_agent_loop` dispatches each
/// model-requested tool here) — can record each tool invocation as a
/// `RunStep`. Absent for aux sub-agents and maintenance sweeps, so their tool
/// use never pollutes the ledger.
#[derive(Clone)]
pub struct RunContext {
    pub run_id: String,
    pub repo: Arc<dyn RunRepository>,
    /// Monotonic step counter, shared across clones so steps within a run get a
    /// stable order even when the context is cloned across tasks.
    seq: Arc<AtomicI64>,
}

impl RunContext {
    pub fn new(run_id: String, repo: Arc<dyn RunRepository>) -> Self {
        Self {
            run_id,
            repo,
            seq: Arc::new(AtomicI64::new(0)),
        }
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// How many tool steps have been claimed so far (the post-turn count).
    pub fn steps_count(&self) -> i64 {
        self.seq.load(Ordering::Relaxed)
    }
}

tokio::task_local! {
    static RUN: RunContext;
}

/// Run `future` with `ctx` as the ambient run-ledger context.
pub async fn with_run<F: std::future::Future>(ctx: RunContext, future: F) -> F::Output {
    RUN.scope(ctx, future).await
}

/// The ambient run-ledger context, if the current turn is being recorded.
pub fn current_run() -> Option<RunContext> {
    RUN.try_with(|c| c.clone()).ok()
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Look up a tool by name for direct dispatch. The in-house tool loop
    /// (`AgentRuntime::turn_body`) uses this to run a model-requested tool
    /// through `execute_isolated`, now that shion owns the loop rather than rig.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// All registered tools, shared via `Arc` (handed to the LLM agent in
    /// `build_llm` so the provider sees their schemas). The registry is purely
    /// this catalog: `run_agent_loop` looks tools up via `get` and dispatches
    /// them through `execute_isolated` — there is no keyword-routed execute path.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.values().cloned().collect()
    }
}

/// Process-wide cap on the size of a tool result handed back to the LLM. A
/// single tool returning tens of KB (a big file read, a full `/api/states`
/// dump, a long web page) floods the context window *every subsequent turn*,
/// since the result stays in history. Sized **above** the per-tool self-caps
/// (`web_fetch` / `homeassistant` cap themselves at 8 KB) so it never fights a
/// tool that already trims sensibly — it only catches the ones that don't.
///
/// Resolved once at startup from `max_tool_result_bytes`
/// (`SHION_MAX_TOOL_RESULT_BYTES` env > config.toml > `DEFAULT_MAX_TOOL_RESULT_BYTES`)
/// via [`set_tool_result_cap`]. A `OnceLock` rather than a threaded parameter
/// because rig's `ToolDyn::call` signature is fixed — same reason the session /
/// run contexts are ambient. Unset (tests, aux paths) → the built-in default.
static TOOL_RESULT_CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Set the process-wide tool-result byte cap. Called once during wiring
/// (`cli/wiring.rs`); a second call is ignored (first wins).
pub fn set_tool_result_cap(bytes: usize) {
    let _ = TOOL_RESULT_CAP.set(bytes);
}

fn tool_result_cap() -> usize {
    TOOL_RESULT_CAP
        .get()
        .copied()
        .unwrap_or(crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES)
}

/// Truncate an over-long tool result at a UTF-8 char boundary, appending a
/// marker that nudges the model to re-query more narrowly. Short results pass
/// through untouched. Applied uniformly to every tool at the choke point below,
/// so no individual tool has to implement its own ceiling.
fn cap_tool_result(mut out: String) -> String {
    let cap = tool_result_cap();
    if out.len() <= cap {
        return out;
    }
    let mut end = cap;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out.push_str(&format!(
        "\n\n…[truncated: result exceeded the {} KB tool-result limit. Re-run with \
         a narrower query — a filter, a specific id, or a smaller range — to see the rest.]",
        cap / 1024
    ));
    out
}

/// Total attempts for a tool whose failure is judged retryable (1 initial +
/// retries). Kept a constant, not config: transient-error retry is an internal
/// robustness backstop, not a user tuning knob. Promote to config only when a
/// real consumer needs to vary it.
const TOOL_RETRY_MAX_ATTEMPTS: usize = 3;
/// Backoff before each retry, indexed by the retry number (the first retry
/// waits the first entry, etc.); the last entry is reused beyond its length.
const TOOL_RETRY_BACKOFF_MS: [u64; 2] = [250, 750];

/// Soft per-turn tool-call budget (backstop). rig's `max_turns` (default 30)
/// bounds *round-trips*, but a single round can request many tools at once;
/// this caps the *total* calls per turn so a runaway loop can't fan out
/// unbounded. Set generously above any legitimate turn — promote to config if a
/// real consumer needs to tune it. Enforced in [`execute_isolated`] against the
/// run-ledger seq, so it applies only to recorded turns (the main agent), never
/// to aux sub-agents or sweeps (which have no counter and no tool loop anyway).
const MAX_TOOL_CALLS_PER_TURN: i64 = 100;

/// How a failed tool call may be retried. Preferred path: a tool classifies its
/// own failure at the source via [`TransientError`] (the reqwest-backed tools do
/// this in `tools::http`, where the typed `reqwest::Error` / status is intact),
/// and [`classify_error`] reads that hint directly. Fallback path: for errors
/// that carry no hint, classify from the error *text* — a heuristic, since a
/// flattened `anyhow!("…: {e}")` has dropped the typed source. Deliberately
/// conservative: an error matching neither is [`Retry::No`] (never retried).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Don't retry — terminal (bad arguments, denied, blocked) or unknown.
    No,
    /// The request provably never reached the server (connection refused, DNS
    /// failure). Safe to retry for *any* tool — no side effect can have landed.
    ConnLevel,
    /// Landed-or-not is ambiguous (timeout, 5xx, rate-limit). Retry only an
    /// idempotent tool, so a side effect is never applied twice.
    Ambiguous,
}

/// Markers that mean the connection never established — the request did not
/// reach the server, so retrying cannot double-apply a side effect.
const CONN_LEVEL_MARKERS: &[&str] = &[
    "connection refused",
    "dns error",
    "failed to lookup address",
    "name resolution",
    "could not resolve",
    "no such host",
];
/// Markers whose side-effect status is ambiguous — the request may have landed
/// and applied before the failure surfaced. Retried for idempotent tools only.
const AMBIGUOUS_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "error sending request",
    "connection reset",
    "broken pipe",
    "temporarily unavailable",
    "http 502",
    "http 503",
    "http 504",
    "http 429",
    "502 bad gateway",
    "503 service",
    "504 gateway",
    "429 too many",
];

fn classify_error(err: &anyhow::Error) -> Retry {
    // Typed hint first: lossless, set where the failure arose. anyhow walks the
    // chain, so a `.context(...)`-wrapped `TransientError` is still found.
    if let Some(te) = err.downcast_ref::<crate::domain::tool::TransientError>() {
        return match te.hint {
            RetryHint::Connection => Retry::ConnLevel,
            RetryHint::Ambiguous => Retry::Ambiguous,
        };
    }
    // Fallback heuristic for errors that didn't classify themselves.
    let msg = format!("{err:#}").to_lowercase();
    if CONN_LEVEL_MARKERS.iter().any(|m| msg.contains(m)) {
        Retry::ConnLevel
    } else if AMBIGUOUS_MARKERS.iter().any(|m| msg.contains(m)) {
        Retry::Ambiguous
    } else {
        Retry::No
    }
}

/// Whether a failed call should be retried, given the tool's idempotency.
fn should_retry(err: &anyhow::Error, idempotent: bool) -> bool {
    match classify_error(err) {
        Retry::No => false,
        Retry::ConnLevel => true,
        Retry::Ambiguous => idempotent,
    }
}

/// Runs a tool on its own tokio task, isolated from the caller. This keeps
/// tool work off the chat task's thread and — because `JoinHandle` catches
/// panics — turns a panicking tool into an error reply instead of a process
/// exit. Called by `AgentRuntime::run_agent_loop` for every model-requested
/// tool (and by the `infra::rig_tool::RigTool` adapter as a trait-required
/// fallback for any rig-driven completion — see its note).
///
/// A transient failure (a network blip mid-fetch, Home Assistant not yet up) is
/// retried with backoff per [`should_retry`]. The run ledger still records a
/// single step for the call's final outcome — the retry is a robustness detail,
/// not extra audit rows — and one seq is claimed per call regardless, so the
/// tool-call budget counts logical calls, not attempts. Panics are never
/// retried (their error text matches no transient marker).
pub async fn execute_isolated(tool: Arc<dyn Tool>, input: String) -> anyhow::Result<String> {
    let name = tool.name();

    // Run-ledger bookkeeping (only when this turn is being recorded). Capture
    // the redacted args and seq up front: the raw `input` is cloned per attempt
    // below, and the seq must be claimed before the tool runs so the span and
    // the persisted step agree.
    let run = current_run();
    let ledger = run.as_ref().map(|r| (r.clone(), r.next_seq()));
    let redacted_args = ledger.as_ref().map(|_| tool.redact_args(&input));
    let started_at = now();
    // Wall-clock timestamps (`now()`) are integer unix seconds — fine for the
    // ledger's started/ended fields, but differencing them only yields whole
    // seconds, so any sub-second tool would log `elapsed_ms = 0`. Measure the
    // duration off a monotonic `Instant` instead, which keeps sub-second
    // precision (and is immune to wall-clock jumps).
    let started_instant = std::time::Instant::now();
    let seq_field = ledger.as_ref().map(|(_, s)| *s).unwrap_or(-1);

    // Carry the turn's session context into the spawned task; `tokio::spawn`
    // starts a fresh task that wouldn't otherwise inherit the task-local.
    let session_ctx = current_session();

    // Soft tool-call budget (backstop): once this turn has reached
    // MAX_TOOL_CALLS_PER_TURN calls, refuse further ones with an error the
    // model sees instead of executing them, so a runaway loop can't fan out
    // unbounded. Inactive without a run ledger (seq_field = -1 < the cap).
    let result = if seq_field >= MAX_TOOL_CALLS_PER_TURN {
        warn!(
            tool = name,
            seq = seq_field,
            budget = MAX_TOOL_CALLS_PER_TURN,
            "tool-call budget reached for this turn; refusing"
        );
        Err(anyhow::anyhow!(
            "tool-call budget of {MAX_TOOL_CALLS_PER_TURN} reached for this turn; \
             stop calling tools and answer the user with what you already have."
        ))
    } else {
        let mut attempt: usize = 0;
        loop {
            // Span so the tool's own logs carry the run's `seq`/`name`. Spans don't
            // cross `tokio::spawn` on their own — instrument the spawned future. A
            // fresh span per attempt keeps each retry's logs distinct.
            let span = info_span!("tool", name, seq = seq_field, attempt);
            let tool_attempt = tool.clone();
            let input_attempt = input.clone();
            let join = match session_ctx.clone() {
                Some(ctx) => tokio::spawn(
                    SESSION
                        .scope(
                            ctx,
                            async move { tool_attempt.execute(input_attempt).await },
                        )
                        .instrument(span),
                ),
                None => tokio::spawn(
                    async move { tool_attempt.execute(input_attempt).await }.instrument(span),
                ),
            };
            let attempt_result = match join.await {
                Ok(result) => result,
                Err(join_err) if join_err.is_panic() => {
                    let panic = join_err.into_panic();
                    let msg = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    Err(anyhow::anyhow!("tool `{name}` panicked: {msg}"))
                }
                Err(join_err) => Err(anyhow::anyhow!("tool `{name}` was cancelled: {join_err}")),
            };

            match &attempt_result {
                Err(error)
                    if attempt + 1 < TOOL_RETRY_MAX_ATTEMPTS
                        && should_retry(error, tool.idempotent()) =>
                {
                    let delay = TOOL_RETRY_BACKOFF_MS[attempt.min(TOOL_RETRY_BACKOFF_MS.len() - 1)];
                    warn!(
                        tool = name,
                        seq = seq_field,
                        attempt = attempt + 1,
                        delay_ms = delay,
                        error = %format!("{error:#}"),
                        "transient tool error; retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    attempt += 1;
                }
                _ => break attempt_result,
            }
        }
    };

    // Record the step — best-effort, never affecting the tool's own result.
    if let (Some((run, seq)), Some(args)) = (ledger, redacted_args) {
        let ended_at = now();
        let (ok, result_s, error_s) = match &result {
            Ok(out) => (true, truncate(out, STEP_FIELD_CAP), String::new()),
            Err(e) => (
                false,
                String::new(),
                truncate(&format!("{e:#}"), STEP_FIELD_CAP),
            ),
        };
        if ok {
            info!(
                tool = name,
                seq,
                elapsed_ms = started_instant.elapsed().as_millis() as u64,
                "tool ok"
            );
        } else {
            warn!(tool = name, seq, error = %error_s, "tool failed");
        }
        let step = RunStep {
            run_id: run.run_id.clone(),
            seq,
            tool_name: name.to_string(),
            args: truncate(&args, STEP_FIELD_CAP),
            result: result_s,
            error: error_s,
            ok,
            started_at,
            ended_at,
        };
        if let Err(error) = run.repo.append_step(&step).await {
            warn!(%error, tool = name, "failed to record run step (non-fatal)");
        }
    }

    // Cap the LLM-facing result *after* the ledger records the original, so the
    // audit trail stays faithful while the model's context stays bounded.
    result.map(cap_tool_result)
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::run::{Run, RunStep};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Captures appended steps; everything else is inert.
    struct RecordingRuns {
        steps: Mutex<Vec<RunStep>>,
    }

    #[async_trait]
    impl RunRepository for RecordingRuns {
        async fn start(&self, _run: &Run) -> anyhow::Result<()> {
            Ok(())
        }
        async fn append_step(&self, step: &RunStep) -> anyhow::Result<()> {
            self.steps.lock().unwrap().push(step.clone());
            Ok(())
        }
        async fn finish(&self, _run: &Run) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list(&self, _limit: usize) -> anyhow::Result<Vec<Run>> {
            Ok(Vec::new())
        }
        async fn get(&self, _id: &str) -> anyhow::Result<Option<Run>> {
            Ok(None)
        }
        async fn steps(&self, _run_id: &str) -> anyhow::Result<Vec<RunStep>> {
            Ok(Vec::new())
        }
        async fn prune(&self, _cutoff: i64) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn reconcile_interrupted(&self, _now: i64) -> anyhow::Result<usize> {
            Ok(0)
        }
    }

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echoes its input"
        }
        async fn execute(&self, input: String) -> anyhow::Result<String> {
            Ok(format!("echoed: {input}"))
        }
    }

    #[tokio::test]
    async fn run_context_records_a_step_per_tool_call() {
        let repo = Arc::new(RecordingRuns {
            steps: Mutex::new(Vec::new()),
        });
        let ctx = RunContext::new("run-1".into(), repo.clone());
        with_run(ctx, async {
            execute_isolated(Arc::new(EchoTool), "hi".into())
                .await
                .unwrap();
        })
        .await;

        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].run_id, "run-1");
        assert_eq!(steps[0].seq, 0);
        assert_eq!(steps[0].tool_name, "echo");
        assert!(steps[0].ok);
        assert!(steps[0].result.contains("echoed: hi"));
        assert!(steps[0].error.is_empty());
    }

    #[tokio::test]
    async fn failed_tool_records_an_error_step() {
        let repo = Arc::new(RecordingRuns {
            steps: Mutex::new(Vec::new()),
        });
        let ctx = RunContext::new("run-2".into(), repo.clone());
        with_run(ctx, async {
            let _ = execute_isolated(Arc::new(PanickingTool), String::new()).await;
        })
        .await;

        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].ok);
        assert!(steps[0].error.contains("panicked"));
        assert!(steps[0].result.is_empty());
    }

    struct BigTool;
    #[async_trait]
    impl Tool for BigTool {
        fn name(&self) -> &'static str {
            "big"
        }
        fn description(&self) -> &'static str {
            "returns a large result"
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            Ok("x".repeat(crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES + 5000))
        }
    }

    #[tokio::test]
    async fn oversized_result_is_capped_with_marker() {
        let out = execute_isolated(Arc::new(BigTool), String::new())
            .await
            .unwrap();
        assert!(
            out.len() <= crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES + 200,
            "should be capped"
        );
        assert!(
            out.contains("truncated"),
            "should carry the truncation marker"
        );
    }

    #[tokio::test]
    async fn small_result_passes_through_uncapped() {
        let out = execute_isolated(Arc::new(EchoTool), "hi".into())
            .await
            .unwrap();
        assert_eq!(out, "echoed: hi");
    }

    #[test]
    fn cap_preserves_multibyte_boundaries() {
        // A run of 3-byte CJK chars whose total exceeds the cap: the cut must
        // land on a char boundary, not mid-codepoint (would panic otherwise).
        let big = "界".repeat(crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES); // 3 bytes each
        let capped = cap_tool_result(big);
        assert!(capped.contains("truncated"));
    }

    #[tokio::test]
    async fn no_run_context_records_nothing() {
        // No `with_run` wrapper: execute_isolated must still work and record nada.
        let out = execute_isolated(Arc::new(EchoTool), "x".into())
            .await
            .unwrap();
        assert!(out.contains("echoed: x"));
    }

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn name(&self) -> &'static str {
            "boom"
        }
        fn description(&self) -> &'static str {
            "always panics"
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            panic!("kaboom");
        }
    }

    #[tokio::test]
    async fn panicking_tool_returns_error_instead_of_crashing() {
        let err = execute_isolated(Arc::new(PanickingTool), String::new())
            .await
            .expect_err("panic should surface as an error");
        let msg = err.to_string();
        assert!(msg.contains("panicked"), "unexpected error: {msg}");
        assert!(msg.contains("kaboom"), "unexpected error: {msg}");
    }

    #[test]
    fn classify_error_buckets_by_marker() {
        // Connection-level wins even when an ambiguous word is also present.
        assert_eq!(
            classify_error(&anyhow::anyhow!(
                "request failed: error sending request: connection refused"
            )),
            Retry::ConnLevel
        );
        assert_eq!(
            classify_error(&anyhow::anyhow!("Home Assistant returned HTTP 503: down")),
            Retry::Ambiguous
        );
        assert_eq!(
            classify_error(&anyhow::anyhow!("operation timed out")),
            Retry::Ambiguous
        );
        // Unknown / terminal errors are never retried.
        assert_eq!(
            classify_error(&anyhow::anyhow!("invalid arguments: bad json")),
            Retry::No
        );
    }

    #[test]
    fn classify_error_prefers_typed_hint_over_text() {
        use crate::domain::tool::TransientError;
        use anyhow::Context as _;
        // A typed hint wins regardless of what the message text would match —
        // here the text says "invalid" (would be Retry::No via the heuristic).
        let conn = anyhow::Error::new(TransientError::new(
            RetryHint::Connection,
            "invalid: but typed as connection-level",
        ));
        assert_eq!(classify_error(&conn), Retry::ConnLevel);

        let amb = anyhow::Error::new(TransientError::new(RetryHint::Ambiguous, "anything"));
        assert_eq!(classify_error(&amb), Retry::Ambiguous);

        // The hint is still found through an added `.context(...)` layer.
        let wrapped = Err::<(), _>(anyhow::Error::new(TransientError::new(
            RetryHint::Connection,
            "boom",
        )))
        .context("while fetching")
        .unwrap_err();
        assert_eq!(classify_error(&wrapped), Retry::ConnLevel);
    }

    /// A tool that fails its first `fail_times` calls (with `error_msg`) then
    /// succeeds, counting every call. Lets a test assert how many attempts the
    /// retry loop made.
    struct FlakyTool {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        fail_times: usize,
        error_msg: &'static str,
        idempotent: bool,
    }

    #[async_trait]
    impl Tool for FlakyTool {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn description(&self) -> &'static str {
            "fails a few times then succeeds"
        }
        fn idempotent(&self) -> bool {
            self.idempotent
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_times {
                Err(anyhow::anyhow!("{}", self.error_msg))
            } else {
                Ok("ok".into())
            }
        }
    }

    fn flaky(
        fail_times: usize,
        error_msg: &'static str,
        idempotent: bool,
    ) -> (Arc<FlakyTool>, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = Arc::new(FlakyTool {
            calls: calls.clone(),
            fail_times,
            error_msg,
            idempotent,
        });
        (tool, calls)
    }

    // `start_paused` auto-advances the retry backoff sleeps, so these stay fast.

    #[tokio::test(start_paused = true)]
    async fn connection_error_is_retried_even_for_non_idempotent_tool() {
        // The request never reached the server, so a side effect can't have
        // landed — safe to retry regardless of idempotency.
        let (tool, calls) = flaky(2, "connection refused", false);
        let out = execute_isolated(tool, String::new()).await.unwrap();
        assert_eq!(out, "ok");
        assert_eq!(calls.load(Ordering::Relaxed), 3); // 2 failures + 1 success
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_error_is_not_retried() {
        let (tool, calls) = flaky(usize::MAX, "invalid arguments: bad json", true);
        let err = execute_isolated(tool, String::new()).await.unwrap_err();
        assert!(err.to_string().contains("invalid arguments"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_error_is_retried_only_for_idempotent_tool() {
        // Idempotent → retried.
        let (tool, calls) = flaky(1, "operation timed out", true);
        assert_eq!(execute_isolated(tool, String::new()).await.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // Non-idempotent → a timeout might have applied server-side; don't retry.
        let (tool, calls) = flaky(usize::MAX, "operation timed out", false);
        let _ = execute_isolated(tool, String::new()).await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_are_bounded_then_error_surfaces() {
        let (tool, calls) = flaky(usize::MAX, "connection refused", false);
        let err = execute_isolated(tool, String::new()).await.unwrap_err();
        assert!(
            err.to_string()
                .to_lowercase()
                .contains("connection refused")
        );
        assert_eq!(calls.load(Ordering::Relaxed), TOOL_RETRY_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn tool_call_budget_refuses_calls_past_the_cap() {
        let repo = Arc::new(RecordingRuns {
            steps: Mutex::new(Vec::new()),
        });
        let ctx = RunContext::new("run-budget".into(), repo.clone());
        let (tool, calls) = flaky(0, "unused", false); // never fails; just counts
        with_run(ctx, async {
            // Calls up to the cap all execute.
            for _ in 0..MAX_TOOL_CALLS_PER_TURN {
                execute_isolated(tool.clone(), String::new()).await.unwrap();
            }
            // The next call is refused without ever reaching the tool.
            let err = execute_isolated(tool.clone(), String::new())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("budget"), "got: {err}");
        })
        .await;

        // The tool ran exactly cap times; the refused call never reached it.
        assert_eq!(
            calls.load(Ordering::Relaxed) as i64,
            MAX_TOOL_CALLS_PER_TURN
        );
        // The refusal is still recorded as a failed step, for audit visibility.
        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps.len() as i64, MAX_TOOL_CALLS_PER_TURN + 1);
        assert!(!steps.last().unwrap().ok);
        assert!(steps.last().unwrap().error.contains("budget"));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_collapses_into_a_single_ledger_step() {
        let repo = Arc::new(RecordingRuns {
            steps: Mutex::new(Vec::new()),
        });
        let ctx = RunContext::new("run-retry".into(), repo.clone());
        let (tool, calls) = flaky(1, "connection refused", false);
        with_run(ctx, async {
            assert_eq!(execute_isolated(tool, String::new()).await.unwrap(), "ok");
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        let steps = repo.steps.lock().unwrap();
        assert_eq!(
            steps.len(),
            1,
            "retries must record one step, not one per attempt"
        );
        assert!(steps[0].ok);
        assert_eq!(steps[0].seq, 0);
    }
}
