//! Tool execution as one deep module (architecture deepening plan §6).
//!
//! [`ToolExecutor`] owns the whole execution pipeline the agent loop used to
//! assemble by hand: catalog lookup, per-turn call budget, arg redaction,
//! panic-isolated spawning, transient-error retry, run-ledger recording, the
//! LLM-facing result cap, and error→outcome mapping. Callers hand it a round
//! of model-requested calls plus an explicit [`ToolTurnContext`] and get back
//! `ToolOutcome`s ready for `TurnDriver::step` — they never see lookup, retry,
//! ledger, or cap decisions. Execution policy (result cap, call budget) is
//! **instance-owned** [`ToolExecutionConfig`], not process globals, so two
//! executors can carry different policies.
//!
//! Every execution path funnels through the one internal core: the runtime's
//! loop via [`ToolExecutor::execute_round`], and rig's trait-required fallback
//! via the same core shared with each `RigTool` adapter.

pub mod context;
mod result;
mod retry;

use std::collections::HashMap;
use std::sync::Arc;

use tracing::{Instrument, info, info_span, warn};

pub use context::{
    RunContext, SessionContext, ToolContext, ToolTurnContext, current_session, with_session,
};

use crate::domain::approval::{ApprovalRequest, Approver};
use crate::domain::events::TurnEvent;
use crate::domain::llm::{ToolCallReq, ToolOutcome};
use crate::domain::run::{RunStep, STEP_FIELD_CAP, truncate};
use crate::domain::tool::{Tool, ToolError};

/// Cap on a `TurnEvent` result/args preview — small so the live event stream
/// stays lightweight (the full result is still capped separately for the model
/// and stored in full-ish in the ledger).
const EVENT_SUMMARY_CAP: usize = 300;

use context::SESSION;
use result::cap_tool_result;
use retry::{TOOL_RETRY_BACKOFF_MS, TOOL_RETRY_MAX_ATTEMPTS, should_retry};

/// Soft per-turn tool-call budget default (backstop). The runtime's
/// `max_turns` bounds *round-trips*, but a single round can request many tools
/// at once; this caps the *total* calls per turn so a runaway loop can't fan
/// out unbounded. Set generously above any legitimate turn. Enforced against
/// the run-ledger seq, so it applies only to ledgered turns (the main agent),
/// never to callers without a run context.
const DEFAULT_MAX_TOOL_CALLS_PER_TURN: i64 = 100;

/// Instance-owned execution policy.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionConfig {
    /// Byte cap on a tool result handed back to the LLM.
    pub max_result_bytes: usize,
    /// Per-turn cap on ledgered tool calls (logical calls, not retry attempts).
    pub max_calls_per_turn: i64,
}

impl Default for ToolExecutionConfig {
    fn default() -> Self {
        Self {
            max_result_bytes: crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES,
            max_calls_per_turn: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
        }
    }
}

impl ToolExecutionConfig {
    /// The default policy with a specific result cap (the one setting that is
    /// user-configurable, via `max_tool_result_bytes`).
    pub fn with_result_cap(max_result_bytes: usize) -> Self {
        Self {
            max_result_bytes,
            ..Self::default()
        }
    }
}

/// The tool-execution module's external interface. Cheap to clone (one `Arc`);
/// the runtime and every `RigTool` adapter share the same core, so all
/// execution paths carry identical retry/ledger/cap semantics.
#[derive(Clone)]
pub struct ToolExecutor {
    core: Arc<ToolExecutionCore>,
}

/// The shared implementation: the immutable catalog plus the execution policy
/// and the approver every migrated tool reaches through its [`ToolContext`].
/// Holds only tools, config, and the approver — never the adapters wrapping it —
/// so there is no reference cycle with `RigTool`.
pub struct ToolExecutionCore {
    tools: HashMap<String, Arc<dyn Tool>>,
    config: ToolExecutionConfig,
    /// The approver placed into each call's [`ToolContext`]. Defaults to
    /// deny-all; wiring installs the real (policy-wrapped) approver via
    /// [`ToolExecutor::with_approver`].
    approver: Arc<dyn Approver>,
}

