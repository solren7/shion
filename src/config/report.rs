//! Redacted configuration diagnostics: load issues and value provenance.
//!
//! The report never holds secret values — only whether a credential is
//! present and where each resolved value came from. `komo doctor` and
//! `komo model list` render it; `ConfigSnapshot::validate_gateway` fails
//! startup on its fatal issues.

use super::Provider;

/// Where a resolved value came from (lowest → highest priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Built-in default (including per-provider model defaults).
    Default,
    /// `~/.komo/config.toml`.
    File,
    /// A `KOMO_*` environment variable.
    Env,
}

/// How bad a [`ConfigIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    /// The gateway (or an agent turn) cannot start with this problem:
    /// malformed `KOMO_*` values, an unusable model selection, an enabled
    /// channel missing its credential.
    Fatal,
    /// Worth surfacing in `doctor` but safe to run with.
    Warning,
}

/// One problem found while resolving configuration. Resolution never aborts
/// on the first problem — every issue is collected so `doctor` can show the
/// complete picture.
#[derive(Debug, Clone)]
pub struct ConfigIssue {
    /// Dotted config path the issue is about (e.g. `channels.telegram`).
    pub path: &'static str,
    pub severity: IssueSeverity,
    pub message: String,
}

/// Redacted diagnostics accompanying a [`super::ConfigSnapshot`].
#[derive(Debug)]
pub struct ConfigReport {
    /// Every problem found during resolution, in resolution order.
    pub issues: Vec<ConfigIssue>,
    /// Where the active provider came from.
    pub provider_origin: Origin,
    /// Where the active model id came from.
    pub model_origin: Origin,
    /// Env-key **presence** per provider (never the key itself). Codex reports
    /// `false` here — its OAuth file (`~/.codex/auth.json`) is a filesystem
    /// check the caller performs, not part of pure config resolution.
    pub provider_key_present: Vec<(Provider, bool)>,
}

impl ConfigReport {
    /// The first fatal issue, if any.
    pub fn fatal(&self) -> Option<&ConfigIssue> {
        self.fatal_matching(|_| true)
    }

    /// The first fatal issue a caller cares about (e.g. only model/env issues
    /// for an agent turn, everything for gateway startup).
    pub fn fatal_matching(&self, relevant: impl Fn(&ConfigIssue) -> bool) -> Option<&ConfigIssue> {
        self.issues
            .iter()
            .find(|i| i.severity == IssueSeverity::Fatal && relevant(i))
    }

    /// Whether `provider`'s env API key is present (always `false` for Codex —
    /// see [`ConfigReport::provider_key_present`]).
    pub fn key_present(&self, provider: Provider) -> bool {
        self.provider_key_present
            .iter()
            .any(|(p, present)| *p == provider && *present)
    }
}
