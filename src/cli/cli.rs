use clap::Command;

use super::init;

pub async fn run() -> toasty::Result<()> {
    let matches = Command::new("shion")
        .about("Program entry commands")
        .subcommand(Command::new("init").about("Initialize the local database and seed sample data"))
        .get_matches();

    match matches.subcommand() {
        Some(("init", _)) => init::run().await,
        _ => Ok(()),
    }
}
