//! Pure resolution: raw [`ConfigSources`] → one [`RuntimeConfig`] snapshot.
//!
//! Precedence is applied here exactly once — built-in defaults < `config.toml`
//! < `KOMO_*` env — and every problem found becomes a [`ConfigIssue`] instead
//! of an early error, so diagnostic consumers (`doctor`) always see the whole
//! picture while startup paths fail fast via `ConfigSnapshot::validate_*`.

use std::fmt;
use std::path::PathBuf;

use super::Provider;
use super::report::{ConfigIssue, ConfigReport, IssueSeverity, Origin};
use super::sources::{ConfigSources, KomoEnv, PolicyFileConfig, PolicyRuleFileConfig};

/// Built-in default for `max_turns` when neither `KOMO_MAX_TURNS` nor
/// config.toml sets one. Multi-file edits easily take 10+ round-trips.
pub const DEFAULT_MAX_TURNS: usize = 30;

/// Built-in per-completion timeout (seconds) when neither `KOMO_LLM_TIMEOUT_SECS`
/// nor config.toml sets one. A backstop so a hung provider request (rig's
/// default reqwest client sets no timeout) fails the turn cleanly instead of
/// wedging it in `running` forever — long enough for a slow tool-using
/// completion, short enough that a stalled request can't hang a turn all day.
pub const DEFAULT_LLM_TIMEOUT_SECS: u64 = 180;

/// Built-in default byte cap on a tool result handed back to the LLM, when
/// neither `KOMO_MAX_TOOL_RESULT_BYTES` nor config.toml sets one. Sized above
/// the per-tool self-caps (web_fetch / homeassistant trim to 8 KB) so it only
/// catches tools that don't self-trim.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Built-in default for `max_history_messages` when neither
/// `KOMO_MAX_HISTORY_MESSAGES` nor config.toml sets one. Counts prior messages
/// (user + assistant alternating, so ~25 turns), enough context for a chat
/// assistant while keeping a long-lived session's per-turn cost bounded. `0`
/// disables the window (replay the whole transcript, the pre-windowing behavior).
pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 50;

/// Built-in reviewer cadence: run the reflective reviewer every N user turns
/// when `KOMO_REVIEW_INTERVAL` doesn't set one.
pub const DEFAULT_REVIEW_INTERVAL: usize = 10;

/// Default maintenance cron when neither `KOMO_SCHEDULE` nor config.toml
/// `schedule` sets one: hourly.
pub const DEFAULT_MAINTENANCE_SCHEDULE: &str = "0 * * * *";

/// Default dreaming-sweep schedule: nightly at 3am, mirroring OpenClaw's
/// dreaming. Unlike the briefing (proactive notifications → opt-in), dreaming is
/// internal memory housekeeping with no user-facing output, so it is **on by
/// default**.
pub const DEFAULT_DREAM_SCHEDULE: &str = "0 3 * * *";

/// Default Home Assistant URL when `HASS_URL` is unset.
const DEFAULT_HASS_URL: &str = "http://homeassistant.local:8123";

/// Default loopback bind address for the HTTP API channel. Loopback-only by
/// default so the API isn't reachable off-host without an explicit override.
const DEFAULT_API_BIND: &str = "127.0.0.1";
/// Default API port (kept distinct from hermes' 8642 to avoid a same-host clash).
const DEFAULT_API_PORT: u16 = 8765;

