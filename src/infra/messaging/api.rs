//! HTTP API ingress channel.
//!
//! Exposes the agent over a loopback HTTP port for local UIs (the Tauri
//! dashboard) and any OpenAI-compatible client (Open WebUI, LobeChat, …). Two
//! families of endpoints:
//!
//!   - **OpenAI-compatible** (`/v1/*`): `chat/completions` (streaming and not)
//!     and `models`, so third-party chat frontends connect by pointing at
//!     `http://127.0.0.1:8765/v1` with the bearer key.
//!   - **dashboard** (`/api/*`): read views over the same repositories the
//!     `shion` CLI uses — sessions, tasks, memories, runs, plus a `status`
//!     aggregate. These back the desktop control panel (roadmap §9).
//!
//! Unlike the chat channels, an HTTP request is synchronous request/response,
//! so it calls the [`MessageHandler`] directly and awaits the reply rather than
//! going through the spawn-and-return [`GatewayDispatcher`]. The turn runs in a
//! **non-interactive** session context ([`SessionContext::detached`]), so a tool
//! that needs approval is denied immediately — there is no human on an HTTP
//! request to answer a `/approve` prompt.
//!
//! Auth is a single bearer key (`API_SERVER_KEY`); the listener binds loopback
//! by default. `/health` is unauthenticated so a probe can check liveness.

use std::convert::Infallible;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{
    agent::{gateway::Channel, interaction::GatewayDispatcher},
    config::ApiConfig,
    domain::{
        gateway::MessageHandler,
        memory::{
            DreamVerdict, MemoryRepository, MemoryStatus, dream_score, dream_verdict,
            parse_memory_status,
        },
        pairing::{PairingRepository, PairingStatus},
        reminder::ReminderRepository,
        repository::{MessageRepository, SessionRepository, SkillRepository},
        run::RunRepository,
        task::TaskRepository,
    },
    services::tool_registry::{SessionContext, with_session},
};
use std::net::SocketAddr;

/// Everything the HTTP handlers need, cheaply cloned per request (all `Arc`).
#[derive(Clone)]
struct AppState {
    api_key: Arc<String>,
    handler: Arc<dyn MessageHandler>,
    sessions: Arc<dyn SessionRepository>,
    messages: Arc<dyn MessageRepository>,
    tasks: Arc<dyn TaskRepository>,
    memories: Arc<dyn MemoryRepository>,
    runs: Arc<dyn RunRepository>,
    reminders: Arc<dyn ReminderRepository>,
    skills: Arc<dyn SkillRepository>,
    pairings: Arc<dyn PairingRepository>,
    /// Channel names enabled on this gateway (for `/api/status`).
    channels: Arc<Vec<String>>,
    /// Resolved config `home_chat` fallback, if any (for `/api/status`).
    home: Option<String>,
}

/// The HTTP API channel. Holds the listen config and the shared handler state.
pub struct ApiChannel {
    bind: String,
    port: u16,
    state: AppState,
}

impl ApiChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &ApiConfig,
        handler: Arc<dyn MessageHandler>,
        sessions: Arc<dyn SessionRepository>,
        messages: Arc<dyn MessageRepository>,
        tasks: Arc<dyn TaskRepository>,
        memories: Arc<dyn MemoryRepository>,
        runs: Arc<dyn RunRepository>,
        reminders: Arc<dyn ReminderRepository>,
        skills: Arc<dyn SkillRepository>,
        pairings: Arc<dyn PairingRepository>,
        channels: Vec<String>,
        home: Option<String>,
    ) -> Self {
        Self {
            bind: config.bind.clone(),
            port: config.port,
            state: AppState {
                api_key: Arc::new(config.server_key.clone()),
                handler,
                sessions,
                messages,
                tasks,
                memories,
                runs,
                reminders,
                skills,
                pairings,
                channels: Arc::new(channels),
                home,
            },
        }
    }
}

#[async_trait]
impl Channel for ApiChannel {
    fn name(&self) -> &str {
        "api"
    }

    async fn serve(
        &self,
        _dispatcher: Arc<GatewayDispatcher>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let addr = format!("{}:{}", self.bind, self.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        // With an ephemeral bind (port 0, the loopback-only default) the real
        // port is only known after bind — read it back and advertise it.
        let local = listener.local_addr()?;
        info!(addr = %local, "api channel listening");
        // Publish how to reach this gateway so the local `shion` CLI can route
        // to it instead of opening the db (which Turso's exclusive lock forbids
        // while the gateway holds it). Removed again on graceful shutdown.
        crate::infra::rendezvous::write(&crate::infra::rendezvous::GatewayInfo {
            pid: std::process::id(),
            bind: self.bind.clone(),
            port: local.port(),
            key: self.state.api_key.as_ref().clone(),
        });
        let app = build_router(self.state.clone());
        let graceful = async move {
            let _ = shutdown.changed().await;
        };
        // `into_make_service_with_connect_info` so handlers can see the peer
        // address — the trusted-chat path is gated to loopback callers.
        let result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(graceful)
        .await;
        crate::infra::rendezvous::clear();
        result?;
        info!("api channel stopped");
        Ok(())
    }
}

/// Build the router: `/health` is public, everything else sits behind the
/// bearer-key middleware.
fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/api/status", get(status))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/messages", get(session_messages))
        .route("/api/tasks", get(list_tasks))
        .route("/api/memories", get(list_memories))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/reminders", get(list_reminders))
        .route("/api/skills", get(list_skills))
        .route("/api/pairings", get(list_pairings))
        .route("/api/dream", get(dream_preview))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

