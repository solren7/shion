//! Interactive gateway layer: lets a chat-channel turn pause for the user's
//! approval mid-execution, and handles the chat control commands (`/new`,
//! `/approve`, `/deny`, `/sethome`, `/wechat login`, `/pair`).
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
        gateway::{MessageHandler, ReplySink, WeChatLogin},
        home::HomeRepository,
        pairing::{ApproveOutcome, PairingRepository, PairingStatus},
        repository::SessionRepository,
        todo::SessionTodoRepository,
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
    /// Per-session serialization gate. A round's tool calls now run
    /// concurrently (`AgentRuntime::run_agent_loop`), so two side-effecting
    /// tools can ask for approval at once; holding this across the
    /// prompt→await→resolve cycle keeps the single `pending` slot from being
    /// raced (a second `register` would otherwise drop the first sender, denying
    /// it). Per session, not global, so a slow approver in one chat never blocks
    /// another chat's prompt.
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    timeout: Duration,
}

impl ApprovalState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            approved: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            timeout: APPROVAL_TIMEOUT,
        }
    }

    /// The approval gate for `session`, created on first use. Held by
    /// [`ChatApprover`] across an interactive prompt so concurrent approvals in
    /// the same session queue instead of racing the `pending` slot.
    fn gate(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .clone()
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
        self.gates.lock().unwrap().remove(session);
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

        // Trusted turn (a `shion chat` routed over the gateway's loopback api
        // channel): the CLI user is the host operator, so run without prompting.
        // The api channel only builds a trusted context for loopback callers.
        if ctx.auto_approve {
            return true;
        }

        // No human to answer (HTTP API, detached REPL context): deny rather than
        // prompt a sink no one reads and wait out the timeout.
        if !ctx.interactive {
            warn!(summary = %request.summary, "approval auto-denied (non-interactive session)");
            return false;
        }

        // Already approved this kind of action for the session?
        if let Some(key) = &request.scope_key {
            if self.state.is_session_approved(&ctx.session_id, key) {
                return true;
            }
        }

        // Serialize concurrent approvals for this session (a round's tools run
        // concurrently now) so they don't race the single `pending` slot. Held
        // until the decision resolves below.
        let gate = self.state.gate(&ctx.session_id);
        let _guard = gate.lock().await;
        // A concurrent approval may have granted this scope "for session" while
        // we waited on the gate — re-check so we don't prompt twice for it.
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
    /// Make this chat the home channel for proactive output.
    SetHome,
    /// Provision the WeChat channel by QR (delivered to this chat).
    WechatLogin,
    /// Approve/list/revoke pairings from chat — the gateway holds the db lock,
    /// so the `shion pair` CLI can't open it while the gateway runs.
    Pair(PairAction),
    /// Ordinary message — run a turn.
    Plain(String),
}

/// The sub-action of a `/pair` chat command.
#[derive(Debug, PartialEq, Eq)]
pub enum PairAction {
    List,
    Approve(String),
    Revoke(String),
    /// Unrecognized `/pair …` — reply with usage.
    Usage,
}

