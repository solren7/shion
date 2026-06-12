use std::sync::Arc;

use crate::{
    agent::{
        daemon::{Maintenance, ReminderSweep, ReviewSweep, Schedule},
        gateway::{Gateway, MaintenanceService},
    },
    cli::{approver::DenyApprover, wiring},
    domain::{approval::Approver, notify::Notifier, reminder::ReminderRepository},
    infra::{
        db::Db,
        feishu::{FeishuChannel, FeishuNotifier, FeishuSender},
        macos_notifier::MacosNotifier,
    },
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

    // Ingress channels, declared in ~/.shion/config.toml. Resolved before
    // the reminder sweep because a feishu `home_chat` takes over reminder
    // delivery from the local macOS notifier.
    let feishu = crate::config::feishu_config()?;
    let feishu_sender = feishu.as_ref().map(|cfg| {
        Arc::new(FeishuSender::new(
            cfg.app_id.clone(),
            cfg.app_secret.clone(),
        ))
    });

    let notifier: Arc<dyn Notifier> = match (&feishu, &feishu_sender) {
        (Some(cfg), Some(sender)) if cfg.home_chat.is_some() => Arc::new(FeishuNotifier::new(
            sender.clone(),
            cfg.home_chat.clone().unwrap(),
        )),
        _ => Arc::new(MacosNotifier),
    };

    let reminder_repo: Arc<dyn ReminderRepository> = db.clone();
    let reminder_sweep: Arc<dyn Maintenance> = Arc::new(ReminderSweep {
        reminders: reminder_repo,
        notifier,
    });

    let handler: Arc<dyn crate::domain::gateway::MessageHandler> = Arc::new(wired.runtime);
    let mut gateway = Gateway::new(handler)
        .with_maintenance(MaintenanceService {
            schedule: review_schedule,
            maintenance: review_sweep,
        })
        .with_maintenance(MaintenanceService {
            schedule: reminder_schedule,
            maintenance: reminder_sweep,
        });

    let mut channels = Vec::new();
    if let (Some(cfg), Some(sender)) = (&feishu, &feishu_sender) {
        gateway = gateway.add_channel(Box::new(FeishuChannel::new(sender.clone(), cfg)));
        channels.push("feishu");
    }

    println!(
        "Shion gateway — maintenance `{}`, reminders every minute, channels: {}. Ctrl-C to stop.\n",
        schedule_expr,
        if channels.is_empty() {
            "none".to_string()
        } else {
            channels.join(", ")
        }
    );

    gateway
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
