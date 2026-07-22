use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use rig::{
    OneOrMany,
    agent::Agent,
    client::{ClientBuilder, CompletionClient},
    completion::{
        AssistantContent, Completion, CompletionModel, Message as RigMessage, Prompt,
        message::{ToolResultContent, UserContent},
    },
    providers::{anthropic, deepseek, openai, openrouter},
    tool::ToolDyn,
};

use crate::{
    config::{ModelConfig, Provider},
    domain::{
        llm::{LlmClient, Step, ToolCallReq, ToolOutcome, TurnDriver},
        message::{Message, Role},
        session::Session,
    },
    infra::{
        codex::{CODEX_BASE_URL, CodexAuth, CodexHttpClient, codex_static_headers},
        rig_tool::RigTool,
    },
    services::memory_enrichment::MemoryEnricher,
};

/// Produces the system prompt (preamble) on demand. Called once per user turn
/// so the prompt is rebuilt per session rather than baked once at startup —
/// the gateway is a long-lived process, so a baked prompt would freeze the
/// volatile tier (date) at boot. The factory's output is day-precision, so it
/// stays byte-identical across turns within a day (upstream prompt cache stays
/// warm) and self-heals across midnight.
pub type PreambleFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Stand-in for a provider whose API key is missing (see [`build_llm`]):
/// construction always succeeds so a fresh install boots, and every call —
/// `begin_turn` inherits the default one-shot driver over `complete` — fails
/// with the fix. The error text reaches the user as the turn's reply.
struct UnconfiguredLlm {
    message: String,
}

#[async_trait]
impl LlmClient for UnconfiguredLlm {
    async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
        anyhow::bail!("{}", self.message)
    }
}

/// Generic [`LlmClient`] over any `rig` completion model. The concrete provider
/// type is erased behind `Arc<dyn LlmClient>` by [`build_llm`].
pub struct RigLlm<M: CompletionModel> {
    agent: Agent<M>,
    /// Maximum tool-calling round-trips per user turn before the agent must
    /// answer (config `max_turns`, env `KOMO_MAX_TURNS`).
    max_turns: usize,
    /// Rebuilds the system prompt each turn (see [`PreambleFn`]).
    preamble: PreambleFn,
    /// Max prior messages replayed as history per turn (config
    /// `max_history_messages`; `0` = unlimited). The backstop against a
    /// long-lived chat session sending its entire transcript every turn — see
    /// [`RigLlm::assemble`].
    max_history_messages: usize,
    /// Optional per-turn memory enrichment. `Some` only for the main agent —
    /// aux/delegate sub-agents must not be fed the user's memory library. The
    /// enricher owns the whole memory policy (selection, screening, rendering,
    /// usage tracking); this adapter only appends the finished prefix.
    enricher: Option<Arc<MemoryEnricher>>,
    /// Drive each round over the streaming API instead of one-shot `send()`.
    /// Required by the ChatGPT Codex backend (it rejects non-streamed requests);
    /// `false` for every other provider, which keeps the simpler non-streaming
    /// path. The streamed chunks are aggregated back into one assistant turn, so
    /// the rest of the loop is identical either way.
    stream: bool,
    /// Per-completion timeout. rig's default reqwest client sets no request
    /// timeout, so a hung provider request would await forever and wedge the
    /// turn in `running`; this caps each completion so a stall fails the turn
    /// cleanly instead. `None` = no timeout (config `llm_timeout_secs = 0`).
    timeout: Option<Duration>,
}

/// Run `fut` under `timeout` (if set), turning a stall into a clean error rather
/// than an indefinite await. Shared by the tool-less `complete` path and every
/// tool-loop round.
async fn with_timeout<F, T>(timeout: Option<Duration>, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "LLM completion timed out after {}s (provider unresponsive; \
                 failing the turn instead of leaving it running — raise \
                 `llm_timeout_secs` / `KOMO_LLM_TIMEOUT_SECS` if this is too tight)",
                d.as_secs()
            ),
        },
        None => fut.await,
    }
}

