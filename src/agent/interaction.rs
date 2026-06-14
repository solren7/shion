//! Interactive gateway layer: lets a chat-channel turn pause for the user's
//! approval mid-execution, and handles the chat control commands (`/new`,
//! `/approve`, `/deny`).
//!
//! Borrowed from hermes-agent's gateway approval. Hermes runs the agent on a
//! worker thread that blocks on a `threading.Event` keyed by session while the
//! async message loop stays responsive and intercepts `/approve` to signal it.
//! shion's tokio-native equivalent:
//!
//!   - each turn is a **spawned task**, so the channel's receive loop keeps
//!     polling while the turn is in flight (no deadlock);
//!   - when a tool needs approval, [`ChatApprover`] sends the prompt to the
//!     chat and **awaits a `oneshot`** registered in [`ApprovalState`], keyed by
//!     session, with a timeout;
//!   - the loop sees the user's `/approve` / `/deny` reply as an ordinary
//!     inbound message, and [`GatewayDispatcher`] resolves the `oneshot` instead
//!     of starting a new turn.
//!
//! The turn's session context (id + reply sink) reaches the approver through
//! the task-local in `services::tool_registry`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{
    domain::{
        approval::{ApprovalRequest, Approver, Risk},
        gateway::{MessageHandler, ReplySink},
        repository::MessageRepository,
    },
    services::tool_registry::{SessionContext, current_session, with_session},
};

/// How long an approval prompt waits for a reply before auto-denying.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// The user's answer to an approval prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Allow this one action.
    Once,
    /// Allow this action and remember its scope key for the rest of the session.
    Session,
    Deny,
}

/// Shared approval state, keyed by session: the pending prompt's reply channel
/// plus the set of scope keys the user has approved "for this session". Shared
/// between [`ChatApprover`] (registers/awaits) and [`GatewayDispatcher`]
/// (resolves on `/approve`, clears on `/new`).
pub struct ApprovalState {
    pending: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
    approved: Mutex<HashMap<String, HashSet<String>>>,
    timeout: Duration,
}

