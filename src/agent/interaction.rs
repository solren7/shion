//! Interactive gateway layer: lets a chat-channel turn pause for the user's
//! approval mid-execution, and handles the chat control commands (`/new`,
//! `/approve`, `/deny`, `/sethome`, `/wechat login`, `/pair`).
//!
//! Borrowed from hermes-agent's gateway approval. Hermes runs the agent on a
//! worker thread that blocks on a `threading.Event` keyed by session while the
//! async message loop stays responsive and intercepts `/approve` to signal it.
//! komo's tokio-native equivalent:
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
//! the task-local in `services::tool_execution`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
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
    services::clarify::ClarifyState,
    services::tool_execution::{SessionContext, current_session, with_session},
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

/// The human-facing description of a pending approval, stored alongside the
/// reply channel so an out-of-band surface — the HTTP
/// `GET /api/interactions/{session}` the GUI polls — can render the prompt
/// without reading the chat reply sink. Chat channels still see the prompt text
/// via the sink; this is the structured mirror.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PendingApproval {
    pub summary: String,
    pub detail: Option<String>,
    /// `"normal"` | `"dangerous"` (a `Risk::Safe` action never prompts).
    pub risk: String,
}

impl PendingApproval {
    fn from_request(request: &ApprovalRequest) -> Self {
        Self {
            summary: request.summary.clone(),
            detail: request.detail.clone(),
            risk: match request.risk {
                Risk::Dangerous => "dangerous",
                _ => "normal",
            }
            .to_string(),
        }
    }
}

