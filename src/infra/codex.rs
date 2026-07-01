//! OpenAI Codex provider auth, borrowed from hermes-agent's `openai-codex`
//! OAuth path.
//!
//! Codex models are reached through the ChatGPT backend
//! (`https://chatgpt.com/backend-api/codex`, an OpenAI **Responses API**
//! surface), authenticated not with an API key but with the OAuth tokens the
//! official Codex CLI writes to `~/.codex/auth.json` (`$CODEX_HOME` honored).
//! We reuse that login wholesale: read the token set, decode the access-token
//! JWT to know when it is expiring, and refresh it against
//! `auth.openai.com/oauth/token` with the Codex CLI's pinned client id.
//!
//! Because the access token lives only a few hours and the gateway is a
//! long-running process, refresh can't happen once at startup. [`CodexAuth`]
//! resolves a fresh token on demand and [`CodexHttpClient`] — a `rig`
//! [`HttpClientExt`] backend — re-stamps the `Authorization` header on **every**
//! outgoing request, so a turn an hour into the process still authenticates.
//! Refreshed tokens are written back to `auth.json` so the Codex CLI and shion
//! stay in sync.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use bytes::Bytes;
use http::header::{AUTHORIZATION, HeaderValue};
use rig::http_client::{self, HttpClientExt, LazyBody, MultipartForm, Request, Response};
use rig::wasm_compat::WasmCompatSend;
use serde::Deserialize;
use tokio::sync::Mutex;

/// Codex CLI's OAuth client id (matches `codex-rs`), used for token refresh.
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OpenAI's OAuth token endpoint (refresh-token grant).
const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// ChatGPT-backed Codex inference endpoint (OpenAI Responses API surface).
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Refresh this many seconds before the access token's `exp`.
const REFRESH_SKEW_SECS: u64 = 120;
/// `originator` allow-listed by the Codex backend's Cloudflare layer; non-codex
/// originators from non-residential IPs are served a 403 challenge.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
/// `User-Agent` shaped like the upstream `codex-rs` CLI (beats SDK fingerprinting).
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.0.0 (shion)";

/// Path to the Codex CLI's shared credential file (`$CODEX_HOME/auth.json`,
/// defaulting to `~/.codex/auth.json`).
fn codex_auth_path() -> PathBuf {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("cannot determine home directory")
                .join(".codex")
        });
    home.join("auth.json")
}

/// The three fields we need out of the Codex token set.
#[derive(Clone)]
struct CodexTokens {
    access_token: String,
    refresh_token: String,
    /// ChatGPT account id (for the `ChatGPT-Account-ID` header). Read from the
    /// `tokens.account_id` field, falling back to the JWT's `chatgpt_account_id`
    /// claim.
    account_id: Option<String>,
}

