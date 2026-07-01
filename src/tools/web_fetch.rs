use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::domain::tool::Tool;

const MAX_BYTES: usize = 8 * 1024;
const USER_AGENT: &str = "shion-agent/0.1";

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

/// Fetches a URL and returns its readable text content (HTML stripped).
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
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
        "Fetch a web page by URL and return its readable text content."
    }

    /// Read-only GET: safe to retry on an ambiguous transient failure.
    fn idempotent(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The absolute URL to fetch." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: String) -> anyhow::Result<String> {
        let args: FetchArgs = serde_json::from_str(&input)
            .map_err(|e| anyhow::anyhow!("invalid web_fetch arguments: {e}"))?;

        let resp = self
            .client
            .get(&args.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| {
                crate::tools::http::transport_error(e, format!("request to {} failed", args.url))
            })?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| crate::tools::http::transport_error(e, "failed to read body"))?;

        let mut text = strip_html(&body);
        truncate_to_char_boundary(&mut text, MAX_BYTES);
        Ok(format!("HTTP {status}\n\n{text}"))
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