/// Reject any request whose `Authorization: Bearer <key>` does not match.
async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token == state.api_key.as_str() => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Maps any handler error to a 500 with a JSON body.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        warn!(error = %self.0, "api request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

// ---- OpenAI-compatible endpoints -------------------------------------------

#[derive(Deserialize)]
struct ChatCompletionRequest {
    #[serde(default)]
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn list_models() -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": [{
            "id": "shion",
            "object": "model",
            "created": 0,
            "owned_by": "shion",
        }],
    }))
}

async fn chat_completions(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    let (session_id, stateful) = resolve_session(&headers);
    let input = build_input(&req.messages, stateful);
    let model = if req.model.is_empty() {
        "shion".to_string()
    } else {
        req.model.clone()
    };

    // A **trusted** turn — `shion chat` routed over the gateway's loopback api
    // channel — auto-approves side-effecting tools (the CLI user is the host
    // operator). Gated to loopback callers so a publicly-bound api can never
    // reach it; everyone else gets the detached (auto-deny) context.
    let trusted = peer.ip().is_loopback() && headers.contains_key("x-shion-trusted");
    let ctx = if trusted {
        SessionContext::trusted(&session_id)
    } else {
        SessionContext::detached(&session_id)
    };

    // Synchronous: drive the turn directly and await the reply.
    let reply = with_session(ctx, state.handler.handle(&session_id, input)).await?;

    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
    let created = now();

    if req.stream {
        Ok(stream_completion(id, created, model, reply).into_response())
    } else {
        Ok(Json(json!({
            "id": id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": reply },
                "finish_reason": "stop",
            }],
        }))
        .into_response())
    }
}

/// SSE rendering of a completed reply. The turn already produced the full text
/// (the tool loop lives inside rig — no token stream yet), so we emit it as one
/// delta chunk followed by the stop chunk and `[DONE]`. Streaming clients see a
/// normal stream; it just isn't token-incremental.
fn stream_completion(
    id: String,
    created: i64,
    model: String,
    reply: String,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let content_chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": reply },
            "finish_reason": Value::Null,
        }],
    });
    let stop_chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
    });
    let events = vec![
        Ok(Event::default().data(content_chunk.to_string())),
        Ok(Event::default().data(stop_chunk.to_string())),
        Ok(Event::default().data("[DONE]")),
    ];
    Sse::new(futures_util::stream::iter(events))
}

/// Continue an existing conversation only when the client opts in with
/// `X-Shion-Session-Id`. Without it, mint an ephemeral session so no server-side
/// history accrues — the client manages its own context.
fn resolve_session(headers: &axum::http::HeaderMap) -> (String, bool) {
    if let Some(id) = headers
        .get("x-shion-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        (format!("api:{id}"), true)
    } else {
        (format!("api:{}", uuid::Uuid::now_v7()), false)
    }
}

/// Reduce the OpenAI `messages` array to one input string for the turn.
///
/// Stateful (header given): the agent already has its history in the db, so we
/// pass only the latest user message. Stateless: the client owns the history,
/// so we flatten the whole exchange into the single ephemeral turn.
fn build_input(messages: &[ChatMessage], stateful: bool) -> String {
    if stateful {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .or_else(|| messages.last())
            .map(|m| m.content.clone())
            .unwrap_or_default()
    } else {
        messages
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ---- dashboard endpoints ---------------------------------------------------

async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let open_tasks = state.tasks.list_open().await?.len();
    let sessions = state.sessions.list().await?.len();
    Ok(Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "channels": state.channels.as_ref(),
        "home_chat": state.home,
        "open_tasks": open_tasks,
        "sessions": sessions,
    })))
}

async fn list_sessions(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Summaries only — never dump every transcript in a list view.
    let sessions: Vec<SessionSummary> = state
        .sessions
        .list()
        .await?
        .into_iter()
        .map(|s| SessionSummary {
            created_at: s.created_at,
            messages: s.messages.len(),
            user_turns: s.user_turns(),
            id: s.id,
        })
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

async fn session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let messages = state.messages.list_by_session(&id).await?;
    Ok(Json(json!({ "session_id": id, "messages": messages })))
}

