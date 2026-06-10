use std::sync::Arc;

use crate::{
    agent::{
        daemon::{Maintenance, ReminderSweep, ReviewSweep, Schedule},
        gateway::{Gateway, MaintenanceService},
    },
    cli::{approver::DenyApprover, wiring},
    domain::{approval::Approver, reminder::ReminderRepository},
    infra::{db::Db, macos_notifier::MacosNotifier},
};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler. Ingress channels will be declared in ~/.shion/config.toml and
/// wired here. Runs until Ctrl-C.
pub async fn run(db_url: &str, schedule_expr: &str) -> anyhow::Result<()> {
    // Fail fast on a bad schedule before opening the db.
    let review_schedule = Schedule::parse(schedule_expr)?;
    let reminder_schedule = Schedule::parse("* * * * *")?;

    let db = Arc::new(Db::connect(db_url).await?);

    // The gateway is unattended: deny approval-gated tool actions rather than
    // block on a stdin prompt no one will answer.
    let approver: Arc<dyn Approver> = Arc::new(DenyApprover);
    let wired = wiring::build(db.clone(), approver).await?;

    let review_sweep: Arc<dyn Maintenance> = Arc::new(ReviewSweep {
        sessions: wired.sessions.clone(),
        reviewer: wired.reviewer.clone(),
    });

    let reminder_repo: Arc<dyn ReminderRepository> = db.clone();
    let reminder_sweep: Arc<dyn Maintenance> = Arc::new(ReminderSweep {
        reminders: reminder_repo,
        notifier: Arc::new(MacosNotifier),
    });

    let handler: Arc<dyn crate::domain::gateway::MessageHandler> = Arc::new(wired.runtime);
    let gateway = Gateway::new(handler)
        .with_maintenance(MaintenanceService {
            schedule: review_schedule,
            maintenance: review_sweep,
        })
        .with_maintenance(MaintenanceService {
            schedule: reminder_schedule,
            maintenance: reminder_sweep,
        });

    println!(
        "Shion gateway — maintenance `{}`, reminders every minute. Ctrl-C to stop.\n",
        schedule_expr
    );

    gateway
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
