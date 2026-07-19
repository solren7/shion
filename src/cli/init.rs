//! `komo init` — bootstrap the config home with commented templates.
//!
//! Writes a default `config.toml` and a `.env` credential template into
//! `~/.komo/` (or `KOMO_HOME`). Existing files are never touched, so the
//! command is safe to re-run and safe inside a running gateway (pure file
//! ops, no db). Pairs with the degraded no-API-key startup: a fresh install
//! boots, `komo init` scaffolds the files, the operator fills in a key.

use std::path::Path;

/// The generated `~/.komo/config.toml`. Runtime settings only — credentials
/// belong in `.env` (see [`ENV_TEMPLATE`]). Everything but the provider line
/// is commented out at its built-in default, so the file documents itself.
const CONFIG_TEMPLATE: &str = r#"# komo runtime settings. Credentials never go here — put them in .env
# next to this file. Priority: built-in defaults < this file < KOMO_* env.

# LLM provider: deepseek | openai | anthropic | openrouter | codex
# (codex needs no API key — it uses the Codex CLI's OAuth login)
provider = "deepseek"
# model = "deepseek-chat"        # defaults per provider
# base_url = ""                  # OpenAI-compatible endpoint override
# aux_model = ""                 # cheaper model for sub-tasks (reviewer/recall/briefing)

# Maintenance sweep cron (5-field Unix cron). Default: hourly.
# schedule = "0 * * * *"

# Daily briefing — opt-in, no default. Uncomment to enable.
# briefing_schedule = "30 8 * * *"
# briefing_workdays_only = true  # skip Chinese non-working days (incl. 调休)

# Usage-driven memory consolidation ("dreaming"). On by default, nightly.
# dream_schedule = "0 3 * * *"   # set to "off" to disable

# --- ingress channels (each needs its credential in .env) -------------------

# [channels.telegram]
# enabled = true
# allow_from = ["123456789"]     # pre-trusted sender ids (skip pairing)
# require_mention = true         # group messages must @mention the bot
# home_chat = "123456789"        # reminders/briefing delivered here

# [channels.feishu]
# enabled = true
# allow_from = ["ou_xxx"]
# require_mention = true
# home_chat = "oc_xxx"

# [channels.wechat]              # DM-only; provision with `komo channel wechat login`
# enabled = true

# [channels.homeassistant]       # HA event ingress (HASS_TOKEN in .env)
# enabled = true
# watch_domains = ["binary_sensor", "lock"]
# watch_entities = []
# cooldown_seconds = 30

# [channels.api]                 # widen the loopback HTTP API (needs API_SERVER_KEY)
# enabled = true
# bind = "0.0.0.0"
# port = 8765
"#;

/// The generated `~/.komo/.env`. Credentials only; empty values read as
/// unset, so the uncommented key line is a safe fill-in-the-blank.
const ENV_TEMPLATE: &str = r#"# komo credentials (this file is chmod 600; never commit it anywhere).
# Empty values are treated as unset. Match the `provider` in config.toml.

DEEPSEEK_API_KEY=
# OPENAI_API_KEY=
# ANTHROPIC_API_KEY=
# OPENROUTER_API_KEY=

# Channels
# TELEGRAM_BOT_TOKEN=
# FEISHU_APP_ID=
# FEISHU_APP_SECRET=

# Home Assistant (shared by the tool and the event channel)
# HASS_TOKEN=
# HASS_URL=http://homeassistant.local:8123

# Bearer key for an externally-bound api channel ([channels.api])
# API_SERVER_KEY=
"#;

pub fn run() -> anyhow::Result<()> {
    let home = crate::config::ensure_komo_home();
    let (config_created, env_created) = init_at(&home)?;
    report("config.toml", &home, config_created);
    report(".env", &home, env_created);
    if config_created || env_created {
        println!(
            "\nNext: put your API key in {}/.env (DEEPSEEK_API_KEY=sk-...),\n\
             then restart the gateway. `komo doctor` verifies the result.",
            home.display()
        );
    }
    Ok(())
}

fn report(name: &str, home: &Path, created: bool) {
    let path = home.join(name);
    if created {
        println!("created   {}", path.display());
    } else {
        println!("unchanged {} (already exists)", path.display());
    }
}

/// Write whichever of the two templates doesn't exist yet. Returns
/// `(config_created, env_created)`. Never overwrites — an operator's edits
/// outrank the template, always.
fn init_at(home: &Path) -> anyhow::Result<(bool, bool)> {
    let config_created = write_if_absent(&home.join("config.toml"), CONFIG_TEMPLATE)?;
    let env_path = home.join(".env");
    let env_created = write_if_absent(&env_path, ENV_TEMPLATE)?;
    // Credentials file: owner-only, same floor `ensure_komo_home` maintains.
    #[cfg(unix)]
    if env_created {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((config_created, env_created))
}

fn write_if_absent(path: &Path, content: &str) -> anyhow::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_init_test_{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_creates_both_templates() {
        let home = tmp("creates");
        let (config_created, env_created) = init_at(&home).unwrap();
        assert!(config_created && env_created);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("provider = \"deepseek\""));
        let env = std::fs::read_to_string(home.join(".env")).unwrap();
        assert!(env.contains("DEEPSEEK_API_KEY="));
    }

    #[test]
    fn init_never_overwrites_existing_files() {
        let home = tmp("preserves");
        std::fs::write(home.join("config.toml"), "provider = \"openai\"\n").unwrap();
        let (config_created, env_created) = init_at(&home).unwrap();
        assert!(!config_created, "existing config must be left alone");
        assert!(env_created, "missing .env is still scaffolded");
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert_eq!(config, "provider = \"openai\"\n");
    }

    #[cfg(unix)]
    #[test]
    fn scaffolded_env_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp("perms");
        init_at(&home).unwrap();
        let mode = std::fs::metadata(home.join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn generated_config_parses_as_toml() {
        // The template must stay valid TOML — a scaffold that breaks parsing
        // would be worse than no scaffold.
        let parsed: Result<toml::Value, _> = toml::from_str(CONFIG_TEMPLATE);
        assert!(parsed.is_ok(), "template must parse: {:?}", parsed.err());
    }
}