/// Classify an inbound message. No-arg commands match case-insensitively on the
/// whole (trimmed) message. `/pair …` takes an argument, so it is parsed from
/// the original text (the code/id keep their case); anything else is plain text.
pub fn classify(text: &str) -> Command {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    if lower == "/pair" || lower.starts_with("/pair ") {
        // Split off the verb + argument from the *original* text so a revoke id
        // (`{platform}:{sender_id}`) and a code keep their case.
        let mut parts = trimmed.split_whitespace();
        let _ = parts.next(); // "/pair"
        let verb = parts.next().map(|v| v.to_lowercase());
        let arg = parts.next().map(|s| s.to_string());
        return Command::Pair(match (verb.as_deref(), arg) {
            (Some("list"), _) | (None, _) => PairAction::List,
            (Some("approve"), Some(code)) => PairAction::Approve(code),
            (Some("revoke"), Some(id)) => PairAction::Revoke(id),
            _ => PairAction::Usage,
        });
    }

    match lower.as_str() {
        "/new" | "/clear" | "/reset" => Command::New,
        "/approve" | "/yes" | "/y" | "/ok" => Command::Approve(Decision::Once),
        "/approve session" | "/approve all" => Command::Approve(Decision::Session),
        "/deny" | "/no" | "/n" => Command::Deny,
        "/sethome" | "/home" => Command::SetHome,
        "/wechat" | "/wechat login" | "/weixin" => Command::WechatLogin,
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
    sessions: Arc<dyn SessionRepository>,
    home: Arc<dyn HomeRepository>,
    todos: Arc<dyn SessionTodoRepository>,
    /// Set when the WeChat channel is enabled — drives `/wechat login`.
    wechat_login: Option<Arc<dyn WeChatLogin>>,
    /// Backs the `/pair` chat commands (same store the `shion pair` CLI uses).
    pairings: Arc<dyn PairingRepository>,
    inflight: Mutex<HashSet<String>>,
}

impl GatewayDispatcher {
    pub fn new(
        handler: Arc<dyn MessageHandler>,
        approvals: Arc<ApprovalState>,
        sessions: Arc<dyn SessionRepository>,
        home: Arc<dyn HomeRepository>,
        todos: Arc<dyn SessionTodoRepository>,
        wechat_login: Option<Arc<dyn WeChatLogin>>,
        pairings: Arc<dyn PairingRepository>,
    ) -> Self {
        Self {
            handler,
            approvals,
            sessions,
            home,
            todos,
            wechat_login,
            pairings,
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
                // The working todo list is session-scoped; a fresh conversation
                // starts with an empty one. (The session id is reused across the
                // rotate, so the row must be cleared explicitly.)
                if let Err(error) = self.todos.clear(session_id).await {
                    warn!(%error, "failed to clear session todos (non-fatal)");
                }
                // Rotate (hermes-style): archive the old transcript, leave the
                // chat's session empty for a fresh conversation.
                match self.sessions.rotate(session_id).await {
                    Ok(archived) => {
                        info!(session = %session_id, ?archived, "session rotated via /new")
                    }
                    Err(error) => warn!(%error, "session rotate failed (non-fatal)"),
                }
                let _ = sink.send("已开始新会话，之前的上下文已归档。").await;
            }
            Command::SetHome => {
                let reply = match self.home.set(session_id).await {
                    Ok(()) => {
                        info!(session = %session_id, "home channel set via /sethome");
                        "✅ 已将当前会话设为提醒与通知的接收频道。"
                    }
                    Err(error) => {
                        warn!(%error, "failed to set home channel");
                        "设置接收频道失败，请稍后再试。"
                    }
                };
                let _ = sink.send(reply).await;
            }
            Command::WechatLogin => self.spawn_wechat_login(sink),
            Command::Pair(action) => {
                let reply = self.handle_pair(action).await;
                let _ = sink.send(&reply).await;
            }
            Command::Plain(input) => self.spawn_turn(session_id, input, sink),
        }
    }

    /// Run a `/pair` command against the shared pairing store. Lives in the
    /// gateway (which holds the db lock) so admitting a new sender no longer
    /// needs the `shion pair` CLI — that CLI can't open the db while the
    /// gateway is running. Any already-admitted sender may run it (same trust
    /// level as `/sethome` and `/wechat login`).
    async fn handle_pair(&self, action: PairAction) -> String {
        match action {
            PairAction::Usage => {
                "用法：/pair list · /pair approve <code> · /pair revoke <platform:sender_id>"
                    .to_string()
            }
            PairAction::List => match self.pairings.list().await {
                Ok(list) if list.is_empty() => {
                    "暂无配对。陌生发送者首次联系时会收到一个配对码。".to_string()
                }
                Ok(list) => {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let mut out = String::from("配对列表：\n");
                    for p in list {
                        let state = match p.status {
                            PairingStatus::Approved => "approved",
                            PairingStatus::Pending if p.is_expired(now) => "expired",
                            PairingStatus::Pending => "pending",
                        };
                        out.push_str(&format!("· {} [{}]\n", p.id, state));
                    }
                    out.push_str("\n批准：/pair approve <发送者给你的 code>");
                    out
                }
                Err(error) => {
                    warn!(%error, "pair list via chat failed");
                    "读取配对列表失败，请稍后再试。".to_string()
                }
            },
            PairAction::Approve(code) => {
                let code = code.trim().to_uppercase();
                match self.pairings.approve_code(&code).await {
                    Ok(ApproveOutcome::Approved(req)) => {
                        info!(id = %req.id, "pairing approved via chat");
                        format!("✅ 已配对 {} —— 对方现在可以对话了。", req.id)
                    }
                    Ok(ApproveOutcome::NotFound) => {
                        format!(
                            "没有匹配 code {code} 的待批准配对（未知或已过期，见 /pair list）。"
                        )
                    }
                    Ok(ApproveOutcome::Locked { retry_after_secs }) => format!(
                        "失败次数过多，批准已锁定，请 {} 分钟后再试。",
                        (retry_after_secs + 59) / 60
                    ),
                    Err(error) => {
                        warn!(%error, "pair approve via chat failed");
                        "批准失败，请稍后再试。".to_string()
                    }
                }
            }
            PairAction::Revoke(id) => match self.pairings.revoke(&id).await {
                Ok(true) => {
                    info!(%id, "pairing revoked via chat");
                    format!("已解除配对 {id}。")
                }
                Ok(false) => format!("没有配对 {id}（见 /pair list）。"),
                Err(error) => {
                    warn!(%error, "pair revoke via chat failed");
                    "解除配对失败，请稍后再试。".to_string()
                }
            },
        }
    }

    /// Run the WeChat QR login off the receive loop: it blocks while the user
    /// scans, and the QR is delivered to this chat as a photo. On success the
    /// login pulses the channel's `ready` signal, bringing it online.
    fn spawn_wechat_login(self: &Arc<Self>, sink: Arc<dyn ReplySink>) {
        let Some(login) = self.wechat_login.clone() else {
            tokio::spawn(async move {
                let _ = sink
                    .send("微信通道未启用：先在 ~/.shion/config.toml 配置 [channels.wechat]。")
                    .await;
            });
            return;
        };
        tokio::spawn(async move {
            let _ = sink.send("正在生成微信登录二维码，请稍候…").await;
            match login.run(sink.clone()).await {
                Ok(user_id) => {
                    let _ = sink
                        .send(&format!("✅ 微信已连接（{user_id}），现在可以直接对话了。"))
                        .await;
                }
                Err(error) => {
                    warn!(%error, "wechat login via chat failed");
                    let _ = sink.send(&format!("微信登录失败：{error}")).await;
                }
            }
        });
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
            // A chat channel has a human who can answer an approval prompt.
            interactive: true,
            // Real human prompting — not the trusted loopback-CLI shortcut.
            auto_approve: false,
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
        assert_eq!(classify("/sethome"), Command::SetHome);
        assert_eq!(classify(" /SetHome "), Command::SetHome);
        assert_eq!(classify("/wechat login"), Command::WechatLogin);
        assert_eq!(classify(" /WeChat "), Command::WechatLogin);
        assert_eq!(classify("hello"), Command::Plain("hello".to_string()));
        // A leading slash inside a longer message is plain text.
        assert_eq!(
            classify("/approve the budget"),
            Command::Plain("/approve the budget".to_string())
        );
    }

    #[test]
    fn classify_parses_pair_subcommands_preserving_arg_case() {
        assert_eq!(classify("/pair"), Command::Pair(PairAction::List));
        assert_eq!(classify("/pair list"), Command::Pair(PairAction::List));
        // The verb is case-insensitive but the code/id keep their case.
        assert_eq!(
            classify("/PAIR approve aB12cD34"),
            Command::Pair(PairAction::Approve("aB12cD34".to_string()))
        );
        assert_eq!(
            classify("/pair revoke feishu:ou_AbC"),
            Command::Pair(PairAction::Revoke("feishu:ou_AbC".to_string()))
        );
        // Missing argument → usage, not a turn.
        assert_eq!(classify("/pair approve"), Command::Pair(PairAction::Usage));
        assert_eq!(
            classify("/pair frobnicate"),
            Command::Pair(PairAction::Usage)
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
