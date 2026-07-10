//! Transient-error retry classification (roadmap §7).

use crate::domain::tool::RetryHint;

/// Total attempts for a tool whose failure is judged retryable (1 initial +
/// retries). Kept a constant, not config: transient-error retry is an internal
/// robustness backstop, not a user tuning knob. Promote to config only when a
/// real consumer needs to vary it.
pub(super) const TOOL_RETRY_MAX_ATTEMPTS: usize = 3;
/// Backoff before each retry, indexed by the retry number (the first retry
/// waits the first entry, etc.); the last entry is reused beyond its length.
pub(super) const TOOL_RETRY_BACKOFF_MS: [u64; 2] = [250, 750];

/// How a failed tool call may be retried. Preferred path: a tool classifies its
/// own failure at the source via [`crate::domain::tool::TransientError`] (the
/// reqwest-backed tools do this in `tools::http`, where the typed
/// `reqwest::Error` / status is intact), and [`classify_error`] reads that hint
/// directly. Fallback path: for errors that carry no hint, classify from the
/// error *text* — a heuristic, since a flattened `anyhow!("…: {e}")` has
/// dropped the typed source. Deliberately conservative: an error matching
/// neither is [`Retry::No`] (never retried).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Retry {
    /// Don't retry — terminal (bad arguments, denied, blocked) or unknown.
    No,
    /// The request provably never reached the server (connection refused, DNS
    /// failure). Safe to retry for *any* tool — no side effect can have landed.
    ConnLevel,
    /// Landed-or-not is ambiguous (timeout, 5xx, rate-limit). Retry only an
    /// idempotent tool, so a side effect is never applied twice.
    Ambiguous,
}

/// Markers that mean the connection never established — the request did not
/// reach the server, so retrying cannot double-apply a side effect.
const CONN_LEVEL_MARKERS: &[&str] = &[
    "connection refused",
    "dns error",
    "failed to lookup address",
    "name resolution",
    "could not resolve",
    "no such host",
];
/// Markers whose side-effect status is ambiguous — the request may have landed
/// and applied before the failure surfaced. Retried for idempotent tools only.
const AMBIGUOUS_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "error sending request",
    "connection reset",
    "broken pipe",
    "temporarily unavailable",
    "http 502",
    "http 503",
    "http 504",
    "http 429",
    "502 bad gateway",
    "503 service",
    "504 gateway",
    "429 too many",
];

pub(super) fn classify_error(err: &anyhow::Error) -> Retry {
    // Typed hint first: lossless, set where the failure arose. anyhow walks the
    // chain, so a `.context(...)`-wrapped `TransientError` is still found.
    if let Some(te) = err.downcast_ref::<crate::domain::tool::TransientError>() {
        return match te.hint {
            RetryHint::Connection => Retry::ConnLevel,
            RetryHint::Ambiguous => Retry::Ambiguous,
        };
    }
    // Fallback heuristic for errors that didn't classify themselves.
    let msg = format!("{err:#}").to_lowercase();
    if CONN_LEVEL_MARKERS.iter().any(|m| msg.contains(m)) {
        Retry::ConnLevel
    } else if AMBIGUOUS_MARKERS.iter().any(|m| msg.contains(m)) {
        Retry::Ambiguous
    } else {
        Retry::No
    }
}

/// Whether a failed call should be retried, given the tool's idempotency.
pub(super) fn should_retry(err: &anyhow::Error, idempotent: bool) -> bool {
    match classify_error(err) {
        Retry::No => false,
        Retry::ConnLevel => true,
        Retry::Ambiguous => idempotent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_buckets_by_marker() {
        // Connection-level wins even when an ambiguous word is also present.
        assert_eq!(
            classify_error(&anyhow::anyhow!(
                "request failed: error sending request: connection refused"
            )),
            Retry::ConnLevel
        );
        assert_eq!(
            classify_error(&anyhow::anyhow!("Home Assistant returned HTTP 503: down")),
            Retry::Ambiguous
        );
        assert_eq!(
            classify_error(&anyhow::anyhow!("operation timed out")),
            Retry::Ambiguous
        );
        // Unknown / terminal errors are never retried.
        assert_eq!(
            classify_error(&anyhow::anyhow!("invalid arguments: bad json")),
            Retry::No
        );
    }

    #[test]
    fn classify_error_prefers_typed_hint_over_text() {
        use crate::domain::tool::TransientError;
        use anyhow::Context as _;
        // A typed hint wins regardless of what the message text would match —
        // here the text says "invalid" (would be Retry::No via the heuristic).
        let conn = anyhow::Error::new(TransientError::new(
            RetryHint::Connection,
            "invalid: but typed as connection-level",
        ));
        assert_eq!(classify_error(&conn), Retry::ConnLevel);

        let amb = anyhow::Error::new(TransientError::new(RetryHint::Ambiguous, "anything"));
        assert_eq!(classify_error(&amb), Retry::Ambiguous);

        // The hint is still found through an added `.context(...)` layer.
        let wrapped = Err::<(), _>(anyhow::Error::new(TransientError::new(
            RetryHint::Connection,
            "boom",
        )))
        .context("while fetching")
        .unwrap_err();
        assert_eq!(classify_error(&wrapped), Retry::ConnLevel);
    }
}
