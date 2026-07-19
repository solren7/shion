//! The always-on gateway: a persistent process that hosts background services
//! and ingress channels, mirroring hermes-agent's gateway.
//!
//! hermes runs its cron ticker as a thread *inside* the gateway because the
//! gateway is already the always-on process; ingress (Telegram/Slack adapters)
//! lives there too. komo follows that shape:
//!
//!   - **background services** — the maintenance scheduler from `daemon.rs`,
//!     run as a tokio task inside the gateway (its only host).
//!   - **ingress channels** — pluggable `Channel`s that route inbound messages
//!     through a `MessageHandler` (the wired `AgentRuntime`). None are wired
//!     today; they will be declared in ~/.komo/config.toml and constructed
//!     from there.
//!
//! All tasks share one `watch` shutdown signal; on Ctrl-C the gateway flips it
//! and joins everything. The OS-level supervisor that keeps the *process* alive
//! across crashes/reboots (launchd `KeepAlive` / systemd `Restart=always`) is
//! still deferred — this is the in-process host only.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::{
    agent::{
        daemon::{Maintenance, Schedule, supervise},
        interaction::GatewayDispatcher,
        runtime::AgentRuntime,
    },
    domain::{gateway::MessageHandler, notify::Notifier},
};

/// How long the shutdown notice may take before we stop waiting and shut down.
const SHUTDOWN_NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// `AgentRuntime` is the production message handler: an inbound message is just
/// another turn in its session lifecycle.
#[async_trait]
impl MessageHandler for AgentRuntime {
    async fn handle(&self, session_id: &str, input: String) -> anyhow::Result<String> {
        self.handle_input(session_id, input).await
    }
}

/// A long-lived ingress: accepts inbound messages over some transport and routes
/// them through `dispatcher` (which classifies control commands and runs agent
/// turns), until `shutdown` flips to `true`.
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn serve(
        &self,
        dispatcher: Arc<GatewayDispatcher>,
        shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()>;
}

/// The scheduled background work the gateway hosts (the `daemon.rs` supervisor
/// loop, reused verbatim).
pub struct MaintenanceService {
    pub schedule: Schedule,
    pub maintenance: Arc<dyn Maintenance>,
}

pub struct Gateway {
    dispatcher: Arc<GatewayDispatcher>,
    channels: Vec<Box<dyn Channel>>,
    services: Vec<MaintenanceService>,
    shutdown_notice: Option<Arc<dyn Notifier>>,
}

impl Gateway {
    pub fn new(dispatcher: Arc<GatewayDispatcher>) -> Self {
        Self {
            dispatcher,
            channels: Vec::new(),
            services: Vec::new(),
            shutdown_notice: None,
        }
    }

    /// Notify the home channel that the gateway is going offline as part of
    /// shutdown (mirrors hermes' offline notice). No-op when unset.
    pub fn with_shutdown_notice(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.shutdown_notice = Some(notifier);
        self
    }

    pub fn add_channel(mut self, channel: Box<dyn Channel>) -> Self {
        self.channels.push(channel);
        self
    }

    /// Register a background maintenance service. Can be called multiple times
    /// to run independent services concurrently (each with its own schedule and
    /// circuit breaker).
    pub fn with_maintenance(mut self, service: MaintenanceService) -> Self {
        self.services.push(service);
        self
    }

    /// Start every service and channel, then run until `shutdown` resolves
    /// (e.g. Ctrl-C). On shutdown, signal all tasks and wait for them to finish.
    pub async fn run<S>(self, shutdown: S) -> anyhow::Result<()>
    where
        S: std::future::Future<Output = ()>,
    {
        let Gateway {
            dispatcher,
            channels,
            services,
            shutdown_notice,
        } = self;
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut handles = Vec::new();

        for service in services {
            let mut rx = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                let stop = async move {
                    let _ = rx.changed().await;
                };
                if let Err(error) = supervise(&service.schedule, service.maintenance, stop).await {
                    error!(%error, "maintenance service stopped");
                }
            }));
        }

        for channel in channels {
            let dispatcher = dispatcher.clone();
            let rx = stop_rx.clone();
            let name = channel.name().to_string();
            handles.push(tokio::spawn(async move {
                if let Err(error) = channel.serve(dispatcher, rx).await {
                    error!(%error, channel = %name, "channel stopped");
                }
            }));
        }

        info!("gateway running");
        shutdown.await;
        info!("shutdown signal received; stopping gateway");

        // Tell the home channel we're going offline before tearing down. The
        // notifier's sender is independent of the channel serve loops, so this
        // still works as they wind down; a bounded timeout keeps a hung network
        // from blocking shutdown.
        if let Some(notice) = &shutdown_notice {
            let send = notice.notify(
                "⚠️ Komo 已下线",
                "网关正在停止，暂时无法响应消息，稍后回来。",
            );
            match tokio::time::timeout(SHUTDOWN_NOTICE_TIMEOUT, send).await {
                Ok(Ok(())) => info!("sent shutdown notice to home channel"),
                Ok(Err(error)) => warn!(%error, "failed to send shutdown notice"),
                Err(_) => warn!("shutdown notice timed out"),
            }
        }

        let _ = stop_tx.send(true);

        for handle in handles {
            let _ = handle.await;
        }
        Ok(())
    }
}