/// Everything the running program needs, fully resolved. Callers consume this
/// (via `ConfigSnapshot`) instead of the raw file/env/secret sources.
///
/// No `Debug` impl on purpose: several fields carry credentials.
pub struct RuntimeConfig {
    /// The `~/.komo` home directory the snapshot was resolved against.
    pub home: PathBuf,
    /// `turso:` URL of the disposable session/state db (`state.db`).
    pub db_url: String,
    /// `turso:` URL of the durable task db (`kanban.db`).
    pub kanban_db_url: String,
    /// `turso:` URL of the durable memory db (`memory.db`).
    pub memory_db_url: String,
    /// Provider/model selection plus the agent-loop knobs that ride with it.
    pub model: ModelConfig,
    /// Reviewer cadence: run the reflective reviewer every N user turns.
    pub review_interval: usize,
    /// Maintenance sweep cron (5-field Unix).
    pub maintenance_schedule: String,
    /// Daily briefing cron; `None` = opt-in feature disabled.
    pub briefing_schedule: Option<String>,
    /// Gate the briefing to Chinese working days.
    pub briefing_workdays_only: bool,
    /// Dreaming sweep cron; `None` = explicitly disabled (default is on).
    pub dream_schedule: Option<String>,
    /// The permission policy plus its load diagnostics.
    pub policy: PolicyReport,
    /// Extra skill directories from `KOMO_SKILLS_PATH` (colon-separated),
    /// highest priority first.
    pub skills_path: Vec<PathBuf>,
    /// The `homeassistant` *tool* credentials (`HASS_TOKEN`/`HASS_URL`);
    /// `None` = token unset, tool not registered.
    pub homeassistant_tool: Option<HomeAssistantConfig>,
    pub feishu: ChannelState<FeishuConfig>,
    pub telegram: ChannelState<TelegramConfig>,
    pub wechat: ChannelState<WeChatConfig>,
    pub homeassistant_channel: ChannelState<HomeAssistantChannelConfig>,
    /// The HTTP api channel is always on (the CLI reaches a running gateway
    /// through it), so this is never `Disabled` — only `Ready` (loopback or
    /// external) or `Misconfigured` (external without a key).
    pub api: ChannelState<ApiConfig>,
}

/// One ingress channel's resolved state.
pub enum ChannelState<T> {
    /// Not declared, or declared with `enabled = false`.
    Disabled,
    /// Enabled and fully configured.
    Ready(T),
    /// Enabled but unusable; the message names what is missing. Resolution
    /// also records a [`ConfigIssue`] — fatal for the chat channels (so
    /// `validate_gateway` fails), a warning for homeassistant (the gateway
    /// boots and the channel stays offline).
    Misconfigured(String),
}

impl<T> ChannelState<T> {
    /// The config when the channel is ready to serve.
    pub fn ready(&self) -> Option<&T> {
        match self {
            ChannelState::Ready(cfg) => Some(cfg),
            _ => None,
        }
    }
}

/// Resolved model selection: provider, model id, API key, and optional overrides.
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
    /// Empty for Codex (OAuth via `~/.codex/auth.json`) and when the key is
    /// missing — the latter is recorded as a fatal issue.
    pub api_key: String,
    /// Optional base-URL override for OpenAI-compatible endpoints.
    pub base_url: Option<String>,
    /// Optional cheaper model for auxiliary sub-tasks.
    pub aux_model: Option<String>,
    /// Maximum tool-calling round-trips per user turn.
    pub max_turns: usize,
    /// Byte cap on a tool result handed back to the LLM (global backstop).
    pub max_tool_result_bytes: usize,
    /// Max prior messages replayed as history per turn (`0` = unlimited).
    pub max_history_messages: usize,
    /// Per-completion timeout in seconds — a hung provider request fails the
    /// turn cleanly rather than wedging it forever (`0` = no timeout).
    pub llm_timeout_secs: u64,
}

impl fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &mask_secret(&self.api_key))
            .field("base_url", &self.base_url)
            .field("aux_model", &self.aux_model)
            .field("max_turns", &self.max_turns)
            .field("max_tool_result_bytes", &self.max_tool_result_bytes)
            .field("max_history_messages", &self.max_history_messages)
            .field("llm_timeout_secs", &self.llm_timeout_secs)
            .finish()
    }
}

/// Show first 3 and last 4 chars; fully mask short keys.
fn mask_secret(s: &str) -> String {
    if s.len() <= 7 {
        return "***".to_string();
    }
    format!("{}...{}", &s[..3], &s[s.len() - 4..])
}

impl ModelConfig {
    /// A variant using the cheaper `aux_model`, falling back to the main model.
    pub fn aux_variant(&self) -> ModelConfig {
        ModelConfig {
            provider: self.provider,
            model: self.aux_model.clone().unwrap_or_else(|| self.model.clone()),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            aux_model: self.aux_model.clone(),
            max_turns: self.max_turns,
            max_tool_result_bytes: self.max_tool_result_bytes,
            max_history_messages: self.max_history_messages,
            llm_timeout_secs: self.llm_timeout_secs,
        }
    }
}

