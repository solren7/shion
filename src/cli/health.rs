//! `shion health` — liveness probe for the running gateway.
//!
//! Reads the rendezvous file and hits the api channel's unauthenticated
//! `/health`. The exit code carries the verdict (0 = healthy, non-zero =
//! anything else), which is the whole point: this is the Docker
//! `HEALTHCHECK` command, so a wedged gateway turns the container unhealthy
//! instead of sitting there "Running". Also handy interactively — it answers
//! in one line where `shion doctor` gives the full report.

use crate::infra::{gateway_client::GatewayClient, rendezvous};

pub async fn run() -> anyhow::Result<()> {
    let Some(info) = rendezvous::read() else {
        anyhow::bail!(
            "unhealthy: no gateway advertised at {} (not running, or it crashed \
             before writing the rendezvous file)",
            rendezvous::path().display()
        );
    };
    let http = reqwest::Client::new();
    let base = info.base_url();
    if GatewayClient::health_ok(&http, &base).await {
        println!("healthy: gateway pid {} answering at {base}", info.pid);
        Ok(())
    } else {
        // Stale rendezvous: the file exists but nothing answers — a crashed
        // gateway leaves this behind (the file is only removed on graceful
        // shutdown).
        anyhow::bail!(
            "unhealthy: gateway advertised at {base} (pid {}) but /health does not answer",
            info.pid
        )
    }
}
