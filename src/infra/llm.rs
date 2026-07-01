use std::sync::Arc;

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
    agent::system_prompt::{render_pinned_memory_block, render_recalled_memory_block},
    config::{ModelConfig, Provider},
    domain::{
        llm::{LlmClient, Step, ToolCallReq, ToolOutcome, TurnDriver},
        memory::{MemoryContext, MemoryRepository},
        message::{Message, Role},
        session::Session,
        tool::Tool,
    },
    infra::{
        codex::{CODEX_BASE_URL, CodexAuth, CodexHttpClient, codex_static_headers},
        rig_tool::RigTool,
    },
};

/// Produces the system prompt (preamble) on demand. Called once per user turn
/// so the prompt is rebuilt per session rather than baked once at startup —
/// the gateway is a long-lived process, so a baked prompt would freeze the
/// volatile tier (date) at boot. The factory's output is day-precision, so it
/// stays byte-identical across turns within a day (upstream prompt cache stays
/// warm) and self-heals across midnight.
pub type PreambleFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Max facts pulled per turn by L3 active recall. Small on purpose: recall is
/// background context, top-ranked relevance only. See `docs/personal-agent-roadmap.md`.
const RECALL_LIMIT: usize = 5;

/// Generic [`LlmClient`] over any `rig` completion model. The concrete provider
/// type is erased behind `Arc<dyn LlmClient>` by [`build_llm`].
pub struct RigLlm<M: CompletionModel> {
    agent: Agent<M>,
    /// Maximum tool-calling round-trips per user turn before the agent must
    /// answer (config `max_turns`, env `SHION_MAX_TURNS`).
    max_turns: usize,
    /// Rebuilds the system prompt each turn (see [`PreambleFn`]).
    preamble: PreambleFn,
    /// Max prior messages replayed as history per turn (config
    /// `max_history_messages`; `0` = unlimited). The backstop against a
    /// long-lived chat session sending its entire transcript every turn — see
    /// [`RigLlm::assemble`].
    max_history_messages: usize,
    /// Optional long-term memory store. When set (the main agent), each turn's
    /// L1 pinned profile is injected after the preamble. `None` for aux/delegate
    /// sub-agents, which must not be fed the user's memory library.
    memories: Option<Arc<dyn MemoryRepository>>,
    /// Drive each round over the streaming API instead of one-shot `send()`.
    /// Required by the ChatGPT Codex backend (it rejects non-streamed requests);
    /// `false` for every other provider, which keeps the simpler non-streaming
    /// path. The streamed chunks are aggregated back into one assistant turn, so
    /// the rest of the loop is identical either way.
    stream: bool,
}