async fn list_tasks(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // `Task` serializes verbatim (snake_case status), so the CLI deserializes it
    // straight back into the domain type and reuses its existing renderer.
    let tasks = state.tasks.list_open().await?;
    Ok(Json(json!({ "tasks": tasks })))
}

#[derive(Deserialize)]
struct MemoryQueryParams {
    status: Option<String>,
}

async fn list_memories(
    State(state): State<AppState>,
    Query(params): Query<MemoryQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let mut memories = state.memories.list().await?;
    if let Some(status) = params.status.as_deref().filter(|s| !s.is_empty()) {
        let want: MemoryStatus = parse_memory_status(status);
        memories.retain(|m| m.status == want);
    }
    // Memory derives Serialize, so it serializes verbatim.
    Ok(Json(json!({ "memories": memories })))
}

#[derive(Deserialize)]
struct RunsQueryParams {
    limit: Option<usize>,
}

async fn list_runs(
    State(state): State<AppState>,
    Query(params): Query<RunsQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let runs = state.runs.list(limit).await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let Some(run) = state.runs.get(&id).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "run not found" })),
        )
            .into_response());
    };
    // `Run` and `RunStep` serialize verbatim; the CLI reuses its run renderer.
    let steps = state.runs.steps(&id).await?;
    Ok(Json(json!({ "run": run, "steps": steps })).into_response())
}

// ---- control-plane read endpoints (CLI ↔ gateway) --------------------------

/// Pending reminders (backs `shion cron list`), soonest first.
async fn list_reminders(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut pending = state.reminders.list_pending().await?;
    pending.sort_by_key(|r| r.run_at);
    Ok(Json(json!({ "reminders": pending })))
}

/// Registered skills (backs `shion skill list`), by name.
async fn list_skills(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut skills = state.skills.list().await?;
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(json!({ "skills": skills })))
}

/// Pairings (backs `shion pair list`). A hash-free view — the salted code hash
/// and per-row salt are never serialized off the host.
async fn list_pairings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let now = now();
    let pairings: Vec<PairingView> = state
        .pairings
        .list()
        .await?
        .into_iter()
        .map(|p| {
            let status = match p.status {
                PairingStatus::Approved => "approved",
                PairingStatus::Pending if p.is_expired(now) => "expired",
                PairingStatus::Pending => "pending",
            };
            PairingView {
                id: p.id,
                status: status.to_string(),
                created_at: p.created_at,
            }
        })
        .collect();
    Ok(Json(json!({ "pairings": pairings })))
}

/// The dreaming dry-run classification (backs `shion dream`, no `--apply`):
/// which candidates would promote / archive, with their scores. Read-only.
async fn dream_preview(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let now = now();
    let memories = state.memories.list().await?;
    let mut promote: Vec<DreamItem> = Vec::new();
    let mut archive: Vec<DreamItem> = Vec::new();
    for m in &memories {
        let item = DreamItem {
            id: m.id.clone(),
            recall_count: m.recall_count,
            score: dream_score(m, now),
            content: m.content.clone(),
        };
        match dream_verdict(m, now) {
            DreamVerdict::Promote => promote.push(item),
            DreamVerdict::Archive => archive.push(item),
            DreamVerdict::Keep => {}
        }
    }
    promote.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(json!({ "promote": promote, "archive": archive })))
}

// ---- shared control-plane view types ---------------------------------------
// Serialized by the endpoints above and deserialized by the CLI gateway client
// (`cli::gateway_client`), so they live here as the single source of truth.

/// A session list row (full transcripts are never dumped in a list view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: i64,
    pub messages: usize,
    pub user_turns: usize,
}

/// A pairing row without the salted code hash / salt (never leaves the host).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingView {
    pub id: String,
    /// `pending` | `approved` | `expired`.
    pub status: String,
    pub created_at: i64,
}

/// One candidate in the dreaming preview, with the score that drove its verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamItem {
    pub id: String,
    pub recall_count: i64,
    pub score: f64,
    pub content: String,
}

/// Unix seconds, for OpenAI `created` fields.
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateful_input_takes_last_user_message() {
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                content: "first".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "reply".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "second".into(),
            },
        ];
        assert_eq!(build_input(&messages, true), "second");
    }

    #[test]
    fn stateless_input_flattens_conversation() {
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "hello".into(),
            },
        ];
        assert_eq!(
            build_input(&messages, false),
            "user: hi\n\nassistant: hello"
        );
    }

    #[test]
    fn resolve_session_is_ephemeral_without_header() {
        let headers = axum::http::HeaderMap::new();
        let (id, stateful) = resolve_session(&headers);
        assert!(id.starts_with("api:"));
        assert!(!stateful);
    }

    #[test]
    fn resolve_session_uses_header_when_present() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-shion-session-id", "panel-1".parse().unwrap());
        let (id, stateful) = resolve_session(&headers);
        assert_eq!(id, "api:panel-1");
        assert!(stateful);
    }
}
