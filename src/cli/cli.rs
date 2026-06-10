use clap::{Parser, Subcommand};

use super::{chat, gateway};

#[derive(Parser)]
#[command(name = "shion", about = "Personal agent framework")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an interactive chat session
    Chat,
    /// Run the always-on gateway: maintenance scheduler (and, later,
    /// config-declared ingress channels). Maintenance cron comes from
    /// `schedule` in ~/.shion/config.toml (or SHION_SCHEDULE); default hourly.
    Gateway,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The database always lives in the config directory; use SHION_HOME to
    // point at a different home (e.g. for tests or a second instance).
    let db = crate::config::default_db_url();
    match cli.command {
        Commands::Chat => chat::run(&db).await,
        Commands::Gateway => {
            let schedule = crate::config::maintenance_schedule();
            gateway::run(&db, &schedule).await
        }
    }
}