/// Shared approval state, keyed by session: the pending prompt's reply channel
/// plus the set of scope keys the user has approved "for this session". Shared
/// between [`ChatApprover`] (registers/awaits) and [`GatewayDispatcher`]
/// (resolves on `/approve`, clears on `/new`).
pub struct ApprovalState {
    pending: Mutex<HashMap<String, (oneshot::Sender<Decision>, PendingApproval)>>,
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
    /// which the old waiter reads as a denial). `info` is the structured prompt
    /// stored for the interactions poll.
    fn register(&self, session: &str, info: PendingApproval) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(session.to_string(), (tx, info));
        rx
    }

    /// Deliver `decision` to the approver waiting on `session`. Returns whether
    /// one was actually waiting (so the dispatcher can tell the user there was
    /// nothing to approve).
    pub fn resolve(&self, session: &str, decision: Decision) -> bool {
        match self.pending.lock().unwrap().remove(session) {
            Some((tx, _info)) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// The structured description of the approval pending for `session`, if any.
    /// Backs the HTTP `GET /api/interactions/{session}` poll the GUI uses to
    /// render an approval modal (chat channels instead see it via the sink).
    pub fn pending_info(&self, session: &str) -> Option<PendingApproval> {
        self.pending
            .lock()
            .unwrap()
            .get(session)
            .map(|(_, info)| info.clone())
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

    /// Reclaim the session's transient serialization gate between turns
    /// (recreated on demand by [`gate`](Self::gate)). Called when a turn
    /// finishes so the `gates` map doesn't accumulate one entry per session for
    /// the gateway's lifetime. The `approved` set is deliberately *not* touched
    /// — it is session-scoped and must survive until `/new`.
    fn release_gate(&self, session: &str) {
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

        // Trusted turn (a `komo chat` routed over the gateway's loopback api
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
        if let Some(key) = &request.scope_key
            && self.state.is_session_approved(&ctx.session_id, key)
        {
            return true;
        }

        // Serialize concurrent approvals for this session (a round's tools run
        // concurrently now) so they don't race the single `pending` slot. Held
        // until the decision resolves below.
        let gate = self.state.gate(&ctx.session_id);
        let _guard = gate.lock().await;
        // A concurrent approval may have granted this scope "for session" while
        // we waited on the gate — re-check so we don't prompt twice for it.
        if let Some(key) = &request.scope_key
            && self.state.is_session_approved(&ctx.session_id, key)
        {
            return true;
        }

        if let Err(error) = ctx.sink.send(&prompt(request)).await {
            warn!(%error, "failed to send approval prompt; denying");
            return false;
        }

        let rx = self
            .state
            .register(&ctx.session_id, PendingApproval::from_request(request));
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
    /// so the `komo pair` CLI can't open it while the gateway runs.
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
    /// Pending `ask_user` questions (mirrors `approvals`): a plain inbound
    /// message resolves a pending question instead of starting a new turn.
    clarify: Arc<ClarifyState>,
    sessions: Arc<dyn SessionRepository>,
    home: Arc<dyn HomeRepository>,
    todos: Arc<dyn SessionTodoRepository>,
    /// Set when the WeChat channel is enabled — drives `/wechat login`.
    wechat_login: Option<Arc<dyn WeChatLogin>>,
    /// Backs the `/pair` chat commands (same store the `komo pair` CLI uses).
    pairings: Arc<dyn PairingRepository>,
    /// Per-session turn state. A session key is present iff a turn is in flight;
    /// its queue holds up to [`QUEUE_CAP`] messages that arrived mid-turn, drained
    /// FIFO as each turn finishes (so a quick follow-up is answered, not dropped).
    inflight: Mutex<HashMap<String, VecDeque<QueuedMessage>>>,
}

/// How many mid-turn messages a session may queue before further ones are
/// rejected. Small on purpose: it absorbs a rapid follow-up without letting a
/// spamming sender build an unbounded backlog.
const QUEUE_CAP: usize = 2;

/// A message that arrived while its session's turn was in flight, held for
/// dispatch when the turn finishes.
struct QueuedMessage {
    input: String,
    sink: Arc<dyn ReplySink>,
}

impl GatewayDispatcher {
    pub fn new(
        handler: Arc<dyn MessageHandler>,
        approvals: Arc<ApprovalState>,
        clarify: Arc<ClarifyState>,
        sessions: Arc<dyn SessionRepository>,
        home: Arc<dyn HomeRepository>,
        todos: Arc<dyn SessionTodoRepository>,
        wechat_login: Option<Arc<dyn WeChatLogin>>,
        pairings: Arc<dyn PairingRepository>,
    ) -> Self {
        Self {
            handler,
            approvals,
            clarify,
            sessions,
            home,
            todos,
            wechat_login,
            pairings,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Number of sessions with a turn currently in flight. The gateway's
    /// bounded shutdown drain polls this so active turns get a chance to finish
    /// (and persist their reply + run) before teardown, leaving fewer runs
    /// marked `interrupted`.
    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().unwrap().len()
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
                // A pending clarify question belongs to the old conversation;
                // its waiter reads the dropped sender as "no answer".
                self.clarify.clear(session_id);
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
            Command::Plain(input) => {
                // A pending `ask_user` question eats the next plain message as
                // its answer — the suspended turn continues with it; no new
                // turn starts. Control commands above keep priority (`/deny`
                // etc. never reach here), and a second message while the turn
                // keeps running queues as usual via `spawn_turn`.
                if self.clarify.resolve(session_id, &input) {
                    return;
                }
                self.spawn_turn(session_id, input, sink)
            }
        }
    }

    /// Run a `/pair` command against the shared pairing store. Lives in the
    /// gateway (which holds the db lock) so admitting a new sender no longer
    /// needs the `komo pair` CLI — that CLI can't open the db while the
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
                    .send("微信通道未启用：先在 ~/.komo/config.toml 配置 [channels.wechat]。")
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
        // One turn at a time per session (keeps a session's history
        // append-ordered). A message that arrives mid-turn is queued (bounded)
        // so a quick follow-up is answered after the current turn instead of
        // dropped; past the cap it's rejected with a hint to resend. (An
        // `/approve` reply is handled above and never reaches here.)
        {
            let mut inflight = self.inflight.lock().unwrap();
            if let Some(queue) = inflight.get_mut(session_id) {
                if queue.len() >= QUEUE_CAP {
                    let sink = sink.clone();
                    tokio::spawn(async move {
                        let _ = sink
                            .send("上一条还在处理、队列已满；这条未处理，请稍后重发。")
                            .await;
                    });
                } else {
                    queue.push_back(QueuedMessage { input, sink });
                }
                return;
            }
            // No turn in flight: mark the session busy (empty queue) and fall
            // through to dispatch.
            inflight.insert(session_id.to_string(), VecDeque::new());
        }
        self.dispatch_turn(session_id.to_string(), input, sink);
    }

    /// Run one turn on a spawned task. The session is already marked in-flight;
    /// [`TurnGuard`] guarantees the session is released (and the next queued
    /// message dispatched) on every exit path, including a panic or cancellation.
    fn dispatch_turn(self: &Arc<Self>, session: String, input: String, sink: Arc<dyn ReplySink>) {
        let this = self.clone();
        let ctx = SessionContext {
            session_id: session.clone(),
            sink: sink.clone(),
            // A chat channel has a human who can answer an approval prompt.
            interactive: true,
            // Real human prompting — not the trusted loopback-CLI shortcut.
            auto_approve: false,
            // Chat channels don't stream tool events (no live watcher wiring).
            event_sink: None,
        };
        tokio::spawn(async move {
            // Armed until normal completion below. If the task is cancelled
            // (e.g. gateway shutdown), its Drop releases the session so it is
            // never left wedged — see `TurnGuard`.
            let mut guard = TurnGuard {
                dispatcher: this.clone(),
                session: session.clone(),
                armed: true,
            };
            // Fresh clarify budget for this turn (and drop any stale question).
            this.clarify.begin_turn(&session);
            // Catch a panic in the turn (LLM client, a repository, etc.) so a
            // single bad turn neither wedges the session nor loses the queued
            // follow-ups: the session is advanced normally below either way.
            let outcome = AssertUnwindSafe(with_session(ctx, this.handler.handle(&session, input)))
                .catch_unwind()
                .await;
            let reply = match outcome {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => {
                    warn!(%error, "message handling failed");
                    format!("处理消息时出错了: {error}")
                }
                Err(_panic) => {
                    warn!(session = %session, "turn panicked");
                    "处理消息时发生内部错误，请重试。".to_string()
                }
            };
            if let Err(error) = sink.send(&reply).await {
                warn!(%error, "failed to send reply");
            }
            // Normal completion: advance the queue ourselves (safe to spawn from
            // this async context) and disarm the guard's emergency path.
            guard.armed = false;
            this.finish_turn(&session);
        });
    }

    /// A turn finished normally: drop any approval it left pending, then either
    /// dispatch the next queued message or clear the session's in-flight flag.
    fn finish_turn(self: &Arc<Self>, session: &str) {
        // Any approval the turn abandoned (a tool call never resolved) is dropped,
        // and the transient serialization gate is reclaimed (the session-scoped
        // "approved for session" set stays until `/new`).
        self.approvals.forget_pending(session);
        self.approvals.release_gate(session);
        // Same for a clarify question the turn never resolved (+ its budget).
        self.clarify.clear(session);
        let next = {
            let mut inflight = self.inflight.lock().unwrap();
            let Some(queue) = inflight.get_mut(session) else {
                return;
            };
            match queue.pop_front() {
                // Keep the session marked in-flight for the next turn.
                Some(msg) => Some(msg),
                // Queue drained: the session is now idle.
                None => {
                    inflight.remove(session);
                    None
                }
            }
        };
        if let Some(QueuedMessage { input, sink }) = next {
            self.dispatch_turn(session.to_string(), input, sink);
        }
    }
}

/// Releases a session's turn state on the exit paths a normal completion can't
/// cover — a panic that escapes the catch, or task cancellation. On drop while
/// still `armed` it forgets any pending approval and clears the in-flight flag
/// (dropping any queued messages), so a session is never left permanently busy.
/// The normal path disarms it and calls [`GatewayDispatcher::finish_turn`], which
/// also advances the queue; the guard deliberately does *not* spawn from Drop.
struct TurnGuard {
    dispatcher: Arc<GatewayDispatcher>,
    session: String,
    armed: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            self.dispatcher.approvals.forget_pending(&self.session);
            self.dispatcher.approvals.release_gate(&self.session);
            self.dispatcher.clarify.clear(&self.session);
            let dropped = self
                .dispatcher
                .inflight
                .lock()
                .unwrap()
                .remove(&self.session)
                .map(|q| q.len())
                .unwrap_or(0);
            // Queued messages can't be dispatched from Drop (no spawning here)
            // and their senders can't be notified (replies are async). This
            // path is effectively cancellation-only — gateway shutdown — so the
            // loss is inherent, but it must at least be visible in the log.
            if dropped > 0 {
                warn!(
                    session = %self.session,
                    dropped,
                    "turn cancelled; queued messages discarded"
                );
            }
        }
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

    fn sample_pending() -> PendingApproval {
        PendingApproval {
            summary: "run shell command: ls".to_string(),
            detail: None,
            risk: "normal".to_string(),
        }
    }

    #[tokio::test]
    async fn register_then_resolve_delivers_the_decision() {
        let state = ApprovalState::new();
        let rx = state.register("s1", sample_pending());
        // The structured prompt is visible to the interactions poll while pending.
        assert_eq!(
            state.pending_info("s1").map(|p| p.summary),
            Some("run shell command: ls".to_string())
        );
        assert!(state.resolve("s1", Decision::Session));
        assert_eq!(rx.await.unwrap(), Decision::Session);
        // Cleared once resolved.
        assert!(state.pending_info("s1").is_none());
    }

    #[tokio::test]
    async fn clear_cancels_a_pending_wait() {
        let state = ApprovalState::new();
        let rx = state.register("s1", sample_pending());
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

    // --- GatewayDispatcher turn queue / panic recovery -----------------------

    use crate::domain::{
        pairing::PairingRequest, repository::SessionRepository, session::Session, todo::TodoItem,
    };
    use tokio::sync::{Semaphore, mpsc};

    /// A handler that announces each entered input on a channel and blocks until
    /// the test grants a completion permit — so a test can hold a turn "in
    /// flight" and observe dispatch order. Panics on the input `"boom"`.
    struct GateHandler {
        entered: mpsc::UnboundedSender<String>,
        permits: Arc<Semaphore>,
    }

    #[async_trait]
    impl MessageHandler for GateHandler {
        async fn handle(&self, _session_id: &str, input: String) -> anyhow::Result<String> {
            let _ = self.entered.send(input.clone());
            if input == "boom" {
                panic!("boom");
            }
            let permit = self.permits.acquire().await.unwrap();
            permit.forget();
            Ok(input)
        }
    }

    /// A sink that records every text sent through it.
    struct RecordingSink {
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ReplySink for RecordingSink {
        async fn send(&self, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    // The plain-text turn path touches only the handler, approval state, and the
    // in-flight map — never these repositories — so the fakes are unreachable.
    struct UnusedSessions;
    #[async_trait]
    impl SessionRepository for UnusedSessions {
        async fn find(&self, _id: &str) -> anyhow::Result<Option<Session>> {
            unimplemented!()
        }
        async fn find_windowed(&self, _id: &str, _limit: usize) -> anyhow::Result<Option<Session>> {
            unimplemented!()
        }
        async fn list(&self) -> anyhow::Result<Vec<Session>> {
            unimplemented!()
        }
        async fn save(&self, _session: &Session) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_empty_sessions(&self) -> anyhow::Result<usize> {
            unimplemented!()
        }
        async fn rotate(&self, _session_id: &str) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }
    }

    struct UnusedHome;
    #[async_trait]
    impl HomeRepository for UnusedHome {
        async fn get(&self) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }
        async fn set(&self, _session_id: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    struct UnusedTodos;
    #[async_trait]
    impl SessionTodoRepository for UnusedTodos {
        async fn get(&self, _session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
            unimplemented!()
        }
        async fn set(&self, _session_id: &str, _items: &[TodoItem]) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn clear(&self, _session_id: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    struct UnusedPairings;
    #[async_trait]
    impl PairingRepository for UnusedPairings {
        async fn upsert(&self, _request: &PairingRequest) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn find(
            &self,
            _platform: &str,
            _sender_id: &str,
        ) -> anyhow::Result<Option<PairingRequest>> {
            unimplemented!()
        }
        async fn count_active_pending(&self, _platform: &str) -> anyhow::Result<usize> {
            unimplemented!()
        }
        async fn approve_code(&self, _code: &str) -> anyhow::Result<ApproveOutcome> {
            unimplemented!()
        }
        async fn list(&self) -> anyhow::Result<Vec<PairingRequest>> {
            unimplemented!()
        }
        async fn revoke(&self, _id: &str) -> anyhow::Result<bool> {
            unimplemented!()
        }
    }

    fn dispatcher_with(handler: Arc<GateHandler>) -> Arc<GatewayDispatcher> {
        dispatcher_with_clarify(handler, Arc::new(ClarifyState::new()))
    }

    fn dispatcher_with_clarify(
        handler: Arc<GateHandler>,
        clarify: Arc<ClarifyState>,
    ) -> Arc<GatewayDispatcher> {
        Arc::new(GatewayDispatcher::new(
            handler,
            Arc::new(ApprovalState::new()),
            clarify,
            Arc::new(UnusedSessions),
            Arc::new(UnusedHome),
            Arc::new(UnusedTodos),
            None,
            Arc::new(UnusedPairings),
        ))
    }

    // A plain message answers a pending clarify question instead of starting a
    // new turn; once nothing is pending, plain messages dispatch normally.
    #[tokio::test]
    async fn plain_message_resolves_pending_clarify_not_a_new_turn() {
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let handler = Arc::new(GateHandler {
            entered: entered_tx,
            permits: permits.clone(),
        });
        let clarify = Arc::new(ClarifyState::new());
        let dispatcher = dispatcher_with_clarify(handler, clarify.clone());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

        // A turn is suspended on a question.
        let rx = clarify.register("s1", "什么颜色？");
        dispatcher.handle("s1", "蓝色的".into(), sink.clone()).await;
        assert_eq!(rx.await.unwrap(), "蓝色的", "message became the answer");
        assert!(
            entered_rx.try_recv().is_err(),
            "the answer must not start a new turn"
        );

        // With nothing pending, the next message dispatches a turn as usual.
        dispatcher.handle("s1", "next".into(), sink.clone()).await;
        assert_eq!(next_entered(&mut entered_rx).await, "next");
        permits.add_permits(1);
    }

    // Control commands keep priority over a pending clarify: `/deny` resolves
    // the approval path and is never eaten as the question's answer.
    #[tokio::test]
    async fn commands_keep_priority_over_pending_clarify() {
        let (entered_tx, _entered_rx) = mpsc::unbounded_channel();
        let handler = Arc::new(GateHandler {
            entered: entered_tx,
            permits: Arc::new(Semaphore::new(0)),
        });
        let clarify = Arc::new(ClarifyState::new());
        let dispatcher = dispatcher_with_clarify(handler, clarify.clone());
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { sent }) as Arc<dyn ReplySink>;

        let _rx = clarify.register("s1", "问题？");
        dispatcher.handle("s1", "/deny".into(), sink.clone()).await;
        assert!(
            clarify.has_pending("s1"),
            "/deny must not consume the clarify question"
        );
    }

    /// Wait for the next entered input, failing the test on timeout so a wedge
    /// surfaces as a failure rather than a hang.
    async fn next_entered(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a turn to start")
            .expect("handler channel closed")
    }

    #[tokio::test]
    async fn mid_turn_messages_queue_fifo_and_cap() {
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let handler = Arc::new(GateHandler {
            entered: entered_tx,
            permits: permits.clone(),
        });
        let dispatcher = dispatcher_with(handler);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;

        // m1 dispatches and blocks in the handler.
        dispatcher.handle("s1", "m1".into(), sink.clone()).await;
        assert_eq!(next_entered(&mut entered_rx).await, "m1");

        // m2, m3 queue behind it; m4 overflows the cap and is rejected.
        dispatcher.handle("s1", "m2".into(), sink.clone()).await;
        dispatcher.handle("s1", "m3".into(), sink.clone()).await;
        dispatcher.handle("s1", "m4".into(), sink.clone()).await;

        // Release turns one at a time; the queue drains in FIFO order.
        permits.add_permits(1);
        assert_eq!(next_entered(&mut entered_rx).await, "m2");
        permits.add_permits(1);
        assert_eq!(next_entered(&mut entered_rx).await, "m3");
        permits.add_permits(1);

        // Let the final reply + rejection settle, then assert the overflow hint
        // was delivered and no fourth turn ever started.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter().any(|t| t.contains("队列已满")),
            "m4 should be rejected with the queue-full hint, got {sent:?}"
        );
        assert!(entered_rx.try_recv().is_err(), "m4 must not have run");
    }

    #[tokio::test]
    async fn a_panicking_turn_does_not_wedge_the_session() {
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let handler = Arc::new(GateHandler {
            entered: entered_tx,
            permits: permits.clone(),
        });
        let dispatcher = dispatcher_with(handler);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { sent: sent.clone() }) as Arc<dyn ReplySink>;

        // First turn panics — the catch keeps the task alive and the guard/finish
        // path releases the session.
        dispatcher.handle("s1", "boom".into(), sink.clone()).await;
        assert_eq!(next_entered(&mut entered_rx).await, "boom");

        // A later message must still be handled (session not permanently busy).
        dispatcher.handle("s1", "after".into(), sink.clone()).await;
        permits.add_permits(1);
        assert_eq!(next_entered(&mut entered_rx).await, "after");
    }
}
