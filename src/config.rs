use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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

/// `SHION_*` environment overrides, deserialized in one place via envy.
/// dotenvy (`main.rs`) populates the process env from `.env` files first,
/// so these see both real env vars and `.env` entries.
#[derive(Debug, Deserialize, Default)]
pub struct ShionEnv {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub aux_model: Option<String>,
    pub schedule: Option<String>,
    pub briefing_schedule: Option<String>,
    pub briefing_workdays_only: Option<bool>,
    pub dream_schedule: Option<String>,
    pub max_turns: Option<usize>,
    pub max_tool_result_bytes: Option<usize>,
    pub max_history_messages: Option<usize>,
    pub review_interval: Option<usize>,
    pub skills_path: Option<String>,
}

impl ShionEnv {
    /// Strict load: a malformed value (e.g. non-numeric `SHION_MAX_TURNS`)
    /// is an error. Use on paths that should fail fast at startup.
    pub fn load() -> anyhow::Result<Self> {
        let env: ShionEnv = envy::prefixed("SHION_")
            .from_env()
            .map_err(|e| anyhow::anyhow!("invalid SHION_* environment variable: {e}"))?;
        Ok(env.normalized())
    }

    /// Lenient load for paths that must not abort: warns on stderr and
    /// drops all `SHION_*` overrides when any value is malformed.
    pub fn load_lenient() -> Self {
        Self::load().unwrap_or_else(|e| {
            eprintln!("shion: {e} (ignoring SHION_* overrides)");
            Self::default()
        })
    }

    /// Treat empty strings as unset, so `SHION_MODEL=` behaves like an
    /// absent variable.
    fn normalized(mut self) -> Self {
        for slot in [
            &mut self.provider,
            &mut self.model,
            &mut self.base_url,
            &mut self.aux_model,
            &mut self.schedule,
            &mut self.briefing_schedule,
            &mut self.dream_schedule,
            &mut self.skills_path,
        ] {
            if slot.as_deref().is_some_and(|s| s.is_empty()) {
                *slot = None;
            }
        }
        self
    }
}

/// Every credential shion reads, loaded once from the environment (and thus from
/// `~/.shion/.env`, which dotenvy folds into the process env at startup) via
/// envy. Secrets live ONLY here, never in `config.toml` — keeping behavior (the
/// `*FileConfig` types) and secrets (this one) in separate types is shion's
/// security boundary, which a single merged config would erode. envy maps each
/// field to its SCREAMING_SNAKE_CASE name, so `feishu_app_id` reads `FEISHU_APP_ID`
/// — the per-channel prefixes are encoded in the field names, no `envy::prefixed`
/// call per channel. All fields are private; the channel resolvers in this module
/// read them directly, and external callers go through [`Secrets::key`].
#[derive(Debug, Deserialize, Default)]
pub struct Secrets {
    // Provider API keys (their env vars are unprefixed).
    deepseek_api_key: Option<String>,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    openrouter_api_key: Option<String>,
    // Channel credentials (formerly the per-channel `*Env` structs).
    feishu_app_id: Option<String>,
    feishu_app_secret: Option<String>,
    telegram_bot_token: Option<String>,
    api_server_key: Option<String>,
    hass_token: Option<String>,
    hass_url: Option<String>,
}

impl Secrets {
    pub fn load() -> Self {
        envy::from_env().unwrap_or_default()
    }

    /// The API key for `provider`, treating empty strings as unset.
    pub fn key(&self, provider: Provider) -> Option<&str> {
        let slot = match provider {
            Provider::DeepSeek => &self.deepseek_api_key,
            Provider::OpenAi => &self.openai_api_key,
            Provider::Anthropic => &self.anthropic_api_key,
            Provider::OpenRouter => &self.openrouter_api_key,
            // Codex has no env API key (OAuth via ~/.codex/auth.json).
            Provider::Codex => return None,
        };
        slot.as_deref().filter(|s| !s.is_empty())
    }
}

