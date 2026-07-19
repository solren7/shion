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
//!     `komo` CLI uses — sessions, tasks, memories, runs, plus a `status`
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
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{
    agent::{daemon::DreamSweep, gateway::Channel, interaction::GatewayDispatcher},
    config::ApiConfig,
    domain::{
        gateway::MessageHandler,
        memory::{MemoryStatus, parse_memory_status},
        pairing::ApproveOutcome,
    },
    services::{
        operator_control::{
            MemoryTransitionAction, ResumeOutcome,
            actions::{
                OperatorActions, TransitionOutcome, not_recoverable_message, resolve_resume,
            },
        },
        tool_execution::{SessionContext, with_session},
    },
};
use std::net::SocketAddr;

/// What the HTTP transport itself needs, cheaply cloned per request (all
/// `Arc`): the bearer key, the chat handler, the operator use cases, and the
/// two `/api/status` facts. Operator behavior lives in [`OperatorActions`] —
/// this is transport state, not a dependency list.
#[derive(Clone)]
struct AppState {
    api_key: Arc<String>,
    handler: Arc<dyn MessageHandler>,
    actions: Arc<OperatorActions>,
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
    pub fn new(
        config: &ApiConfig,
        handler: Arc<dyn MessageHandler>,
        actions: Arc<OperatorActions>,
        channels: Vec<String>,
        home: Option<String>,
    ) -> Self {
        Self {
            bind: config.bind.clone(),
            port: config.port,
            state: AppState {
                api_key: Arc::new(config.server_key.clone()),
                handler,
                actions,
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
        // Publish how to reach this gateway so the local `komo` CLI can route
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
    // Control-plane writes: host-operator actions (memory governance, run
    // prune, session clean, pairing admission, dream apply). Loopback-gated as
    // a *layer*, not per-handler checks, so a write route added here is gated
    // by construction — a publicly-bound api (`[channels.api] enabled = true`)
    // never reaches these, valid key or not.
    let operator_writes = Router::new()
        .route("/api/memories/{id}/promote", post(memory_promote))
        .route("/api/memories/{id}/reject", post(memory_reject))
        .route("/api/memories/{id}/pin", post(memory_pin))
        .route("/api/runs/prune", post(prune_runs))
        .route("/api/sessions/clean", post(clean_sessions))
        .route("/api/pairings/approve", post(pair_approve))
        .route("/api/pairings/{id}/revoke", post(pair_revoke))
        .route("/api/dream/apply", post(dream_apply))
        .route_layer(middleware::from_fn(require_loopback));

    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/api/status", get(status))
        .route("/api/home", get(get_home))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/messages", get(session_messages))
        .route("/api/tasks", get(list_tasks))
        .route("/api/memories", get(list_memories))
        .route("/api/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/reminders", get(list_reminders))
        .route("/api/skills", get(list_skills))
        .route("/api/skills/{name}/audit", get(skill_audit))
        .route("/api/pairings", get(list_pairings))
        .route("/api/dream", get(dream_preview))
        .merge(operator_writes)
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

/// Reject any control-plane write not arriving over loopback. These are
/// host-operator actions — like the trusted chat path, they must be unreachable
/// on an external bind regardless of the bearer key.
async fn require_loopback(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "operator write endpoints are loopback-only" })),
        )
            .into_response();
    }
    next.run(req).await
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
        Some(token) if bearer_matches(token, state.api_key.as_str()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Constant-time bearer-token check. Both sides are SHA-256'd to a fixed-size
/// digest (so neither length nor content leaks) and compared with the shared
/// constant-time primitive (`domain::pairing::ct_eq`) — a plain `==` on the
/// tokens would let a timing side-channel probe the key byte by byte when the
/// api channel is bound externally (`[channels.api] enabled = true`), where
/// the key is the only auth.
fn bearer_matches(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let digest_hex = |s: &str| -> String {
        Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    crate::domain::pairing::ct_eq(&digest_hex(presented), &digest_hex(expected))
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
            "id": "komo",
            "object": "model",
            "created": 0,
            "owned_by": "komo",
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
        "komo".to_string()
    } else {
        req.model.clone()
    };

    // A **trusted** turn — `komo chat` routed over the gateway's loopback api
    // channel — auto-approves side-effecting tools (the CLI user is the host
    // operator). Gated to loopback callers so a publicly-bound api can never
    // reach it; everyone else gets the detached (auto-deny) context.
    let trusted = peer.ip().is_loopback() && headers.contains_key("x-komo-trusted");
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
/// `X-Komo-Session-Id`. Without it, mint an ephemeral session so no server-side
/// history accrues — the client manages its own context.
fn resolve_session(headers: &axum::http::HeaderMap) -> (String, bool) {
    if let Some(id) = headers
        .get("x-komo-session-id")
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
    let open_tasks = state.actions.open_tasks().await?.len();
    let sessions = state.actions.session_summaries().await?.len();
    Ok(Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "channels": state.channels.as_ref(),
        "home_chat": state.home,
        "open_tasks": open_tasks,
        "sessions": sessions,
    })))
}

