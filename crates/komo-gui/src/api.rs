//! HTTP client for the komo gateway's api channel.
//!
//! A near-copy of `komo::infra::gateway_client::GatewayClient`, trimmed to the
//! subset the GUI needs and reusing `komo-core` DTOs so responses deserialize
//! into the same types the gateway serialized. Discovery is the same rendezvous
//! file the CLI reads (`~/.komo/gateway.json`), and auth is the same bearer key.
//!
//! `ApiClient` is `Clone` (reqwest's `Client` is internally `Arc`) so it can
//! live in a Dioxus context and be cheaply captured into spawned turn tasks.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use komo_core::domain::{
    memory::Memory,
    message::Message,
    reminder::Reminder,
    run::{Run, RunStep},
    skill::Skill,
    task::Task,
};
use komo_core::operator_view::{
    DreamItem, PairingView, ResumeOutcome, SessionSummary, SkillInvocation,
};
use komo_core::rendezvous::{self, GatewayInfo};

/// Per-request budget. Longer than the CLI client's 300s: an interactive turn
/// may block on a human answering an approval (up to the 5-min approval
/// timeout) *after* the model has already done work.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Health-probe budget — short, so a stale rendezvous file fails fast.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How a chat turn is run server-side (loopback only; ignored on an external
/// bind). See `komo::domain::context::SessionContext`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChatMode {
    /// Prompt for approval / clarify and suspend the turn; the GUI resolves the
    /// prompt out-of-band via the `interactions` endpoints.
    Interactive,
    /// Auto-approve side-effecting tools (like `komo chat`).
    Trusted,
}

/// The `/api/status` aggregate.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct StatusSnapshot {
    pub ok: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub home_chat: Option<String>,
    #[serde(default)]
    pub open_tasks: usize,
    #[serde(default)]
    pub sessions: usize,
}

/// A pending approval prompt (mirrors `agent::interaction::PendingApproval`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PendingApproval {
    pub summary: String,
    #[serde(default)]
    pub detail: Option<String>,
    /// `"normal"` | `"dangerous"`.
    pub risk: String,
}

/// What a suspended interactive turn is waiting on (either field may be null).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Interactions {
    #[serde(default)]
    pub approval: Option<PendingApproval>,
    #[serde(default)]
    pub question: Option<String>,
}