/// Ensure `~/.shion/` exists (0700) and return its path.
/// Tightens `.env` inside to 0600 if present.
/// Permission failures are silently ignored (containers, Windows).
pub fn ensure_shion_home() -> PathBuf {
    let home = shion_home();
    if let Err(e) = std::fs::create_dir_all(&home) {
        eprintln!("shion: could not create {}: {e}", home.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
        let env_path = home.join(".env");
        if env_path.exists() {
            let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
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

/// Maintenance cron schedule: `SHION_SCHEDULE` env > config.toml `schedule`
/// > hourly default.
pub fn maintenance_schedule() -> String {
    ShionEnv::load_lenient()
        .schedule
        .or_else(|| FileConfig::load(&shion_home()).schedule)
        .unwrap_or_else(|| "0 * * * *".to_string())
}

/// Daily-briefing cron schedule: `SHION_BRIEFING_SCHEDULE` env >
/// config.toml `briefing_schedule`. Opt-in (no default): the proactive
/// briefing only runs when the user picks a time, so shion never starts
/// pushing notifications uninvited.
pub fn briefing_schedule() -> Option<String> {
    ShionEnv::load_lenient()
        .briefing_schedule
        .or_else(|| FileConfig::load(&shion_home()).briefing_schedule)
}

/// Default dreaming-sweep schedule: nightly at 3am, mirroring OpenClaw's
/// dreaming. Unlike the briefing (proactive notifications → opt-in), dreaming is
/// internal memory housekeeping with no user-facing output, so it is **on by
/// default**.
pub const DEFAULT_DREAM_SCHEDULE: &str = "0 3 * * *";

/// Dreaming-sweep cron schedule: `SHION_DREAM_SCHEDULE` env > config.toml
/// `dream_schedule` > [`DEFAULT_DREAM_SCHEDULE`]. The usage-driven memory
/// consolidation runs by default; to disable it set `dream_schedule = "off"`
/// (or empty). `None` means disabled — wiring then skips the sweep entirely.
pub fn dream_schedule() -> Option<String> {
    let configured = ShionEnv::load_lenient()
        .dream_schedule
        .or_else(|| FileConfig::load(&shion_home()).dream_schedule);
    resolve_dream_schedule(configured)
}

/// Pure resolution of the dreaming schedule from its configured value: unset →
/// the default; empty or `off`/`none`/`disabled` → `None` (off); anything else
/// is taken as the cron expression. Split out so the default-on / opt-out logic
/// is testable without touching the real env or config file.
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

/// Whether the daily briefing should only fire on Chinese working days
/// (statutory holidays and 调休-adjusted weekends respected): `SHION_BRIEFING_WORKDAYS_ONLY`
/// env > config.toml `briefing_workdays_only` > `false`. When true, the briefing
/// sweep is wrapped in a `WorkdayGated` that skips non-workdays.
pub fn briefing_workdays_only() -> bool {
    ShionEnv::load_lenient()
        .briefing_workdays_only
        .or_else(|| FileConfig::load(&shion_home()).briefing_workdays_only)
        .unwrap_or(false)
}

/// Directory holding the cached Chinese workday calendar, one `{year}.json` per
/// year: `<shion_home>/workdays/`. Disposable — delete a file to force a
/// re-fetch from the holiday API.
pub fn workday_cache_dir() -> PathBuf {
    shion_home().join("workdays")
}

/// Settings read from `~/.shion/config.toml`. All fields are optional;
/// absent keys fall back to `SHION_*` env vars then built-in defaults.
/// API keys must never appear here — keep them in `~/.shion/.env`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct FileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub aux_model: Option<String>,
    /// 5-field Unix cron expression for gateway maintenance (default: hourly).
    pub schedule: Option<String>,
    /// 5-field Unix cron expression for the daily briefing. Unset = disabled
    /// (the briefing is opt-in; e.g. `0 8 * * *` for 8am daily).
    pub briefing_schedule: Option<String>,
    /// Gate the daily briefing to Chinese working days only (statutory holidays
    /// and 调休-adjusted weekends respected). Default false.
    pub briefing_workdays_only: Option<bool>,
    /// 5-field Unix cron expression for the usage-driven memory "dreaming" sweep.
    /// Unset = on by default (nightly `0 3 * * *`); set to `"off"` (or empty) to
    /// disable.
    pub dream_schedule: Option<String>,
    /// Maximum tool-calling round-trips per user turn (default: 30).
    pub max_turns: Option<usize>,
    /// Byte cap on a tool result handed back to the LLM, a global backstop
    /// against context-window bloat (default: 16384). See
    /// `services::tool_registry::cap_tool_result`.
    pub max_tool_result_bytes: Option<usize>,
    /// Max prior messages replayed as history per turn — the global backstop
    /// against an ever-growing chat session sending its whole transcript to the
    /// model every turn (default: 50; `0` = unlimited). See
    /// `infra::llm::RigLlm::assemble`.
    pub max_history_messages: Option<usize>,
    /// Ingress channel declarations (`[channels.*]` tables), shaped after
    /// hermes-agent's per-platform config blocks.
    pub channels: Option<ChannelsFileConfig>,
    /// Configurable permission policy (`[policy]` + `[[policy.rule]]`).
    pub policy: Option<PolicyFileConfig>,
}

/// `[policy]` table: the configurable permission layer (roadmap §3). Parsed into
/// a `domain::policy::Policy` by [`policy_config`].
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PolicyFileConfig {
    /// Fallback for a `Risk::Normal` action no rule matches: `ask` (default,
    /// = current behavior), `deny`, or `allow`. `Risk::Dangerous` always asks
    /// unless an explicit `include_dangerous` allow rule grants it.
    pub default_normal: Option<String>,
    /// `[[policy.rule]]` entries, evaluated deny-first.
    pub rule: Vec<PolicyRuleFileConfig>,
}

/// One `[[policy.rule]]` entry.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PolicyRuleFileConfig {
    /// `shell` | `file` | `network` | `homeassistant`.
    pub category: String,
    /// `prefix` | `suffix` | `exact` | `contains`.
    #[serde(rename = "match")]
    pub matcher: String,
    /// The string compared against the action's target (command/path/host/...).
    pub value: String,
    /// `allow` | `deny`.
    pub effect: String,
    /// `file`-only: `read` | `write`. Omit = either.
    pub access: Option<String>,
    /// Channel scope (`["cli", "feishu"]`). Omit/empty = all channels.
    pub channels: Option<Vec<String>>,
    /// Let an `allow` rule grant `Risk::Dangerous` actions too (default false).
    pub include_dangerous: Option<bool>,
}

/// Resolve the permission policy from `~/.shion/config.toml`. Absent `[policy]`
/// (or any invalid rule) degrades to the empty policy — i.e. the current
/// interactive-only behavior, never more permissive.
pub fn policy_config() -> crate::domain::policy::Policy {
    FileConfig::load(&shion_home())
        .policy
        .map(build_policy)
        .unwrap_or_default()
}

fn build_policy(cfg: PolicyFileConfig) -> crate::domain::policy::Policy {
    use crate::domain::policy::{Policy, Verdict};

    let default_normal = cfg
        .default_normal
        .as_deref()
        .and_then(Verdict::parse_default)
        .unwrap_or(Verdict::Ask);

    let mut rules = Vec::new();
    for (i, r) in cfg.rule.into_iter().enumerate() {
        match build_rule(r) {
            Some(rule) => rules.push(rule),
            None => eprintln!("shion: [policy] rule #{i} is invalid, ignoring it"),
        }
    }
    Policy::new(rules, default_normal)
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
    })
}

