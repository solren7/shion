mod agent;
mod cli;
mod config;
mod domain;
mod infra;
mod services;
mod tools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // cwd .env first (developer override), then ~/.shion/.env.
    // dotenvy never overwrites an already-set variable, so the first loader wins.
    let _ = dotenvy::dotenv();
    let _ = dotenvy::from_path(config::ensure_shion_home().join(".env"));
    cli::run().await
}
