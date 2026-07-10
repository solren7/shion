use std::sync::Arc;

use tracing::{Instrument, info, info_span, warn};

use crate::{
    domain::{
        llm::{LlmClient, Step, ToolOutcome},
        message::Message,
        repository::{MessageRepository, SessionRepository},
        reviewer::Reviewer,
        run::{RUN_FIELD_CAP, Run, RunRepository, RunStatus, truncate},
        session::Session,
    },
    services::tool_execution::{
        RunContext, SessionContext, ToolExecutor, ToolTurnContext, current_session, with_session,
    },
};

/// Fed back to the model in place of tool results once the per-turn round
/// budget (`max_turns`) is exceeded, so it answers instead of calling more
/// tools. The turn then terminates regardless of the model's next move.
const BUDGET_REACHED_NOTE: &str = "Tool-call budget for this turn reached; do not call any \
     more tools. Reply to the user now using what you already have.";

pub struct AgentRuntime {
    pub llm: Arc<dyn LlmClient>,
    pub sessions: Arc<dyn SessionRepository>,
    pub messages: Arc<dyn MessageRepository>,
    /// Run ledger: every turn is recorded here, with one step per tool call
    /// (captured by the tool executor). See `domain/run.rs`, roadmap §7.
    pub runs: Arc<dyn RunRepository>,
    /// Tool catalog the in-house loop dispatches against. shion (not rig) now
    /// owns the multi-step loop and hands each round of requested calls to the
    /// executor, which owns lookup/retry/ledger/cap. See `run_agent_loop`.
    pub tool_executor: ToolExecutor,
    /// Max tool-calling rounds per turn before the loop forces a final answer
    /// (config `max_turns`). The hard, loop-level budget — distinct from the
    /// executor's per-call fan-out cap.
    pub max_turns: usize,
    /// How many recent messages to load for the turn's agent loop (mirrors the
    /// LLM's `max_history_messages`; `0` = load the whole transcript). Keeps the
    /// per-turn hot path off a full-transcript read for long-lived chat
    /// sessions — the LLM windows again to the same bound, so this is loss-free.
    pub history_window: usize,
    pub reviewer: Option<Arc<dyn Reviewer>>,
    pub review_interval: usize,
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl AgentRuntime {
    pub async fn handle_input(
        &self,
        session_id: &str,
        user_input: String,
    ) -> anyhow::Result<String> {
        // Session-scoped tools (e.g. `todo`) read the turn's session from the
        // ambient context. The gateway dispatcher sets it (with a real reply
        // sink); the REPL calls us directly, so establish a detached context
        // here when none exists. Don't override an existing one — that would
        // drop the gateway's sink and break mid-turn approval.
        if current_session().is_none() {
            let ctx = SessionContext::detached(session_id);
            return with_session(ctx, self.run_turn(session_id, user_input)).await;
        }
        self.run_turn(session_id, user_input).await
    }

    /// One turn = one [`Run`]. Opens a ledger entry, runs the turn body under a
    /// `RunContext` (so tool calls record steps) and a `run` tracing span, then
    /// finalizes the entry with the outcome. All ledger writes are best-effort:
    /// a ledger failure is logged but never changes the turn's result.
    async fn run_turn(&self, session_id: &str, user_input: String) -> anyhow::Result<String> {
        let mut run = Run::start(session_id, &user_input);
        if let Err(error) = self.runs.start(&run).await {
            warn!(%error, "failed to open run ledger entry (non-fatal)");
        }

        let span = info_span!("run", run_id = %run.id, session = %session_id);
        let ctx = RunContext::new(run.id.clone(), self.runs.clone());
        // Keep a handle to read the tool-step count after the turn (the seq
        // counter is shared via `Arc`, so this clone sees the final value).
        let probe = ctx.clone();

        let outcome = self
            .turn_body(session_id, user_input, ctx)
            .instrument(span)
            .await;

        run.plan = match probe.steps_count() {
            0 => "respond".to_string(),
            n => format!("{n} tool call(s)"),
        };
        run.ended_at = Some(now());
        match &outcome {
            Ok(reply) => {
                run.status = RunStatus::Done;
                run.final_output = truncate(reply, RUN_FIELD_CAP);
                info!(run_id = %run.id, "run done");
            }
            Err(error) => {
                run.status = RunStatus::Failed;
                run.error = truncate(&format!("{error:#}"), RUN_FIELD_CAP);
                warn!(run_id = %run.id, %error, "run failed");
            }
        }
        if let Err(error) = self.runs.finish(&run).await {
            warn!(%error, "failed to finalize run ledger entry (non-fatal)");
        }

        outcome
    }

    /// The turn's actual work: persist the user message, drive the agent loop
    /// (shion owns it — model round-trip, execute requested tools, feed results
    /// back, repeat), persist the reply, and kick off the periodic reviewer.
    async fn turn_body(
        &self,
        session_id: &str,
        user_input: String,
        run: RunContext,
    ) -> anyhow::Result<String> {
        // Load only the recent window for the agent loop — the LLM windows the
        // history to the same bound anyway, so a long-lived chat session no
        // longer deserializes its whole transcript every turn. The reviewer
        // (below) still gets the full transcript, on the turns it actually runs.
        let mut session = match self
            .sessions
            .find_windowed(session_id, self.history_window)
            .await?
        {
            Some(s) => s,
            None => {
                let s = Session::new(session_id);
                self.sessions.save(&s).await?;
                s
            }
        };

        let user_msg = Message::user(&user_input);
        self.messages.save(session_id, &user_msg).await?;
        session.messages.push(user_msg);

        let reply = self.run_agent_loop(&session, run).await?;

        let assistant_msg = Message::assistant(&reply);
        self.messages.save(session_id, &assistant_msg).await?;
        session.messages.push(assistant_msg);

        if let Some(reviewer) = &self.reviewer {
            let interval = self.review_interval.max(1);
            // Cadence is driven by the true user-turn total (a cheap COUNT), not
            // the windowed in-memory session, whose count would plateau at the
            // window size and mis-fire the modulo.
            let turns = self.messages.count_user_turns(session_id).await?;
            if turns % interval == 0 {
                // The reflective reviewer needs the whole transcript, so reload
                // the full session (this turn's messages are already persisted)
                // rather than handing it the truncated working window.
                if let Some(snapshot) = self.sessions.find(session_id).await? {
                    let reviewer = reviewer.clone();
                    // Advance the shared review watermark on success so the
                    // background sweep doesn't re-review what this just covered.
                    let sessions = self.sessions.clone();
                    let sid = session_id.to_string();
                    tokio::spawn(async move {
                        match reviewer.review(&snapshot).await {
                            Ok(outcome) => {
                                if !outcome.is_empty() {
                                    info!(?outcome, "self-improvement review");
                                }
                                if let Err(error) = sessions.mark_reviewed(&sid, turns).await {
                                    warn!(%error, "failed to advance review watermark");
                                }
                            }
                            Err(error) => warn!(%error, "review failed (non-fatal)"),
                        }
                    });
                }
            }
        }

        Ok(reply)
    }

    /// shion's own tool-calling loop (roadmap §7 — the loop lives here, not in
    /// rig, so control points can sit between rounds). Drive the model a round
    /// at a time: a [`Step::Final`] ends the turn; [`Step::ToolCalls`] go to the
    /// tool executor as one round (it owns lookup, retry, the per-call budget,
    /// the ledger, and the result cap) and the outcomes are threaded back. Once
    /// the per-turn *round* budget is exceeded, feed [`BUDGET_REACHED_NOTE`]
    /// back in place of results and force a final answer.
    async fn run_agent_loop(&self, session: &Session, run: RunContext) -> anyhow::Result<String> {
        // The executor gets the turn's context explicitly: the run handle this
        // turn opened, and the session established by the dispatcher / api /
        // handle_input (read once here — the one ambient-to-explicit bridge).
        let context = ToolTurnContext {
            session: current_session().unwrap_or_else(|| SessionContext::detached(&session.id)),
            run: Some(run),
        };
        let mut driver = self.llm.begin_turn(session).await?;
        let mut step = driver.first().await?;
        let mut rounds = 0usize;

        loop {
            match step {
                Step::Final(text) => return Ok(text),
                Step::ToolCalls(calls) => {
                    rounds += 1;
                    let over_budget = rounds > self.max_turns;

                    let results: Vec<ToolOutcome> = if over_budget {
                        calls
                            .iter()
                            .map(|call| ToolOutcome {
                                id: call.id.clone(),
                                call_id: call.call_id.clone(),
                                content: BUDGET_REACHED_NOTE.to_string(),
                            })
                            .collect()
                    } else {
                        // One round, delegated whole: the executor runs the
                        // calls concurrently (order-preserving) and maps tool
                        // errors / unknown names into outcome content the model
                        // can recover from — only a driver/LLM error aborts the
                        // turn.
                        self.tool_executor.execute_round(&calls, &context).await
                    };

                    let next = driver.step(results).await?;
                    // Over budget, the note went back as well-formed tool results;
                    // terminate now no matter what the model did with it.
                    step = if over_budget {
                        return Ok(match next {
                            Step::Final(text) => text,
                            Step::ToolCalls(_) => "(Reached the tool-call limit for this turn; \
                                 answering with what I have.)"
                                .to_string(),
                        });
                    } else {
                        next
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            llm::{LlmClient, Step, ToolCallReq, TurnDriver},
            run::RunStatus,
            session::Session,
            tool::Tool,
        },
        infra::persistence::db::Db,
        tools::time::TimeTool,
    };
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// An [`LlmClient`] that replays a scripted sequence of [`Step`]s and records
    /// the tool results fed back to each `step()` — no rig, no network. Lets us
    /// drive `run_agent_loop` deterministically and assert dispatch, threading,
    /// the ledger, and the round budget.
    struct ScriptedLlm {
        script: Mutex<VecDeque<Step>>,
        received: Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    }

    #[async_trait]
    impl LlmClient for ScriptedLlm {
        async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
            Ok("unused".to_string())
        }
        async fn begin_turn(&self, _session: &Session) -> anyhow::Result<Box<dyn TurnDriver>> {
            // One turn per test, so hand the whole script to the driver.
            let steps = std::mem::take(&mut *self.script.lock().unwrap());
            Ok(Box::new(ScriptedDriver {
                steps,
                received: self.received.clone(),
            }))
        }
    }

    struct ScriptedDriver {
        steps: VecDeque<Step>,
        received: Arc<Mutex<Vec<Vec<ToolOutcome>>>>,
    }

    #[async_trait]
    impl TurnDriver for ScriptedDriver {
        async fn first(&mut self) -> anyhow::Result<Step> {
            Ok(self.steps.pop_front().expect("script exhausted at first()"))
        }
        async fn step(&mut self, results: Vec<ToolOutcome>) -> anyhow::Result<Step> {
            self.received.lock().unwrap().push(results);
            Ok(self.steps.pop_front().expect("script exhausted at step()"))
        }
    }

    /// A tool that echoes its raw input, for asserting result threading.
    struct EchoArgsTool;
    #[async_trait]
    impl Tool for EchoArgsTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echoes its input args"
        }
        async fn execute(&self, input: String) -> anyhow::Result<String> {
            Ok(format!("echo:{input}"))
        }
    }

    /// A tool that always errors, for asserting failures feed back (not abort).
    struct FailTool;
    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &'static str {
            "fail"
        }
        fn description(&self) -> &'static str {
            "always errors"
        }
        async fn execute(&self, _input: String) -> anyhow::Result<String> {
            anyhow::bail!("boom")
        }
    }

    fn sqlite_url(name: &str) -> String {
        let path = std::env::temp_dir().join(name);
        crate::infra::persistence::reset_test_db(&path);
        format!("turso:{}", path.display())
    }

    fn call(name: &str, args: &str) -> ToolCallReq {
        ToolCallReq {
            id: format!("id-{name}"),
            call_id: None,
            name: name.to_string(),
            args: args.to_string(),
        }
    }

    /// Build a runtime whose LLM replays `script`, with `tools` registered and a
    /// round budget of `max_turns`. Returns the runtime and a handle to the tool
    /// results fed back to the driver, round by round.
    fn scripted_runtime(
        db: Arc<Db>,
        script: Vec<Step>,
        tools: Vec<Arc<dyn Tool>>,
        max_turns: usize,
    ) -> (AgentRuntime, Arc<Mutex<Vec<Vec<ToolOutcome>>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut executor =
            ToolExecutor::new(crate::services::tool_execution::ToolExecutionConfig::default());
        for t in tools {
            executor.register(t);
        }
        let rt = AgentRuntime {
            llm: Arc::new(ScriptedLlm {
                script: Mutex::new(script.into()),
                received: received.clone(),
            }),
            sessions: db.clone(),
            messages: db.clone(),
            runs: db.clone(),
            tool_executor: executor,
            max_turns,
            history_window: 0,
            reviewer: None,
            review_interval: 10,
        };
        (rt, received)
    }

    #[tokio::test]
    async fn turn_with_a_tool_call_records_a_run_with_a_step() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_tool_run.db"))
                .await
                .unwrap(),
        );
        let (rt, _) = scripted_runtime(
            db.clone(),
            vec![
                Step::ToolCalls(vec![call("time", "{}")]),
                Step::Final("the time is now".into()),
            ],
            vec![Arc::new(TimeTool)],
            30,
        );

        rt.handle_input("cli:s1", "hi".into()).await.unwrap();

        let runs = RunRepository::list(&*db, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Done);
        assert_eq!(runs[0].plan, "1 tool call(s)");

        let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool_name, "time");
        assert!(steps[0].ok);
    }

    #[tokio::test]
    async fn turn_without_tools_records_a_run_without_steps() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_direct_run.db"))
                .await
                .unwrap(),
        );
        let (rt, _) = scripted_runtime(
            db.clone(),
            vec![Step::Final("hello there".into())],
            vec![],
            30,
        );

        let reply = rt.handle_input("cli:s2", "hi".into()).await.unwrap();
        assert_eq!(reply, "hello there");

        let runs = RunRepository::list(&*db, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Done);
        assert_eq!(runs[0].plan, "respond");
        assert_eq!(runs[0].final_output, "hello there");

        let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
        assert!(steps.is_empty());
    }

    #[tokio::test]
    async fn multi_round_threads_tool_results_back() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_threading.db"))
                .await
                .unwrap(),
        );
        let (rt, received) = scripted_runtime(
            db.clone(),
            vec![
                Step::ToolCalls(vec![call("echo", "A")]),
                Step::ToolCalls(vec![call("echo", "B")]),
                Step::Final("done".into()),
            ],
            vec![Arc::new(EchoArgsTool)],
            30,
        );

        let reply = rt.handle_input("cli:s3", "hi".into()).await.unwrap();
        assert_eq!(reply, "done");

        let rec = received.lock().unwrap();
        assert_eq!(rec.len(), 2, "two tool rounds before the final answer");
        assert_eq!(rec[0][0].content, "echo:A");
        assert_eq!(rec[0][0].id, "id-echo");
        assert_eq!(rec[1][0].content, "echo:B");
    }

    #[tokio::test]
    async fn tool_error_feeds_back_without_aborting() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_toolerr.db"))
                .await
                .unwrap(),
        );
        let (rt, received) = scripted_runtime(
            db.clone(),
            vec![
                Step::ToolCalls(vec![call("fail", "{}")]),
                Step::Final("recovered".into()),
            ],
            vec![Arc::new(FailTool)],
            30,
        );

        let reply = rt.handle_input("cli:s4", "hi".into()).await.unwrap();
        assert_eq!(reply, "recovered");
        assert!(received.lock().unwrap()[0][0].content.contains("failed"));

        let runs = RunRepository::list(&*db, 10).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Done);
    }

    #[tokio::test]
    async fn unknown_tool_feeds_back_without_aborting() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_unknown.db"))
                .await
                .unwrap(),
        );
        let (rt, received) = scripted_runtime(
            db.clone(),
            vec![
                Step::ToolCalls(vec![call("nope", "{}")]),
                Step::Final("ok".into()),
            ],
            vec![],
            30,
        );

        let reply = rt.handle_input("cli:s5", "hi".into()).await.unwrap();
        assert_eq!(reply, "ok");
        assert!(
            received.lock().unwrap()[0][0]
                .content
                .contains("unknown tool")
        );
    }

    #[tokio::test]
    async fn round_budget_forces_a_final_answer() {
        let db = Arc::new(
            Db::connect(&sqlite_url("shion_rt_budget.db"))
                .await
                .unwrap(),
        );
        // Driver keeps requesting tools; with max_turns=2 the loop must stop.
        let (rt, _) = scripted_runtime(
            db.clone(),
            vec![
                Step::ToolCalls(vec![call("time", "{}")]),
                Step::ToolCalls(vec![call("time", "{}")]),
                Step::ToolCalls(vec![call("time", "{}")]),
                Step::ToolCalls(vec![call("time", "{}")]),
            ],
            vec![Arc::new(TimeTool)],
            2,
        );

        let reply = rt.handle_input("cli:s6", "hi".into()).await.unwrap();
        assert!(reply.contains("tool-call limit"), "got: {reply}");

        let runs = RunRepository::list(&*db, 10).await.unwrap();
        assert_eq!(runs[0].status, RunStatus::Done);
        // Only the first two rounds actually dispatched; round 3 got the budget
        // note instead of executing, so exactly two ledger steps.
        let steps = RunRepository::steps(&*db, &runs[0].id).await.unwrap();
        assert_eq!(steps.len(), 2);
    }
}
