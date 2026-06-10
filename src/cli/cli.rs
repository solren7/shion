use clap::{Parser, Subcommand};

use super::{chat, gateway, inspect, model, service};

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
    /// Inspect scheduled reminders (recurring crons and one-shots)
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Inspect stored chat sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show or switch the active LLM provider and model
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// Show the current provider/model and list available providers
    List,
    /// Switch provider (and optionally model); persists to config.toml
    Set {
        /// Provider: deepseek | openai | anthropic | openrouter
        provider: String,
        /// Model id (defaults to the provider's default model)
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum CronAction {
    /// List pending reminders with their schedules and next fire times
    List,
}

#[derive(Subcommand)]
enum SessionAction {
    /// List stored sessions with creation time and message counts
    List,
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
        Commands::Cron { action } => match action {
            CronAction::List => inspect::cron_list(&db).await,
        },
        Commands::Session { action } => match action {
            SessionAction::List => inspect::session_list(&db).await,
        },
        Commands::Model { action } => match action {
            ModelAction::List => model::list(),
            ModelAction::Set { provider, model } => model::set(&provider, model),
        },
    }
}
