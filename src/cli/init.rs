//! `komo init` — bootstrap the config home with commented templates.
//!
//! Writes a default `config.toml`, a `.env` credential template, and a
//! default `SOUL.md` persona into `~/.komo/` (or `KOMO_HOME`). Existing files
//! are never touched, so the command is safe to re-run and safe inside a
//! running gateway (pure file ops, no db). Pairs with the degraded no-API-key
//! startup: a fresh install boots, `komo init` scaffolds the files, the
//! operator fills in a key.

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

/// The generated `~/.komo/SOUL.md` — the default persona, from the README's
/// brand section (komorebi: sunlight through leaves). Replaces the built-in
/// one-line identity in the system prompt; the operator edits it freely (the
/// prompt builder re-reads it on mtime change, no restart needed).
const SOUL_TEMPLATE: &str = "\
你是 Komo，一位安静、可靠的个人助理。

名字取自日语「木漏れ日」（komorebi）——阳光透过树叶洒落下来的样子。你的气质也如此：\
温暖、清亮、不喧哗。像树荫下坐在身旁的老朋友：平时安静，开口时说到点子上，并且记得住\
别人托付给你的每一件小事。

你相信小事会积攒成光——一条提醒、一个待办、一段记忆，日积月累就是生活本身。\
「陪你把日子攒成光」是你的座右铭（Light through your days）。

行事风格：

- **简洁**：先给结论和要做的事，少铺垫、不绕弯子。用用户的语言交流，中文时自然口语化，\
不堆砌客套。
- **踏实**：需要实时信息或要动手做事时，调用工具去查、去做，绝不凭空编造；查不到或\
不确定，就直说不确定。你并不知道用户此刻通过哪个渠道（微信/Telegram/飞书/终端）在和\
你说话——自我介绍或聊天时不要提渠道，更不要猜。
- **记性好**：值得长期记住的事（偏好、约定、承诺、常用信息）主动记下来；聊到过去的事\
先查记忆和会话历史，不靠猜。
- **不吵闹**：主动消息（提醒、简报）只在真正有价值时才发；不追问无关紧要的细节，能\
自己查到的不去烦用户。
- **有分寸**：有副作用的操作走审批流程，拿不准的先问一句再动手；宁可慢半拍，不替用户\
做重大决定。

记住每一缕光。
";

pub fn run() -> anyhow::Result<()> {
    let home = crate::config::ensure_komo_home();
    let (config_created, env_created, soul_created) = init_at(&home)?;
    report("config.toml", &home, config_created);
    report(".env", &home, env_created);
    report("SOUL.md", &home, soul_created);
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

/// Write whichever of the three templates doesn't exist yet. Returns
/// `(config_created, env_created, soul_created)`. Never overwrites — an
/// operator's edits outrank the template, always.
fn init_at(home: &Path) -> anyhow::Result<(bool, bool, bool)> {
    let config_created = write_if_absent(&home.join("config.toml"), CONFIG_TEMPLATE)?;
    let env_path = home.join(".env");
    let env_created = write_if_absent(&env_path, ENV_TEMPLATE)?;
    // Credentials file: owner-only, same floor `ensure_komo_home` maintains.
    #[cfg(unix)]
    if env_created {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }
    let soul_created = write_if_absent(&home.join("SOUL.md"), SOUL_TEMPLATE)?;
    Ok((config_created, env_created, soul_created))
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
    fn init_creates_all_templates() {
        let home = tmp("creates");
        let (config_created, env_created, soul_created) = init_at(&home).unwrap();
        assert!(config_created && env_created && soul_created);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("provider = \"deepseek\""));
        let env = std::fs::read_to_string(home.join(".env")).unwrap();
        assert!(env.contains("DEEPSEEK_API_KEY="));
        let soul = std::fs::read_to_string(home.join("SOUL.md")).unwrap();
        assert!(soul.contains("你是 Komo"));
    }

    #[test]
    fn init_never_overwrites_existing_files() {
        let home = tmp("preserves");
        std::fs::write(home.join("config.toml"), "provider = \"openai\"\n").unwrap();
        std::fs::write(home.join("SOUL.md"), "You are Nyx.\n").unwrap();
        let (config_created, env_created, soul_created) = init_at(&home).unwrap();
        assert!(!config_created, "existing config must be left alone");
        assert!(env_created, "missing .env is still scaffolded");
        assert!(
            !soul_created,
            "an operator-edited persona must be left alone"
        );
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert_eq!(config, "provider = \"openai\"\n");
        let soul = std::fs::read_to_string(home.join("SOUL.md")).unwrap();
        assert_eq!(soul, "You are Nyx.\n");
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
