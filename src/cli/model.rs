//! Model inspection and switching (`shion model list`, `shion model set`).
//!
//! `list` shows the resolved provider/model and where each value comes from
//! (env var > config.toml > built-in default), plus every available provider.
//! `set` persists a new selection into `~/.shion/config.toml`. Neither touches
//! the database or requires the API key to be present.

use crate::config::{self, ApiKeys, FileConfig, Provider, ShionEnv};

fn key_present(keys: &ApiKeys, provider: Provider) -> bool {
    keys.key(provider).is_some()
}

/// Show the current provider/model (with its source) and list all providers.
pub fn list() -> anyhow::Result<()> {
    let home = config::ensure_shion_home();
    let file = FileConfig::load(&home);
    let env = ShionEnv::load_lenient();
    let keys = ApiKeys::load();

    let provider_str = env
        .provider
        .clone()
        .or_else(|| file.provider.clone())
        .unwrap_or_else(|| "deepseek".to_string());
    let provider = Provider::parse(&provider_str)?;
    let provider_source = if env.provider.is_some() {
        "env SHION_PROVIDER"
    } else if file.provider.is_some() {
        "config.toml"
    } else {
        "default"
    };

    let model = env
        .model
        .clone()
        .or_else(|| file.model.clone())
        .unwrap_or_else(|| provider.default_model().to_string());
    let model_source = if env.model.is_some() {
        "env SHION_MODEL"
    } else if file.model.is_some() {
        "config.toml"
    } else {
        "provider default"
    };

    println!("Current");
    println!("  provider  {}  ({provider_source})", provider.name());
    println!("  model     {model}  ({model_source})");
    println!(
        "  api key   {}  {}",
        provider.api_key_var(),
        if key_present(&keys, provider) {
            "✓ set"
        } else {
            "✗ missing"
        }
    );

    println!();
    println!("Available providers  (* = active)");
    for p in Provider::ALL {
        println!(
            "  {} {:<11} default {:<26} key {} {}",
            if p == provider { "*" } else { " " },
            p.name(),
            p.default_model(),
            p.api_key_var(),
            if key_present(&keys, p) { "✓" } else { "·" },
        );
    }

    println!();
    println!("Switch with: shion model set <provider> [model]");
    Ok(())
}

/// Switch the provider (and optionally the model), persisting to config.toml.
pub fn set(provider_str: &str, model: Option<String>) -> anyhow::Result<()> {
    let home = config::ensure_shion_home();
    let provider = Provider::parse(provider_str)?;
    let path = config::write_model_selection(&home, provider, model.as_deref())?;

    let effective = model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    println!("provider = {}", provider.name());
    if model.is_some() {
        println!("model    = {effective}");
    } else {
        println!("model    = {effective}  (provider default)");
    }
    println!("wrote {}", path.display());

    let env = ShionEnv::load_lenient();
    if env.provider.is_some() || env.model.is_some() {
        eprintln!(
            "note: SHION_PROVIDER/SHION_MODEL are set and override config.toml; \
             unset them for this change to take effect"
        );
    }
    if !key_present(&ApiKeys::load(), provider) {
        eprintln!(
            "note: {} is not set — add it to {}/.env before using {}",
            provider.api_key_var(),
            home.display(),
            provider.name()
        );
    }
    Ok(())
}