impl ToolExecutor {
    pub fn new(config: ToolExecutionConfig) -> Self {
        Self {
            core: Arc::new(ToolExecutionCore {
                tools: HashMap::new(),
                config,
                approver: Arc::new(DenyAllApprover),
            }),
        }
    }

    /// Install the approver handed to every tool via its [`ToolContext`]. Called
    /// during wiring before the executor is shared (like [`register`]).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        let core = Arc::get_mut(&mut self.core)
            .expect("set the approver during wiring, before the executor is shared");
        core.approver = approver;
        self
    }

    /// Add a tool to the catalog. Registration happens during wiring, before
    /// the executor is shared — the catalog is immutable once clones exist.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let core = Arc::get_mut(&mut self.core)
            .expect("register tools during wiring, before the executor is shared");
        core.tools.insert(tool.name().to_string(), tool);
    }

    /// The catalog as the model adapter needs it (schemas for function
    /// calling). A read-only view — execution always goes through the executor.
    pub fn definitions(&self) -> Vec<Arc<dyn Tool>> {
        self.core.tools.values().cloned().collect()
    }

    /// The shared execution core, for adapters (`RigTool`) that must satisfy a
    /// foreign trait's call signature while keeping one execution semantics.
    pub fn core(&self) -> Arc<ToolExecutionCore> {
        self.core.clone()
    }

    /// Execute one round of model-requested tool calls concurrently, preserving
    /// order. Unknown tools and tool errors are mapped into the outcome content
    /// (the model can recover); nothing here aborts the turn.
    ///
    /// Concurrency is safe for approval prompts: the interactive approver
    /// serializes them per session, so two side-effecting tools in one round
    /// still prompt one at a time.
    pub async fn execute_round(
        &self,
        calls: &[ToolCallReq],
        context: &ToolTurnContext,
    ) -> Vec<ToolOutcome> {
        let futures = calls.iter().map(|call| async move {
            let content = match self.core.tools.get(&call.name) {
                Some(tool) => match self
                    .core
                    .execute(tool.clone(), call.args.clone(), context)
                    .await
                {
                    Ok(out) => out,
                    Err(error) => format!("tool `{}` failed: {error:#}", call.name),
                },
                None => format!("error: unknown tool `{}`", call.name),
            };
            ToolOutcome {
                id: call.id.clone(),
                call_id: call.call_id.clone(),
                content,
            }
        });
        futures_util::future::join_all(futures).await
    }
}

