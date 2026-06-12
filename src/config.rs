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
}

impl Provider {
    /// Every supported provider, in display order.
    pub const ALL: [Provider; 4] = [
        Provider::DeepSeek,
        Provider::OpenAi,
        Provider::Anthropic,
        Provider::OpenRouter,
    ];

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "deepseek" | "ds" => Provider::DeepSeek,
            "openai" | "oai" | "gpt" => Provider::OpenAi,
            "anthropic" | "claude" => Provider::Anthropic,
            "openrouter" | "or" => Provider::OpenRouter,
            other => anyhow::bail!(
                "unknown provider `{other}` \
                 (expected: deepseek | openai | anthropic | openrouter)"
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
        }
    }

    /// Default model id when `model` is unset.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek-chat",
            Provider::OpenAi => "gpt-4o-mini",
            Provider::Anthropic => "claude-3-5-sonnet-latest",
            Provider::OpenRouter => "deepseek/deepseek-chat",
        }
    }

    /// Environment variable holding this provider's API key.
    pub fn api_key_var(self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
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
    pub max_turns: Option<usize>,
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
            &mut self.skills_path,
        ] {
            if slot.as_deref().is_some_and(|s| s.is_empty()) {
                *slot = None;
            }
        }
        self
    }
}

/// Provider API keys, read from the (unprefixed) environment via envy.
#[derive(Debug, Deserialize, Default)]
pub struct ApiKeys {
    deepseek_api_key: Option<String>,
    openai_api_key: Option<String>,
    anthropic_api_key: Option<String>,
    openrouter_api_key: Option<String>,
}

impl ApiKeys {
    pub fn load() -> Self {
        envy::from_env().unwrap_or_default()
    }

    /// The key for `provider`, treating empty strings as unset.
    pub fn key(&self, provider: Provider) -> Option<&str> {
        let slot = match provider {
            Provider::DeepSeek => &self.deepseek_api_key,
            Provider::OpenAi => &self.openai_api_key,
            Provider::Anthropic => &self.anthropic_api_key,
            Provider::OpenRouter => &self.openrouter_api_key,
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

/// Default database URL: `sqlite:<shion_home>/shion.db`.
/// Creates the config directory on first use so SQLite can create the file.
pub fn default_db_url() -> String {
    format!("sqlite:{}", ensure_shion_home().join("shion.db").display())
}

/// Maintenance cron schedule: `SHION_SCHEDULE` env > config.toml `schedule`
/// > hourly default.
pub fn maintenance_schedule() -> String {
    ShionEnv::load_lenient()
        .schedule
        .or_else(|| FileConfig::load(&shion_home()).schedule)
        .unwrap_or_else(|| "0 * * * *".to_string())
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
    /// Maximum tool-calling round-trips per user turn (default: 30).
    pub max_turns: Option<usize>,
    /// Ingress channel declarations (`[channels.*]` tables), shaped after
    /// hermes-agent's per-platform config blocks.
    pub channels: Option<ChannelsFileConfig>,
}

/// `[channels]` namespace in config.toml: one optional table per transport.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ChannelsFileConfig {
    pub feishu: Option<FeishuFileConfig>,
    pub telegram: Option<TelegramFileConfig>,
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

/// `FEISHU_*` app credentials from the environment (`~/.shion/.env`).
#[derive(Debug, Deserialize, Default)]
struct FeishuEnv {
    app_id: Option<String>,
    app_secret: Option<String>,
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
    /// Whether group messages must @mention the bot (default true; DMs
    /// always bypass this gate).
    pub require_mention: Option<bool>,
    /// Chat id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier.
    pub home_chat: Option<String>,
}

/// `TELEGRAM_*` bot credentials from the environment (`~/.shion/.env`).
#[derive(Debug, Deserialize, Default)]
struct TelegramEnv {
    bot_token: Option<String>,
}

/// Resolved Telegram channel settings.
pub struct TelegramConfig {
    pub bot_token: String,
    pub allow_from: Vec<String>,
    pub require_mention: bool,
    pub home_chat: Option<String>,
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
    let env: TelegramEnv = envy::prefixed("TELEGRAM_").from_env().unwrap_or_default();
    let bot_token = env.bot_token.filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "[channels.telegram] is enabled but TELEGRAM_BOT_TOKEN is not set (put it in ~/.shion/.env)"
        )
    })?;
    Ok(Some(TelegramConfig {
        bot_token,
        allow_from: telegram.allow_from,
        require_mention: telegram.require_mention.unwrap_or(true),
        home_chat: telegram.home_chat,
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
    let env: FeishuEnv = envy::prefixed("FEISHU_").from_env().unwrap_or_default();
    let missing = |var: &'static str| {
        anyhow::anyhow!(
            "[channels.feishu] is enabled but {var} is not set (put it in ~/.shion/.env)"
        )
    };
    let app_id = env
        .app_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| missing("FEISHU_APP_ID"))?;
    let app_secret = env
        .app_secret
        .filter(|s| !s.is_empty())
        .ok_or_else(|| missing("FEISHU_APP_SECRET"))?;
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
}

/// Built-in default for `max_turns` when neither `SHION_MAX_TURNS` nor
/// config.toml sets one. Multi-file edits easily take 10+ round-trips.
pub const DEFAULT_MAX_TURNS: usize = 30;

impl fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &mask_secret(&self.api_key))
            .field("base_url", &self.base_url)
            .field("aux_model", &self.aux_model)
            .field("max_turns", &self.max_turns)
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

        let key_var = provider.api_key_var();
        let api_key = ApiKeys::load()
            .key(provider)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("{key_var} is not set (required for {provider:?})"))?;

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
        let keys = ApiKeys {
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
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("sk-abcdefghijklmnopqr"),
            "full key must not appear in Debug output"
        );
        assert!(s.contains("sk-"), "prefix should be visible");
    }
}
