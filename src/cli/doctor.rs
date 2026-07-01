//! `shion doctor` — config & gateway health aggregation (roadmap §9).
//!
//! A single read-only snapshot of "what is shion configured to do, and what is
//! missing": the active model/provider and whether its API key is present, the
//! maintenance/briefing schedules, each ingress channel's enabled+credentials
//! state, the resolved home channel, and the run ledger's recent failures.
//!
//! Channels aren't a running-process query — they are constructed fresh in
//! `cli/gateway.rs` from config + env at startup. So this command re-derives the
//! same state the gateway would, which is why it doubles as config-health: it
//! reuses the very `*_config()` resolvers the gateway uses, where `Ok(None)` =
//! disabled, `Ok(Some)` = ready, and `Err` = enabled-but-misconfigured (the
//! error message names the missing credential).

use crate::config::{
    self, FileConfig, Provider, Secrets, ShionEnv, api_config, feishu_config,
    homeassistant_channel_config, homeassistant_config, telegram_config, wechat_config,
    wechat_cred_path,
};
use crate::domain::{home::HomeRepository, run::RunRepository};
use crate::infra::persistence::db::Db;

/// Status glyph for a channel/credential line.
const OK: &str = "✓";
const OFF: &str = "·";
const BAD: &str = "✗";

fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

pub async fn doctor(db_url: &str) -> anyhow::Result<()> {
    let home = config::shion_home();
    println!("home: {}", home.display());

    model_health();
    schedule_health();
    println!("\nchannels:");
    channel_health();
    home_channel_health(db_url).await;
    run_health(db_url).await;
    Ok(())
}

/// Provider/model resolution and whether the provider's API key is present.
/// Mirrors `ModelConfig::resolve`'s priority (env > config.toml > default) but
/// reports a missing key as a health line instead of erroring.
fn model_health() {
    let env = ShionEnv::load_lenient();
    let file = FileConfig::load(&config::shion_home());
    let provider_str = env
        .provider
        .or(file.provider)
        .unwrap_or_else(|| "deepseek".to_string());
    match Provider::parse(&provider_str) {
        Ok(provider) => {
            let model = env
                .model
                .or(file.model)
                .unwrap_or_else(|| provider.default_model().to_string());
            println!("\nmodel: {} / {}", provider.name(), model);
            if provider.uses_api_key() {
                let has_key = Secrets::load().key(provider).is_some();
                let mark = if has_key { OK } else { BAD };
                println!(
                    "  {mark} {} {}",
                    provider.api_key_var(),
                    if has_key { "set" } else { "MISSING" }
                );
            } else {
                // Codex authenticates from ~/.codex/auth.json — validate that
                // login rather than looking for an env key.
                match crate::infra::codex::CodexAuth::load() {
                    Ok(_) => println!("  {OK} Codex OAuth (~/.codex/auth.json)"),
                    Err(e) => println!("  {BAD} Codex auth: {e}"),
                }
            }
        }
        Err(e) => println!("\nmodel: {BAD} {e}"),
    }
}

/// Maintenance cron, daily briefing (opt-in), and the workday gate.
fn schedule_health() {
    println!("\nsweeps:");
    println!("  maintenance  {}", config::maintenance_schedule());
    match config::briefing_schedule() {
        Some(s) => {
            let gate = if config::briefing_workdays_only() {
                " (Chinese workdays only)"
            } else {
                ""
            };
            println!("  briefing     {s}{gate}");
        }
        None => println!("  briefing     {OFF} disabled (set briefing_schedule to enable)"),
    }
    println!("  reminders    every minute");
    println!("  tasks        every minute");
}