impl ToolExecutionCore {
    /// Run one tool call through the full pipeline. The invariant order:
    ///
    /// 1. claim a ledger seq (budget counts logical calls, not attempts)
    /// 2. redact args for the audit record
    /// 3. execute on an isolated, panic-catching task with the session context
    ///    installed and a `tool` tracing span
    /// 4. map panics/cancellation to errors
    /// 5. retry per the transient classification (typed hint first)
    /// 6. record the (original, truncated) step — best-effort
    /// 7. cap the LLM-facing result
    pub async fn execute(
        &self,
        tool: Arc<dyn Tool>,
        input: String,
        context: &ToolTurnContext,
    ) -> anyhow::Result<String> {
        let name = tool.name();

        // Ledger bookkeeping (only when this turn is recorded). Capture the
        // redacted args and seq up front: the raw `input` is cloned per attempt
        // below, and the seq must be claimed before the tool runs so the span
        // and the persisted step agree.
        let ledger = context.run.as_ref().map(|r| (r, r.next_seq()));
        let redacted_args = ledger.as_ref().map(|_| tool.redact_args(&input));
        let started_at = now();
        // Wall-clock timestamps (`now()`) are integer unix seconds — fine for
        // the ledger's started/ended fields, but differencing them only yields
        // whole seconds, so any sub-second tool would log `elapsed_ms = 0`.
        // Measure the duration off a monotonic `Instant` instead.
        let started_instant = std::time::Instant::now();
        let seq_field = ledger.as_ref().map(|(_, s)| *s).unwrap_or(-1);

        // Live event: a watcher (streaming client) sees the call start. Args are
        // the redacted form when ledgered, else redacted on the spot — never the
        // raw input. No-op when no sink is attached (the common case).
        if let Some(sink) = &context.session.event_sink {
            let args = redacted_args
                .clone()
                .unwrap_or_else(|| tool.redact_args(&input));
            sink.emit(TurnEvent::ToolStarted {
                seq: seq_field,
                name: name.to_string(),
                args: truncate(&args, EVENT_SUMMARY_CAP),
            });
        }

        // Parse the model's JSON arguments once, here, so every tool sees a
        // typed `Value` (and `parse_args` can produce the canonical
        // `InvalidInput` error). Non-JSON args and the empty (no-arg) call are
        // preserved: an unparseable string is wrapped as `Value::String` so the
        // legacy bridge in `Tool::call` can hand the original text back to an
        // unmigrated tool; migrated tools reject it via `parse_args`.
        let value = serde_json::from_str::<serde_json::Value>(&input)
            .unwrap_or_else(|_| serde_json::Value::String(input.clone()));

        // Soft tool-call budget (backstop): once this turn has reached the cap,
        // refuse further calls with an error the model sees instead of
        // executing them. Inactive without a run ledger (seq_field = -1).
        let result: anyhow::Result<String> = if seq_field >= self.config.max_calls_per_turn {
            warn!(
                tool = name,
                seq = seq_field,
                budget = self.config.max_calls_per_turn,
                "tool-call budget reached for this turn; refusing"
            );
            Err(anyhow::anyhow!(
                "tool-call budget of {} reached for this turn; \
                 stop calling tools and answer the user with what you already have.",
                self.config.max_calls_per_turn
            ))
        } else {
            let mut attempt: usize = 0;
            let outcome: Result<crate::domain::tool::ToolOutput, ToolError> = loop {
                // Span so the tool's own logs carry the run's `seq`/`name`.
                // Spans don't cross `tokio::spawn` on their own — instrument
                // the spawned future. A fresh span per attempt keeps each
                // retry's logs distinct.
                let span = info_span!("tool", name, seq = seq_field, attempt);
                let tool_attempt = tool.clone();
                let value_attempt = value.clone();
                // Build the explicit per-call context, and also install the
                // turn's session as the ambient scope for the spawned task —
                // the approvers still read it (they don't take a context
                // parameter), and a fresh task doesn't inherit task-locals.
                let ctx = ToolContext::new(
                    context.session.clone(),
                    context.run.clone(),
                    self.approver.clone(),
                );
                let scope = context.session.clone();
                let join = tokio::spawn(
                    SESSION
                        .scope(scope, async move {
                            tool_attempt.call(value_attempt, &ctx).await
                        })
                        .instrument(span),
                );
                let attempt_result: Result<crate::domain::tool::ToolOutput, ToolError> =
                    match join.await {
                        Ok(result) => result,
                        Err(join_err) if join_err.is_panic() => {
                            let panic = join_err.into_panic();
                            let msg = panic
                                .downcast_ref::<String>()
                                .map(String::as_str)
                                .or_else(|| panic.downcast_ref::<&str>().copied())
                                .unwrap_or("unknown panic");
                            Err(ToolError::Failed(anyhow::anyhow!(
                                "tool `{name}` panicked: {msg}"
                            )))
                        }
                        Err(join_err) => Err(ToolError::Failed(anyhow::anyhow!(
                            "tool `{name}` was cancelled: {join_err}"
                        ))),
                    };

                match &attempt_result {
                    // Only genuine failures retry; InvalidInput/Denied are
                    // recoverable and terminal.
                    Err(ToolError::Failed(error))
                        if attempt + 1 < TOOL_RETRY_MAX_ATTEMPTS
                            && should_retry(error, tool.idempotent()) =>
                    {
                        let delay =
                            TOOL_RETRY_BACKOFF_MS[attempt.min(TOOL_RETRY_BACKOFF_MS.len() - 1)];
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
            };

            // Classify into the ledger-facing `anyhow::Result<String>`:
            // recoverable errors become model-facing content (never retried);
            // a genuine failure stays an `Err` so the ledger marks the step
            // failed and `execute_round` surfaces it.
            match outcome {
                Ok(out) => Ok(out.text),
                Err(ToolError::InvalidInput(m)) => Ok(format!(
                    "invalid input for tool `{name}`: {m}. \
                     Rewrite the arguments to match the tool's schema."
                )),
                Err(ToolError::Denied(m)) => Ok(m),
                Err(ToolError::Failed(e)) => Err(e),
            }
        };

        // Live event: the call finished (after retries collapse). Emitted
        // regardless of ledger state so a watcher sees every call resolve.
        if let Some(sink) = &context.session.event_sink {
            let (ok, summary) = match &result {
                Ok(out) => (true, truncate(out, EVENT_SUMMARY_CAP)),
                Err(e) => (false, truncate(&format!("{e:#}"), EVENT_SUMMARY_CAP)),
            };
            sink.emit(TurnEvent::ToolFinished {
                seq: seq_field,
                name: name.to_string(),
                ok,
                summary,
            });
        }

        // Record the step — best-effort, never affecting the tool's own result.
        // Retries collapse into this one step: the retry is a robustness
        // detail, not extra audit rows.
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

        // Cap the LLM-facing result *after* the ledger records the original, so
        // the audit trail stays faithful while the model's context stays bounded.
        result.map(|out| cap_tool_result(out, self.config.max_result_bytes))
    }

    /// The trait-required fallback for a rig-driven completion (not on komo's
    /// hot path — the runtime owns the loop). Bridges from rig's fixed call
    /// signature: session from the ambient context if any, no run ledger.
    pub async fn execute_fallback(
        &self,
        tool: Arc<dyn Tool>,
        input: String,
    ) -> anyhow::Result<String> {
        let context = ToolTurnContext {
            session: current_session().unwrap_or_else(|| SessionContext::detached("")),
            run: None,
        };
        self.execute(tool, input, &context).await
    }
}

/// The executor's default approver until wiring installs the real one
/// (`ToolExecutor::with_approver`): deny everything. A tool that reaches
/// `ctx.approve(..)` before an approver is set is refused rather than silently
/// allowed — matters only in tests that don't exercise a migrated gated tool.
struct DenyAllApprover;

#[async_trait::async_trait]
impl Approver for DenyAllApprover {
    async fn approve(&self, _request: &ApprovalRequest) -> bool {
        false
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    //! Behavior tests through the `ToolExecutor` interface — a round of calls
    //! in, outcomes out. The executor owns lookup/retry/ledger/cap, so that is
    //! where they are asserted.

    use super::*;
    use crate::domain::run::{Run, RunRepository, RunStep};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Captures appended steps; everything else is inert. `fail_appends` makes
    /// every ledger write fail, for the write-failure contract.
    struct RecordingRuns {
        steps: Mutex<Vec<RunStep>>,
        fail_appends: bool,
    }

    impl RecordingRuns {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(Vec::new()),
                fail_appends: false,
            })
        }
    }

