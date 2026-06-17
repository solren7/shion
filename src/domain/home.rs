use async_trait::async_trait;

/// The "home" channel: where proactive output (reminders, task due notices, the
/// gateway's shutdown notice) is delivered. Borrowed from hermes-agent's
/// home-channel concept, but settable at runtime via the `/sethome` chat command
/// instead of only through config.
///
/// The value is a session id (`{platform}:{chat_id}`, e.g. `telegram:123456`),
/// so the resolved notifier can pick the matching channel sender. A `/sethome`
/// override here wins over the config `home_chat`.
#[async_trait]
pub trait HomeRepository: Send + Sync {
    /// The current home session id, or `None` when unset.
    async fn get(&self) -> anyhow::Result<Option<String>>;
    /// Set the home to `session_id` (`{platform}:{chat_id}`).
    async fn set(&self, session_id: &str) -> anyhow::Result<()>;
}