impl Interactions {
    pub fn is_empty(&self) -> bool {
        self.approval.is_none() && self.question.is_none()
    }
}

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    key: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// Discover a running gateway via `~/.komo/gateway.json` and probe it. `None`
    /// = no gateway advertised, or the advertised one didn't answer `/health`
    /// (a stale file from a crashed gateway).
    pub async fn connect() -> Option<ApiClient> {
        Self::from_info(rendezvous::read()?).await
    }

    async fn from_info(info: GatewayInfo) -> Option<ApiClient> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let base = info.base_url();
        Self::health_ok(&http, &base).await.then_some(ApiClient {
            base,
            key: info.key,
            http,
        })
    }

    async fn health_ok(http: &reqwest::Client, base: &str) -> bool {
        http.get(format!("{base}/health"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Re-probe the connected gateway's `/health` (used to detect it going away).
    pub async fn is_healthy(&self) -> bool {
        Self::health_ok(&self.http, &self.base).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// GET a `{ "<key>": T }` envelope and pull out the inner value.
    async fn get_field<T: DeserializeOwned>(&self, path: &str, key: &str) -> Result<T> {
        let mut map: Map<String, Value> = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let v = map
            .remove(key)
            .with_context(|| format!("response missing `{key}` field"))?;
        Ok(serde_json::from_value(v)?)
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Map<String, Value>> {
        Ok(self
            .http
            .post(self.url(path))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    // ---- dashboard reads ---------------------------------------------------

    pub async fn status(&self) -> Result<StatusSnapshot> {
        Ok(self
            .http
            .get(self.url("/api/status"))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn tasks(&self) -> Result<Vec<Task>> {
        self.get_field("/api/tasks", "tasks").await
    }

    pub async fn memories(&self, status: Option<&str>) -> Result<Vec<Memory>> {
        let path = match status {
            Some(s) => format!("/api/memories?status={s}"),
            None => "/api/memories".to_string(),
        };
        self.get_field(&path, "memories").await
    }

    pub async fn runs(&self, limit: usize) -> Result<Vec<Run>> {
        self.get_field(&format!("/api/runs?limit={limit}"), "runs")
            .await
    }

    /// One run with its steps; `None` if the gateway 404s (no such run).
    pub async fn run(&self, id: &str) -> Result<Option<(Run, Vec<RunStep>)>> {
        let resp = self
            .http
            .get(self.url(&format!("/api/runs/{id}")))
            .bearer_auth(&self.key)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut map: Map<String, Value> = resp.error_for_status()?.json().await?;
        let run: Run =
            serde_json::from_value(map.remove("run").context("run response missing `run`")?)?;
        let steps: Vec<RunStep> = match map.remove("steps") {
            Some(v) => serde_json::from_value(v)?,
            None => Vec::new(),
        };
        Ok(Some((run, steps)))
    }

    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        self.get_field("/api/sessions", "sessions").await
    }

    pub async fn session_messages(&self, id: &str) -> Result<Vec<Message>> {
        self.get_field(&format!("/api/sessions/{id}/messages"), "messages")
            .await
    }

    pub async fn reminders(&self) -> Result<Vec<Reminder>> {
        self.get_field("/api/reminders", "reminders").await
    }

    pub async fn skills(&self) -> Result<Vec<Skill>> {
        self.get_field("/api/skills", "skills").await
    }

    pub async fn skill_audit(&self, name: &str) -> Result<Vec<SkillInvocation>> {
        self.get_field(&format!("/api/skills/{name}/audit"), "invocations")
            .await
    }

    pub async fn pairings(&self) -> Result<Vec<PairingView>> {
        self.get_field("/api/pairings", "pairings").await
    }

    /// The dreaming dry-run: (would-promote, would-archive).
    pub async fn dream_preview(&self) -> Result<(Vec<DreamItem>, Vec<DreamItem>)> {
        let mut map: Map<String, Value> = self
            .http
            .get(self.url("/api/dream"))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let take = |m: &mut Map<String, Value>, k: &str| -> Result<Vec<DreamItem>> {
            match m.remove(k) {
                Some(v) => Ok(serde_json::from_value(v)?),
                None => Ok(Vec::new()),
            }
        };
        let promote = take(&mut map, "promote")?;
        let archive = take(&mut map, "archive")?;
        Ok((promote, archive))
    }

    /// What a suspended interactive turn on `session` is waiting on.
    pub async fn interactions(&self, session: &str) -> Result<Interactions> {
        Ok(self
            .http
            .get(self.url(&format!("/api/interactions/{session}")))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    // ---- operator writes (loopback-gated server-side) ----------------------

    pub async fn memory_transition(&self, id: &str, action: &str) -> Result<()> {
        self.post_json(&format!("/api/memories/{id}/{action}"), json!({}))
            .await?;
        Ok(())
    }

    pub async fn prune_runs(&self, cutoff: i64) -> Result<usize> {
        let map = self
            .post_json(&format!("/api/runs/prune?cutoff={cutoff}"), json!({}))
            .await?;
        Ok(map.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize)
    }

    pub async fn clean_sessions(&self) -> Result<usize> {
        let map = self.post_json("/api/sessions/clean", json!({})).await?;
        Ok(map.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize)
    }

    pub async fn pair_approve(&self, code: &str) -> Result<()> {
        self.post_json("/api/pairings/approve", json!({ "code": code }))
            .await?;
        Ok(())
    }

    pub async fn pair_revoke(&self, id: &str) -> Result<bool> {
        let map = self
            .post_json(&format!("/api/pairings/{id}/revoke"), json!({}))
            .await?;
        Ok(map.get("revoked").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Run one dreaming cycle: (promoted, archived).
    pub async fn dream_apply(&self) -> Result<(usize, usize)> {
        let map = self.post_json("/api/dream/apply", json!({})).await?;
        let n = |k: &str| map.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        Ok((n("promoted"), n("archived")))
    }

    pub async fn resume_run(&self, id: &str) -> Result<ResumeOutcome> {
        let map = self
            .post_json(&format!("/api/runs/{id}/resume"), json!({}))
            .await?;
        Ok(serde_json::from_value(Value::Object(map))?)
    }

    /// Resolve a pending approval for `session`. `decision` = once|session|deny.
    pub async fn resolve_approval(&self, session: &str, decision: &str) -> Result<bool> {
        let map = self
            .post_json(
                &format!("/api/interactions/{session}/approval"),
                json!({ "decision": decision }),
            )
            .await?;
        Ok(map
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// Answer a pending clarify question for `session`.
    pub async fn answer_question(&self, session: &str, text: &str) -> Result<bool> {
        let map = self
            .post_json(
                &format!("/api/interactions/{session}/answer"),
                json!({ "text": text }),
            )
            .await?;
        Ok(map
            .get("resolved")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    // ---- chat --------------------------------------------------------------

    /// Run one chat turn on `session`, returning the assistant's full reply.
    pub async fn chat(&self, session: &str, message: &str, mode: ChatMode) -> Result<String> {
        let body = json!({
            "model": "komo",
            "stream": false,
            "messages": [{ "role": "user", "content": message }],
        });
        let mut req = self
            .http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.key)
            .header("X-Komo-Session-Id", session);
        req = match mode {
            ChatMode::Interactive => req.header("X-Komo-Interactive", "1"),
            ChatMode::Trusted => req.header("X-Komo-Trusted", "1"),
        };
        let v: Value = req
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}
