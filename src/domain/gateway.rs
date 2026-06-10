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