/// The resolved policy plus load diagnostics (for `komo policy list` / doctor).
pub struct PolicyReport {
    pub policy: crate::domain::policy::Policy,
    /// Config indices (0-based `[[policy.rule]]` order) of ignored invalid rules.
    pub invalid: Vec<usize>,
    /// Whether a `[policy]` table was present at all.
    pub configured: bool,
}

/// Resolved Feishu channel settings.
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    pub allow_from: Vec<String>,
    pub require_mention: bool,
    pub home_chat: Option<String>,
}

/// Resolved Telegram channel settings.
pub struct TelegramConfig {
    pub bot_token: String,
    pub allow_from: Vec<String>,
    pub allowed_chats: Vec<String>,
    pub require_mention: bool,
    pub home_chat: Option<String>,
}

/// Resolved WeChat channel settings.
pub struct WeChatConfig {
    pub allow_from: Vec<String>,
    pub home_chat: Option<String>,
}

/// Resolved Home Assistant settings (shared by the tool and the channel).
pub struct HomeAssistantConfig {
    pub base_url: String,
    pub token: String,
}

/// Resolved Home Assistant channel settings (behavior + shared credentials).
pub struct HomeAssistantChannelConfig {
    pub base_url: String,
    pub token: String,
    pub watch_domains: Vec<String>,
    pub watch_entities: Vec<String>,
    pub ignore_entities: Vec<String>,
    pub watch_all: bool,
    pub cooldown_seconds: u64,
}

/// Resolved HTTP API channel settings.
pub struct ApiConfig {
    pub bind: String,
    /// `0` means "let the OS assign an ephemeral port" — the actual port is read
    /// back after bind and published in the rendezvous file for the CLI.
    pub port: u16,
    pub server_key: String,
}

