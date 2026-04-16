mod agent;
mod cli;
mod domain;
mod infra;
mod services;
mod tools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
