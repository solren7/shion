//! Configuration as one resolved snapshot.
//!
//! Raw sources (`~/.shion/config.toml`, `SHION_*` env vars, `.env` secrets) are
//! read once by [`sources::ConfigSources`] and resolved purely into a
//! [`ConfigSnapshot`]: the [`RuntimeConfig`] every caller consumes plus a
//! redacted [`ConfigReport`] of issues and provenance. Precedence (built-in
//! defaults < `config.toml` < `SHION_*`), credential-missing semantics, and
//! per-value defaults live in `resolved.rs` — callers never re-derive them.
//!
//! Resolution never aborts: problems are recorded as [`ConfigIssue`]s so
//! diagnostics (`shion doctor`) always see the whole picture, while startup
//! paths fail fast via [`ConfigSnapshot::validate_agent`] /
//! [`ConfigSnapshot::validate_gateway`].

pub mod report;
pub mod resolved;
pub mod sources;
mod write;

use std::path::PathBuf;

pub use report::{ConfigIssue, ConfigReport, IssueSeverity, Origin};
pub use resolved::{
    ApiConfig, ChannelState, DEFAULT_DREAM_SCHEDULE, DEFAULT_MAX_HISTORY_MESSAGES,
    DEFAULT_MAX_TOOL_RESULT_BYTES, DEFAULT_MAX_TURNS, FeishuConfig, HomeAssistantChannelConfig,
    HomeAssistantConfig, ModelConfig, PolicyReport, RuntimeConfig, TelegramConfig, WeChatConfig,
};
pub use sources::{ConfigSources, FileConfig, Secrets, ShionEnv};
pub use write::write_model_selection;

/// Supported LLM providers (all OpenAI-compatible or natively wired in `rig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    DeepSeek,
    OpenAi,
    Anthropic,
    OpenRouter,
    /// OpenAI Codex via the ChatGPT backend, authenticated with the Codex CLI's
    /// OAuth tokens (`~/.codex/auth.json`) rather than an API key. See
    /// `infra/codex.rs`.
    Codex,
}

impl Provider {
    /// Every supported provider, in display order.
    pub const ALL: [Provider; 5] = [
        Provider::DeepSeek,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::OpenRouter,
        Provider::Codex,
    ];

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "deepseek" | "ds" => Provider::DeepSeek,
            "openai" | "oai" | "gpt" => Provider::OpenAi,
            "anthropic" | "claude" => Provider::Anthropic,
            "openrouter" | "or" => Provider::OpenRouter,
            "codex" | "openai-codex" => Provider::Codex,
            other => anyhow::bail!(
                "unknown provider `{other}` \
                 (expected: deepseek | openai | anthropic | openrouter | codex)"
            ),
        })
    }

    /// Canonical lowercase name, as written into `config.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::OpenRouter => "openrouter",
            Provider::Codex => "codex",
        }
    }

    /// Default model id when `model` is unset.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek-chat",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Anthropic => "claude-3-5-sonnet-latest",
            Provider::OpenRouter => "deepseek/deepseek-chat",
            // A slug the ChatGPT Codex backend currently accepts (others seen:
            // gpt-5.4, gpt-5.4-mini). Account-/tier-dependent — override via
            // config.toml `model`; discover live at GET /codex/models.
            Provider::Codex => "gpt-5.5",
        }
    }

    /// Environment variable holding this provider's API key. Codex has none —
    /// it authenticates from `~/.codex/auth.json` (see [`Provider::uses_api_key`]).
    pub fn api_key_var(self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::Codex => "",
        }
    }

    /// Whether this provider authenticates with an environment API key.
    /// Codex is the exception: its credentials come from the Codex CLI's OAuth
    /// login, resolved at build time in `infra/codex.rs`.
    pub fn uses_api_key(self) -> bool {
        !matches!(self, Provider::Codex)
    }
}

/// One resolved view of everything shion is configured to do, plus the
/// redacted diagnostics that explain it. Load once per process (or construct
/// from explicit [`ConfigSources`] in tests) and pass it down — callers never
/// re-read `config.toml`, the env, or `.env`.
pub struct ConfigSnapshot {
    pub runtime: RuntimeConfig,
    pub report: ConfigReport,
}