/// Resolve one consistent read of the sources into the runtime snapshot plus
/// its redacted report. Never fails: problems become [`ConfigIssue`]s.
pub(super) fn resolve(sources: ConfigSources) -> (RuntimeConfig, ConfigReport) {
    let ConfigSources {
        home,
        file,
        env,
        secrets,
        env_error,
    } = sources;

    let skills_path = skills_dirs(&env);
    let mut issues = Vec::new();
    if let Some(message) = env_error {
        issues.push(ConfigIssue {
            path: "env",
            severity: IssueSeverity::Fatal,
            message,
        });
    }

    // Provider/model, with provenance for `doctor` / `model list`.
    let (provider_str, provider_origin) = pick(env.provider, file.provider, || {
        Provider::DeepSeek.name().to_string()
    });
    let provider = Provider::parse(&provider_str).unwrap_or_else(|e| {
        issues.push(ConfigIssue {
            path: "model.provider",
            severity: IssueSeverity::Fatal,
            message: e.to_string(),
        });
        Provider::DeepSeek
    });
    let (model, model_origin) = pick(env.model, file.model, || {
        provider.default_model().to_string()
    });

    // Codex authenticates from `~/.codex/auth.json`, not an env key — its
    // `api_key` stays empty and is resolved in `infra/codex.rs`.
    //
    // A missing key is a *warning*, not a fatal issue: a fresh install (first
    // gateway boot in Docker, `komo init` before any credential exists) must
    // come up rather than crash-loop. `build_llm` degrades to a client whose
    // every call reports this same fix, so turns fail with guidance instead.
    let api_key = if provider.uses_api_key() {
        match secrets.key(provider) {
            Some(key) => key.to_string(),
            None => {
                issues.push(ConfigIssue {
                    path: "model.api_key",
                    severity: IssueSeverity::Warning,
                    message: format!(
                        "{} is not set (required for {provider:?}) — agent turns will \
                         fail until it is added to ~/.komo/.env (see `komo init`)",
                        provider.api_key_var()
                    ),
                });
                String::new()
            }
        }
    } else {
        String::new()
    };

    let provider_key_present = Provider::ALL
        .iter()
        .map(|p| (*p, secrets.key(*p).is_some()))
        .collect();

    let model = ModelConfig {
        provider,
        model,
        api_key,
        base_url: env.base_url.or(file.base_url),
        aux_model: env.aux_model.or(file.aux_model),
        max_turns: env
            .max_turns
            .or(file.max_turns)
            .unwrap_or(DEFAULT_MAX_TURNS),
        max_tool_result_bytes: env
            .max_tool_result_bytes
            .or(file.max_tool_result_bytes)
            .unwrap_or(DEFAULT_MAX_TOOL_RESULT_BYTES),
        max_history_messages: env
            .max_history_messages
            .or(file.max_history_messages)
            .unwrap_or(DEFAULT_MAX_HISTORY_MESSAGES),
        llm_timeout_secs: env
            .llm_timeout_secs
            .or(file.llm_timeout_secs)
            .unwrap_or(DEFAULT_LLM_TIMEOUT_SECS),
    };

    let policy = match file.policy {
        Some(cfg) => build_policy(cfg, &mut issues),
        None => PolicyReport {
            policy: Default::default(),
            invalid: Vec::new(),
            configured: false,
        },
    };

    // The homeassistant tool credentials, shared with the HA event channel.
    let homeassistant_tool = secrets
        .hass_token
        .clone()
        .filter(|s| !s.is_empty())
        .map(|token| HomeAssistantConfig {
            // Trim a trailing slash so `{base_url}/api/...` never doubles up.
            base_url: secrets
                .hass_url
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_HASS_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            token,
        });

    let channels = file.channels.unwrap_or_default();
    let feishu = match channels.feishu.filter(|c| c.enabled) {
        None => ChannelState::Disabled,
        Some(cfg) => {
            let creds = require_secret(&secrets.feishu_app_id, "feishu", "FEISHU_APP_ID").and_then(
                |app_id| {
                    require_secret(&secrets.feishu_app_secret, "feishu", "FEISHU_APP_SECRET")
                        .map(|app_secret| (app_id, app_secret))
                },
            );
            match creds {
                Ok((app_id, app_secret)) => ChannelState::Ready(FeishuConfig {
                    app_id,
                    app_secret,
                    allow_from: cfg.allow_from,
                    require_mention: cfg.require_mention.unwrap_or(true),
                    home_chat: cfg.home_chat,
                }),
                Err(message) => misconfigured(&mut issues, "channels.feishu", message),
            }
        }
    };
    let telegram = match channels.telegram.filter(|c| c.enabled) {
        None => ChannelState::Disabled,
        Some(cfg) => match require_secret(
            &secrets.telegram_bot_token,
            "telegram",
            "TELEGRAM_BOT_TOKEN",
        ) {
            Ok(bot_token) => ChannelState::Ready(TelegramConfig {
                bot_token,
                allow_from: cfg.allow_from,
                allowed_chats: cfg.allowed_chats,
                require_mention: cfg.require_mention.unwrap_or(true),
                home_chat: cfg.home_chat,
            }),
            Err(message) => misconfigured(&mut issues, "channels.telegram", message),
        },
    };
    // WeChat has no credential to check here — login is QR-based and the token
    // lives in `~/.komo/wechat/credentials.json`, verified at serve time.
    let wechat = match channels.wechat.filter(|c| c.enabled) {
        None => ChannelState::Disabled,
        Some(cfg) => ChannelState::Ready(WeChatConfig {
            allow_from: cfg.allow_from,
            home_chat: cfg.home_chat,
        }),
    };
    let homeassistant_channel = match channels.homeassistant.filter(|c| c.enabled) {
        None => ChannelState::Disabled,
        Some(cfg) => match &homeassistant_tool {
            Some(creds) => ChannelState::Ready(HomeAssistantChannelConfig {
                base_url: creds.base_url.clone(),
                token: creds.token.clone(),
                watch_domains: cfg.watch_domains,
                watch_entities: cfg.watch_entities,
                ignore_entities: cfg.ignore_entities,
                watch_all: cfg.watch_all,
                cooldown_seconds: cfg.cooldown_seconds.unwrap_or(30),
            }),
            // A warning, not fatal: HA is a local convenience integration whose
            // credential lives outside config.toml, so an enabled-but-tokenless
            // channel must not crash-loop the whole gateway (same principle as
            // the missing model API key). The channel just stays offline.
            None => {
                let message = "[channels.homeassistant] is enabled but HASS_TOKEN is not set \
                 (put it in ~/.komo/.env); the channel stays offline"
                    .to_string();
                issues.push(ConfigIssue {
                    path: "channels.homeassistant",
                    severity: IssueSeverity::Warning,
                    message: message.clone(),
                });
                ChannelState::Misconfigured(message)
            }
        },
    };
    let api_file = channels.api.unwrap_or_default();
    let api = if api_file.enabled {
        // Externally reachable: honor the configured bind/port and require a key.
        match require_secret(&secrets.api_server_key, "api", "API_SERVER_KEY") {
            Ok(server_key) => ChannelState::Ready(ApiConfig {
                bind: api_file
                    .bind
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_API_BIND.to_string()),
                port: api_file.port.unwrap_or(DEFAULT_API_PORT),
                server_key,
            }),
            Err(message) => misconfigured(&mut issues, "channels.api", message),
        }
    } else {
        // Always-on, loopback-only, CLI-facing: ephemeral port (discovered via
        // the rendezvous file), and the configured key if any, else a generated
        // one. Loopback-only, so a v4 token is ample.
        ChannelState::Ready(ApiConfig {
            bind: DEFAULT_API_BIND.to_string(),
            port: 0,
            server_key: secrets
                .api_server_key
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
        })
    };

    let db_url = |file: &str| format!("turso:{}", home.join(file).display());
    let runtime = RuntimeConfig {
        db_url: db_url("state.db"),
        kanban_db_url: db_url("kanban.db"),
        memory_db_url: db_url("memory.db"),
        home,
        model,
        review_interval: env
            .review_interval
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_REVIEW_INTERVAL),
        maintenance_schedule: env
            .schedule
            .or(file.schedule)
            .unwrap_or_else(|| DEFAULT_MAINTENANCE_SCHEDULE.to_string()),
        briefing_schedule: env.briefing_schedule.or(file.briefing_schedule),
        briefing_workdays_only: env
            .briefing_workdays_only
            .or(file.briefing_workdays_only)
            .unwrap_or(false),
        dream_schedule: resolve_dream_schedule(env.dream_schedule.or(file.dream_schedule)),
        policy,
        skills_path,
        homeassistant_tool,
        feishu,
        telegram,
        wechat,
        homeassistant_channel,
        api,
    };
    let report = ConfigReport {
        issues,
        provider_origin,
        model_origin,
        provider_key_present,
    };
    (runtime, report)
}

