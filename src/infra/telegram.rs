//! Telegram ingress channel.
//!
//! Receives messages via long polling (`getUpdates`) — like the feishu
//! WebSocket connection, no public callback URL is needed, which is right
//! for a laptop process. Each text message routes through the
//! `MessageHandler` as one session turn; replies go out via `sendMessage`.
//!
//! Everything is plain reqwest against the Bot API — no SDK dependency.
//!
//! Access control mirrors the feishu adapter: an `allow_from` user-id
//! allowlist (empty = open), a `require_mention` gate for group chats
//! (DMs always bypass), and an optional `home_chat` that receives
//! proactive output (reminders) via `TelegramNotifier`.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::{
    agent::{
        gateway::Channel,
        pairing::{Gate, PairingGuard},
    },
    config::TelegramConfig,
    domain::{gateway::MessageHandler, notify::Notifier, pairing::PairingRepository},
};

const TELEGRAM_BASE_URL: &str = "https://api.telegram.org";
/// Long-poll wait passed to `getUpdates`.
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Telegram rejects messages over 4096 UTF-16 code units; longer replies
/// are split into consecutive messages.
const MAX_MESSAGE_UTF16: usize = 4096;

/// Outbound side of the integration. Shared by the ingress channel (replies)
/// and `TelegramNotifier` (proactive messages to the home chat).
pub struct TelegramSender {
    bot_token: String,
    http: reqwest::Client,
}

/// Bot API envelope: `result` is present on `ok`, `description` on failure.
#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    #[serde(default)]
    description: String,
    result: Option<T>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    text: Option<String>,
    from: Option<User>,
    chat: Chat,
}

#[derive(Deserialize)]
struct User {
    id: i64,
    #[serde(default)]
    is_bot: bool,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

impl TelegramSender {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("{TELEGRAM_BASE_URL}/bot{}/{method}", self.bot_token)
    }

    /// Send a plain text message into a chat (works for both private and
    /// group chats). Over-long texts are split at the API limit.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        for chunk in chunk_text(text, MAX_MESSAGE_UTF16) {
            let response: ApiResponse<serde_json::Value> = self
                .http
                .post(self.url("sendMessage"))
                .json(&serde_json::json!({ "chat_id": chat_id, "text": chunk }))
                .send()
                .await?
                .json()
                .await?;
            if !response.ok {
                anyhow::bail!("telegram send failed: {}", response.description);
            }
        }
        Ok(())
    }

    /// The bot's own username, needed for the group @mention gate.
    async fn bot_username(&self) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Me {
            username: Option<String>,
        }

        let response: ApiResponse<Me> = self
            .http
            .get(self.url("getMe"))
            .send()
            .await?
            .json()
            .await?;
        if !response.ok {
            anyhow::bail!("telegram getMe failed: {}", response.description);
        }
        response
            .result
            .and_then(|me| me.username)
            .ok_or_else(|| anyhow::anyhow!("telegram bot has no username"))
    }

    /// One long-poll round. `offset` acknowledges everything below it.
    async fn get_updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let response: ApiResponse<Vec<Update>> = self
            .http
            .post(self.url("getUpdates"))
            // Outlive the server-side long-poll wait.
            .timeout(POLL_TIMEOUT + Duration::from_secs(10))
            .json(&serde_json::json!({
                "offset": offset,
                "timeout": POLL_TIMEOUT.as_secs(),
                "allowed_updates": ["message"],
            }))
            .send()
            .await?
            .json()
            .await?;
        if !response.ok {
            anyhow::bail!("telegram getUpdates failed: {}", response.description);
        }
        Ok(response.result.unwrap_or_default())
    }
}

/// Delivers proactive output (reminders) to the configured home chat.
pub struct TelegramNotifier {
    sender: Arc<TelegramSender>,
    chat_id: String,
}

impl TelegramNotifier {
    pub fn new(sender: Arc<TelegramSender>, chat_id: String) -> Self {
        Self { sender, chat_id }
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let text = if body.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n{body}")
        };
        self.sender.send_text(&self.chat_id, &text).await
    }
}

/// Which inbound messages the agent handles. Sender identity (allowlist /
/// pairing) is the `PairingGuard`'s job, not this struct's.
#[derive(Clone, Default)]
struct AdmitPolicy {
    /// Group messages must @mention the bot (DMs always pass).
    require_mention: bool,
}

pub struct TelegramChannel {
    sender: Arc<TelegramSender>,
    policy: AdmitPolicy,
    guard: PairingGuard,
}

/// One inbound text message, reduced to what the agent needs.
struct Inbound {
    sender_id: String,
    chat_id: String,
    text: String,
}