/// `auth.json` shape — only the fields we read; everything else is preserved
/// verbatim on write-back via a raw [`serde_json::Value`].
#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decode a JWT's payload (middle segment) without verifying its signature — we
/// only inspect claims (`exp`, account id). Returns `None` for any malformed token.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Extract the `chatgpt_account_id` claim from a Codex access token.
fn account_id_from_jwt(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Whether `token` expires within `skew` seconds. A token whose `exp` can't be
/// read is treated as *not* expiring — we'd rather try it and let the wire
/// return 401 than refresh blindly on every request.
fn is_expiring(token: &str, skew: u64) -> bool {
    match jwt_claims(token).and_then(|c| c.get("exp").and_then(|e| e.as_u64())) {
        Some(exp) => exp <= now_secs() + skew,
        None => false,
    }
}

/// Read and validate the Codex token set from `path`.
fn read_tokens(path: &Path) -> anyhow::Result<CodexTokens> {
    let content = std::fs::read_to_string(path).with_context(|| {
        format!(
            "{} not found — run the Codex CLI (`codex`) to log in first",
            path.display()
        )
    })?;
    let file: AuthFile = serde_json::from_str(&content)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let tokens = file.tokens.ok_or_else(|| {
        anyhow!(
            "{} has no `tokens` block — re-run `codex` to log in",
            path.display()
        )
    })?;
    let access_token = tokens
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.access_token", path.display()))?;
    let refresh_token = tokens.refresh_token.unwrap_or_default();
    let account_id = tokens
        .account_id
        .filter(|s| !s.is_empty())
        .or_else(|| account_id_from_jwt(&access_token));
    Ok(CodexTokens {
        access_token,
        refresh_token,
        account_id,
    })
}

/// Write refreshed tokens back into `auth.json`, preserving every other field
/// (`auth_mode`, `OPENAI_API_KEY`, …) so the Codex CLI keeps working. Atomic
/// (temp file + rename), 0600. Best-effort — failure is logged by the caller.
fn write_back(path: &Path, tokens: &CodexTokens) -> anyhow::Result<()> {
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root.as_object_mut().expect("object ensured above");
    let entry = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(tobj) = entry.as_object_mut() {
        tobj.insert("access_token".into(), tokens.access_token.clone().into());
        tobj.insert("refresh_token".into(), tokens.refresh_token.clone().into());
        if let Some(acc) = &tokens.account_id {
            tobj.insert("account_id".into(), acc.clone().into());
        }
    }
    obj.insert(
        "last_refresh".into(),
        chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .into(),
    );

    let body = serde_json::to_string_pretty(&root)?;
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("auth.json"),
        std::process::id()
    ));
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolves and refreshes Codex OAuth credentials from `~/.codex/auth.json`.
/// Cheap to clone behind an `Arc`; shared by [`CodexHttpClient`].
pub struct CodexAuth {
    path: PathBuf,
    http: reqwest::Client,
    /// Stable account id, snapshotted at construction for the static
    /// `ChatGPT-Account-ID` header.
    account_id: Option<String>,
    /// Access token at construction, used to seed the rig client's api-key
    /// type-state (overwritten per request by [`CodexHttpClient`]).
    initial_access_token: String,
    /// Live token set; the lock serializes concurrent refreshes so the
    /// single-use refresh token is never spent twice in parallel.
    state: Mutex<CodexTokens>,
}

impl CodexAuth {
    /// An auth handle with no credentials, used only to satisfy the `Default`
    /// bound rig requires on the HTTP backend type. It is never the auth of a
    /// real client (those go through [`CodexAuth::load`]); if [`Self::resolve`]
    /// were ever called on it, it surfaces a clear error rather than a panic.
    fn placeholder() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::new(),
            http: reqwest::Client::new(),
            account_id: None,
            initial_access_token: String::new(),
            state: Mutex::new(CodexTokens {
                access_token: String::new(),
                refresh_token: String::new(),
                account_id: None,
            }),
        })
    }

    /// Load credentials from `$CODEX_HOME/auth.json` (default `~/.codex`).
    /// Errors if the file is absent or malformed — surfaced at startup so the
    /// user is told to run `codex` rather than hitting a 401 mid-turn.
    pub fn load() -> anyhow::Result<Arc<Self>> {
        let path = codex_auth_path();
        let tokens = read_tokens(&path)?;
        Ok(Arc::new(Self {
            account_id: tokens.account_id.clone(),
            initial_access_token: tokens.access_token.clone(),
            http: reqwest::Client::new(),
            path,
            state: Mutex::new(tokens),
        }))
    }

    /// The ChatGPT account id for the `ChatGPT-Account-ID` request header.
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    /// The access token captured at construction (seeds the rig client builder).
    pub fn initial_access_token(&self) -> &str {
        &self.initial_access_token
    }

    /// Return a non-expiring access token, refreshing in place if the current
    /// one is within [`REFRESH_SKEW_SECS`] of expiry. Adopts a newer token from
    /// the shared file first (the Codex CLI may have rotated it), and persists
    /// any refresh back to `auth.json`.
    pub async fn resolve(&self) -> anyhow::Result<String> {
        let mut guard = self.state.lock().await;
        if !is_expiring(&guard.access_token, REFRESH_SKEW_SECS) {
            return Ok(guard.access_token.clone());
        }

        // The Codex CLI (or another shion run) may have already refreshed the
        // shared file. Adopt it before spending our own (single-use) refresh
        // token: if it's fresh we're done, otherwise use its newer refresh token.
        if let Ok(fresh) = read_tokens(&self.path) {
            let was_fresh = !is_expiring(&fresh.access_token, REFRESH_SKEW_SECS);
            *guard = fresh;
            if was_fresh {
                return Ok(guard.access_token.clone());
            }
        }

        let refreshed = self
            .refresh(&guard.refresh_token)
            .await
            .context("refreshing Codex token (run `codex` to re-login if this persists)")?;
        *guard = refreshed;
        if let Err(e) = write_back(&self.path, &guard) {
            tracing::warn!("codex: could not persist refreshed tokens: {e}");
        }
        Ok(guard.access_token.clone())
    }

    /// Exchange a refresh token for a new access token at OpenAI's OAuth endpoint.
    async fn refresh(&self, refresh_token: &str) -> anyhow::Result<CodexTokens> {
        if refresh_token.is_empty() {
            bail!("Codex auth has no refresh_token — run `codex` to log in again");
        }
        // Codex refresh tokens are `rt.<n>.<base64url>` — all URL-safe chars, so
        // direct interpolation needs no percent-encoding. (reqwest's `.form()`
        // helper is compiled out by our `default-features = false` build.)
        let form = format!(
            "grant_type=refresh_token&refresh_token={refresh_token}&client_id={CODEX_OAUTH_CLIENT_ID}"
        );
        let resp = self
            .http
            .post(CODEX_OAUTH_TOKEN_URL)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/json")
            .body(form)
            .send()
            .await
            .context("Codex token endpoint request failed")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Codex token refresh failed ({status}): {body}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&body).context("Codex token refresh returned invalid JSON")?;
        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Codex token refresh response missing access_token"))?
            .to_string();
        // refresh_token may rotate; keep the old one if the response omits it.
        let refresh_token = json
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| refresh_token.to_string());
        let account_id = self
            .account_id
            .clone()
            .or_else(|| account_id_from_jwt(&access_token));
        Ok(CodexTokens {
            access_token,
            refresh_token,
            account_id,
        })
    }
}

