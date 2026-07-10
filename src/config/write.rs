//! The one config *write* path: persisting a model selection.

use std::path::{Path, PathBuf};

use super::Provider;

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