/// The `/sethome` runtime override (`None` when unset). The config `home_chat`
/// fallback is *not* resolved here — the CLI derives it from the same
/// config.toml locally; only the db-held override needs the gateway.
async fn get_home(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let over = state.actions.home_override().await?;
    Ok(Json(json!({ "override": over })))
}

async fn list_sessions(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // Summaries only — never dump every transcript in a list view.
    let sessions = state.actions.session_summaries().await?;
    Ok(Json(json!({ "sessions": sessions })))
}

async fn session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let messages = state.actions.session_messages(&id).await?;
    Ok(Json(json!({ "session_id": id, "messages": messages })))
}

async fn list_tasks(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    // `Task` serializes verbatim (snake_case status), so the CLI deserializes it
    // straight back into the domain type and reuses its existing renderer.
    let tasks = state.actions.open_tasks().await?;
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
    let status: Option<MemoryStatus> = params
        .status
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(parse_memory_status);
    // Memory derives Serialize, so it serializes verbatim.
    let memories = state.actions.list_memories(status).await?;
    Ok(Json(json!({ "memories": memories })))
}

// Memory governance writes (`komo memory promote/reject/pin` while the gateway
// holds the db lock). Host-operator actions — loopback-gated by the
// `require_loopback` layer on the operator-writes router.

async fn memory_promote(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    memory_transition(&state, &id, MemoryTransitionAction::Promote).await
}

async fn memory_reject(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    memory_transition(&state, &id, MemoryTransitionAction::Reject).await
}

async fn memory_pin(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    memory_transition(&state, &id, MemoryTransitionAction::Pin).await
}

/// Apply one governance transition (the shared operator definition — the
/// domain owns the semantics) and return the updated memory. 404 on an
/// unknown id.
async fn memory_transition(
    state: &AppState,
    id: &str,
    action: MemoryTransitionAction,
) -> Result<Response, ApiError> {
    match state.actions.memory_transition(id, action).await? {
        TransitionOutcome::Applied(memory) => Ok(Json(json!({ "memory": memory })).into_response()),
        TransitionOutcome::NotFound => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no memory with id `{id}`") })),
        )
            .into_response()),
    }
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
    let runs = state.actions.runs(limit).await?;
    Ok(Json(json!({ "runs": runs })))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // `Run` and `RunStep` serialize verbatim; the CLI reuses its run renderer.
    match state.actions.run(&id).await? {
        Some((run, steps)) => Ok(Json(json!({ "run": run, "steps": steps })).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "run not found" })),
        )
            .into_response()),
    }
}

/// Resume an interrupted run (backs `komo run resume` while the gateway holds
/// the db lock): compose the priming input from the ledger and drive one normal
/// turn in the run's original session, then clear the `recoverable` flag.
/// Trust follows the chat rule — loopback + `X-Komo-Trusted` auto-approves
/// (the CLI user is the host operator); anyone else runs detached (auto-deny).
async fn resume_run(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // Eligibility + priming come from the shared operator definition, so this
    // endpoint and the CLI's direct path can never disagree.
    let (run, steps, input) = match resolve_resume(state.actions.runs.as_ref(), &id).await? {
        crate::services::operator_control::actions::ResumeTarget::Missing => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "run not found" })),
            )
                .into_response());
        }
        crate::services::operator_control::actions::ResumeTarget::NotRecoverable { status } => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": not_recoverable_message(&id, &status) })),
            )
                .into_response());
        }
        crate::services::operator_control::actions::ResumeTarget::Ready { run, steps, input } => {
            (run, steps, input)
        }
    };

    let trusted = peer.ip().is_loopback() && headers.contains_key("x-komo-trusted");
    let ctx = if trusted {
        SessionContext::trusted(&run.session_id)
    } else {
        SessionContext::detached(&run.session_id)
    };
    let reply = with_session(ctx, state.handler.handle(&run.session_id, input)).await?;

    if let Err(error) = state.actions.runs.mark_resumed(&id).await {
        warn!(%error, run_id = %id, "failed to clear recoverable flag after resume");
    }
    info!(run_id = %id, session = %run.session_id, "run resumed");
    Ok(Json(ResumeOutcome {
        run_id: id,
        session_id: run.session_id,
        steps: steps.len(),
        reply,
    })
    .into_response())
}

