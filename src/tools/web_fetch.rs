use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{
    approval::{ActionRef, ApprovalRequest},
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
};

const MAX_BYTES: usize = 8 * 1024;
const USER_AGENT: &str = "komo-agent/0.1";

/// Per-request timeout for the fetch client. `reqwest`'s default client sets no
/// timeout at all, so a server that accepts the connection then never responds
/// would hang the call until the executor's outer wall-clock backstop fired.
/// This inner timeout fails faster and, being a proper request timeout, is
/// classified transient — an idempotent GET is retried once or twice.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Cap on caller-supplied request headers — enough for auth + content
/// negotiation, far below anything abusive.
const MAX_HEADERS: usize = 8;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    /// Optional request headers — the door to authenticated JSON APIs
    /// (`X-Auth-Token` for Miniflux, `Authorization: Bearer …`), which is what
    /// lets data-source skills work through this one read-only tool.
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
}

/// Fetches a URL and returns its readable text content (HTML stripped).
///
/// A GET is read-only (`Risk::Safe`), but it is still an outbound request to an
/// arbitrary URL — untrusted page content can steer the model into fetching an
/// attacker's host with sensitive query params. So the fetch consults the
/// approver with an [`ActionRef::Network`] before sending: the policy layer's
/// deny rules can blackhole hosts (`category = "network"`), while an unmatched
/// URL proceeds without any prompt (safe actions never escalate).
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        // A timeout-less client can hang indefinitely on an unresponsive server;
        // fall back to the default client only if the builder somehow fails.
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL (GET) and return its readable text content. Optional request \
         headers support authenticated JSON APIs (e.g. an X-Auth-Token)."
    }

    /// Read-only GET: safe to retry on an ambiguous transient failure.
    fn idempotent(&self) -> bool {
        true
    }

    /// Header values are exactly credential-shaped (API tokens, bearer auth) —
    /// mask them before the args land in the run ledger. Header *names* stay,
    /// so an audit still shows what kind of auth was sent, just not the secret.
    fn redact_args(&self, args: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(args) {
            Ok(mut v) => {
                if let Some(headers) = v.get_mut("headers").and_then(|h| h.as_object_mut()) {
                    for (_, value) in headers.iter_mut() {
                        *value = serde_json::json!("<redacted>");
                    }
                }
                v.to_string()
            }
            Err(_) => "<web_fetch args redacted>".to_string(),
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The absolute URL to fetch." },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Optional request headers (e.g. {\"X-Auth-Token\": \"…\"} for an authenticated API)."
                }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs = parse_args(&input)?;
        if args.headers.len() > MAX_HEADERS {
            return Err(ToolError::InvalidInput(format!(
                "too many headers ({}, max {MAX_HEADERS})",
                args.headers.len()
            )));
        }

        let request =
            ApprovalRequest::safe(format!("fetch {}", args.url)).with_action(ActionRef::Network {
                url: args.url.clone(),
            });
        if !ctx.approve(&request).await {
            return Ok(ToolOutput::text(format!(
                "URL blocked by the permission policy (a `network` deny rule matches {}); \
                 nothing was fetched.",
                args.url
            )));
        }

        let mut request = self
            .client
            .get(&args.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (name, value) in &args.headers {
            request = request.header(name, value);
        }
        let resp = request.send().await.map_err(|e| {
            crate::tools::http::transport_error(e, format!("request to {} failed", args.url))
        })?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| crate::tools::http::transport_error(e, "failed to read body"))?;

        let mut text = strip_html(&body);
        truncate_to_char_boundary(&mut text, MAX_BYTES);
        Ok(ToolOutput::text(format!("HTTP {status}\n\n{text}")))
    }
}

/// Truncates to at most `max_bytes`, backing up so the cut never splits a
/// multi-byte UTF-8 character (String::truncate panics off-boundary).
fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n…[truncated]");
}

/// Crude HTML-to-text: drop script/style blocks, remove tags, collapse blanks.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_lowercase();

    let mut i = 0;
    while i < html.len() {
        // Skip <script>...</script> and <style>...</style> wholesale.
        let mut skipped = false;
        for (tag, end) in [("<script", "</script>"), ("<style", "</style>")] {
            if lower[i..].starts_with(tag) {
                i = match lower[i..].find(end) {
                    Some(rel) => i + rel + end.len(),
                    None => html.len(),
                };
                skipped = true;
                break;
            }
        }
        if skipped {
            continue;
        }

        let rest = &html[i..];
        if rest.starts_with('<') {
            match rest.find('>') {
                Some(close) => {
                    i += close + 1;
                    continue;
                }
                None => break,
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    // Collapse runs of whitespace/blank lines.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::approval::Approver;
    use crate::domain::context::{SessionContext, ToolContext};
    use std::sync::Arc;

    struct DenyAll;
    #[async_trait]
    impl Approver for DenyAll {
        async fn approve(&self, _request: &ApprovalRequest) -> bool {
            false
        }
    }

    fn deny_ctx() -> ToolContext {
        ToolContext::new(
            SessionContext::detached("cli:test"),
            None,
            Arc::new(DenyAll),
        )
    }

    #[tokio::test]
    async fn denied_fetch_reports_the_block_and_sends_nothing() {
        let tool = WebFetchTool::new();
        let out = tool
            .call(
                json!({ "url": "https://blocked.example.com/x" }),
                &deny_ctx(),
            )
            .await
            .unwrap();
        assert!(out.text.contains("blocked by the permission policy"));
        assert!(out.text.contains("blocked.example.com"));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_char() {
        // 3-byte CJK chars: MAX_BYTES (8192) is not a multiple of 3, so the
        // naive byte-offset truncate lands mid-codepoint and panics.
        let mut text = "深".repeat(MAX_BYTES / 3 + 10);
        truncate_to_char_boundary(&mut text, MAX_BYTES);
        assert!(text.ends_with("…[truncated]"));
        assert!(text.is_char_boundary(text.len() - "\n…[truncated]".len()));
    }

    #[test]
    fn truncation_leaves_short_text_untouched() {
        let mut text = "short".to_string();
        truncate_to_char_boundary(&mut text, MAX_BYTES);
        assert_eq!(text, "short");
    }

    #[test]
    fn redact_masks_header_values_but_keeps_names_and_url() {
        let tool = WebFetchTool::new();
        let args = json!({
            "url": "http://miniflux:8080/v1/entries",
            "headers": { "X-Auth-Token": "super-secret-token" }
        })
        .to_string();
        let redacted = tool.redact_args(&args);
        assert!(!redacted.contains("super-secret-token"));
        assert!(redacted.contains("X-Auth-Token"));
        assert!(redacted.contains("miniflux:8080"));
    }

    #[tokio::test]
    async fn too_many_headers_is_an_error() {
        let tool = WebFetchTool::new();
        let headers: std::collections::HashMap<String, String> = (0..9)
            .map(|i| (format!("H-{i}"), "v".to_string()))
            .collect();
        let err = tool
            .call(
                json!({ "url": "https://example.com", "headers": headers }),
                &deny_ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too many headers"));
    }

    #[test]
    fn strips_tags_scripts_and_styles() {
        let html = "<html><head><style>a{}</style></head><body><script>var x=1;</script>\
            <h1>Hello</h1><p>World &amp; more</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("var x"));
        assert!(!text.contains("a{}"));
    }
}
