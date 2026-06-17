use async_trait::async_trait;

/// Processes one inbound message for a session and returns the agent's reply.
///
/// This is the seam between an ingress channel (which knows a transport — a unix
/// socket, HTTP, a chat platform) and the agent. Channels depend only on this
/// trait; `AgentRuntime` is the production implementation.
#[async_trait]
pub trait MessageHandler: Send + Sync {
    async fn handle(&self, session_id: &str, input: String) -> anyhow::Result<String>;
}

/// Sends a message back into the conversation a turn belongs to, on the same
/// channel it arrived on.
///
/// A turn's eventual reply is returned from [`MessageHandler::handle`], but some
/// work needs to emit prose *mid-turn* — e.g. an approval prompt the user must
/// answer before a tool proceeds. The channel provides a `ReplySink` for the
/// active turn; the agent reaches it through the ambient session context (see
/// `services::tool_registry::current_session`).
#[async_trait]
pub trait ReplySink: Send + Sync {
    async fn send(&self, text: &str) -> anyhow::Result<()>;
}