impl ConfigSnapshot {
    /// Read all sources once (ensuring `~/.shion` exists) and resolve.
    /// Never fails — problems land in the report; validate before starting
    /// long-running work.
    pub fn load() -> Self {
        Self::from_sources(ConfigSources::load(ensure_shion_home()))
    }

    /// Pure resolution seam: tests provide sources directly instead of
    /// mutating the real process environment or filesystem.
    pub fn from_sources(sources: ConfigSources) -> Self {
        let (runtime, report) = resolved::resolve(sources);
        Self { runtime, report }
    }

    /// Fail on the issues that make an agent turn impossible: a malformed
    /// `SHION_*` env or an unusable model selection. Channel problems don't
    /// block a chat turn — the gateway checks those via [`Self::validate_gateway`].
    pub fn validate_agent(&self) -> anyhow::Result<()> {
        self.first_fatal(|issue| issue.path == "env" || issue.path.starts_with("model"))
    }

    /// Fail on *any* fatal issue — the gateway hosts every surface, so an
    /// enabled-but-misconfigured channel must stop startup, matching the old
    /// per-resolver fail-fast behavior.
    pub fn validate_gateway(&self) -> anyhow::Result<()> {
        self.first_fatal(|_| true)
    }

    fn first_fatal(&self, relevant: impl Fn(&ConfigIssue) -> bool) -> anyhow::Result<()> {
        match self
            .report
            .issues
            .iter()
            .find(|i| i.severity == IssueSeverity::Fatal && relevant(i))
        {
            Some(issue) => Err(anyhow::anyhow!("{}", issue.message)),
            None => Ok(()),
        }
    }
}

/// Returns the `~/.shion` config directory. Overridable via `SHION_HOME`.
///
/// Read directly (not via `ShionEnv`): this is the bootstrap variable that
/// decides where `~/.shion/.env` lives, so it must work before dotenvy has
/// loaded that file.
pub fn shion_home() -> PathBuf {
    std::env::var("SHION_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("cannot determine home directory")
                .join(".shion")
        })
}