    #[async_trait]
    impl RunRepository for RecordingRuns {
        async fn start(&self, _run: &Run) -> anyhow::Result<()> {
            Ok(())
        }
        async fn append_step(&self, step: &RunStep) -> anyhow::Result<()> {
            if self.fail_appends {
                anyhow::bail!("ledger unavailable");
            }
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
        async fn mark_resumed(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn steps_by_tool(
            &self,
            _tool_name: &str,
            _limit: usize,
        ) -> anyhow::Result<Vec<RunStep>> {
            Ok(Vec::new())
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

    struct SecretTool;
    #[async_trait]
    impl Tool for SecretTool {
        fn name(&self) -> &'static str {
            "secretive"
        }
        fn description(&self) -> &'static str {
            "redacts its args"
        }
        fn redact_args(&self, _args: &str) -> String {
            "[redacted]".to_string()
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            Ok("done".into())
        }
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

    /// A tool that fails its first `fail_times` calls (with `error_msg`) then
    /// succeeds, counting every call. Lets a test assert how many attempts the
    /// retry loop made.
    struct FlakyTool {
        calls: Arc<AtomicUsize>,
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
    ) -> (Arc<FlakyTool>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = Arc::new(FlakyTool {
            calls: calls.clone(),
            fail_times,
            error_msg,
            idempotent,
        });
        (tool, calls)
    }

    fn executor(tools: Vec<Arc<dyn Tool>>, config: ToolExecutionConfig) -> ToolExecutor {
        let mut executor = ToolExecutor::new(config);
        for t in tools {
            executor.register(t);
        }
        executor
    }

    fn call(name: &str, args: &str) -> ToolCallReq {
        ToolCallReq {
            id: format!("id-{name}"),
            call_id: None,
            name: name.to_string(),
            args: args.to_string(),
        }
    }

    fn ledgered(repo: Arc<RecordingRuns>) -> ToolTurnContext {
        ToolTurnContext {
            session: SessionContext::detached("cli:test"),
            run: Some(RunContext::new("run-1".into(), repo)),
        }
    }

    fn unledgered() -> ToolTurnContext {
        ToolTurnContext {
            session: SessionContext::detached("cli:test"),
            run: None,
        }
    }

    async fn one(
        executor: &ToolExecutor,
        req: ToolCallReq,
        context: &ToolTurnContext,
    ) -> ToolOutcome {
        executor
            .execute_round(std::slice::from_ref(&req), context)
            .await
            .remove(0)
    }

    #[tokio::test]
    async fn round_preserves_order_and_maps_unknown_tools() {
        let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
        let outcomes = executor
            .execute_round(
                &[call("echo", "a"), call("nope", "{}"), call("echo", "b")],
                &unledgered(),
            )
            .await;
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0].content, "echoed: a");
        assert_eq!(outcomes[1].content, "error: unknown tool `nope`");
        assert_eq!(outcomes[2].content, "echoed: b");
        assert_eq!(outcomes[1].id, "id-nope", "ids line up with calls");
    }