/// One line per ingress channel: enabled?, credentials present?
fn channel_health() {
    // Each resolver: Ok(None) = disabled, Ok(Some) = ready, Err = misconfigured.
    macro_rules! line {
        ($label:expr, $resolved:expr) => {
            match $resolved {
                Ok(Some(_)) => println!("  {OK} {:<14} enabled", $label),
                Ok(None) => println!("  {OFF} {:<14} disabled", $label),
                Err(e) => println!("  {BAD} {:<14} {e}", $label),
            }
        };
    }
    line!("feishu", feishu_config());
    line!("telegram", telegram_config());
    line!("homeassistant", homeassistant_channel_config());
    // The api channel is always on (it's how the CLI reaches a running gateway);
    // `enabled` only widens it from loopback-only to externally reachable.
    match api_config() {
        Ok(cfg) if cfg.port != 0 => {
            println!(
                "  {OK} {:<14} enabled (external {}:{})",
                "api", cfg.bind, cfg.port
            )
        }
        Ok(_) => println!("  {OK} {:<14} on (loopback-only, CLI)", "api"),
        Err(e) => println!("  {BAD} {:<14} {e}", "api"),
    }

    // WeChat resolves with no credential check (login is QR-based, creds in a
    // separate file), so verify the file ourselves.
    match wechat_config() {
        Ok(Some(_)) => {
            if wechat_cred_path().exists() {
                println!("  {OK} {:<14} enabled", "wechat");
            } else {
                println!(
                    "  {BAD} {:<14} enabled but not logged in (run `shion wechat login`)",
                    "wechat"
                );
            }
        }
        Ok(None) => println!("  {OFF} {:<14} disabled", "wechat"),
        Err(e) => println!("  {BAD} {:<14} {e}", "wechat"),
    }

    // The homeassistant *tool* (agent controls HA) is independent of the channel.
    let ha_tool = if homeassistant_config().is_some() {
        format!("{OK} HASS_TOKEN set")
    } else {
        format!("{OFF} HASS_TOKEN unset (homeassistant tool not registered)")
    };
    println!("  {ha_tool}");
}

/// Resolved proactive-output home: the `/sethome` runtime override (db) wins
/// over the config `home_chat` fallback (feishu first).
async fn home_channel_health(db_url: &str) {
    println!("\nhome channel (proactive output):");
    match Db::connect(db_url).await {
        Ok(db) => match HomeRepository::get(&db).await {
            Ok(Some(session)) => println!("  {OK} /sethome override → {session}"),
            Ok(None) => match config_home_chat() {
                Some((platform, chat)) => {
                    println!("  {OK} config home_chat → {platform}:{chat}")
                }
                None => {
                    println!("  {OFF} none set — proactive output falls back to the macOS notifier")
                }
            },
            Err(e) => println!("  {BAD} could not read home setting: {e}"),
        },
        Err(e) => println!("  {BAD} could not open db: {e}"),
    }
}

/// The config `home_chat` fallback, feishu-first (matches `HomeNotifier`).
fn config_home_chat() -> Option<(&'static str, String)> {
    if let Ok(Some(c)) = feishu_config() {
        if let Some(chat) = c.home_chat {
            return Some(("feishu", chat));
        }
    }
    if let Ok(Some(c)) = telegram_config() {
        if let Some(chat) = c.home_chat {
            return Some(("telegram", chat));
        }
    }
    if let Ok(Some(c)) = wechat_config() {
        if let Some(chat) = c.home_chat {
            return Some(("wechat", chat));
        }
    }
    None
}

/// Recent run-ledger health: how many of the last 50 turns failed, with the
/// most recent few. The roadmap §9 "last error" view.
async fn run_health(db_url: &str) {
    println!("\nrecent runs:");
    let db = match Db::connect(db_url).await {
        Ok(db) => db,
        Err(e) => {
            println!("  {BAD} could not open db: {e}");
            return;
        }
    };
    let runs = match RunRepository::list(&db, 50).await {
        Ok(r) => r,
        Err(e) => {
            println!("  {BAD} could not read run ledger: {e}");
            return;
        }
    };
    if runs.is_empty() {
        println!("  (no runs recorded)");
        return;
    }
    let failed: Vec<_> = runs
        .iter()
        .filter(|r| r.status == crate::domain::run::RunStatus::Failed)
        .collect();
    println!("  last {} turns, {} failed", runs.len(), failed.len());
    for r in failed.iter().take(3) {
        let msg = if r.error.is_empty() { "—" } else { &r.error };
        println!("  {BAD} {} {} {}", r.id, local_time(r.started_at), msg);
    }
}