/// env > file > default, tagging where the value came from.
fn pick(
    env: Option<String>,
    file: Option<String>,
    default: impl FnOnce() -> String,
) -> (String, Origin) {
    match (env, file) {
        (Some(v), _) => (v, Origin::Env),
        (None, Some(v)) => (v, Origin::File),
        (None, None) => (default(), Origin::Default),
    }
}

/// Record the fatal issue an enabled-but-broken channel produces and return its
/// state. One message serves both surfaces: the state (doctor's channel line)
/// and the issue (`validate_gateway`'s fail-fast error).
fn misconfigured<T>(
    issues: &mut Vec<ConfigIssue>,
    path: &'static str,
    message: String,
) -> ChannelState<T> {
    issues.push(ConfigIssue {
        path,
        severity: IssueSeverity::Fatal,
        message: message.clone(),
    });
    ChannelState::Misconfigured(message)
}

/// Resolve a required channel credential read from `~/.komo/.env`. Channels
/// keep secrets in the environment, never in `config.toml`; an enabled channel
/// missing its secret gets one uniform message.
fn require_secret(value: &Option<String>, channel: &str, var: &str) -> Result<String, String> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            format!("[channels.{channel}] is enabled but {var} is not set (put it in ~/.komo/.env)")
        })
}

/// Pure resolution of the dreaming schedule from its configured value: unset →
/// the default (dreaming is on by default); empty or `off`/`none`/`disabled` →
/// `None` (off); anything else is taken as the cron expression.
fn resolve_dream_schedule(configured: Option<String>) -> Option<String> {
    match configured {
        Some(s)
            if s.trim().is_empty()
                || matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "off" | "none" | "disabled"
                ) =>
        {
            None
        }
        Some(s) => Some(s),
        None => Some(DEFAULT_DREAM_SCHEDULE.to_string()),
    }
}