/// Static headers (besides the per-request bearer) the Codex backend needs to
/// pass its Cloudflare layer. Applied once to the rig client's default headers.
pub fn codex_static_headers(account_id: Option<&str>) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(CODEX_USER_AGENT),
    );
    if let Some(acc) = account_id
        && let Ok(v) = HeaderValue::from_str(acc)
    {
        headers.insert("chatgpt-account-id", v);
    }
    headers
}

/// A `rig` HTTP backend that stamps a freshly-resolved Codex bearer token onto
/// every request before delegating to `reqwest`. This is what lets a
/// long-running process keep authenticating as the hourly access token rotates.
#[derive(Clone)]
pub struct CodexHttpClient {
    inner: reqwest::Client,
    auth: Arc<CodexAuth>,
}

impl CodexHttpClient {
    pub fn new(auth: Arc<CodexAuth>) -> Self {
        Self {
            inner: reqwest::Client::new(),
            auth,
        }
    }
}

// rig's `CompletionModel for ResponsesCompletionModel<H>` impl bounds `H: Default
// + Debug` (over-broad — neither is exercised on the completion path). We satisfy
// them without exposing the auth handle: `Default` yields a credential-less
// client that is never the backend of a real request.
impl Default for CodexHttpClient {
    fn default() -> Self {
        Self {
            inner: reqwest::Client::new(),
            auth: CodexAuth::placeholder(),
        }
    }
}

impl std::fmt::Debug for CodexHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexHttpClient").finish_non_exhaustive()
    }
}

