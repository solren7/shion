mod cli;
#[tokio::main]
async fn main() -> toasty::Result<()> {
    cli::run().await
}
