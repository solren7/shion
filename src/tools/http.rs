//! Shared error helpers for the reqwest-backed tools (`web_fetch`,
//! `web_search`, `homeassistant`). They classify a failure into a typed
//! [`RetryHint`] at the point the `reqwest::Error` / status code is still
//! intact, so the retry layer (`services::tool_execution`'s retry classifier) acts
//! on a lossless signal instead of sniffing the error's Display string.

use std::fmt::Display;

use crate::domain::tool::{RetryHint, TransientError};

/// Classify a transport-level `reqwest::Error`. `None` = not transient (a
/// terminal error such as an invalid URL or a body-decode failure — never
/// retried).
fn classify(e: &reqwest::Error) -> Option<RetryHint> {
    if e.is_connect() {
        // TCP/TLS connect (incl. DNS) failed — the request never reached the
        // server, so retrying cannot double-apply a side effect.
        Some(RetryHint::Connection)
    } else if e.is_timeout() || e.is_request() {
        // Timed out, or the send failed mid-flight: it may or may not have
        // landed server-side, so only an idempotent tool should retry.
        Some(RetryHint::Ambiguous)
    } else {
        None
    }
}

/// Wrap a transport-level `reqwest::Error` into an `anyhow::Error`, tagging it
/// with a [`RetryHint`] when the failure is transient. `context` prefixes the
/// message (e.g. `"request to <url> failed"`).
pub fn transport_error(e: reqwest::Error, context: impl Display) -> anyhow::Error {
    let message = format!("{context}: {e}");
    match classify(&e) {
        Some(hint) => anyhow::Error::new(TransientError::new(hint, message)),
        None => anyhow::anyhow!("{message}"),
    }
}

/// Build an error for a non-success HTTP status, tagging 5xx and 429 as
/// [`RetryHint::Ambiguous`] (the server may recover) and every 4xx as terminal
/// (a bad request won't fix itself on retry). `context` names the endpoint.
pub fn status_error(
    status: reqwest::StatusCode,
    context: impl Display,
    body: impl Display,
) -> anyhow::Error {
    let message = format!("{context} returned HTTP {status}: {body}");
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        anyhow::Error::new(TransientError::new(RetryHint::Ambiguous, message))
    } else {
        anyhow::anyhow!("{message}")
    }
}
