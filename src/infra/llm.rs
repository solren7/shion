use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use rig::{
    agent::Agent,
    client::{ClientBuilder, CompletionClient},
    completion::{CompletionModel, Message as RigMessage, Prompt},
    providers::{anthropic, deepseek, openai, openrouter},
    tool::ToolDyn,
};

use crate::{
    config::{ModelConfig, Provider},
    domain::{
        llm::LlmClient,
        message::{Message, Role},
        session::Session,
        tool::Tool,
    },
    infra::rig_tool::RigTool,
};

const PREAMBLE: &str = "You are Shion, a concise and helpful personal agent. \
    When a request needs live information or an action, call one of your tools \
    (for example, use the `time` tool to get the current date and time). \
    Questions about your own state — your sessions, conversation history, \
    memories, or skills — refer to Shion's database, not the operating system: \
    answer them with the `session`, `memory`, or `skill` tools, never with \
    shell commands like `tmux ls` or `who`. \
    You CAN schedule reminders: call the `reminder` tool (action=create) with a \
    message and a delay. Reminders are delivered as desktop notifications by the \
    `shion gateway` background process — you do NOT count down yourself, and you \
    must never pretend to track time in the conversation. If the user asks for a \
    reminder, create it with the tool and relay the tool's confirmation. \
    For recurring reminders (\"every day at 9am\"), pass a 5-field cron expression \
    via the `cron` parameter (e.g. \"0 9 * * *\"); times are the user's local \
    timezone. One-shot reminders use `after` or `at` as before.";

/// Maximum tool-calling round-trips per user turn before the agent must answer.
const MAX_TURNS: usize = 5;

/// Generic [`LlmClient`] over any `rig` completion model. The concrete provider
/// type is erased behind `Arc<dyn LlmClient>` by [`build_llm`].
pub struct RigLlm<M: CompletionModel> {
    agent: Agent<M>,
}

#[async_trait]
impl<M> LlmClient for RigLlm<M>
where
    M: CompletionModel + 'static,
{
    async fn complete(&self, session: &Session) -> anyhow::Result<String> {
        // The current prompt is the most recent user message; everything before
        // it forms the conversation history sent to the model.
        let last_user_idx = session
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .context("no user message to respond to")?;
        let prompt = session.messages[last_user_idx].content.clone();
        let history: Vec<RigMessage> = session.messages[..last_user_idx]
            .iter()
            .filter_map(to_rig_message)
            .collect();

        let reply = self
            .agent
            .prompt(prompt)
            .with_history(history)
            .max_turns(MAX_TURNS)
            .await
            .context("LLM completion failed")?;
        Ok(reply)
    }
}

/// Build an LLM client for the configured provider, exposing `tools` via
/// function calling and appending `skills_note` (the skills catalog) to the
/// system preamble. The concrete provider model type is erased.
pub fn build_llm(
    config: &ModelConfig,
    tools: Vec<Arc<dyn Tool>>,
    skills_note: Option<String>,
) -> anyhow::Result<Arc<dyn LlmClient>> {
    let adapters: Vec<Box<dyn ToolDyn>> = tools
        .into_iter()
        .map(|t| Box::new(RigTool(t)) as Box<dyn ToolDyn>)
        .collect();
    let preamble = match skills_note {
        Some(note) => format!("{PREAMBLE}\n\n{note}"),
        None => PREAMBLE.to_string(),
    };
    let model = config.model.clone();
    let key = config.api_key.clone();
    let base = config.base_url.as_deref();

    let llm: Arc<dyn LlmClient> = match config.provider {
        Provider::DeepSeek => {
            let client = with_base_url(deepseek::Client::builder().api_key(key), base)
                .build()
                .context("failed to build DeepSeek client")?;
            let agent = client
                .agent(model)
                .preamble(&preamble)
                .tools(adapters)
                .build();
            Arc::new(RigLlm { agent }) as Arc<dyn LlmClient>
        }
        Provider::OpenAi => {
            let client = with_base_url(openai::Client::builder().api_key(key), base)
                .build()
                .context("failed to build OpenAI client")?;
            let agent = client
                .agent(model)
                .preamble(&preamble)
                .tools(adapters)
                .build();
            Arc::new(RigLlm { agent }) as Arc<dyn LlmClient>
        }
        Provider::Anthropic => {
            let client = with_base_url(anthropic::Client::builder().api_key(key), base)
                .build()
                .context("failed to build Anthropic client")?;
            let agent = client
                .agent(model)
                .preamble(&preamble)
                .tools(adapters)
                .build();
            Arc::new(RigLlm { agent }) as Arc<dyn LlmClient>
        }
        Provider::OpenRouter => {
            let client = with_base_url(openrouter::Client::builder().api_key(key), base)
                .build()
                .context("failed to build OpenRouter client")?;
            let agent = client
                .agent(model)
                .preamble(&preamble)
                .tools(adapters)
                .build();
            Arc::new(RigLlm { agent }) as Arc<dyn LlmClient>
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