/// `[channels]` namespace in config.toml: one optional table per transport.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ChannelsFileConfig {
    pub feishu: Option<FeishuFileConfig>,
    pub telegram: Option<TelegramFileConfig>,
    pub wechat: Option<WeChatFileConfig>,
    pub homeassistant: Option<HomeAssistantChannelFileConfig>,
    pub api: Option<ApiFileConfig>,
}

/// Resolve a required channel credential read from `~/.shion/.env`. Channels
/// keep secrets in the environment, never in `config.toml`; when a channel is
/// `enabled` but its secret is absent (or empty), fail fast at startup with one
/// uniform message instead of silently starting a half-configured channel.
fn require_secret(value: Option<String>, channel: &str, var: &str) -> anyhow::Result<String> {
    value.filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "[channels.{channel}] is enabled but {var} is not set (put it in ~/.shion/.env)"
        )
    })
}

/// `[channels.homeassistant]` table: HA as an event-ingress channel. The URL
/// and token are *not* here — they come from `HASS_URL` / `HASS_TOKEN` (shared
/// with the `homeassistant` tool). This table only carries event-filter
/// behavior. Event forwarding is closed by default: with no `watch_*` set and
/// `watch_all = false`, every event is dropped.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct HomeAssistantChannelFileConfig {
    pub enabled: bool,
    /// Forward state changes for entities in these domains (e.g. "binary_sensor").
    pub watch_domains: Vec<String>,
    /// Forward state changes for these specific entity ids.
    pub watch_entities: Vec<String>,
    /// Never forward these entity ids (takes precedence over watch_*).
    pub ignore_entities: Vec<String>,
    /// Forward *every* entity's state change (ignore the watch lists).
    pub watch_all: bool,
    /// Per-entity minimum seconds between forwarded events (default 30).
    pub cooldown_seconds: Option<u64>,
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