impl<M> RigLlm<M>
where
    M: CompletionModel + 'static,
{
    /// Assemble this turn's `(preamble, prompt, history)`: split the session
    /// into the latest user prompt + prior history, rebuild the system prompt,
    /// and append the memory-enrichment prefix (main agent only). Run once
    /// per turn — never per tool-loop round (recall is keyed on the user
    /// message, and re-running it each round would churn the cached prefix).
    async fn assemble(
        &self,
        session: &Session,
    ) -> anyhow::Result<(String, String, Vec<RigMessage>)> {
        // The current prompt is the most recent user message; everything before
        // it forms the conversation history sent to the model.
        let last_user_idx = session
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .context("no user message to respond to")?;
        let prompt = session.messages[last_user_idx].content.clone();

        // Window the replayed history to the most recent `max_history_messages`
        // (0 = keep everything). Without this a long-lived chat session
        // (telegram/feishu/wechat are keyed by chat id and only rotate on an
        // explicit `/new`) would resend its entire transcript every turn —
        // unbounded token cost and latency, eventually overflowing the context
        // window. The stable system-prompt + memory prefix is untouched, so the
        // upstream prompt cache is unaffected by trimming the tail.
        let prior = &session.messages[..last_user_idx];
        let mut window: &[Message] = match self.max_history_messages {
            0 => prior,
            n => &prior[prior.len().saturating_sub(n)..],
        };
        // The transcript strictly alternates user/assistant, so a tail cut can
        // open on an assistant message; drop it so the history starts on a user
        // turn (Anthropic rejects a leading assistant message).
        if window.first().is_some_and(|m| m.role == Role::Assistant) {
            window = &window[1..];
        }
        let history: Vec<RigMessage> = window.iter().filter_map(to_rig_message).collect();

        // Rebuild the system prompt for this turn. `Agent` is cheap to clone
        // (`Arc<model>` + an `Arc`-backed tool handle), and its `preamble` field
        // is public, so we clone-and-override rather than mutate shared state —
        // keeping concurrent sessions in the gateway independent.
        let mut preamble = (self.preamble)();

        // Memory injection (main agent only): the enricher returns the finished
        // pinned+recall prefix, appended after the volatile tier so the
        // stable+context+volatile bytes stay cache-stable. Enrichment failure
        // is absorbed inside the enricher (memory is background context — it
        // must never fail a reply).
        if let Some(enricher) = &self.enricher
            && let Some(prefix) = enricher.enrich(&session.id, &prompt).await
        {
            preamble.push_str("\n\n");
            preamble.push_str(prefix.as_str());
        }

        Ok((preamble, prompt, history))
    }

    /// Clone the agent with this turn's assembled preamble installed.
    fn agent_with_preamble(&self, preamble: String) -> Agent<M> {
        let mut agent = self.agent.clone();
        agent.preamble = Some(preamble);
        agent
    }
}

#[async_trait]
impl<M> LlmClient for RigLlm<M>
where
    M: CompletionModel + 'static,
{
    async fn complete(&self, session: &Session) -> anyhow::Result<String> {
        // Tool-less callers (aux/delegate/reviewer/briefing): rig's own loop does
        // a single completion and returns, since no tools are exposed.
        let (preamble, prompt, history) = self.assemble(session).await?;
        let agent = self.agent_with_preamble(preamble);
        if self.stream {
            // Codex: one streamed completion, aggregated to its text. (No tools
            // are exposed here, so a single round is the whole answer.)
            let (choice, _) = with_timeout(
                self.timeout,
                stream_completion(&agent, RigMessage::user(prompt), history),
            )
            .await?;
            return Ok(choice_text(&choice));
        }
        let reply = with_timeout(self.timeout, async {
            agent
                .prompt(prompt)
                .history(history)
                .max_turns(self.max_turns)
                .await
                .context("LLM completion failed")
        })
        .await?;
        Ok(reply)
    }

    async fn begin_turn(&self, session: &Session) -> anyhow::Result<Box<dyn TurnDriver>> {
        let (preamble, prompt, history) = self.assemble(session).await?;
        Ok(Box::new(RigTurnDriver {
            agent: self.agent_with_preamble(preamble),
            history,
            pending: Some(RigMessage::user(prompt)),
            stream: self.stream,
            timeout: self.timeout,
        }))
    }
}

