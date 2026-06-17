use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    agent::{
        daemon::{BriefingSweep, Maintenance, ReminderSweep, ReviewSweep, Schedule, TaskSweep},
        gateway::{Gateway, MaintenanceService},
        interaction::{ApprovalState, ChatApprover, GatewayDispatcher},
    },
    cli::wiring,
    domain::{
        approval::Approver, gateway::MessageHandler, home::HomeRepository, notify::Notifier,
        pairing::PairingRepository, reminder::ReminderRepository, repository::SessionRepository,
        task::TaskRepository, todo::SessionTodoRepository,
    },
    infra::{
        db::Db,
        feishu::{FeishuChannel, FeishuSender},
        home_notifier::{HomeNotifier, TextSender},
        kanban::KanbanDb,
        macos_notifier::MacosNotifier,
        telegram::{TelegramChannel, TelegramSender},
    },
};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler. Ingress channels will be declared in ~/.shion/config.toml and
/// wired here. Runs until Ctrl-C.
pub async fn run(db_url: &str, kanban_url: &str, schedule_expr: &str) -> anyhow::Result<()> {
    // Fail fast on a bad schedule before opening the db.
    let review_schedule = Schedule::parse(schedule_expr)?;
    let reminder_schedule = Schedule::parse("* * * * *")?;
    // Opt-in daily briefing: parse its schedule now so a typo fails at startup.
    let briefing_expr = crate::config::briefing_schedule();
    let briefing_schedule = briefing_expr.as_deref().map(Schedule::parse).transpose()?;

    let db = Arc::new(Db::connect(db_url).await?);
    // Durable tasks in their own file, separate from disposable session state.
    let kanban = Arc::new(KanbanDb::connect(kanban_url).await?);

    // Tool actions that need approval are gated over the chat channel: the
    // agent sends an approval prompt and waits for the user's `/approve` (or
    // `/deny`) reply. Shared with the dispatcher so the reply resolves the wait.
    let approvals = Arc::new(ApprovalState::new());
    let approver: Arc<dyn Approver> = Arc::new(ChatApprover::new(approvals.clone()));
    let wired = wiring::build(db.clone(), kanban.clone(), approver).await?;

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

    // A single home notifier delivers all proactive output (reminders, task
    // due notices, the shutdown notice). It resolves the home chat at
    // notify-time — a `/sethome` override (db) wins over the config `home_chat`
    // (feishu first, preserving the prior priority) — and degrades to the local
    // macOS notifier when no chat home resolves.
    let mut senders: HashMap<String, Arc<dyn TextSender>> = HashMap::new();
    if let Some(sender) = &feishu_sender {
        senders.insert("feishu".to_string(), sender.clone());
    }
    if let Some(sender) = &telegram_sender {
        senders.insert("telegram".to_string(), sender.clone());
    }
    let config_home = feishu
        .as_ref()
        .and_then(|cfg| cfg.home_chat.clone())
        .map(|chat| format!("feishu:{chat}"))
        .or_else(|| {
            telegram
                .as_ref()
                .and_then(|cfg| cfg.home_chat.clone())
                .map(|chat| format!("telegram:{chat}"))
        });
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let notifier: Arc<dyn Notifier> = Arc::new(HomeNotifier::new(
        senders,
        home_repo.clone(),
        config_home,
        Arc::new(MacosNotifier),
    ));

    let reminder_repo: Arc<dyn ReminderRepository> = db.clone();
    let reminder_sweep: Arc<dyn Maintenance> = Arc::new(ReminderSweep {
        reminders: reminder_repo,
        notifier: notifier.clone(),
    });
    let task_repo: Arc<dyn TaskRepository> = kanban.clone();
    let task_sweep: Arc<dyn Maintenance> = Arc::new(TaskSweep {
        tasks: task_repo,
        notifier: notifier.clone(),
    });

    let handler: Arc<dyn MessageHandler> = Arc::new(wired.runtime);
    let sessions: Arc<dyn SessionRepository> = db.clone();
    let todos: Arc<dyn SessionTodoRepository> = db.clone();
    let dispatcher = Arc::new(GatewayDispatcher::new(
        handler, approvals, sessions, home_repo, todos,
    ));
    let mut gateway = Gateway::new(dispatcher)
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

    // Daily briefing — only when the user opted in with `briefing_schedule`.
    // Reads tasks + memories, composes on the aux LLM, delivers via the same
    // home notifier as reminders.
    if let Some(schedule) = briefing_schedule {
        let briefing_sweep: Arc<dyn Maintenance> = Arc::new(BriefingSweep {
            tasks: kanban.clone(),
            memories: wired.memories.clone(),
            llm: wired.aux_llm.clone(),
            notifier: notifier.clone(),
        });
        gateway = gateway.with_maintenance(MaintenanceService {
            schedule,
            maintenance: briefing_sweep,
        });
    }

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

    // Send the offline notice on shutdown only when a chat channel exists; with
    // none, the home notifier would fall back to a macOS popup, which is noise
    // on a foreground Ctrl-C.
    if !channels.is_empty() {
        gateway = gateway.with_shutdown_notice(notifier);
    }

    println!(
        "Shion gateway — maintenance `{}`, reminders every minute, briefing {}, channels: {}. Ctrl-C to stop.\n",
        schedule_expr,
        briefing_expr
            .as_deref()
            .map(|e| format!("`{e}`"))
            .unwrap_or_else(|| "off".to_string()),
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