/// Resolve the Home Assistant ingress channel. `None` = not enabled; an error =
/// enabled but `HASS_TOKEN` is missing (fail fast at startup, like the other
/// channels).
pub fn homeassistant_channel_config() -> anyhow::Result<Option<HomeAssistantChannelConfig>> {
    let file = FileConfig::load(&shion_home());
    let Some(ha) = file.channels.and_then(|c| c.homeassistant) else {
        return Ok(None);
    };
    if !ha.enabled {
        return Ok(None);
    }
    let creds = homeassistant_config().ok_or_else(|| {
        anyhow::anyhow!(
            "[channels.homeassistant] is enabled but HASS_TOKEN is not set (put it in ~/.shion/.env)"
        )
    })?;
    Ok(Some(HomeAssistantChannelConfig {
        base_url: creds.base_url,
        token: creds.token,
        watch_domains: ha.watch_domains,
        watch_entities: ha.watch_entities,
        ignore_entities: ha.ignore_entities,
        watch_all: ha.watch_all,
        cooldown_seconds: ha.cooldown_seconds.unwrap_or(30),
    }))
}

/// `[channels.feishu]` table. App credentials never live here — they are
/// read from `FEISHU_APP_ID` / `FEISHU_APP_SECRET` (in `~/.shion/.env`).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct FeishuFileConfig {
    pub enabled: bool,
    /// Sender open_id allowlist. Empty = anyone who can reach the bot.
    pub allow_from: Vec<String>,
    /// Whether group messages must @mention someone to be handled
    /// (default true; DMs always bypass this gate).
    pub require_mention: Option<bool>,
    /// Chat id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier.
    pub home_chat: Option<String>,
}

/// Resolved Feishu channel settings.
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    pub allow_from: Vec<String>,
    pub require_mention: bool,
    pub home_chat: Option<String>,
}

/// `[channels.telegram]` table. The bot token never lives here — it is read
/// from `TELEGRAM_BOT_TOKEN` (in `~/.shion/.env`).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct TelegramFileConfig {
    pub enabled: bool,
    /// Sender user-id allowlist. Empty = anyone who can reach the bot.
    pub allow_from: Vec<String>,
    /// Group chat-id allowlist: when non-empty, group messages are only handled
    /// in these chats (DMs always pass). Mirrors hermes' `allowed_chats`.
    pub allowed_chats: Vec<String>,
    /// Whether group messages must @mention the bot (default true; DMs
    /// always bypass this gate).
    pub require_mention: Option<bool>,
    /// Chat id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier.
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

