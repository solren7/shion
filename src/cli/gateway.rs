use std::sync::Arc;

use crate::{
    agent::{
        daemon::{Maintenance, ReviewSweep, Schedule},
        gateway::{Gateway, MaintenanceService},
    },
    cli::{approver::DenyApprover, wiring},
    domain::{approval::Approver, gateway::MessageHandler},
    infra::db::Db,
};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler. Ingress channels will be declared in ~/.shion/config.toml and
/// wired here. Runs until Ctrl-C.
pub async fn run(db_url: &str, schedule_expr: &str) -> anyhow::Result<()> {
    // Fail fast on a bad schedule before opening the db.
    let schedule = Schedule::parse(schedule_expr)?;

    let db = Arc::new(Db::connect(db_url).await?);

    // The gateway is unattended: deny approval-gated tool actions rather than
    // block on a stdin prompt no one will answer.
    let approver: Arc<dyn Approver> = Arc::new(DenyApprover);
    let wired = wiring::build(db.clone(), approver).await?;

    let maintenance: Arc<dyn Maintenance> = Arc::new(ReviewSweep {
        sessions: wired.sessions.clone(),
        reviewer: wired.reviewer.clone(),
    });

    let handler: Arc<dyn MessageHandler> = Arc::new(wired.runtime);
    let gateway = Gateway::new(handler).with_maintenance(MaintenanceService {
        schedule,
        maintenance,
    });

    println!(
        "Shion gateway — maintenance `{}`. Ctrl-C to stop.\n",
        schedule_expr
    );

    gateway
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