    #[tokio::test]
    async fn ledgered_call_records_one_step() {
        let repo = RecordingRuns::new();
        let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
        let out = one(&executor, call("echo", "hi"), &ledgered(repo.clone())).await;
        assert_eq!(out.content, "echoed: hi");

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
    async fn redaction_happens_before_the_ledger() {
        let repo = RecordingRuns::new();
        let executor = executor(vec![Arc::new(SecretTool)], ToolExecutionConfig::default());
        one(
            &executor,
            call("secretive", "token=hunter2"),
            &ledgered(repo.clone()),
        )
        .await;
        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps[0].args, "[redacted]");
        assert!(!steps[0].args.contains("hunter2"));
    }

    #[tokio::test]
    async fn ledger_failure_never_changes_the_tool_result() {
        let repo = Arc::new(RecordingRuns {
            steps: Mutex::new(Vec::new()),
            fail_appends: true,
        });
        let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
        let out = one(&executor, call("echo", "hi"), &ledgered(repo)).await;
        assert_eq!(out.content, "echoed: hi");
    }

    #[tokio::test]
    async fn panicking_tool_becomes_an_error_outcome_and_error_step() {
        let repo = RecordingRuns::new();
        let executor = executor(
            vec![Arc::new(PanickingTool)],
            ToolExecutionConfig::default(),
        );
        let out = one(&executor, call("boom", "{}"), &ledgered(repo.clone())).await;
        assert!(out.content.contains("panicked"), "got: {}", out.content);
        assert!(out.content.contains("kaboom"));

        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].ok);
        assert!(steps[0].error.contains("panicked"));
        assert!(steps[0].result.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn connection_error_is_retried_even_for_non_idempotent_tool() {
        // The request never reached the server, so a side effect can't have
        // landed — safe to retry regardless of idempotency.
        let (tool, calls) = flaky(2, "connection refused", false);
        let executor = executor(vec![tool], ToolExecutionConfig::default());
        let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
        assert_eq!(out.content, "ok");
        assert_eq!(calls.load(Ordering::Relaxed), 3); // 2 failures + 1 success
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_error_is_not_retried() {
        let (tool, calls) = flaky(usize::MAX, "invalid arguments: bad json", true);
        let executor = executor(vec![tool], ToolExecutionConfig::default());
        let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
        assert!(out.content.contains("invalid arguments"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_error_is_retried_only_for_idempotent_tool() {
        // Idempotent → retried.
        let (tool, calls) = flaky(1, "operation timed out", true);
        let executor = self::executor(vec![tool], ToolExecutionConfig::default());
        let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
        assert_eq!(out.content, "ok");
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // Non-idempotent → a timeout might have applied server-side; don't retry.
        let (tool, calls) = flaky(usize::MAX, "operation timed out", false);
        let executor = self::executor(vec![tool], ToolExecutionConfig::default());
        let _ = one(&executor, call("flaky", "{}"), &unledgered()).await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_are_bounded_then_error_surfaces() {
        let (tool, calls) = flaky(usize::MAX, "connection refused", false);
        let executor = executor(vec![tool], ToolExecutionConfig::default());
        let out = one(&executor, call("flaky", "{}"), &unledgered()).await;
        assert!(out.content.to_lowercase().contains("connection refused"));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            retry::TOOL_RETRY_MAX_ATTEMPTS
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_collapses_into_a_single_ledger_step() {
        let repo = RecordingRuns::new();
        let (tool, calls) = flaky(1, "connection refused", false);
        let executor = executor(vec![tool], ToolExecutionConfig::default());
        let out = one(&executor, call("flaky", "{}"), &ledgered(repo.clone())).await;
        assert_eq!(out.content, "ok");
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

    #[tokio::test]
    async fn budget_counts_logical_calls_and_refuses_past_the_cap() {
        let repo = RecordingRuns::new();
        let (tool, calls) = flaky(0, "unused", false); // never fails; just counts
        let executor = executor(
            vec![tool],
            ToolExecutionConfig {
                max_calls_per_turn: 5,
                ..Default::default()
            },
        );
        let context = ledgered(repo.clone());
        for _ in 0..5 {
            let out = one(&executor, call("flaky", "{}"), &context).await;
            assert_eq!(out.content, "ok");
        }
        // The next call is refused without ever reaching the tool.
        let out = one(&executor, call("flaky", "{}"), &context).await;
        assert!(out.content.contains("budget"), "got: {}", out.content);
        assert_eq!(calls.load(Ordering::Relaxed), 5);

        // The refusal is still recorded as a failed step, for audit visibility.
        let steps = repo.steps.lock().unwrap();
        assert_eq!(steps.len(), 6);
        assert!(!steps.last().unwrap().ok);
        assert!(steps.last().unwrap().error.contains("budget"));
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
            Ok("x".repeat(10_000))
        }
    }

    #[tokio::test]
    async fn two_executors_carry_different_result_caps() {
        // The cap is instance policy, not a process global: the same tool
        // through two executors gets two different ceilings.
        let tight = executor(
            vec![Arc::new(BigTool)],
            ToolExecutionConfig {
                max_result_bytes: 1024,
                ..Default::default()
            },
        );
        let roomy = executor(
            vec![Arc::new(BigTool)],
            ToolExecutionConfig {
                max_result_bytes: 64 * 1024,
                ..Default::default()
            },
        );
        let capped = one(&tight, call("big", "{}"), &unledgered()).await;
        assert!(capped.content.len() <= 1024 + 200);
        assert!(capped.content.contains("truncated"));

        let free = one(&roomy, call("big", "{}"), &unledgered()).await;
        assert_eq!(free.content.len(), 10_000, "no truncation under the cap");
    }

    #[tokio::test]
    async fn unledgered_context_records_nothing_and_still_works() {
        let executor = executor(vec![Arc::new(EchoTool)], ToolExecutionConfig::default());
        let out = one(&executor, call("echo", "x"), &unledgered()).await;
        assert_eq!(out.content, "echoed: x");
    }
}