/// Default Home Assistant URL when `HASS_URL` is unset.
const DEFAULT_HASS_URL: &str = "http://homeassistant.local:8123";

/// Resolved Home Assistant settings.
pub struct HomeAssistantConfig {
    pub base_url: String,
    pub token: String,
}

/// Resolve the Home Assistant tool config from the environment: `HASS_TOKEN`
/// (required) and `HASS_URL` (optional, defaults to homeassistant.local:8123).
/// `None` means no token is set, so the `homeassistant` tool is not registered.
pub fn homeassistant_config() -> Option<HomeAssistantConfig> {
    let secrets = Secrets::load();
    let token = secrets.hass_token.filter(|s| !s.is_empty())?;
    let base_url = secrets
        .hass_url
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_HASS_URL.to_string());
    Some(HomeAssistantConfig {
        // Trim a trailing slash so `{base_url}/api/...` never doubles up.
        base_url: base_url.trim_end_matches('/').to_string(),
        token,
    })
}

/// Resolve the Telegram channel config. `None` means the channel is not
/// enabled; an error means it is enabled but misconfigured (fail fast at
/// startup).
pub fn telegram_config() -> anyhow::Result<Option<TelegramConfig>> {
    let file = FileConfig::load(&shion_home());
    let Some(telegram) = file.channels.and_then(|c| c.telegram) else {
        return Ok(None);
    };
    if !telegram.enabled {
        return Ok(None);
    }
    let bot_token = require_secret(
        Secrets::load().telegram_bot_token,
        "telegram",
        "TELEGRAM_BOT_TOKEN",
    )?;
    Ok(Some(TelegramConfig {
        bot_token,
        allow_from: telegram.allow_from,
        allowed_chats: telegram.allowed_chats,
        require_mention: telegram.require_mention.unwrap_or(true),
        home_chat: telegram.home_chat,
    }))
}

/// Default loopback bind address for the HTTP API channel. Loopback-only by
/// default so the API isn't reachable off-host without an explicit override.
const DEFAULT_API_BIND: &str = "127.0.0.1";
/// Default API port (kept distinct from hermes' 8642 to avoid a same-host clash).
const DEFAULT_API_PORT: u16 = 8765;

/// `[channels.api]` table: the OpenAI-compatible + dashboard HTTP API. The
/// bearer key never lives here — it is read from `API_SERVER_KEY` (in
/// `~/.shion/.env`), like the other channels' credentials.
///
/// The api channel is **always on**: the `shion` CLI (and `shion chat`) reach a
/// running gateway through it, because Turso's exclusive db lock means the CLI
/// can't open the db itself while the gateway holds it. `enabled = true` widens
/// it from the default loopback-only, ephemeral-port, CLI-only listener to an
/// **externally reachable** one on a stable `bind`/`port` (for Open WebUI / the
/// dashboard), and then `API_SERVER_KEY` is required.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiFileConfig {
    /// Expose the api externally on a stable address (requires `API_SERVER_KEY`).
    /// When false/absent the channel still runs, but loopback-only on an
    /// ephemeral port with an auto-generated key — enough for the local CLI.
    pub enabled: bool,
    /// Bind address (default `127.0.0.1`). Set `0.0.0.0` only behind a trusted
    /// proxy — the key is the only auth.
    pub bind: Option<String>,
    /// Listen port (default 8765 when `enabled`; an ephemeral port otherwise).
    pub port: Option<u16>,
}

/// Resolved HTTP API channel settings.
pub struct ApiConfig {
    pub bind: String,
    /// `0` means "let the OS assign an ephemeral port" — the actual port is read
    /// back after bind and published in the rendezvous file for the CLI.
    pub port: u16,
    pub server_key: String,
}

