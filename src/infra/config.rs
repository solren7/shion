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
        // Aliases borrowed from hermes' ProviderProfile.aliases.
        Ok(match s.trim().to_lowercase().as_str() {
            "deepseek" | "ds" => Provider::DeepSeek,
            "openai" | "oai" | "gpt" => Provider::OpenAi,
            "anthropic" | "claude" => Provider::Anthropic,
            "openrouter" | "or" => Provider::OpenRouter,
            other => anyhow::bail!(
                "unknown SHION_PROVIDER `{other}` \
                 (expected: deepseek | openai | anthropic | openrouter)"
            ),
        })
    }

    /// Default model id when `SHION_MODEL` is unset.
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

/// Resolved model selection: which provider, which model, and the API key.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
    pub api_key: String,
    /// Optional base-URL override. Lets one provider path (e.g. OpenAI-compatible)
    /// reach any compatible endpoint: Groq, Together, a local server, an internal
    /// gateway. (Borrowed from pi/hermes: one compatible path covers many.)
    pub base_url: Option<String>,
    /// Optional cheaper model for auxiliary sub-tasks (delegation, future
    /// summarization). Borrowed from hermes' `default_aux_model`.
    pub aux_model: Option<String>,
}

impl ModelConfig {
    /// Resolve from the environment:
    /// - `SHION_PROVIDER` (default `deepseek`; aliases: ds/gpt/claude/or)
    /// - `SHION_MODEL` (default depends on the provider)
    /// - `SHION_BASE_URL` (optional endpoint override)
    /// - `SHION_AUX_MODEL` (optional cheaper model for sub-agents)
    /// - the provider's API-key variable (e.g. `DEEPSEEK_API_KEY`)
    pub fn from_env() -> anyhow::Result<Self> {
        let provider = match std::env::var("SHION_PROVIDER") {
            Ok(v) => Provider::parse(&v)?,
            Err(_) => Provider::DeepSeek,
        };
        let model =
            std::env::var("SHION_MODEL").unwrap_or_else(|_| provider.default_model().to_string());
        let key_var = provider.api_key_var();
        let api_key = std::env::var(key_var)
            .map_err(|_| anyhow::anyhow!("{key_var} is not set (required for {provider:?})"))?;
        Ok(Self {
            provider,
            model,
            api_key,
            base_url: std::env::var("SHION_BASE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            aux_model: std::env::var("SHION_AUX_MODEL")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }

    /// A variant using the cheaper `aux_model` (falling back to the main model)
    /// — used for delegated sub-agents.
    pub fn aux_variant(&self) -> ModelConfig {
        ModelConfig {
            model: self.aux_model.clone().unwrap_or_else(|| self.model.clone()),
            ..self.clone()
        }
    }
}
