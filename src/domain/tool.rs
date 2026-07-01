use async_trait::async_trait;

/// A failure's retry-safety, classified at its source (where the typed cause is
/// still intact — e.g. a `reqwest::Error`, before it is flattened to a string)
/// and carried on the error via [`TransientError`]. The retry layer
/// (`services::tool_registry`) reads this in preference to sniffing the error's
/// Display text. Mirrors the buckets that layer acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryHint {
    /// The request provably never reached the server (connection refused, DNS
    /// failure). Safe to retry for any tool — no side effect can have landed.
    Connection,
    /// Landed-or-not is ambiguous (timeout, 5xx, rate-limit). Retry only an
    /// idempotent tool, so a side effect is never applied twice.
    Ambiguous,
}

/// An error that classifies its own retry-safety via a [`RetryHint`]. A tool
/// builds one at the failure's source (see `tools::http`) so the retry layer
/// decides from a typed signal rather than a heuristic string match; anything
/// that doesn't classify itself falls back to that heuristic.
#[derive(Debug)]
pub struct TransientError {
    pub hint: RetryHint,
    pub message: String,
}

impl TransientError {
    pub fn new(hint: RetryHint, message: impl Into<String>) -> Self {
        Self {
            hint,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TransientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransientError {}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// JSON Schema describing this tool's arguments, exposed to the LLM for
    /// function calling. Defaults to "no arguments". Tools that take arguments
    /// override this and parse the matching JSON object from `execute`'s input.
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    /// Execute the tool. `input` carries the tool's arguments: a JSON object
    /// matching [`parameters_schema`](Tool::parameters_schema) when invoked by
    /// the LLM, or an empty string for argument-less tools.
    async fn execute(&self, input: String) -> anyhow::Result<String>;

    /// Whether `execute` is safe to retry after a transient failure whose
    /// side-effect status is *ambiguous* — a timeout or 5xx that may already
    /// have landed and applied server-side. Read-only tools (`web_fetch`,
    /// `web_search`) return `true`; any tool that can mutate external state
    /// keeps the default `false`, so a retry can never double-apply an effect
    /// (e.g. fire a Home Assistant service or run a shell command twice).
    ///
    /// Connection-level failures (the request provably never reached the
    /// server — connection refused, DNS failure) are retried regardless of
    /// this flag; see `services::tool_registry::execute_isolated`.
    fn idempotent(&self) -> bool {
        false
    }

    /// Sanitize the raw arguments before they are written to the run ledger
    /// (`services::tool_registry::execute_isolated`). The ledger stores tool
    /// args verbatim by default (this identity impl); tools carrying sensitive
    /// payloads override it so secrets/large bodies never land in `shion.db`.
    /// `shell` scrubs secret-looking substrings, `file` drops write bodies.
    fn redact_args(&self, args: &str) -> String {
        args.to_string()
    }
}