/// Resolve the (always-on) HTTP API channel config. An error means the channel
/// is explicitly `enabled` for external use but `API_SERVER_KEY` is missing
/// (fail fast — a keyless externally-bound API is never started). When not
/// enabled, the channel still runs loopback-only with a generated key so the
/// local CLI can always reach a running gateway.
pub fn api_config() -> anyhow::Result<ApiConfig> {
    let file = FileConfig::load(&shion_home());
    let api = file.channels.and_then(|c| c.api).unwrap_or_default();
    if api.enabled {
        // Externally reachable: honor the configured bind/port and require a key.
        let server_key = require_secret(Secrets::load().api_server_key, "api", "API_SERVER_KEY")?;
        Ok(ApiConfig {
            bind: api
                .bind
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_BIND.to_string()),
            port: api.port.unwrap_or(DEFAULT_API_PORT),
            server_key,
        })
    } else {
        // Always-on, loopback-only, CLI-facing: ephemeral port (discovered via
        // the rendezvous file), and the configured key if any, else a generated
        // one. Loopback-only, so a v4 token is ample.
        let server_key = Secrets::load()
            .api_server_key
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        Ok(ApiConfig {
            bind: DEFAULT_API_BIND.to_string(),
            port: 0,
            server_key,
        })
    }
}

/// `[channels.wechat]` table: WeChat (微信) over the iLink personal-bot
/// protocol. There are no credentials here or in `.env` — login is QR-based and
/// the resulting token is stored in `~/.shion/wechat/credentials.json`
/// (provisioned once with `shion wechat login`). DM-only, so no group/mention
/// keys.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct WeChatFileConfig {
    pub enabled: bool,
    /// Sender iLink user-id allowlist. Empty = anyone who can reach the bot
    /// (still gated by pairing).
    pub allow_from: Vec<String>,
    /// iLink user id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier. Note: delivery still requires that user to
    /// have messaged the bot since the gateway started (see `infra/messaging/wechat.rs`).
    pub home_chat: Option<String>,
}

/// Resolved WeChat channel settings.
pub struct WeChatConfig {
    pub allow_from: Vec<String>,
    pub home_chat: Option<String>,
}

/// Where the QR-login credentials are stored. Shared by the gateway channel and
/// the `shion wechat login` provisioning command.
pub fn wechat_cred_path() -> PathBuf {
    shion_home().join("wechat").join("credentials.json")
}

/// Resolve the WeChat channel config. `None` means the channel is not enabled.
/// Unlike the other channels there is no fail-fast credential check here —
/// credentials are provisioned interactively (`shion wechat login`), and the
/// channel reports its own missing-creds state at serve time.
pub fn wechat_config() -> anyhow::Result<Option<WeChatConfig>> {
    let file = FileConfig::load(&shion_home());
    let Some(wechat) = file.channels.and_then(|c| c.wechat) else {
        return Ok(None);
    };
    if !wechat.enabled {
        return Ok(None);
    }
    Ok(Some(WeChatConfig {
        allow_from: wechat.allow_from,
        home_chat: wechat.home_chat,
    }))
}

/// Resolve the Feishu channel config. `None` means the channel is not enabled;
/// an error means it is enabled but misconfigured (fail fast at startup).
pub fn feishu_config() -> anyhow::Result<Option<FeishuConfig>> {
    let file = FileConfig::load(&shion_home());
    let Some(feishu) = file.channels.and_then(|c| c.feishu) else {
        return Ok(None);
    };
    if !feishu.enabled {
        return Ok(None);
    }
    let secrets = Secrets::load();
    let app_id = require_secret(secrets.feishu_app_id, "feishu", "FEISHU_APP_ID")?;
    let app_secret = require_secret(secrets.feishu_app_secret, "feishu", "FEISHU_APP_SECRET")?;
    Ok(Some(FeishuConfig {
        app_id,
        app_secret,
        allow_from: feishu.allow_from,
        require_mention: feishu.require_mention.unwrap_or(true),
        home_chat: feishu.home_chat,
    }))
}

