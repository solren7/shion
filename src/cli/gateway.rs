use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    agent::{
        daemon::{
            BriefingSweep, DreamSweep, Maintenance, ReminderSweep, ReviewSweep, Schedule,
            TaskSweep, WorkdayGated,
        },
        gateway::{Gateway, MaintenanceService},
        interaction::{ApprovalState, ChatApprover, GatewayDispatcher},
    },
    cli::wiring,
    config::{ConfigSnapshot, IssueSeverity},
    domain::{
        approval::Approver,
        gateway::{MessageHandler, WeChatLogin},
        home::HomeRepository,
        notify::Notifier,
        pairing::PairingRepository,
        reminder::ReminderRepository,
        repository::SessionRepository,
        run::RunRepository,
        task::TaskRepository,
        todo::SessionTodoRepository,
    },
    infra::{
        messaging::{
            api::ApiChannel,
            feishu::{FeishuChannel, FeishuSender},
            home_notifier::{HomeNotifier, TextSender},
            homeassistant::HomeAssistantChannel,
            macos_notifier::MacosNotifier,
            telegram::{TelegramChannel, TelegramSender},
            wechat::{WeChatChannel, WeChatQrLogin, WeChatSender, build_bot},
        },
        persistence::{db::Db, kanban::KanbanDb},
        workday::HolidayCalendar,
    },
    services::operator_control::actions::OperatorActions,
};