/// Ensure `~/.shion/` exists (0700) and return its path.
/// Tightens `.env` inside to 0600 if present.
/// Permission failures are silently ignored (containers, Windows).
///
/// Permissions are only applied when they are actually wrong: the home dir is
/// chmod'd solely on the run that creates it, and `.env` only when its mode
/// differs from 0600. Re-chmod'ing an existing path on every startup rewrites
/// the ACL on filesystems that keep one (ZFS/NFSv4 — a mounted TrueNAS
/// dataset), which would clobber operator-set ACLs on each gateway restart.
pub fn ensure_shion_home() -> PathBuf {
    let home = shion_home();
    let newly_created = !home.exists();
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("shion: could not create {}: {e}", home.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if newly_created {
            let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
        }
        let env_path = home.join(".env");
        if let Ok(meta) = std::fs::metadata(&env_path) {
            if meta.permissions().mode() & 0o777 != 0o600 {
                let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    home
}

/// `turso:<shion_home>/<file>` (Turso engine). Creates the config directory on
/// first use so the engine can create the database file.
fn db_url(file: &str) -> String {
    format!("turso:{}", ensure_shion_home().join(file).display())
}

/// Default database URL: `turso:<shion_home>/shion.db` (Turso engine).
/// Holds disposable state (sessions, messages, session todos, pairings,
/// reminders, skills, settings) — deletable to reset.
pub fn default_db_url() -> String {
    db_url("shion.db")
}

/// Kanban database URL: `turso:<shion_home>/kanban.db`. Durable cross-session
/// tasks live here, separate from `shion.db` so resetting the latter never
/// wipes real task data.
pub fn default_kanban_db_url() -> String {
    db_url("kanban.db")
}

/// Memory database URL: `turso:<shion_home>/memory.db`. Durable long-term
/// memories live here, separate from `shion.db` (like `kanban.db`) so resetting
/// the session db never wipes real personal data.
pub fn default_memory_db_url() -> String {
    db_url("memory.db")
}

/// Directory holding the cached Chinese workday calendar, one `{year}.json` per
/// year: `<shion_home>/workdays/`. Disposable — delete a file to force a
/// re-fetch from the holiday API.
pub fn workday_cache_dir() -> PathBuf {
    shion_home().join("workdays")
}

/// Where the WeChat QR-login credentials are stored. Shared by the gateway
/// channel and the `shion channel wechat login` provisioning command.
pub fn wechat_cred_path() -> PathBuf {
    shion_home().join("wechat").join("credentials.json")
}

// ---------------------------------------------------------------------------
// Legacy per-field resolvers. Thin shims over one ConfigSnapshot load, kept
// only while callers migrate to consuming the snapshot directly (architecture
// deepening plan, Phase A). Do not add callers.
// ---------------------------------------------------------------------------

/// Maintenance cron schedule: `SHION_SCHEDULE` env > config.toml `schedule`
/// > hourly default.
pub fn maintenance_schedule() -> String {
    ConfigSnapshot::load().runtime.maintenance_schedule
}

/// Daily-briefing cron schedule: `SHION_BRIEFING_SCHEDULE` env >
/// config.toml `briefing_schedule`. Opt-in (no default).
pub fn briefing_schedule() -> Option<String> {
    ConfigSnapshot::load().runtime.briefing_schedule
}

/// Whether the daily briefing should only fire on Chinese working days.
pub fn briefing_workdays_only() -> bool {
    ConfigSnapshot::load().runtime.briefing_workdays_only
}

/// Dreaming-sweep cron schedule; `None` means disabled.
pub fn dream_schedule() -> Option<String> {
    ConfigSnapshot::load().runtime.dream_schedule
}

/// Resolve the permission policy from `~/.shion/config.toml`.
pub fn policy_config() -> crate::domain::policy::Policy {
    ConfigSnapshot::load().runtime.policy.policy
}

/// [`policy_config`] with load diagnostics.
pub fn policy_report() -> PolicyReport {
    ConfigSnapshot::load().runtime.policy
}

/// Resolve the Home Assistant tool config from the environment.
pub fn homeassistant_config() -> Option<HomeAssistantConfig> {
    ConfigSnapshot::load().runtime.homeassistant_tool
}

/// Resolve the Home Assistant ingress channel.
pub fn homeassistant_channel_config() -> anyhow::Result<Option<HomeAssistantChannelConfig>> {
    ConfigSnapshot::load()
        .runtime
        .homeassistant_channel
        .into_result()
}

/// Resolve the Feishu channel config.
pub fn feishu_config() -> anyhow::Result<Option<FeishuConfig>> {
    ConfigSnapshot::load().runtime.feishu.into_result()
}

/// Resolve the Telegram channel config.
pub fn telegram_config() -> anyhow::Result<Option<TelegramConfig>> {
    ConfigSnapshot::load().runtime.telegram.into_result()
}

/// Resolve the WeChat channel config.
pub fn wechat_config() -> anyhow::Result<Option<WeChatConfig>> {
    ConfigSnapshot::load().runtime.wechat.into_result()
}

/// Resolve the (always-on) HTTP API channel config.
pub fn api_config() -> anyhow::Result<ApiConfig> {
    match ConfigSnapshot::load().runtime.api {
        ChannelState::Ready(cfg) => Ok(cfg),
        ChannelState::Misconfigured(msg) => Err(anyhow::anyhow!(msg)),
        ChannelState::Disabled => unreachable!("the api channel is always on"),
    }
}

impl ModelConfig {
    /// Resolve configuration with three-layer priority (lowest → highest):
    /// built-in defaults < `~/.shion/config.toml` < `SHION_*` env vars.
    /// Errors on the issues that would make an agent turn impossible.
    pub fn resolve() -> anyhow::Result<Self> {
        let snapshot = ConfigSnapshot::load();
        snapshot.validate_agent()?;
        Ok(snapshot.runtime.model)
    }

    /// Backward-compatible alias for `resolve()`.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::resolve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shion_home_respects_env_override() {
        let dir = std::env::temp_dir().join("shion_config_test_home_override");
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: single-threaded test context; we restore immediately.
        unsafe { std::env::set_var("SHION_HOME", dir.to_str().unwrap()) };
        let home = shion_home();
        unsafe { std::env::remove_var("SHION_HOME") };
        assert_eq!(home, dir);
    }
}
