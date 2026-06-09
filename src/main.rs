mod agent;
mod cli;
mod domain;
mod infra;
mod services;
mod tools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment variables from a local .env file if present.
    let _ = dotenvy::dotenv();
    cli::run().await
}