impl<M> RigLlm<M>
where
    M: CompletionModel + 'static,
{
    /// Assemble this turn's `(preamble, prompt, history)`: split the session
    /// into the latest user prompt + prior history, rebuild the system prompt,
    /// and inject L1 pinned + L3 recalled memories (main agent only). Run once
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

        // L1 pinned-memory injection: appended after the volatile tier so the
        // stable+context+volatile prefix stays byte-stable (pinned changes only
        // when the pinned set changes, far less than per turn). Failure is
        // non-fatal — memory is background context, it must not fail a reply —
        // but it is logged, or "why doesn't it know me today" is unanswerable.
        if let Some(memories) = &self.memories {
            let ctx = MemoryContext::from_session(&session.id);

            // L1 pinned profile. Capture the ids so the same memory is not also
            // echoed by L3 recall below (a pinned memory is active + in-scope, so
            // it would otherwise surface twice).
            let mut pinned_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            match memories.pinned(&ctx).await {
                Ok(pinned) => {
                    pinned_ids = pinned.iter().map(|m| m.id.clone()).collect();
                    if let Some(block) = render_pinned_memory_block(&pinned) {
                        preamble.push_str("\n\n");
                        preamble.push_str(&block);
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to load pinned memories"),
            }

            // L3 active recall: facts relevant to this turn's message, appended
            // after pinned so the volatile|pinned|recall order holds (pinned is
            // cross-turn stable, recall is per-query cold — stable goes first to
            // keep the prefix cacheable). Same non-fatal-but-logged contract.
            match memories.recall(&ctx, &prompt, RECALL_LIMIT).await {
                Ok(mut hits) => {
                    hits.retain(|h| !pinned_ids.contains(&h.memory.id));
                    if let Some(block) = render_recalled_memory_block(&hits) {
                        preamble.push_str("\n\n");
                        preamble.push_str(&block);
                    }
                    // Record the recall usage signal off the reply path: it only
                    // touches `last_used_at`, so it must not add latency or fail
                    // the answer. Spawned best-effort, warn on error.
                    let ids: Vec<String> = hits.iter().map(|h| h.memory.id.clone()).collect();
                    if !ids.is_empty() {
                        let repo = memories.clone();
                        tokio::spawn(async move {
                            let now = time::OffsetDateTime::now_utc().unix_timestamp();
                            if let Err(error) = repo.mark_used(&ids, now).await {
                                tracing::warn!(%error, "failed to record recall usage");
                            }
                        });
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to recall memories"),
            }
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
            let (choice, _) = stream_completion(&agent, RigMessage::user(prompt), history).await?;
            return Ok(choice_text(&choice));
        }
        let reply = agent
            .prompt(prompt)
            .with_history(history)
            .max_turns(self.max_turns)
            .await
            .context("LLM completion failed")?;
        Ok(reply)
    }

    async fn begin_turn(&self, session: &Session) -> anyhow::Result<Box<dyn TurnDriver>> {
        let (preamble, prompt, history) = self.assemble(session).await?;
        Ok(Box::new(RigTurnDriver {
            agent: self.agent_with_preamble(preamble),
            history,
            pending: Some(RigMessage::user(prompt)),
            stream: self.stream,
        }))
    }
}

/// A [`TurnDriver`] backed by a per-turn rig [`Agent`] clone. Holds the growing
/// conversation history (excluding the not-yet-sent prompt) so each round is a
/// single `agent.completion(...).send()` — rig does one completion, shion owns
/// the loop.
struct RigTurnDriver<M: CompletionModel> {
    agent: Agent<M>,
    history: Vec<RigMessage>,
    /// The opening prompt; consumed by `first()`, then `None`.
    pending: Option<RigMessage>,
    /// Stream each round instead of one-shot `send()` (see [`RigLlm::stream`]).
    stream: bool,
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
            stream_completion(&self.agent, prompt.clone(), self.history.clone()).await?
        } else {
            let resp = self
                .agent
                .completion(prompt.clone(), self.history.clone())
                .await
                .context("failed to build completion request")?
                .send()
                .await
                .context("LLM completion failed")?;
            (resp.choice, resp.message_id)
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

/// Split a model's assistant turn into shion's [`Step`]: any tool call makes it
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
/// provider model type is erased. `memories` is the optional long-term store
/// for L1 pinned injection — `Some` for the main agent, `None` for aux/delegate
/// sub-agents.
pub fn build_llm(
    config: &ModelConfig,
    tools: Vec<Arc<dyn Tool>>,
    preamble: PreambleFn,
    memories: Option<Arc<dyn MemoryRepository>>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    let adapters: Vec<Box<dyn ToolDyn>> = tools
        .into_iter()
        .map(|t| Box::new(RigTool(t)) as Box<dyn ToolDyn>)
        .collect();
    let model = config.model.clone();
    let key = config.api_key.clone();
    let base = config.base_url.as_deref();
    let max_turns = config.max_turns;
    let max_history_messages = config.max_history_messages;
    // The ChatGPT Codex backend only accepts streamed requests; everyone else
    // uses the simpler one-shot path. Declared before `rig_llm!` so the macro's
    // (hygienic) body can capture it alongside `max_turns`/`preamble`/`memories`.
    let stream = matches!(config.provider, Provider::Codex);
    // Seed the agent with an initial preamble; `complete` overrides it per turn.
    let initial = preamble();

    // Each provider's client/agent type differs (erased to `Arc<dyn LlmClient>`
    // at the end), so the four arms can't share a value — but the agent-build
    // and `RigLlm` wrapping are identical. This macro factors that tail out;
    // only one arm runs, so moving `adapters`/`preamble`/`memories` per arm is
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
                memories,
                stream,
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
            let auth = CodexAuth::load().context("loading Codex credentials")?;
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

/// Map a shion message into a rig chat-history message. The system prompt is
/// supplied via the preamble, and tool outputs are folded into the following
/// assistant reply, so both `System` and `Tool` roles are skipped here.
fn to_rig_message(msg: &Message) -> Option<RigMessage> {
    match msg.role {
        Role::User => Some(RigMessage::user(msg.content.clone())),
        Role::Assistant => Some(RigMessage::assistant(msg.content.clone())),
        Role::System | Role::Tool => None,
    }
}
