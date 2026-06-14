//! The always-on gateway: a persistent process that hosts background services
//! and ingress channels, mirroring hermes-agent's gateway.
//!
//! hermes runs its cron ticker as a thread *inside* the gateway because the
//! gateway is already the always-on process; ingress (Telegram/Slack adapters)
//! lives there too. shion follows that shape:
//!
//!   - **background services** — the maintenance scheduler from `daemon.rs`,
//!     run as a tokio task inside the gateway (its only host).
//!   - **ingress channels** — pluggable `Channel`s that route inbound messages
//!     through a `MessageHandler` (the wired `AgentRuntime`). None are wired
//!     today; they will be declared in ~/.shion/config.toml and constructed
//!     from there.
//!
//! All tasks share one `watch` shutdown signal; on Ctrl-C the gateway flips it
//! and joins everything. The OS-level supervisor that keeps the *process* alive
//! across crashes/reboots (launchd `KeepAlive` / systemd `Restart=always`) is
//! still deferred — this is the in-process host only.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;
use tracing::{error, info};

use crate::{
    agent::{
        daemon::{Maintenance, Schedule, supervise},
        interaction::GatewayDispatcher,
        runtime::AgentRuntime,
    },
    domain::gateway::MessageHandler,
};

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
}

impl Gateway {
    pub fn new(dispatcher: Arc<GatewayDispatcher>) -> Self {
        Self {
            dispatcher,
            channels: Vec::new(),
            services: Vec::new(),
        }
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
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut handles = Vec::new();

        for service in self.services {
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

        for channel in self.channels {
            let dispatcher = self.dispatcher.clone();
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
        let _ = stop_tx.send(true);

        for handle in handles {
            let _ = handle.await;
        }
        Ok(())
    }
}
