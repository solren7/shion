//! HTTP client the `komo` CLI uses to reach a **running gateway**.
//!
//! Turso takes an exclusive cross-process lock on each db file, so while the
//! gateway runs the CLI can't open the db itself. Instead it talks to the
//! gateway's always-on loopback api channel (`infra/messaging/api.rs`), which
//! the gateway advertises in `~/.komo/gateway.json` (see `infra/rendezvous`).
//!
//! [`GatewayClient::try_connect`] is the single "is a gateway reachable?" check
//! every CLI command makes: `Some` → route over HTTP, `None` → open the db
//! directly (today's path). The read methods return the **domain types** the
//! endpoints serialize verbatim, so the existing CLI renderers are reused.

use std::time::Duration;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::domain::{
    memory::Memory,
    reminder::Reminder,
    run::{Run, RunStep},
    task::Task,
};
use crate::infra::rendezvous::{self, GatewayInfo};
use crate::services::operator_control::{
    DreamItem, PairingView, ResumeOutcome, SessionSummary, SkillInvocation,
};

/// How long to wait for the gateway to answer a request (a turn can take a
/// while — chat goes through the full agent loop server-side).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// The liveness probe must be quick: a stale rendezvous file (crashed gateway)
/// should fall back to the db fast, not hang the CLI.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct GatewayClient {
    base: String,
    key: String,
    http: reqwest::Client,
}

/// Result of a gateway-routed `pair approve`, mirroring the db path's
/// `ApproveOutcome` so the CLI prints the same message either way.
pub enum PairApprove {
    Approved(String),
    NotFound,
    Locked(i64),
}

impl GatewayClient {
    /// Reachable gateway → `Some`; no rendezvous file, unparseable, or the probe
    /// fails (stale file / crashed gateway) → `None` (caller falls back to db).
    pub async fn try_connect() -> Option<GatewayClient> {
        Self::from_info(rendezvous::read()?).await
    }

