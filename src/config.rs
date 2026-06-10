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
    fn parse(s: &str) -> anyhow::Result<Self> {
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
    std::env::var("SHION_SCHEDULE")
        .ok()
        .filter(|s| !s.is_empty())
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

/// Resolved model selection: provider, model id, API key, and optional overrides.
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
    pub api_key: String,
    /// Optional base-URL override for OpenAI-compatible endpoints.
    pub base_url: Option<String>,
    /// Optional cheaper model for auxiliary sub-tasks.
    pub aux_model: Option<String>,
}

impl fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &mask_secret(&self.api_key))
            .field("base_url", &self.base_url)
            .field("aux_model", &self.aux_model)
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

        let provider_str = std::env::var("SHION_PROVIDER")
            .ok()
            .filter(|s| !s.is_empty())
            .or(file.provider)
            .unwrap_or_else(|| "deepseek".to_string());
        let provider = Provider::parse(&provider_str)?;

        let model = std::env::var("SHION_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .or(file.model)
            .unwrap_or_else(|| provider.default_model().to_string());

        let key_var = provider.api_key_var();
        let api_key = std::env::var(key_var)
            .map_err(|_| anyhow::anyhow!("{key_var} is not set (required for {provider:?})"))?;

        let base_url = std::env::var("SHION_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or(file.base_url);

        let aux_model = std::env::var("SHION_AUX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .or(file.aux_model);

        Ok(Self {
            provider,
            model,
            api_key,
            base_url,
            aux_model,
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
    fn file_config_loads_schedule() {
        let dir = tmp("schedule");
        fs::write(dir.join("config.toml"), "schedule = \"*/30 * * * *\"\n").unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.schedule.as_deref(), Some("*/30 * * * *"));
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
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("sk-abcdefghijklmnopqr"),
            "full key must not appear in Debug output"
        );
        assert!(s.contains("sk-"), "prefix should be visible");
    }
}