/// `KOMO_SKILLS_PATH` (colon-separated) → extra skill dirs, order preserved.
fn skills_dirs(env: &KomoEnv) -> Vec<PathBuf> {
    env.skills_path
        .as_deref()
        .map(|extra| {
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn build_policy(cfg: PolicyFileConfig, issues: &mut Vec<ConfigIssue>) -> PolicyReport {
    use crate::domain::policy::{Policy, Verdict};

    let default_normal = cfg
        .default_normal
        .as_deref()
        .and_then(Verdict::parse_default)
        .unwrap_or(Verdict::Ask);

    let mut rules = Vec::new();
    let mut invalid = Vec::new();
    for (i, r) in cfg.rule.into_iter().enumerate() {
        match build_rule(r) {
            Some(rule) => rules.push(rule),
            None => {
                issues.push(ConfigIssue {
                    path: "policy.rule",
                    severity: IssueSeverity::Warning,
                    message: format!("[[policy.rule]] #{i} is invalid, ignoring it"),
                });
                invalid.push(i);
            }
        }
    }
    PolicyReport {
        policy: Policy::new(rules, default_normal),
        invalid,
        configured: true,
    }
}

fn build_rule(r: PolicyRuleFileConfig) -> Option<crate::domain::policy::Rule> {
    use crate::domain::policy::{Access, Category, Effect, Matcher, Rule};

    if r.value.is_empty() {
        return None;
    }
    Some(Rule {
        channels: r.channels.filter(|c| !c.is_empty()),
        category: Category::parse(&r.category)?,
        matcher: Matcher::parse(&r.matcher)?,
        value: r.value,
        access: match r.access {
            Some(a) => Some(Access::parse(&a)?),
            None => None,
        },
        effect: Effect::parse(&r.effect)?,
        include_dangerous: r.include_dangerous.unwrap_or(false),
        unattended: r.unattended.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::super::ConfigSnapshot;
    use super::super::sources::{
        ApiFileConfig, ChannelsFileConfig, FileConfig, HomeAssistantChannelFileConfig, Secrets,
        TelegramFileConfig,
    };
    use super::*;
    use std::path::PathBuf;

    fn sources() -> ConfigSources {
        ConfigSources {
            home: PathBuf::from("/tmp/komo-test-home"),
            file: FileConfig::default(),
            env: KomoEnv::default(),
            secrets: Secrets::default(),
            env_error: None,
        }
    }

    fn with_deepseek_key(mut s: ConfigSources) -> ConfigSources {
        s.secrets.deepseek_api_key = Some("sk-test".into());
        s
    }

    #[test]
    fn defaults_resolve_without_file_or_env() {
        let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
        let rt = &snap.runtime;
        assert_eq!(rt.model.provider, Provider::DeepSeek);
        assert_eq!(rt.model.model, "deepseek-chat");
        assert_eq!(rt.model.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(rt.maintenance_schedule, DEFAULT_MAINTENANCE_SCHEDULE);
        assert_eq!(rt.briefing_schedule, None, "briefing stays opt-in");
        assert_eq!(
            rt.dream_schedule.as_deref(),
            Some(DEFAULT_DREAM_SCHEDULE),
            "dreaming is on by default"
        );
        assert_eq!(rt.review_interval, DEFAULT_REVIEW_INTERVAL);
        assert_eq!(snap.report.provider_origin, Origin::Default);
        assert_eq!(snap.report.model_origin, Origin::Default);
        assert!(snap.report.fatal().is_none());
        assert!(snap.validate_gateway().is_ok());
    }

    #[test]
    fn precedence_is_default_then_file_then_env() {
        let mut s = sources();
        s.secrets.openai_api_key = Some("sk-env".into());
        s.file.provider = Some("deepseek".into());
        s.file.model = Some("file-model".into());
        s.file.max_turns = Some(7);
        s.env.provider = Some("openai".into());
        s.env.model = Some("env-model".into());
        let snap = ConfigSnapshot::from_sources(s);
        assert_eq!(snap.runtime.model.provider, Provider::OpenAi);
        assert_eq!(snap.runtime.model.model, "env-model");
        assert_eq!(snap.runtime.model.max_turns, 7, "file wins over default");
        assert_eq!(snap.report.provider_origin, Origin::Env);
        assert_eq!(snap.report.model_origin, Origin::Env);
    }

    #[test]
    fn file_model_reports_file_origin() {
        let mut s = with_deepseek_key(sources());
        s.file.model = Some("deepseek-reasoner".into());
        let snap = ConfigSnapshot::from_sources(s);
        assert_eq!(snap.report.model_origin, Origin::File);
        assert_eq!(snap.report.provider_origin, Origin::Default);
    }

    #[test]
    fn missing_api_key_warns_but_does_not_block_startup() {
        let snap = ConfigSnapshot::from_sources(sources());
        assert_eq!(snap.runtime.model.api_key, "");
        // Degraded, not dead: a fresh install must boot (build_llm degrades to
        // an every-call-errors client), so the issue is a warning.
        assert!(snap.report.fatal().is_none());
        let issue = snap
            .report
            .issues
            .iter()
            .find(|i| i.path == "model.api_key")
            .expect("missing key is reported");
        assert_eq!(issue.severity, IssueSeverity::Warning);
        assert!(issue.message.contains("DEEPSEEK_API_KEY"));
        assert!(snap.validate_gateway().is_ok());
        assert!(snap.validate_agent().is_ok());
    }

    #[test]
    fn codex_needs_no_api_key() {
        let mut s = sources();
        s.file.provider = Some("codex".into());
        let snap = ConfigSnapshot::from_sources(s);
        assert_eq!(snap.runtime.model.provider, Provider::Codex);
        assert_eq!(snap.runtime.model.model, "gpt-5.5");
        assert!(
            snap.report.fatal().is_none(),
            "codex auth is OAuth, not an env key"
        );
        assert!(!snap.report.key_present(Provider::Codex));
    }

    #[test]
    fn invalid_provider_is_fatal_and_falls_back() {
        let mut s = sources();
        s.file.provider = Some("nonsense".into());
        let snap = ConfigSnapshot::from_sources(s);
        let fatal = snap.report.fatal().expect("bad provider is fatal");
        assert_eq!(fatal.path, "model.provider");
        assert_eq!(
            snap.runtime.model.provider,
            Provider::DeepSeek,
            "resolution continues on the default provider"
        );
        assert!(snap.validate_agent().is_err());
    }

    #[test]
    fn env_error_is_fatal_for_startup_not_diagnostics() {
        let mut s = with_deepseek_key(sources());
        s.env_error = Some("invalid KOMO_* environment variable: bad".into());
        let snap = ConfigSnapshot::from_sources(s);
        let fatal = snap.report.fatal().expect("env error is fatal");
        assert_eq!(fatal.path, "env");
        assert!(snap.validate_agent().is_err());
        // Diagnostics still get a fully-resolved snapshot.
        assert_eq!(snap.runtime.model.provider, Provider::DeepSeek);
    }

    #[test]
    fn disabled_channel_missing_secret_is_not_an_issue() {
        let mut s = with_deepseek_key(sources());
        s.file.channels = Some(ChannelsFileConfig {
            telegram: Some(TelegramFileConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        assert!(matches!(snap.runtime.telegram, ChannelState::Disabled));
        assert!(snap.report.fatal().is_none());
    }

    #[test]
    fn enabled_channel_missing_secret_is_one_fatal_issue() {
        let mut s = with_deepseek_key(sources());
        s.file.channels = Some(ChannelsFileConfig {
            telegram: Some(TelegramFileConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        let ChannelState::Misconfigured(msg) = &snap.runtime.telegram else {
            panic!("enabled without token must be misconfigured");
        };
        assert!(msg.contains("TELEGRAM_BOT_TOKEN"));
        assert_eq!(
            snap.report
                .issues
                .iter()
                .filter(|i| i.path == "channels.telegram")
                .count(),
            1
        );
        // The gateway fails fast; a chat turn doesn't need the channel.
        assert!(snap.validate_gateway().is_err());
        assert!(snap.validate_agent().is_ok());
    }

    #[test]
    fn homeassistant_without_token_warns_but_does_not_block_startup() {
        let mut s = with_deepseek_key(sources());
        s.file.channels = Some(ChannelsFileConfig {
            homeassistant: Some(HomeAssistantChannelFileConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        let ChannelState::Misconfigured(msg) = &snap.runtime.homeassistant_channel else {
            panic!("enabled without HASS_TOKEN must be misconfigured");
        };
        assert!(msg.contains("HASS_TOKEN"));
        // Degraded, not dead: the gateway boots with the HA channel offline.
        let issue = snap
            .report
            .issues
            .iter()
            .find(|i| i.path == "channels.homeassistant")
            .expect("missing token is reported");
        assert_eq!(issue.severity, IssueSeverity::Warning);
        assert!(snap.report.fatal().is_none());
        assert!(snap.validate_gateway().is_ok());
    }

    #[test]
    fn api_defaults_to_loopback_ephemeral_with_auto_key() {
        let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
        let api = snap.runtime.api.ready().expect("api is always on");
        assert_eq!(api.bind, "127.0.0.1");
        assert_eq!(api.port, 0, "ephemeral port by default");
        assert!(!api.server_key.is_empty(), "auto-generated key");
    }

    #[test]
    fn external_api_requires_a_key() {
        let mut s = with_deepseek_key(sources());
        s.file.channels = Some(ChannelsFileConfig {
            api: Some(ApiFileConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        assert!(matches!(snap.runtime.api, ChannelState::Misconfigured(_)));
        assert!(snap.validate_gateway().is_err());

        let mut s = with_deepseek_key(sources());
        s.secrets.api_server_key = Some("k".into());
        s.file.channels = Some(ChannelsFileConfig {
            api: Some(ApiFileConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        let api = snap
            .runtime
            .api
            .ready()
            .expect("keyed external api is ready");
        assert_eq!(api.port, 8765, "stable default port when external");
        assert_eq!(api.server_key, "k");
    }

    #[test]
    fn report_never_contains_secret_values() {
        let mut s = sources();
        s.secrets.deepseek_api_key = Some("sk-super-secret-value".into());
        s.secrets.telegram_bot_token = Some("123:telegram-secret".into());
        s.file.channels = Some(ChannelsFileConfig {
            telegram: Some(TelegramFileConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let snap = ConfigSnapshot::from_sources(s);
        let dump = format!("{:?}", snap.report);
        assert!(!dump.contains("sk-super-secret-value"));
        assert!(!dump.contains("telegram-secret"));
        assert!(snap.report.key_present(Provider::DeepSeek));
    }

    #[test]
    fn dream_schedule_defaults_on_and_can_be_disabled() {
        // Unset → on by default at the nightly slot.
        assert_eq!(
            resolve_dream_schedule(None).as_deref(),
            Some(DEFAULT_DREAM_SCHEDULE)
        );
        // A custom cron is taken verbatim.
        assert_eq!(
            resolve_dream_schedule(Some("0 4 * * *".into())).as_deref(),
            Some("0 4 * * *")
        );
        // Empty or off-like values disable it.
        for off in ["", "  ", "off", "OFF", "none", "disabled"] {
            assert_eq!(
                resolve_dream_schedule(Some(off.into())),
                None,
                "`{off}` should disable dreaming"
            );
        }
    }

    #[test]
    fn skills_path_splits_on_colons() {
        let mut s = with_deepseek_key(sources());
        s.env.skills_path = Some("/a/skills:/b/skills:".into());
        let snap = ConfigSnapshot::from_sources(s);
        assert_eq!(
            snap.runtime.skills_path,
            vec![PathBuf::from("/a/skills"), PathBuf::from("/b/skills")]
        );
    }

    #[test]
    fn db_urls_derive_from_home() {
        let snap = ConfigSnapshot::from_sources(with_deepseek_key(sources()));
        assert_eq!(snap.runtime.db_url, "turso:/tmp/komo-test-home/state.db");
        assert_eq!(
            snap.runtime.kanban_db_url,
            "turso:/tmp/komo-test-home/kanban.db"
        );
        assert_eq!(
            snap.runtime.memory_db_url,
            "turso:/tmp/komo-test-home/memory.db"
        );
    }

    #[test]
    fn debug_output_masks_api_key() {
        let cfg = ModelConfig {
            provider: Provider::DeepSeek,
            model: "deepseek-chat".into(),
            api_key: "sk-abcdefghijklmnopqr".into(),
            base_url: None,
            aux_model: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            max_history_messages: DEFAULT_MAX_HISTORY_MESSAGES,
            llm_timeout_secs: DEFAULT_LLM_TIMEOUT_SECS,
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("sk-abcdefghijklmnopqr"),
            "full key must not appear in Debug output"
        );
        assert!(s.contains("sk-"), "prefix should be visible");
    }
}
