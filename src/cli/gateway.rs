use std::{path::PathBuf, sync::Arc};

use crate::{
    agent::{
        daemon::{Maintenance, ReviewSweep, Schedule},
        gateway::{Gateway, MaintenanceService},
    },
    cli::{approver::DenyApprover, wiring},
    domain::{approval::Approver, gateway::MessageHandler},
    infra::{db::Db, unix_channel::UnixSocketChannel},
};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler and a unix-socket ingress channel. Runs until Ctrl-C.
pub async fn run(
    db_url: &str,
    schedule_expr: &str,
    socket_path: Option<&str>,
) -> anyhow::Result<()> {
    // Fail fast on a bad schedule before opening the db or binding the socket.
    let schedule = Schedule::parse(schedule_expr)?;
    let path = resolve_socket_path(socket_path)?;

    let db = Arc::new(Db::connect(db_url).await?);

    // The gateway is unattended: deny approval-gated tool actions rather than
    // block on a stdin prompt no one will answer.
    let approver: Arc<dyn Approver> = Arc::new(DenyApprover);
    let wired = wiring::build(db.clone(), approver).await?;

    let maintenance: Arc<dyn Maintenance> = Arc::new(ReviewSweep {
        sessions: wired.sessions.clone(),
        reviewer: wired.reviewer.clone(),
    });

    // Binding doubles as the single-instance guard.
    let channel = UnixSocketChannel::bind(path.clone()).await?;

    let handler: Arc<dyn MessageHandler> = Arc::new(wired.runtime);
    let gateway = Gateway::new(handler)
        .with_maintenance(MaintenanceService {
            schedule,
            maintenance,
        })
        .add_channel(Box::new(channel));

    println!(
        "Shion gateway — socket `{}`, maintenance `{}`. Ctrl-C to stop.\n",
        path.display(),
        schedule_expr
    );

    gateway
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// Resolve the ingress socket path: explicit flag, else `$SHION_GATEWAY_SOCKET`,
/// else `~/.shion/gateway.sock`.
fn resolve_socket_path(socket_path: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(path) = socket_path {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("SHION_GATEWAY_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME not set; pass --socket to choose a gateway socket"))?;
    Ok(PathBuf::from(home).join(".shion").join("gateway.sock"))
}
