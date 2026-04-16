use clap::{Parser, Subcommand};

use super::chat;

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
        /// Session identifier (reuse to continue a prior conversation)
        #[arg(long, default_value = "default")]
        session: String,
    },
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Chat { db, session } => chat::run(&db, &session).await,
    }
}
