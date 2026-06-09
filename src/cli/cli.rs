use clap::{Parser, Subcommand};

use super::{chat, daemon, gateway};

#[derive(Parser)]
#[command(name = "shion", about = "Personal agent framework")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an interactive chat session
    Chat {
        /// SQLite database URL
        #[arg(long, default_value = "sqlite:./shion.db")]
        db: String,
    },
    /// Run the background maintenance daemon on a cron schedule
    Daemon {
        /// SQLite database URL
        #[arg(long, default_value = "sqlite:./shion.db")]
        db: String,
        /// 5-field Unix cron expression for the maintenance schedule
        /// (default: hourly)
        #[arg(long, default_value = "0 * * * *")]
        schedule: String,
    },
    /// Run the always-on gateway: maintenance scheduler + message ingress
    Gateway {
        /// SQLite database URL
        #[arg(long, default_value = "sqlite:./shion.db")]
        db: String,
        /// 5-field Unix cron expression for the maintenance schedule
        #[arg(long, default_value = "0 * * * *")]
        schedule: String,
        /// Unix socket path for message ingress
        /// (default: $SHION_GATEWAY_SOCKET or ~/.shion/gateway.sock)
        #[arg(long)]
        socket: Option<String>,
    },
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Chat { db } => chat::run(&db).await,
        Commands::Daemon { db, schedule } => daemon::run(&db, &schedule).await,
        Commands::Gateway {
            db,
            schedule,
            socket,
        } => gateway::run(&db, &schedule, socket.as_deref()).await,
    }
}