/// A [`TurnDriver`] backed by a per-turn rig [`Agent`] clone. Holds the growing
/// conversation history (excluding the not-yet-sent prompt) so each round is a
/// single `agent.completion(...).send()` — rig does one completion, komo owns
/// the loop.
struct RigTurnDriver<M: CompletionModel> {
    agent: Agent<M>,
    history: Vec<RigMessage>,
    /// The opening prompt; consumed by `first()`, then `None`.
    pending: Option<RigMessage>,
    /// Stream each round instead of one-shot `send()` (see [`RigLlm::stream`]).
    stream: bool,
    /// Per-round completion timeout (see [`RigLlm::timeout`]).
    timeout: Option<Duration>,
}

impl<M> RigTurnDriver<M>
where
    M: CompletionModel + 'static,
{
    /// Send one round-trip: complete over `history + prompt`, then commit both
    /// the prompt and the assistant turn (verbatim — text + tool calls +
    /// reasoning together) to history so the next round sees a provider-correct
    /// transcript.
    async fn run(&mut self, prompt: RigMessage) -> anyhow::Result<Step> {
        let (choice, message_id) = if self.stream {
            with_timeout(
                self.timeout,
                stream_completion(&self.agent, prompt.clone(), self.history.clone()),
            )
            .await?
        } else {
            with_timeout(self.timeout, async {
                let resp = self
                    .agent
                    .completion(prompt.clone(), self.history.clone())
                    .await
                    .context("failed to build completion request")?
                    .send()
                    .await
                    .context("LLM completion failed")?;
                Ok::<_, anyhow::Error>((resp.choice, resp.message_id))
            })
            .await?
        };
        self.history.push(prompt);
        self.history.push(RigMessage::Assistant {
            id: message_id,
            content: choice.clone(),
        });
        Ok(choice_to_step(&choice))
    }
}

#[async_trait]
impl<M> TurnDriver for RigTurnDriver<M>
where
    M: CompletionModel + 'static,
{
    async fn first(&mut self) -> anyhow::Result<Step> {
        let prompt = self.pending.take().context("turn driver already started")?;
        self.run(prompt).await
    }

    async fn step(&mut self, results: Vec<ToolOutcome>) -> anyhow::Result<Step> {
        // One user message carrying every tool result, mirroring rig's own
        // `tool_result_user_content`: key by `call_id` when present (OpenAI),
        // else `id` (Anthropic).
        let contents: Vec<UserContent> = results
            .into_iter()
            .map(|r| {
                let content = ToolResultContent::from_tool_output(r.content);
                match r.call_id {
                    Some(call_id) => UserContent::tool_result_with_call_id(r.id, call_id, content),
                    None => UserContent::tool_result(r.id, content),
                }
            })
            .collect();
        let content = OneOrMany::many(contents)
            .map_err(|_| anyhow::anyhow!("no tool results to send back"))?;
        self.run(RigMessage::User { content }).await
    }
}

/// Run one streamed completion to exhaustion and return the aggregated assistant
/// turn — `(choice, message_id)`, the same pair the non-streaming `send()` yields.
/// rig accumulates the streamed deltas into `choice`/`message_id` as the inner
/// stream drains, so we consume every chunk (surfacing any provider error) and
/// then read the final aggregate. Used for backends that require streaming
/// (Codex); identical downstream handling to the one-shot path.
async fn stream_completion<M>(
    agent: &Agent<M>,
    prompt: RigMessage,
    history: Vec<RigMessage>,
) -> anyhow::Result<(OneOrMany<AssistantContent>, Option<String>)>
where
    M: CompletionModel + 'static,
{
    let mut stream = agent
        .completion(prompt, history)
        .await
        .context("failed to build completion request")?
        .stream()
        .await
        .context("LLM completion failed")?;
    while let Some(item) = stream.next().await {
        item.context("LLM completion failed")?;
    }
    Ok((stream.choice.clone(), stream.message_id.clone()))
}