impl FileConfig {
    /// Load from `<home>/config.toml`.
    ///
    /// - File absent → `Default` (not an error).
    /// - Parse error → warn on stderr, return `Default`, **leave file untouched**.
    pub fn load(home: &Path) -> FileConfig {
        let path = home.join("config.toml");
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return FileConfig::default();
            }
            Err(e) => {
                eprintln!("shion: could not read {}: {e}", path.display());
                return FileConfig::default();
            }
        };
        match toml::from_str(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "shion: {} is invalid (falling back to defaults): {e}",
                    path.display()
                );
                FileConfig::default()
            }
        }
    }
}

/// Persist the provider/model selection into `<home>/config.toml`, preserving
/// every other key already present (schedule, base_url, aux_model, …).
///
/// `model: None` removes the `model` key so the provider's default applies.
/// Returns the path written. Note: any `SHION_PROVIDER` / `SHION_MODEL` env
/// vars still take priority over the file at resolve time.
pub fn write_model_selection(
    home: &Path,
    provider: Provider,
    model: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let path = home.join("config.toml");
    let mut table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s)
            .map_err(|e| anyhow::anyhow!("{} is invalid TOML: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e.into()),
    };

    table.insert(
        "provider".to_string(),
        toml::Value::String(provider.name().to_string()),
    );
    match model {
        Some(m) => {
            table.insert("model".to_string(), toml::Value::String(m.to_string()));
        }
        None => {
            table.remove("model");
        }
    }

    std::fs::write(&path, toml::to_string_pretty(&table)?)?;
    Ok(path)
}

/// Resolved model selection: provider, model id, API key, and optional overrides.
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
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
}

/// Built-in default for `max_turns` when neither `SHION_MAX_TURNS` nor
/// config.toml sets one. Multi-file edits easily take 10+ round-trips.
pub const DEFAULT_MAX_TURNS: usize = 30;