/// Run the always-on gateway: a persistent process hosting the maintenance
/// scheduler and the config-declared ingress channels. Runs until Ctrl-C.
/// Everything is read from the caller's one resolved `config` snapshot.
pub async fn run(config: &ConfigSnapshot) -> anyhow::Result<()> {
    // The gateway hosts every surface, so any fatal config issue (unusable
    // model, enabled-but-credential-less channel) stops startup here, before
    // the db is opened. Warnings are logged and tolerated.
    config.validate_gateway()?;
    for issue in &config.report.issues {
        if issue.severity == IssueSeverity::Warning {
            tracing::warn!(path = issue.path, "{}", issue.message);
        }
    }
    let rt = &config.runtime;

    // Fail fast on a bad schedule before opening the db.
    let schedule_expr = rt.maintenance_schedule.as_str();
    let review_schedule = Schedule::parse(schedule_expr)?;
    let reminder_schedule = Schedule::parse("* * * * *")?;
    // Opt-in daily briefing: parse its schedule now so a typo fails at startup.
    let briefing_expr = &rt.briefing_schedule;
    let briefing_schedule = briefing_expr.as_deref().map(Schedule::parse).transpose()?;
    // Usage-driven memory consolidation ("dreaming", on by default); parse now
    // so a typo fails at startup.
    let dream_expr = &rt.dream_schedule;
    let dream_schedule = dream_expr.as_deref().map(Schedule::parse).transpose()?;

    let db = Arc::new(Db::connect(&rt.db_url).await?);
    // Reconcile runs left `Running` by a crashed earlier process (launchd
    // restarts the gateway): flip them to failed/"interrupted" so the ledger is
    // truthful. Best-effort — a reconciliation failure must not block startup.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match RunRepository::reconcile_interrupted(&*db, now).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "reconciled interrupted runs on startup"),
        Err(error) => tracing::warn!(%error, "failed to reconcile interrupted runs"),
    }
    // Durable tasks in their own file, separate from disposable session state.
    let kanban = Arc::new(KanbanDb::connect(&rt.kanban_db_url).await?);

    // Tool actions that need approval are gated over the chat channel: the
    // agent sends an approval prompt and waits for the user's `/approve` (or
    // `/deny`) reply. Shared with the dispatcher so the reply resolves the wait.
    let approvals = Arc::new(ApprovalState::new());
    let approver: Arc<dyn Approver> = Arc::new(ChatApprover::new(approvals.clone()));
    let wired = wiring::build(config, db.clone(), kanban.clone(), approver).await?;

    let review_sweep: Arc<dyn Maintenance> = Arc::new(ReviewSweep {
        review: wired.review.clone(),
    });

    // Ingress channels, from the snapshot (validate_gateway above already
    // refused any enabled-but-misconfigured one). Resolved before the reminder
    // sweep because a channel `home_chat` takes over reminder delivery from
    // the local macOS notifier (feishu wins if both set one).
    let feishu = rt.feishu.ready();
    let feishu_sender = feishu.map(|cfg| {
        Arc::new(FeishuSender::new(
            cfg.app_id.clone(),
            cfg.app_secret.clone(),
        ))
    });
    let telegram = rt.telegram.ready();
    let telegram_sender = telegram.map(|cfg| Arc::new(TelegramSender::new(cfg.bot_token.clone())));
    // WeChat shares one bot instance between its sender and channel so the
    // channel's poll loop populates the context-token map the sender reads.
    let wechat = rt.wechat.ready();
    let wechat_cred_path = crate::config::wechat_cred_path();
    // HTTP API channel (OpenAI-compatible + dashboard); always on.
    let api = rt
        .api
        .ready()
        .ok_or_else(|| anyhow::anyhow!("api channel misconfigured"))?;
    let wechat_bot = wechat.map(|_| build_bot(&wechat_cred_path));
    let wechat_sender = wechat_bot
        .as_ref()
        .map(|bot| Arc::new(WeChatSender::new(bot.clone())));
    // Shared between the login coordinator (`/wechat login`) and the channel:
    // a successful login pulses this so the channel starts polling without a
    // restart.
    let wechat_ready = Arc::new(tokio::sync::Notify::new());
    let wechat_provisioning = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wechat_login: Option<Arc<dyn WeChatLogin>> = wechat_bot.as_ref().map(|bot| {
        Arc::new(WeChatQrLogin::new(
            wechat_cred_path.clone(),
            wechat_ready.clone(),
            bot.clone(),
            wechat_provisioning.clone(),
        )) as Arc<dyn WeChatLogin>
    });

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
    if let Some(sender) = &wechat_sender {
        senders.insert("wechat".to_string(), sender.clone());
    }
    let config_home = feishu
        .and_then(|cfg| cfg.home_chat.clone())
        .map(|chat| format!("feishu:{chat}"))
        .or_else(|| {
            telegram
                .and_then(|cfg| cfg.home_chat.clone())
                .map(|chat| format!("telegram:{chat}"))
        })
        .or_else(|| {
            wechat
                .and_then(|cfg| cfg.home_chat.clone())
                .map(|chat| format!("wechat:{chat}"))
        });
    let home_repo: Arc<dyn HomeRepository> = db.clone();
    let notifier: Arc<dyn Notifier> = Arc::new(HomeNotifier::new(
        senders,
        home_repo.clone(),
        config_home.clone(),
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
        handler.clone(),
        approvals,
        wired.clarify.clone(),
        sessions,
        home_repo,
        todos,
        wechat_login,
        db.clone(),
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
        let mut briefing_sweep: Arc<dyn Maintenance> = Arc::new(BriefingSweep {
            tasks: kanban.clone(),
            memories: wired.memories.clone(),
            llm: wired.aux_llm.clone(),
            notifier: notifier.clone(),
            // Tool-capable agent turn (read-only tools + unattended policy
            // gating); the sweep degrades to the plain compose on error.
            runtime: Some(wired.briefing_runtime.clone()),
        });
        // Opt-in: only fire on Chinese working days (statutory holidays and
        // 调休-adjusted weekends respected). The calendar is built only when
        // gating is on, so the holiday API is never touched otherwise.
        if rt.briefing_workdays_only {
            let calendar = Arc::new(HolidayCalendar::new(crate::config::workday_cache_dir()));
            briefing_sweep = Arc::new(WorkdayGated {
                inner: briefing_sweep,
                calendar,
            });
        }
        gateway = gateway.with_maintenance(MaintenanceService {
            schedule,
            maintenance: briefing_sweep,
        });
    }

    // Dreaming — only when the user opted in with `dream_schedule`. Reads the
    // whole memory library, promotes well-recalled candidates to active, and
    // archives ones that never earned a recall. Never auto-pins.
    if let Some(schedule) = dream_schedule {
        let dream_sweep: Arc<dyn Maintenance> = Arc::new(DreamSweep {
            memories: wired.memories.clone(),
        });
        gateway = gateway.with_maintenance(MaintenanceService {
            schedule,
            maintenance: dream_sweep,
        });
    }

    // Senders outside `allow_from` go through the pairing handshake; the
    // pairing store is shared with the `komo pair` CLI via the same db.
    let pairings: Arc<dyn PairingRepository> = db.clone();
    let mut channels = Vec::new();
    if let (Some(cfg), Some(sender)) = (feishu, &feishu_sender) {
        gateway = gateway.add_channel(Box::new(FeishuChannel::new(
            sender.clone(),
            cfg,
            pairings.clone(),
        )));
        channels.push("feishu");
    }
    if let (Some(cfg), Some(sender)) = (telegram, &telegram_sender) {
        gateway = gateway.add_channel(Box::new(TelegramChannel::new(
            sender.clone(),
            cfg,
            pairings.clone(),
        )));
        channels.push("telegram");
    }
    if let (Some(cfg), Some(bot)) = (wechat, &wechat_bot) {
        gateway = gateway.add_channel(Box::new(WeChatChannel::new(
            bot.clone(),
            cfg,
            wechat_cred_path.clone(),
            wechat_ready.clone(),
            wechat_provisioning.clone(),
            pairings.clone(),
        )));
        channels.push("wechat");
    }
    // Whether an interactive chat channel exists — gates the shutdown notice
    // (HA is event-only, so an HA-only gateway must not pop a macOS notice).
    let has_chat_channel = !channels.is_empty();

    // Home Assistant event ingress: forwards filtered `state_changed` events to
    // the agent. No pairing — it is a trusted local integration keyed by
    // HASS_TOKEN, not a chat with arbitrary senders.
    if let Some(cfg) = rt.homeassistant_channel.ready() {
        gateway = gateway.add_channel(Box::new(HomeAssistantChannel::new(cfg)));
        channels.push("homeassistant");
    }

    // HTTP API channel: serves the local dashboard UI and any OpenAI-compatible
    // client. It calls the handler directly (synchronous request/response), so
    // it needs the repositories rather than just the dispatcher. Added last so
    // `/api/status` can report every other channel that came up.
    // The api channel is **always on** (see `config::api_config`): it is how the
    // local `komo` CLI reaches this gateway while we hold the exclusive Turso db
    // lock. By default it is loopback-only on an ephemeral port (published in the
    // rendezvous file); `[channels.api] enabled = true` widens it to an external
    // bind/port for Open WebUI / the dashboard.
    {
        let enabled = {
            let mut names: Vec<String> = channels.iter().map(|s| s.to_string()).collect();
            names.push("api".to_string());
            names
        };
        // The operator use cases behind the /api/* routes — the same shared
        // definitions the CLI's direct adapter runs, here over the gateway's
        // repositories.
        let actions = Arc::new(OperatorActions {
            sessions: db.clone(),
            messages: db.clone(),
            tasks: kanban.clone(),
            memories: wired.memories.clone(),
            runs: db.clone(),
            reminders: db.clone(),
            skills: wired.skills.clone(),
            pairings: pairings.clone(),
            home: db.clone(),
        });
        gateway = gateway.add_channel(Box::new(ApiChannel::new(
            api,
            handler.clone(),
            actions,
            enabled,
            config_home.clone(),
        )));
        channels.push("api");
    }

    // Send the offline notice on shutdown only when a chat channel exists; with
    // none, the home notifier would fall back to a macOS popup, which is noise
    // on a foreground Ctrl-C.
    if has_chat_channel {
        gateway = gateway.with_shutdown_notice(notifier);
    }

    let fmt_opt = |e: &Option<String>| {
        e.as_deref()
            .map(|e| format!("`{e}`"))
            .unwrap_or_else(|| "off".to_string())
    };
    println!(
        "Komo gateway — maintenance `{}`, reminders every minute, briefing {}, dreaming {}, channels: {}. Ctrl-C to stop.\n",
        schedule_expr,
        fmt_opt(briefing_expr),
        fmt_opt(dream_expr),
        if channels.is_empty() {
            "none".to_string()
        } else {
            channels.join(", ")
        }
    );

    gateway.run(shutdown_signal()).await
}

/// Resolve when the process is asked to stop. Catches both Ctrl-C (SIGINT, the
/// foreground case) and SIGTERM — the signal `launchctl bootout` sends when
/// `komo gateway stop`/`restart` tears the job down. Without the SIGTERM arm
/// launchd would kill the process before the shutdown notice could be sent.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(error) => {
                tracing::warn!(%error, "failed to install SIGTERM handler; relying on Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
