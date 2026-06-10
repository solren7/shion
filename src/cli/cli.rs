use clap::{Parser, Subcommand};

use super::{chat, gateway, service};

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
    /// With no action, runs in the foreground (this is what launchd invokes).
    Gateway {
        #[command(subcommand)]
        action: Option<GatewayAction>,
    },
}

#[derive(Subcommand)]
enum GatewayAction {
    /// Install and start the gateway under launchd (auto-restart on crash,
    /// start at login)
    Start,
    /// Stop the gateway and remove it from launchd
    Stop,
    /// Restart the gateway under launchd (regenerates the plist, so a
    /// reinstalled binary is picked up)
    Restart,
    /// Show launchd state for the gateway
    Status,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The database always lives in the config directory; use SHION_HOME to
    // point at a different home (e.g. for tests or a second instance).
    let db = crate::config::default_db_url();
    match cli.command {
        Commands::Chat => chat::run(&db).await,
        Commands::Gateway { action } => match action {
            None => {
                let schedule = crate::config::maintenance_schedule();
                gateway::run(&db, &schedule).await
            }
            Some(GatewayAction::Start) => service::start(),
            Some(GatewayAction::Stop) => service::stop(),
            Some(GatewayAction::Restart) => service::restart(),
            Some(GatewayAction::Status) => service::status(),
        },
    }
}