// ---- control-plane write endpoints ------------------------------------------
//
// These back the maintenance CLIs (`run prune`, `session clean`,
// `pair approve|revoke`, `dream --apply`) while the gateway holds the db lock.
// All of them are loopback-gated by the `require_loopback` layer on the
// operator-writes router (see `build_router`) — not per-handler checks.

#[derive(Deserialize)]
struct PruneParams {
    cutoff: i64,
}

/// Drop runs (and their steps) started before `cutoff`. The client resolves
/// `--before`/`--keep` into the cutoff (it can read runs over `/api/runs`).
async fn prune_runs(
    State(state): State<AppState>,
    Query(params): Query<PruneParams>,
) -> Result<Response, ApiError> {
    let removed = state.actions.prune_runs(params.cutoff).await?;
    Ok(Json(json!({ "removed": removed })).into_response())
}

/// Delete every session with no messages (backs `komo session clean`).
async fn clean_sessions(State(state): State<AppState>) -> Result<Response, ApiError> {
    let removed = state.actions.clean_sessions().await?;
    Ok(Json(json!({ "removed": removed })).into_response())
}

#[derive(Deserialize)]
struct ApproveParams {
    code: String,
}

/// Approve the pending pairing bearing `code` (backs `komo pair approve`). The
/// outcome variant is echoed so the CLI prints the same message it would locally.
async fn pair_approve(
    State(state): State<AppState>,
    Json(body): Json<ApproveParams>,
) -> Result<Response, ApiError> {
    let json = match state.actions.pair_approve(&body.code).await? {
        ApproveOutcome::Approved(request) => json!({ "outcome": "approved", "id": request.id }),
        ApproveOutcome::NotFound => json!({ "outcome": "not_found" }),
        ApproveOutcome::Locked { retry_after_secs } => {
            json!({ "outcome": "locked", "retry_after_secs": retry_after_secs })
        }
    };
    Ok(Json(json).into_response())
}

/// Remove a pairing by id (backs `komo pair revoke`).
async fn pair_revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let revoked = state.actions.pair_revoke(&id).await?;
    Ok(Json(json!({ "revoked": revoked })).into_response())
}

/// Run one dreaming consolidation cycle (backs `komo dream --apply`) — the same
/// `DreamSweep` the gateway schedules.
async fn dream_apply(State(state): State<AppState>) -> Result<Response, ApiError> {
    let summary = DreamSweep {
        memories: state.actions.memories.clone(),
    }
    .apply()
    .await?;
    Ok(Json(json!({
        "promoted": summary.memories_promoted,
        "archived": summary.memories_archived,
    }))
    .into_response())
}

// ---- control-plane read endpoints (CLI ↔ gateway) --------------------------

/// Pending reminders (backs `komo cron list`), soonest first.
async fn list_reminders(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let pending = state.actions.pending_reminders().await?;
    Ok(Json(json!({ "reminders": pending })))
}

/// Registered skills (backs `komo skill list`), by name.
async fn list_skills(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let skills = state.actions.list_skills().await?;
    Ok(Json(json!({ "skills": skills })))
}

/// Which turns loaded a skill (backs `komo skill audit` while the gateway
/// holds the db lock). Derived from the run ledger via the shared operator
/// projection.
async fn skill_audit(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let invocations = state.actions.skill_audit(&name).await?;
    Ok(Json(json!({ "invocations": invocations })))
}

/// Pairings (backs `komo pair list`). A hash-free view — the salted code hash
/// and per-row salt are never serialized off the host.
async fn list_pairings(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let pairings = state.actions.pairing_views().await?;
    Ok(Json(json!({ "pairings": pairings })))
}

/// The dreaming dry-run classification (backs `komo dream`, no `--apply`):
/// which candidates would promote / archive, with their scores. Read-only.
async fn dream_preview(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = state.actions.dream_preview().await?;
    Ok(Json(
        json!({ "promote": report.promote, "archive": report.archive }),
    ))
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
        headers.insert("x-komo-session-id", "panel-1".parse().unwrap());
        let (id, stateful) = resolve_session(&headers);
        assert_eq!(id, "api:panel-1");
        assert!(stateful);
    }
}
