mod agent;
mod cli;
mod config;
mod domain;
mod infra;
mod services;
mod tools;
mod tui;

// Global allocator: mimalloc — installed by turso's default `mimalloc`
// feature (via toasty-driver-turso), not declared here. Declaring our own
// `#[global_allocator]` is a hard link error while that feature is on:
//   error: the `#[global_allocator]` in this crate conflicts with global allocator in: turso
// If turso/toasty ever stop providing one, declare mimalloc here explicitly.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // cwd .env first (developer override), then ~/.shion/.env.
    // dotenvy never overwrites an already-set variable, so the first loader wins.
    let _ = dotenvy::dotenv();
    let _ = dotenvy::from_path(config::ensure_shion_home().join(".env"));
    init_tracing();
    cli::run().await
}

/// Install the tracing subscriber. Without this every `info!`/`warn!`/`debug!`
/// in the codebase is a no-op (events emitted, no consumer). Logs go to stderr
/// (launchd captures the gateway's via the plist's `StandardErrorPath`); the
/// level is controlled by `SHION_LOG` (e.g. `SHION_LOG=debug`), defaulting to
/// `info`. `try_init` so a second call (e.g. in tests) is a harmless no-op.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    // Default: shion's own logs at info, but mute two sources of noise —
    // toasty's per-connect schema chatter, and rig's `prompt_request` INFO
    // events, which log every tool call's *full result* verbatim (a wall of
    // text for any list-returning tool). shion's own `tool ok` span line
    // (name/seq/elapsed, no result) still records each call concisely.
    // `SHION_LOG` overrides the whole filter (e.g. `debug` to see everything).
    //
    // For every subcommand except the gateway, toasty's connection-pool ERROR
    // lines are muted too: a CLI that can't open the db (it's locked by the
    // running gateway) already surfaces that failure in its own output, and
    // the raw pool spam would just repeat it. The always-on gateway keeps
    // them — there they are real diagnostics, not an expected condition.
    let pool_noise = if std::env::args().nth(1).as_deref() == Some("gateway") {
        ""
    } else {
        ",toasty::db::pool=off"
    };
    let filter = EnvFilter::try_from_env("SHION_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("info,toasty=warn,rig_core=warn{pool_noise}")));

    // The chat TUI owns the terminal (alternate screen): a stderr log line
    // would tear the display, so route tracing to a file for that mode.
    // Falls back to stderr if the log file can't be opened.
    //
    // The gateway tees stderr with a daily-rotated file in ~/.shion/logs
    // (`gateway.YYYY-MM-DD.log`, 30 files kept, older ones auto-deleted):
    // stderr keeps `docker logs` / launchd capture working, the dated files
    // are the durable month of history `shion logs` reads.
    let writer = if will_run_tui() {
        open_tui_log()
            .map(|f| fmt::writer::BoxMakeWriter::new(std::sync::Mutex::new(f)))
            .unwrap_or_else(|| fmt::writer::BoxMakeWriter::new(std::io::stderr))
    } else if std::env::args().nth(1).as_deref() == Some("gateway") {
        match open_gateway_log() {
            Some(daily) => {
                use tracing_subscriber::fmt::writer::MakeWriterExt;
                fmt::writer::BoxMakeWriter::new((std::io::stderr).and(daily))
            }
            None => fmt::writer::BoxMakeWriter::new(std::io::stderr),
        }
    } else {
        fmt::writer::BoxMakeWriter::new(std::io::stderr)
    };
    let _ = fmt().with_env_filter(filter).with_writer(writer).try_init();
}

/// Daily-rotating gateway log under `~/.shion/logs`, one file per day
/// (`gateway.YYYY-MM-DD.log`), a month of history kept — the appender deletes
/// older files itself. `None` (e.g. unwritable dir) degrades to stderr-only.
fn open_gateway_log() -> Option<tracing_appender::rolling::RollingFileAppender> {
    const KEEP_DAYS: usize = 30;
    let dir = config::ensure_shion_home().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("gateway")
        .filename_suffix("log")
        .max_log_files(KEEP_DAYS)
        .build(dir)
        .ok()
}

/// Whether this invocation will run the full-screen chat TUI (`shion chat` /
/// `shion session resume` on a TTY — off a TTY they error out early instead;
/// see `cli/app.rs::require_terminal`) — checked here because the tracing
/// writer must be chosen before the CLI parses.
fn will_run_tui() -> bool {
    use std::io::IsTerminal;
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str);
    let is_chat = sub == Some("chat")
        || (sub == Some("session") && args.get(2).map(String::as_str) == Some("resume"));
    is_chat && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Append-mode log file for TUI sessions (`~/.shion/logs/chat-tui.log`).
fn open_tui_log() -> Option<std::fs::File> {
    let dir = config::ensure_shion_home().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("chat-tui.log"))
        .ok()
}