/// Built-in default byte cap on a tool result handed back to the LLM, when
/// neither `SHION_MAX_TOOL_RESULT_BYTES` nor config.toml sets one. Sized above
/// the per-tool self-caps (web_fetch / homeassistant trim to 8 KB) so it only
/// catches tools that don't self-trim.
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Built-in default for `max_history_messages` when neither
/// `SHION_MAX_HISTORY_MESSAGES` nor config.toml sets one. Counts prior messages
/// (user + assistant alternating, so ~25 turns), enough context for a chat
/// assistant while keeping a long-lived session's per-turn cost bounded. `0`
/// disables the window (replay the whole transcript, the pre-windowing behavior).
pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 50;

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
    /// Resolve configuration with three-layer priority (lowest → highest):
    ///
    ///   1. Built-in defaults
    ///   2. `~/.shion/config.toml`
    ///   3. `SHION_*` environment variables
    ///
    /// API keys are only ever read from environment variables / `~/.shion/.env`,
    /// never from `config.toml`.
    pub fn resolve() -> anyhow::Result<Self> {
        let home = ensure_shion_home();
        let file = FileConfig::load(&home);
        let env = ShionEnv::load()?;

        let provider_str = env
            .provider
            .or(file.provider)
            .unwrap_or_else(|| "deepseek".to_string());
        let provider = Provider::parse(&provider_str)?;

        let model = env
            .model
            .or(file.model)
            .unwrap_or_else(|| provider.default_model().to_string());

        // Codex authenticates from `~/.codex/auth.json`, not an env key — its
        // `api_key` field stays empty and is resolved in `infra/codex.rs`.
        let api_key = if provider.uses_api_key() {
            let key_var = provider.api_key_var();
            Secrets::load()
                .key(provider)
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!("{key_var} is not set (required for {provider:?})")
                })?
        } else {
            String::new()
        };

        Ok(Self {
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
        })
    }

    /// Backward-compatible alias for `resolve()`.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::resolve()
    }

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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shion_config_test_{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
    fn file_config_missing_file_yields_defaults() {
        let dir = tmp("missing");
        let cfg = FileConfig::load(&dir);
        assert!(cfg.provider.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.base_url.is_none());
        assert!(cfg.aux_model.is_none());
    }

    #[test]
    fn file_config_broken_toml_yields_defaults_and_keeps_file() {
        let dir = tmp("broken");
        let path = dir.join("config.toml");
        fs::write(&path, "not valid toml = = =").unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let cfg = FileConfig::load(&dir);
        assert!(cfg.provider.is_none());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "file must not be modified"
        );
    }

    #[test]
    fn file_config_loads_provider_and_model() {
        let dir = tmp("valid");
        fs::write(
            dir.join("config.toml"),
            "provider = \"openai\"\nmodel = \"gpt-4o\"\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn file_config_loads_max_turns() {
        let dir = tmp("max_turns");
        fs::write(dir.join("config.toml"), "max_turns = 50\n").unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.max_turns, Some(50));
    }

    #[test]
    fn file_config_loads_schedule() {
        let dir = tmp("schedule");
        fs::write(dir.join("config.toml"), "schedule = \"*/30 * * * *\"\n").unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.schedule.as_deref(), Some("*/30 * * * *"));
    }

    #[test]
    fn file_config_loads_briefing_schedule() {
        let dir = tmp("briefing");
        fs::write(
            dir.join("config.toml"),
            "briefing_schedule = \"0 8 * * *\"\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.briefing_schedule.as_deref(), Some("0 8 * * *"));
    }

    #[test]
    fn file_config_loads_feishu_channel_table() {
        let dir = tmp("feishu");
        fs::write(
            dir.join("config.toml"),
            concat!(
                "[channels.feishu]\n",
                "enabled = true\n",
                "allow_from = [\"ou_a\", \"ou_b\"]\n",
                "require_mention = false\n",
                "home_chat = \"oc_home\"\n",
            ),
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        let feishu = cfg
            .channels
            .and_then(|c| c.feishu)
            .expect("feishu table should parse");
        assert!(feishu.enabled);
        assert_eq!(feishu.allow_from, vec!["ou_a", "ou_b"]);
        assert_eq!(feishu.require_mention, Some(false));
        assert_eq!(feishu.home_chat.as_deref(), Some("oc_home"));
    }

    #[test]
    fn file_config_feishu_defaults_are_lenient() {
        let dir = tmp("feishu_defaults");
        fs::write(
            dir.join("config.toml"),
            "[channels.feishu]\nenabled = true\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        let feishu = cfg.channels.and_then(|c| c.feishu).expect("table parses");
        assert!(feishu.allow_from.is_empty());
        assert_eq!(feishu.require_mention, None);
        assert_eq!(feishu.home_chat, None);
    }

    #[test]
    fn api_keys_maps_provider_and_filters_empty() {
        let keys = Secrets {
            deepseek_api_key: Some("sk-x".into()),
            openai_api_key: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(keys.key(Provider::DeepSeek), Some("sk-x"));
        assert_eq!(keys.key(Provider::OpenAi), None, "empty string = unset");
        assert_eq!(keys.key(Provider::Anthropic), None);
    }

    #[test]
    fn shion_env_normalizes_empty_strings_to_unset() {
        let env = ShionEnv {
            provider: Some("openai".into()),
            model: Some(String::new()),
            ..Default::default()
        }
        .normalized();
        assert_eq!(env.provider.as_deref(), Some("openai"));
        assert_eq!(env.model, None);
    }

    #[test]
    fn shion_home_respects_env_override() {
        let dir = tmp("home_override");
        // SAFETY: single-threaded test context; we restore immediately.
        unsafe { std::env::set_var("SHION_HOME", dir.to_str().unwrap()) };
        let home = shion_home();
        unsafe { std::env::remove_var("SHION_HOME") };
        assert_eq!(home, dir);
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
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("sk-abcdefghijklmnopqr"),
            "full key must not appear in Debug output"
        );
        assert!(s.contains("sk-"), "prefix should be visible");
    }
}