impl TelegramChannel {
    pub fn new(
        sender: Arc<TelegramSender>,
        config: &TelegramConfig,
        pairings: Arc<dyn PairingRepository>,
    ) -> Self {
        Self {
            sender,
            policy: AdmitPolicy {
                require_mention: config.require_mention,
            },
            guard: PairingGuard::new("telegram", config.allow_from.clone(), pairings),
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn serve(
        &self,
        handler: Arc<dyn MessageHandler>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        // The group mention gate needs the bot's username; keep retrying so a
        // gateway started offline comes up once the network does.
        let username = loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                result = self.sender.bot_username() => match result {
                    Ok(name) => break name,
                    Err(error) => {
                        warn!(%error, "telegram getMe failed; retrying");
                        tokio::select! {
                            _ = shutdown.changed() => return Ok(()),
                            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
                        }
                    }
                }
            }
        };
        info!(bot = %username, "telegram channel connected");

        // Messages are handled one at a time, in arrival order; the next poll
        // (offset past everything received) acknowledges the batch. The chat
        // id keys the session, so a private chat is one continuous
        // conversation.
        let mut offset = 0i64;
        loop {
            let updates = tokio::select! {
                _ = shutdown.changed() => break,
                result = self.sender.get_updates(offset) => match result {
                    Ok(updates) => updates,
                    Err(error) => {
                        warn!(%error, "telegram polling failed; retrying");
                        tokio::select! {
                            _ = shutdown.changed() => break,
                            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
                        }
                        continue;
                    }
                }
            };
            for update in updates {
                offset = offset.max(update.update_id + 1);
                let Some(message) = update.message else {
                    continue;
                };
                let Some(msg) = admit(message, &self.policy, &username) else {
                    continue;
                };
                // Pairing gate: unknown senders get a pairing code instead of
                // the agent until `shion pair approve` runs on the host.
                match self.guard.check(&msg.sender_id, &msg.chat_id).await {
                    Ok(Gate::Allowed) => {}
                    Ok(Gate::Denied { reply }) => {
                        info!(sender = %msg.sender_id, "telegram sender unpaired; sent pairing code");
                        if let Err(error) = self.sender.send_text(&msg.chat_id, &reply).await {
                            error!(%error, chat = %msg.chat_id, "failed to send pairing prompt");
                        }
                        continue;
                    }
                    Err(error) => {
                        warn!(%error, "pairing check failed; dropping message");
                        continue;
                    }
                }
                let session_id = format!("telegram:{}", msg.chat_id);
                info!(chat = %msg.chat_id, "telegram message received");
                let reply = match handler.handle(&session_id, msg.text).await {
                    Ok(reply) => reply,
                    Err(error) => {
                        warn!(%error, "telegram message handling failed");
                        format!("处理消息时出错了: {error}")
                    }
                };
                if let Err(error) = self.sender.send_text(&msg.chat_id, &reply).await {
                    error!(%error, chat = %msg.chat_id, "failed to send telegram reply");
                }
            }
        }
        info!("telegram channel stopped");
        Ok(())
    }
}

/// Reduce an update's message to an `Inbound`, or `None` when the agent
/// should ignore it (policy rejection, non-text, bot sender, empty after
/// mention strip).
fn admit(message: Message, policy: &AdmitPolicy, bot_username: &str) -> Option<Inbound> {
    let from = message.from?;
    if from.is_bot {
        return None;
    }
    let text = message.text?;
    let mention = format!("@{bot_username}");
    let is_group = matches!(message.chat.kind.as_str(), "group" | "supergroup");
    if is_group && policy.require_mention && !text.contains(&mention) {
        return None;
    }
    let text = text
        .replace(&mention, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    Some(Inbound {
        sender_id: from.id.to_string(),
        chat_id: message.chat.id.to_string(),
        text,
    })
}

/// Split `text` into pieces of at most `max` UTF-16 code units (the unit
/// Telegram's length limit counts), never breaking a `char`.
fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    for ch in text.chars() {
        let len = ch.len_utf16();
        if units + len > max && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str, chat_kind: &str, user_id: i64) -> Message {
        Message {
            text: Some(text.to_string()),
            from: Some(User {
                id: user_id,
                is_bot: false,
            }),
            chat: Chat {
                id: 100,
                kind: chat_kind.to_string(),
            },
        }
    }

    #[test]
    fn admit_passes_private_text_message_with_sender_id() {
        let policy = AdmitPolicy {
            require_mention: true,
        };
        let inbound = admit(message("hello", "private", 1), &policy, "shion_bot").unwrap();
        assert_eq!(inbound.chat_id, "100");
        assert_eq!(inbound.sender_id, "1");
        assert_eq!(inbound.text, "hello");
    }

    #[test]
    fn admit_requires_mention_in_groups_and_strips_it() {
        let policy = AdmitPolicy {
            require_mention: true,
        };
        assert!(admit(message("hello", "supergroup", 1), &policy, "shion_bot").is_none());
        let inbound = admit(
            message("@shion_bot what time is it", "supergroup", 1),
            &policy,
            "shion_bot",
        )
        .unwrap();
        assert_eq!(inbound.text, "what time is it");
    }

    #[test]
    fn admit_rejects_bot_senders() {
        let mut msg = message("hello", "private", 1);
        msg.from.as_mut().unwrap().is_bot = true;
        assert!(admit(msg, &AdmitPolicy::default(), "shion_bot").is_none());
    }

    #[test]
    fn chunk_text_splits_on_utf16_units_without_breaking_chars() {
        let chunks = chunk_text(&"啊".repeat(5), 2);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "啊啊");
        assert_eq!(chunks[2], "啊");

        // '🚀' is 2 UTF-16 units; three of them at max 4 → 2 + 1.
        let chunks = chunk_text(&"🚀".repeat(3), 4);
        assert_eq!(chunks, vec!["🚀🚀", "🚀"]);

        assert!(chunk_text("", 10).is_empty());
    }
}