/// Reshape a rig Responses request body for the ChatGPT Codex backend.
///
/// rig hardcodes `instructions: null` and carries the system prompt as a
/// `role:"system"` item inside `input` (it targets vanilla OpenAI, which accepts
/// either). The Codex backend instead *requires* a top-level `instructions`
/// field, like the Codex CLI sends. So we lift the first system message out of
/// `input` into `instructions`. Non-JSON or non-`/responses` bodies (no `input`
/// key) pass through untouched.
fn adapt_codex_body(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let Some(obj) = value.as_object_mut() else {
        return body;
    };
    if !obj.contains_key("input") {
        return body;
    }
    // The Codex backend refuses server-side response storage; rig never sends
    // `store`, so the backend's `true` default trips a 400. Pin it off.
    obj.insert("store".into(), serde_json::Value::Bool(false));
    let has_instructions = obj.get("instructions").is_some_and(|i| !i.is_null());
    if !has_instructions {
        let lifted = obj
            .get_mut("input")
            .and_then(|i| i.as_array_mut())
            .and_then(|input| {
                let pos = input
                    .iter()
                    .position(|it| it.get("role").and_then(|r| r.as_str()) == Some("system"))?;
                Some(system_item_text(&input.remove(pos)))
            })
            .filter(|t| !t.is_empty())
            // The backend rejects a missing/empty `instructions` outright; fall
            // back to a neutral line if the request somehow carried no preamble.
            .unwrap_or_else(|| "You are a helpful assistant.".to_string());
        obj.insert("instructions".into(), serde_json::Value::String(lifted));
    }
    serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
}

/// Concatenate the `input_text` chunks of a Responses `role:"system"` input item.
fn system_item_text(item: &serde_json::Value) -> String {
    item.get("content")
        .and_then(|c| c.as_array())
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Overwrite the `Authorization` header with `Bearer <token>`, marked sensitive.
fn set_bearer(headers: &mut http::HeaderMap, token: &str) -> http_client::Result<()> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(http_client::Error::InvalidHeaderValue)?;
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    Ok(())
}

fn auth_error(e: anyhow::Error) -> http_client::Error {
    http_client::Error::Instance(format!("{e:#}").into())
}