/// Concatenate the text blocks of an assistant turn (ignoring tool calls /
/// reasoning) — the final answer for a tool-less completion.
fn choice_text(choice: &OneOrMany<AssistantContent>) -> String {
    let mut text = String::new();
    for content in choice.iter() {
        if let AssistantContent::Text(t) = content {
            text.push_str(&t.text);
        }
    }
    text
}

/// Split a model's assistant turn into komo's [`Step`]: any tool call makes it
/// a [`Step::ToolCalls`]; otherwise the concatenated text is the final answer.
/// Reasoning/image blocks are ignored for control flow (the driver still echoes
/// them back into history verbatim).
fn choice_to_step(choice: &OneOrMany<AssistantContent>) -> Step {
    let mut calls = Vec::new();
    let mut text = String::new();
    for content in choice.iter() {
        match content {
            AssistantContent::ToolCall(tc) => calls.push(ToolCallReq {
                id: tc.id.clone(),
                call_id: tc.call_id.clone(),
                name: tc.function.name.clone(),
                args: tc.function.arguments.to_string(),
            }),
            AssistantContent::Text(t) => text.push_str(&t.text),
            _ => {}
        }
    }
    if calls.is_empty() {
        Step::Final(text)
    } else {
        Step::ToolCalls(calls)
    }
}