    /// Build a client for an advertised gateway and confirm it answers `/health`.
    /// Split out from [`try_connect`] so it is testable without a rendezvous file.
    async fn from_info(info: GatewayInfo) -> Option<GatewayClient> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let base = info.base_url();
        Self::health_ok(&http, &base).await.then(|| GatewayClient {
            base,
            key: info.key,
            http,
        })
    }

    /// One quick unauthenticated `/health` probe. Shared by [`from_info`] and
    /// `komo health` (the Docker HEALTHCHECK command).
    pub async fn health_ok(http: &reqwest::Client, base: &str) -> bool {
        http.get(format!("{base}/health"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// GET `path` and pull `key` out of the `{ "<key>": T }` envelope.
    async fn get_field<T: DeserializeOwned>(&self, path: &str, key: &str) -> anyhow::Result<T> {
        let mut map: Map<String, Value> = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let val = map
            .remove(key)
            .with_context(|| format!("gateway response missing `{key}`"))?;
        Ok(serde_json::from_value(val)?)
    }

    pub async fn memories(&self) -> anyhow::Result<Vec<Memory>> {
        self.get_field("/api/memories", "memories").await
    }

    pub async fn tasks(&self) -> anyhow::Result<Vec<Task>> {
        self.get_field("/api/tasks", "tasks").await
    }

    pub async fn runs(&self, limit: usize) -> anyhow::Result<Vec<Run>> {
        self.get_field(&format!("/api/runs?limit={limit}"), "runs")
            .await
    }

    /// One run with its steps; `None` if the gateway has no such run (404).
    pub async fn run(&self, id: &str) -> anyhow::Result<Option<(Run, Vec<RunStep>)>> {
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
        let run: Run = serde_json::from_value(
            map.remove("run")
                .context("gateway response missing `run`")?,
        )?;
        let steps: Vec<RunStep> =
            serde_json::from_value(map.remove("steps").unwrap_or_else(|| Value::Array(vec![])))?;
        Ok(Some((run, steps)))
    }

    /// Resume an interrupted run server-side: the gateway composes the priming
    /// input from its ledger, drives the turn (trusted — loopback + the marker
    /// header, same as `chat`), and clears the `recoverable` flag. 404 and 409
    /// come back as clear errors rather than raw HTTP failures.
    pub async fn resume(&self, id: &str) -> anyhow::Result<ResumeOutcome> {
        let resp = self
            .http
            .post(self.url(&format!("/api/runs/{id}/resume")))
            .bearer_auth(&self.key)
            .header("X-Komo-Trusted", "1")
            .send()
            .await?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => anyhow::bail!("no run with id `{id}`"),
            reqwest::StatusCode::CONFLICT => {
                let v: Value = resp.json().await.unwrap_or_default();
                let msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("run is not recoverable")
                    .to_string();
                anyhow::bail!(msg);
            }
            _ => {}
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<SessionSummary>> {
        self.get_field("/api/sessions", "sessions").await
    }

    pub async fn reminders(&self) -> anyhow::Result<Vec<Reminder>> {
        self.get_field("/api/reminders", "reminders").await
    }

    /// Which turns loaded a skill (derived from the run ledger server-side).
    pub async fn skill_audit(&self, name: &str) -> anyhow::Result<Vec<SkillInvocation>> {
        self.get_field(&format!("/api/skills/{name}/audit"), "invocations")
            .await
    }

    pub async fn pairings(&self) -> anyhow::Result<Vec<PairingView>> {
        self.get_field("/api/pairings", "pairings").await
    }

    /// The `/sethome` runtime override (`None` when unset). The config
    /// `home_chat` fallback is derived locally from the same config.toml.
    pub async fn home_override(&self) -> anyhow::Result<Option<String>> {
        self.get_field("/api/home", "override").await
    }

    /// The dreaming dry-run: `(promote, archive)` candidate lists.
    pub async fn dream_preview(&self) -> anyhow::Result<(Vec<DreamItem>, Vec<DreamItem>)> {
        let mut map: Map<String, Value> = self
            .http
            .get(self.url("/api/dream"))
            .bearer_auth(&self.key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let take = |map: &mut Map<String, Value>, k: &str| -> anyhow::Result<Vec<DreamItem>> {
            Ok(serde_json::from_value(
                map.remove(k).unwrap_or_else(|| Value::Array(vec![])),
            )?)
        };
        let promote = take(&mut map, "promote")?;
        let archive = take(&mut map, "archive")?;
        Ok((promote, archive))
    }

    /// Apply a memory governance transition (`promote` | `reject` | `pin`)
    /// through the gateway (which holds the db lock). The endpoint is
    /// loopback-gated server-side; a 404 becomes a clear "no such id" error.
    pub async fn memory_transition(&self, id: &str, action: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("/api/memories/{id}/{action}")))
            .bearer_auth(&self.key)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("no memory with id `{id}`");
        }
        resp.error_for_status()?;
        Ok(())
    }

    /// POST a loopback-gated control-plane write and return the JSON reply
    /// object. The one request path all the maintenance write routes share —
    /// auth, error mapping, and the version-skew case live here: a running
    /// gateway from before the endpoint existed answers 404, which would
    /// otherwise surface as an opaque reqwest error with no db fallback
    /// possible (the old gateway holds the lock), so it becomes an actionable
    /// "restart the gateway" message instead.
    async fn post_json(&self, path: &str, body: Value) -> anyhow::Result<Map<String, Value>> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "the running gateway doesn't serve `{path}` — it predates this command.\n\
                 Restart it onto the current binary (`komo gateway restart`) and retry."
            );
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    /// [`post_json`], pulling one field out of the `{ "<field>": T }` envelope.
    async fn post_field<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        field: &str,
    ) -> anyhow::Result<T> {
        let mut map = self.post_json(path, body).await?;
        let val = map
            .remove(field)
            .with_context(|| format!("gateway response missing `{field}`"))?;
        Ok(serde_json::from_value(val)?)
    }

    /// Prune runs started before `cutoff` server-side; returns the count removed.
    pub async fn prune_runs(&self, cutoff: i64) -> anyhow::Result<usize> {
        self.post_field(
            &format!("/api/runs/prune?cutoff={cutoff}"),
            json!({}),
            "removed",
        )
        .await
    }

    /// Delete empty sessions server-side; returns the count removed.
    pub async fn clean_sessions(&self) -> anyhow::Result<usize> {
        self.post_field("/api/sessions/clean", json!({}), "removed")
            .await
    }

    /// Approve a pending pairing by code server-side.
    pub async fn pair_approve(&self, code: &str) -> anyhow::Result<PairApprove> {
        let v = self
            .post_json("/api/pairings/approve", json!({ "code": code }))
            .await?;
        match v.get("outcome").and_then(|o| o.as_str()) {
            Some("approved") => Ok(PairApprove::Approved(
                v.get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string(),
            )),
            Some("locked") => Ok(PairApprove::Locked(
                v.get("retry_after_secs")
                    .and_then(|s| s.as_i64())
                    .unwrap_or(0),
            )),
            _ => Ok(PairApprove::NotFound),
        }
    }

    /// Revoke a pairing by id server-side; returns whether a row was removed.
    pub async fn pair_revoke(&self, id: &str) -> anyhow::Result<bool> {
        self.post_field(&format!("/api/pairings/{id}/revoke"), json!({}), "revoked")
            .await
    }

    /// Run one dreaming consolidation cycle server-side; returns
    /// `(promoted, archived)` counts.
    pub async fn dream_apply(&self) -> anyhow::Result<(usize, usize)> {
        let mut map = self.post_json("/api/dream/apply", json!({})).await?;
        let mut take = |k: &str| -> anyhow::Result<usize> {
            Ok(serde_json::from_value(
                map.remove(k).unwrap_or(Value::from(0)),
            )?)
        };
        let promoted = take("promoted")?;
        let archived = take("archived")?;
        Ok((promoted, archived))
    }

    /// Run one chat turn server-side and return the reply. Sends the stable
    /// session id (so history threads) and the trusted marker (so the gateway
    /// auto-approves side-effecting tools — it is gated to loopback callers).
    pub async fn chat(&self, session_id: &str, message: &str) -> anyhow::Result<String> {
        let body = json!({
            "model": "komo",
            "stream": false,
            "messages": [{ "role": "user", "content": message }],
        });
        let resp = self
            .http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.key)
            .header("X-Komo-Session-Id", session_id)
            .header("X-Komo-Trusted", "1")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        let reply = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_info_returns_none_when_nothing_listening() {
        // Port 1 is privileged and (essentially) never has a listener → the
        // health probe fails fast and we fall back to the db.
        let info = GatewayInfo {
            pid: 0,
            bind: "127.0.0.1".into(),
            port: 1,
            key: "k".into(),
        };
        assert!(GatewayClient::from_info(info).await.is_none());
    }
}