impl ApprovalState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            approved: Mutex::new(HashMap::new()),
            timeout: APPROVAL_TIMEOUT,
        }
    }

    /// Register a pending approval for `session`, returning the receiver the
    /// approver awaits. Replaces any prior pending approval (its sender drops,
    /// which the old waiter reads as a denial).
    fn register(&self, session: &str) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(session.to_string(), tx);
        rx
    }

    /// Deliver `decision` to the approver waiting on `session`. Returns whether
    /// one was actually waiting (so the dispatcher can tell the user there was
    /// nothing to approve).
    pub fn resolve(&self, session: &str, decision: Decision) -> bool {
        match self.pending.lock().unwrap().remove(session) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Drop any pending approval for `session` without resolving it (the waiter
    /// reads the dropped sender as a denial).
    fn forget_pending(&self, session: &str) {
        self.pending.lock().unwrap().remove(session);
    }

    fn is_session_approved(&self, session: &str, scope_key: &str) -> bool {
        self.approved
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|keys| keys.contains(scope_key))
    }

    fn remember(&self, session: &str, scope_key: &str) {
        self.approved
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .insert(scope_key.to_string());
    }

    /// Forget all approval state for `session` (on `/new`): cancel any pending
    /// wait and drop the session's "allow for this session" set.
    pub fn clear(&self, session: &str) {
        self.forget_pending(session);
        self.approved.lock().unwrap().remove(session);
    }
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Approver for chat channels: routes the approval prompt to the conversation
/// and awaits the user's `/approve` or `/deny` reply.
///
/// Mirrors `CliApprover`'s policy — `Risk::Safe` actions run without prompting;
/// `Normal`/`Dangerous` ask — but over chat instead of a TTY. Without a chat
/// session in context (maintenance sweeps, aux sub-agents) there is no one to
/// ask, so it denies, matching the old `DenyApprover` behavior there.
pub struct ChatApprover {
    state: Arc<ApprovalState>,
}

impl ChatApprover {
    pub fn new(state: Arc<ApprovalState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Approver for ChatApprover {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        if request.risk == Risk::Safe {
            return true;
        }
        let Some(ctx) = current_session() else {
            warn!(summary = %request.summary, "approval auto-denied (no chat session in context)");
            return false;
        };

        // Already approved this kind of action for the session?
        if let Some(key) = &request.scope_key {
            if self.state.is_session_approved(&ctx.session_id, key) {
                return true;
            }
        }

        if let Err(error) = ctx.sink.send(&prompt(request)).await {
            warn!(%error, "failed to send approval prompt; denying");
            return false;
        }

        let rx = self.state.register(&ctx.session_id);
        match tokio::time::timeout(self.state.timeout, rx).await {
            Ok(Ok(Decision::Once)) => true,
            Ok(Ok(Decision::Session)) => {
                if let Some(key) = &request.scope_key {
                    self.state.remember(&ctx.session_id, key);
                }
                true
            }
            // Explicit deny, or the sender was dropped (superseded / cleared).
            Ok(Ok(Decision::Deny)) | Ok(Err(_)) => false,
            Err(_) => {
                self.state.forget_pending(&ctx.session_id);
                let _ = ctx.sink.send("审批超时，已自动拒绝。").await;
                false
            }
        }
    }
}

fn prompt(request: &ApprovalRequest) -> String {
    let mut s = match request.risk {
        Risk::Dangerous => format!("🛑 需要审批（危险操作）：{}", request.summary),
        _ => format!("⚠️ 需要审批：{}", request.summary),
    };
    if let Some(detail) = &request.detail {
        s.push_str(&format!("\n（{detail}）"));
    }
    s.push_str("\n回复 /approve 批准本次 · /approve session 批准本会话内同类操作 · /deny 拒绝");
    s
}

/// A control command parsed from an inbound message, or plain text for the agent.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Start a fresh session (clear context + approval state).
    New,
    /// Resolve a pending approval.
    Approve(Decision),
    Deny,
    /// Ordinary message — run a turn.
    Plain(String),
}

/// Classify an inbound message. Commands are matched case-insensitively on the
/// whole (trimmed) message; anything else is plain text.
pub fn classify(text: &str) -> Command {
    match text.trim().to_lowercase().as_str() {
        "/new" | "/clear" | "/reset" => Command::New,
        "/approve" | "/yes" | "/y" | "/ok" => Command::Approve(Decision::Once),
        "/approve session" | "/approve all" => Command::Approve(Decision::Session),
        "/deny" | "/no" | "/n" => Command::Deny,
        _ => Command::Plain(text.to_string()),
    }
}

/// Front door for inbound chat messages: classifies control commands and,
/// for plain text, runs the agent turn off the channel's receive loop so the
/// loop can still deliver an `/approve` reply while the turn is suspended.
///
/// Channels build a [`ReplySink`] for the conversation and call
/// [`GatewayDispatcher::handle`]; the dispatcher owns replying (including the
/// turn's eventual answer), so channels no longer send agent replies directly.
pub struct GatewayDispatcher {
    handler: Arc<dyn MessageHandler>,
    approvals: Arc<ApprovalState>,
    messages: Arc<dyn MessageRepository>,
    inflight: Mutex<HashSet<String>>,
}

impl GatewayDispatcher {
    pub fn new(
        handler: Arc<dyn MessageHandler>,
        approvals: Arc<ApprovalState>,
        messages: Arc<dyn MessageRepository>,
    ) -> Self {
        Self {
            handler,
            approvals,
            messages,
            inflight: Mutex::new(HashSet::new()),
        }
    }

    /// Handle one inbound message. Returns promptly: a plain message spawns its
    /// turn and returns, so the caller's receive loop is never blocked.
    pub async fn handle(
        self: &Arc<Self>,
        session_id: &str,
        text: String,
        sink: Arc<dyn ReplySink>,
    ) {
        match classify(&text) {
            Command::Approve(decision) => {
                let acked = self.approvals.resolve(session_id, decision);
                let reply = match (acked, decision) {
                    (true, Decision::Session) => "✅ 已批准（本会话内同类操作将自动放行）",
                    (true, _) => "✅ 已批准",
                    (false, _) => "当前没有待审批的操作。",
                };
                let _ = sink.send(reply).await;
            }
            Command::Deny => {
                let reply = if self.approvals.resolve(session_id, Decision::Deny) {
                    "已拒绝。"
                } else {
                    "当前没有待审批的操作。"
                };
                let _ = sink.send(reply).await;
            }
            Command::New => {
                self.approvals.clear(session_id);
                let removed = self.messages.clear_session(session_id).await.unwrap_or(0);
                info!(session = %session_id, removed, "session reset via /new");
                let _ = sink.send("已开始新会话，上下文已清空。").await;
            }
            Command::Plain(input) => self.spawn_turn(session_id, input, sink),
        }
    }

    fn spawn_turn(self: &Arc<Self>, session_id: &str, input: String, sink: Arc<dyn ReplySink>) {
        // One turn at a time per session: a second message that arrives while a
        // turn is in flight is rejected (an `/approve` reply, handled above,
        // never reaches here). This keeps a session's history append-ordered.
        if !self.inflight.lock().unwrap().insert(session_id.to_string()) {
            let sink = sink.clone();
            tokio::spawn(async move {
                let _ = sink.send("正在处理上一条消息，请稍候…").await;
            });
            return;
        }

        let this = self.clone();
        let session = session_id.to_string();
        let ctx = SessionContext {
            session_id: session.clone(),
            sink: sink.clone(),
        };
        tokio::spawn(async move {
            let reply = match with_session(ctx, this.handler.handle(&session, input)).await {
                Ok(reply) => reply,
                Err(error) => {
                    warn!(%error, "message handling failed");
                    format!("处理消息时出错了: {error}")
                }
            };
            if let Err(error) = sink.send(&reply).await {
                warn!(%error, "failed to send reply");
            }
            // Turn done: clear the in-flight flag and any approval left pending
            // (e.g. the agent abandoned a tool call without resolving it).
            this.approvals.forget_pending(&session);
            this.inflight.lock().unwrap().remove(&session);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_commands_case_insensitively() {
        assert_eq!(classify("/new"), Command::New);
        assert_eq!(classify("  /CLEAR "), Command::New);
        assert_eq!(classify("/approve"), Command::Approve(Decision::Once));
        assert_eq!(
            classify("/approve session"),
            Command::Approve(Decision::Session)
        );
        assert_eq!(classify("/deny"), Command::Deny);
        assert_eq!(classify("hello"), Command::Plain("hello".to_string()));
        // A leading slash inside a longer message is plain text.
        assert_eq!(
            classify("/approve the budget"),
            Command::Plain("/approve the budget".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_returns_false_when_nothing_pending() {
        let state = ApprovalState::new();
        assert!(!state.resolve("s1", Decision::Once));
    }

    #[tokio::test]
    async fn register_then_resolve_delivers_the_decision() {
        let state = ApprovalState::new();
        let rx = state.register("s1");
        assert!(state.resolve("s1", Decision::Session));
        assert_eq!(rx.await.unwrap(), Decision::Session);
    }

    #[tokio::test]
    async fn clear_cancels_a_pending_wait() {
        let state = ApprovalState::new();
        let rx = state.register("s1");
        state.clear("s1");
        // Sender dropped → receiver errors → treated as denial.
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn session_approval_cache_remembers_scope_keys() {
        let state = ApprovalState::new();
        assert!(!state.is_session_approved("s1", "file:write"));
        state.remember("s1", "file:write");
        assert!(state.is_session_approved("s1", "file:write"));
        // Scoped per session.
        assert!(!state.is_session_approved("s2", "file:write"));
        state.clear("s1");
        assert!(!state.is_session_approved("s1", "file:write"));
    }
}
