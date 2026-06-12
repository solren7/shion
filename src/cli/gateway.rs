use std::sync::Arc;

use crate::{
    agent::{
        daemon::{Maintenance, ReminderSweep, ReviewSweep, Schedule, TaskSweep},
        gateway::{Gateway, MaintenanceService},
    },
    cli::{approver::DenyApprover, wiring},
    domain::{
        approval::Approver, notify::Notifier, pairing::PairingRepository,
        reminder::ReminderRepository, task::TaskRepository,
    },
    infra::{
        db::Db,
        feishu::{FeishuChannel, FeishuNotifier, FeishuSender},
        macos_notifier::MacosNotifier,
        telegram::{TelegramChannel, TelegramNotifier, TelegramSender},
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
    // the reminder sweep because a channel `home_chat` takes over reminder
    // delivery from the local macOS notifier (feishu wins if both set one).
    let feishu = crate::config::feishu_config()?;
    let feishu_sender = feishu.as_ref().map(|cfg| {
        Arc::new(FeishuSender::new(
            cfg.app_id.clone(),
            cfg.app_secret.clone(),
        ))
    });
    let telegram = crate::config::telegram_config()?;
    let telegram_sender = telegram
        .as_ref()
        .map(|cfg| Arc::new(TelegramSender::new(cfg.bot_token.clone())));

    let notifier: Arc<dyn Notifier> = if let (Some(cfg), Some(sender)) = (&feishu, &feishu_sender)
        && let Some(home) = &cfg.home_chat
    {
        Arc::new(FeishuNotifier::new(sender.clone(), home.clone()))
    } else if let (Some(cfg), Some(sender)) = (&telegram, &telegram_sender)
        && let Some(home) = &cfg.home_chat
    {
        Arc::new(TelegramNotifier::new(sender.clone(), home.clone()))
    } else {
        Arc::new(MacosNotifier)
    };

    let reminder_repo: Arc<dyn ReminderRepository> = db.clone();
    let reminder_sweep: Arc<dyn Maintenance> = Arc::new(ReminderSweep {
        reminders: reminder_repo,
        notifier: notifier.clone(),
    });
    let task_repo: Arc<dyn TaskRepository> = db.clone();
    let task_sweep: Arc<dyn Maintenance> = Arc::new(TaskSweep {
        tasks: task_repo,
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
        })
        .with_maintenance(MaintenanceService {
            schedule: Schedule::parse("* * * * *")?,
            maintenance: task_sweep,
        });

    // Senders outside `allow_from` go through the pairing handshake; the
    // pairing store is shared with the `shion pair` CLI via the same db.
    let pairings: Arc<dyn PairingRepository> = db.clone();
    let mut channels = Vec::new();
    if let (Some(cfg), Some(sender)) = (&feishu, &feishu_sender) {
        gateway = gateway.add_channel(Box::new(FeishuChannel::new(
            sender.clone(),
            cfg,
            pairings.clone(),
        )));
        channels.push("feishu");
    }
    if let (Some(cfg), Some(sender)) = (&telegram, &telegram_sender) {
        gateway = gateway.add_channel(Box::new(TelegramChannel::new(
            sender.clone(),
            cfg,
            pairings.clone(),
        )));
        channels.push("telegram");
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