/// Build an LLM client for the configured provider, exposing `tools` via
/// function calling. `preamble` is a factory (see [`PreambleFn`]) invoked once
/// per turn to (re)assemble the system prompt — typically wrapping a
/// [`crate::agent::system_prompt::SystemPromptBuilder`]. The factory's initial
/// output is baked into the agent; each turn overrides it. The concrete
/// provider model type is erased. `enricher` is the optional per-turn memory
/// enrichment — `Some` only for the main agent, `None` for aux/delegate
/// sub-agents (they must not be fed the user's memory library).
pub fn build_llm(
    config: &ModelConfig,
    tools: Option<&crate::services::tool_execution::ToolExecutor>,
    preamble: PreambleFn,
    enricher: Option<Arc<MemoryEnricher>>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    // A missing API key degrades instead of failing construction: a fresh
    // install (first Docker boot, pre-`komo init`) must still bring the
    // gateway up — channels serve, pairing works — while every LLM call
    // reports the fix. Config resolution records the matching warning.
    if config.provider.uses_api_key() && config.api_key.is_empty() {
        return Ok(Arc::new(UnconfiguredLlm {
            message: format!(
                "{} is not set (required for {:?}). Add it to ~/.komo/.env \
                 (run `komo init` to scaffold one) or the container \
                 environment, then restart the gateway.",
                config.provider.api_key_var(),
                config.provider
            ),
        }));
    }
    // Each RigTool shares the executor's core, so the trait-required fallback
    // path carries the same retry/ledger/cap semantics as the runtime's loop.
    let adapters: Vec<Box<dyn ToolDyn>> = tools
        .map(|executor| {
            let core = executor.core();
            executor
                .definitions()
                .into_iter()
                .map(|t| Box::new(RigTool::new(t, core.clone())) as Box<dyn ToolDyn>)
                .collect()
        })
        .unwrap_or_default();
    let model = config.model.clone();
    let key = config.api_key.clone();
    let base = config.base_url.as_deref();
    let max_turns = config.max_turns;
    let max_history_messages = config.max_history_messages;
    // The ChatGPT Codex backend only accepts streamed requests; everyone else
    // uses the simpler one-shot path. Declared before `rig_llm!` so the macro's
    // (hygienic) body can capture it alongside `max_turns`/`preamble`/`enricher`.
    let stream = matches!(config.provider, Provider::Codex);
    // Cap each completion so a hung provider request fails the turn instead of
    // wedging it in `running` (rig's client sets no request timeout). `0` = off.
    let timeout =
        (config.llm_timeout_secs > 0).then(|| Duration::from_secs(config.llm_timeout_secs));
    // Seed the agent with an initial preamble; `complete` overrides it per turn.
    let initial = preamble();

    // Each provider's client/agent type differs (erased to `Arc<dyn LlmClient>`
    // at the end), so the four arms can't share a value — but the agent-build
    // and `RigLlm` wrapping are identical. This macro factors that tail out;
    // only one arm runs, so moving `adapters`/`preamble`/`enricher` per arm is
    // fine. `client` is the only thing that varies.
    macro_rules! rig_llm {
        ($client:expr) => {{
            let agent = $client
                .agent(model.clone())
                .preamble(&initial)
                .tools(adapters)
                .build();
            Arc::new(RigLlm {
                agent,
                max_turns,
                preamble,
                max_history_messages,
                enricher,
                stream,
                timeout,
            }) as Arc<dyn LlmClient>
        }};
    }

    let llm: Arc<dyn LlmClient> = match config.provider {
        Provider::DeepSeek => {
            let client = with_base_url(deepseek::Client::builder().api_key(key), base)
                .build()
                .context("failed to build DeepSeek client")?;
            rig_llm!(client)
        }
        Provider::OpenAi => {
            let client = with_base_url(openai::Client::builder().api_key(key), base)
                .build()
                .context("failed to build OpenAI client")?;
            rig_llm!(client)
        }
        Provider::Anthropic => {
            let client = with_base_url(anthropic::Client::builder().api_key(key), base)
                .build()
                .context("failed to build Anthropic client")?;
            rig_llm!(client)
        }
        Provider::OpenRouter => {
            let client = with_base_url(openrouter::Client::builder().api_key(key), base)
                .build()
                .context("failed to build OpenRouter client")?;
            rig_llm!(client)
        }
        Provider::Codex => {
            // Codex speaks the OpenAI Responses API (rig's default `openai`
            // client) but at the ChatGPT backend, authenticated with the Codex
            // CLI's OAuth tokens. `CodexHttpClient` re-stamps a fresh bearer on
            // every request; the static Cloudflare-dodging headers are baked in
            // here. `base` (config base_url) overrides the endpoint if set.
            //
            // Missing/broken credentials degrade like a missing API key: the
            // gateway must boot (a fresh box, or a container without
            // ~/.codex mounted) instead of crash-looping, with every LLM call
            // reporting the fix as the turn's reply.
            let auth = match CodexAuth::load() {
                Ok(auth) => auth,
                Err(error) => {
                    tracing::warn!(%error, "Codex credentials unavailable; LLM degraded");
                    return Ok(Arc::new(UnconfiguredLlm {
                        message: format!(
                            "Codex credentials unavailable: {error:#}. Run `codex` to log \
                             in (it writes ~/.codex/auth.json; $CODEX_HOME honored), then \
                             restart the gateway."
                        ),
                    }));
                }
            };
            let client = openai::Client::builder()
                .api_key(auth.initial_access_token())
                .base_url(base.unwrap_or(CODEX_BASE_URL))
                .http_headers(codex_static_headers(auth.account_id()))
                .http_client(CodexHttpClient::new(auth))
                .build()
                .context("failed to build Codex client")?;
            rig_llm!(client)
        }
    };
    Ok(llm)
}

/// Apply an optional base-URL override to any provider's client builder.
fn with_base_url<Ext, A, H>(
    builder: ClientBuilder<Ext, A, H>,
    base_url: Option<&str>,
) -> ClientBuilder<Ext, A, H>
where
    Ext: Clone,
{
    match base_url {
        Some(url) => builder.base_url(url),
        None => builder,
    }
}

/// Map a komo message into a rig chat-history message. The system prompt is
/// supplied via the preamble, and tool outputs are folded into the following
/// assistant reply, so both `System` and `Tool` roles are skipped here.
fn to_rig_message(msg: &Message) -> Option<RigMessage> {
    match msg.role {
        Role::User => Some(RigMessage::user(msg.content.clone())),
        Role::Assistant => Some(RigMessage::assistant(msg.content.clone())),
        Role::System | Role::Tool => None,
    }
}