impl HttpClientExt for CodexHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>>
    + WasmCompatSend
    + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let inner = self.inner.clone();
        let auth = self.auth.clone();
        // Collapse the generic body to `Bytes` *before* the async block: the
        // returned future is `'static`, so it must not carry the unbounded `T`.
        let (parts, body) = req.into_parts();
        let body = adapt_codex_body(body.into());
        async move {
            let token = auth.resolve().await.map_err(auth_error)?;
            let mut parts = parts;
            set_bearer(&mut parts.headers, &token)?;
            inner
                .send::<Bytes, U>(Request::from_parts(parts, body))
                .await
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>>
    + WasmCompatSend
    + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let inner = self.inner.clone();
        let auth = self.auth.clone();
        // `MultipartForm` is a concrete `'static` type, so capturing it in the
        // `'static` future is fine (unlike the generic `T` in `send`).
        let (parts, body) = req.into_parts();
        async move {
            let token = auth.resolve().await.map_err(auth_error)?;
            let mut parts = parts;
            set_bearer(&mut parts.headers, &token)?;
            inner
                .send_multipart::<U>(Request::from_parts(parts, body))
                .await
        }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<http_client::StreamingResponse>>
    + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let inner = self.inner.clone();
        let auth = self.auth.clone();
        // Same `instructions` reshaping as `send` — collapse to `Bytes` so we can
        // rewrite the JSON, then hand a `Request<Bytes>` to the inner backend.
        let (parts, body) = req.into_parts();
        let body = adapt_codex_body(body.into());
        async move {
            let token = auth.resolve().await.map_err(auth_error)?;
            let mut parts = parts;
            set_bearer(&mut parts.headers, &token)?;
            let mut resp = inner
                .send_streaming::<Bytes>(Request::from_parts(parts, body))
                .await?;
            // The Codex backend streams a valid SSE body but omits the
            // `Content-Type` header, which rig's SSE reader requires. Stamp the
            // known-correct type so the stream is accepted.
            if !resp.headers().contains_key(http::header::CONTENT_TYPE) {
                resp.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
            }
            Ok(resp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT with the given payload (header.payload.sig shape).
    fn fake_jwt(payload: serde_json::Value) -> String {
        let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b64(b"{\"alg\":\"none\"}"),
            b64(payload.to_string().as_bytes()),
            "sig"
        )
    }

    #[test]
    fn jwt_exp_is_decoded() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs() + 3600 }));
        assert!(
            !is_expiring(&token, REFRESH_SKEW_SECS),
            "an hour out is not expiring"
        );
    }

    #[test]
    fn expired_token_is_flagged() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs().saturating_sub(10) }));
        assert!(is_expiring(&token, REFRESH_SKEW_SECS));
    }

    #[test]
    fn token_within_skew_is_expiring() {
        let token = fake_jwt(serde_json::json!({ "exp": now_secs() + 30 }));
        assert!(is_expiring(&token, REFRESH_SKEW_SECS), "30s < 120s skew");
    }

    #[test]
    fn unreadable_exp_is_not_expiring() {
        assert!(!is_expiring("not-a-jwt", REFRESH_SKEW_SECS));
        let token = fake_jwt(serde_json::json!({ "sub": "x" }));
        assert!(
            !is_expiring(&token, REFRESH_SKEW_SECS),
            "no exp claim → assume valid"
        );
    }

    #[test]
    fn account_id_is_pulled_from_claim() {
        let token = fake_jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-123" }
        }));
        assert_eq!(account_id_from_jwt(&token).as_deref(), Some("acc-123"));
    }

    #[test]
    fn read_tokens_parses_chatgpt_shape() {
        let dir = std::env::temp_dir().join(format!("shion_codex_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,
               "tokens":{"access_token":"at","refresh_token":"rt","account_id":"acc"},
               "last_refresh":"2026-06-14T00:00:00Z"}"#,
        )
        .unwrap();
        let t = read_tokens(&path).unwrap();
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token, "rt");
        assert_eq!(t.account_id.as_deref(), Some("acc"));
    }

    #[test]
    fn adapt_lifts_system_message_into_instructions() {
        let body = serde_json::json!({
            "model": "gpt-5.5",
            "input": [
                { "role": "system", "type": "message",
                  "content": [{ "type": "input_text", "text": "Be terse." }] },
                { "role": "user", "type": "message",
                  "content": [{ "type": "input_text", "text": "hi" }] }
            ]
        });
        let out = adapt_codex_body(Bytes::from(serde_json::to_vec(&body).unwrap()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["instructions"], "Be terse.");
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "system message lifted out of input");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn adapt_synthesizes_instructions_when_no_system_message() {
        let body = serde_json::json!({
            "model": "gpt-5.5",
            "input": [
                { "role": "user", "type": "message",
                  "content": [{ "type": "input_text", "text": "hi" }] }
            ]
        });
        let out = adapt_codex_body(Bytes::from(serde_json::to_vec(&body).unwrap()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["instructions"].as_str().is_some_and(|s| !s.is_empty()),
            "a non-empty instructions field is always present"
        );
    }

    #[test]
    fn adapt_leaves_non_responses_body_untouched() {
        let raw = Bytes::from_static(b"{\"grant_type\":\"refresh_token\"}");
        assert_eq!(adapt_codex_body(raw.clone()), raw);
        let not_json = Bytes::from_static(b"not json");
        assert_eq!(adapt_codex_body(not_json.clone()), not_json);
    }

    #[test]
    fn adapt_preserves_existing_instructions() {
        let body = serde_json::json!({
            "instructions": "keep me",
            "input": [{ "role": "system", "type": "message",
                        "content": [{ "type": "input_text", "text": "nope" }] }]
        });
        let out = adapt_codex_body(Bytes::from(serde_json::to_vec(&body).unwrap()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["instructions"], "keep me");
        assert_eq!(v["input"].as_array().unwrap().len(), 1, "input untouched");
    }

    #[test]
    fn write_back_preserves_other_fields() {
        let dir = std::env::temp_dir().join(format!("shion_codex_wb_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,
               "tokens":{"access_token":"old","refresh_token":"oldrt","account_id":"acc"}}"#,
        )
        .unwrap();
        write_back(
            &path,
            &CodexTokens {
                access_token: "new".into(),
                refresh_token: "newrt".into(),
                account_id: Some("acc".into()),
            },
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["auth_mode"], "chatgpt");
        assert_eq!(v["tokens"]["access_token"], "new");
        assert_eq!(v["tokens"]["refresh_token"], "newrt");
        assert!(v.get("last_refresh").is_some());
    }
}
