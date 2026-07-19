//! Home Assistant event-ingress channel.
//!
//! Connects to HA's WebSocket API (`/api/websocket`), authenticates with the
//! long-lived access token, and subscribes to `state_changed` events. Each
//! qualifying state change is formatted into a human-readable line and handed
//! to the agent as one session turn (session id `homeassistant:events`) — so
//! the agent can *react* to the home ("front door unlocked", "garage left
//! open", "temperature crossed a threshold"). The turn's reply is delivered
//! back to HA as a persistent notification.
//!
//! Event forwarding is **closed by default**: with no `watch_domains`,
//! `watch_entities`, and `watch_all = false`, every event is dropped. This
//! mirrors hermes' adapter — without it, a busy home would trigger an agent
//! turn (an LLM call) on every sensor tick. A per-entity cooldown further caps
//! the rate.
//!
//! Credentials are shared with the `homeassistant` tool (`HASS_URL` /
//! `HASS_TOKEN`); only the event-filter behavior lives in
//! `[channels.homeassistant]`.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::watch};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::{
    agent::{gateway::Channel, interaction::GatewayDispatcher},
    config::HomeAssistantChannelConfig,
    infra::messaging::home_notifier::TextSender,
};

/// One continuous session for all HA events (mirrors hermes' `ha_events`).
const SESSION_ID: &str = "homeassistant:events";

/// HA persistent-notification message cap.
const MAX_MESSAGE_LENGTH: usize = 4096;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

use super::reconnect_backoff as backoff_delay;

/// Returns `true` if shutdown was requested while waiting.
async fn sleep_or_shutdown(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = shutdown.changed() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

// ── outbound: persistent notifications ──────────────────────────────────────

/// Outbound side: posts the agent's output to HA as a persistent notification.
/// Also usable as a `TextSender` so HA can be a proactive-output target.
pub struct HomeAssistantSender {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HomeAssistantSender {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    async fn notify(&self, title: &str, message: &str) -> anyhow::Result<()> {
        let mut message = message.to_string();
        if message.len() > MAX_MESSAGE_LENGTH {
            let mut end = MAX_MESSAGE_LENGTH;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        let resp = self
            .http
            .post(format!(
                "{}/api/services/persistent_notification/create",
                self.base_url
            ))
            .bearer_auth(&self.token)
            .json(&json!({ "title": title, "message": message }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("HA notification request failed: {e}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("HA notification returned HTTP {}", resp.status());
        }
        Ok(())
    }
}

#[async_trait]
impl TextSender for HomeAssistantSender {
    async fn send_text(&self, _chat_id: &str, text: &str) -> anyhow::Result<()> {
        self.notify("Komo", text).await
    }
}

/// Delivers a turn's output (and any mid-turn approval prompts) as HA
/// persistent notifications.
struct HomeAssistantReplySink {
    sender: Arc<HomeAssistantSender>,
}

#[async_trait]
impl crate::domain::gateway::ReplySink for HomeAssistantReplySink {
    async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.sender.notify("Komo", text).await
    }
}

// ── event filtering ─────────────────────────────────────────────────────────

/// Decides which `state_changed` events reach the agent. Closed by default.
#[derive(Clone, Default)]
struct Filters {
    watch_domains: Vec<String>,
    watch_entities: Vec<String>,
    ignore_entities: Vec<String>,
    watch_all: bool,
}

impl Filters {
    fn should_forward(&self, entity_id: &str) -> bool {
        if self.ignore_entities.iter().any(|e| e == entity_id) {
            return false;
        }
        if self.watch_all {
            return true;
        }
        let domain = entity_id.split('.').next().unwrap_or("");
        self.watch_domains.iter().any(|d| d == domain)
            || self.watch_entities.iter().any(|e| e == entity_id)
    }

    fn is_open(&self) -> bool {
        self.watch_all || !self.watch_domains.is_empty() || !self.watch_entities.is_empty()
    }
}

// ── channel ──────────────────────────────────────────────────────────────────

pub struct HomeAssistantChannel {
    base_url: String,
    token: String,
    filters: Filters,
    cooldown: Duration,
    sender: Arc<HomeAssistantSender>,
}

impl HomeAssistantChannel {
    pub fn new(config: &HomeAssistantChannelConfig) -> Self {
        Self {
            base_url: config.base_url.clone(),
            token: config.token.clone(),
            filters: Filters {
                watch_domains: config.watch_domains.clone(),
                watch_entities: config.watch_entities.clone(),
                ignore_entities: config.ignore_entities.clone(),
                watch_all: config.watch_all,
            },
            cooldown: Duration::from_secs(config.cooldown_seconds),
            sender: Arc::new(HomeAssistantSender::new(
                config.base_url.clone(),
                config.token.clone(),
            )),
        }
    }

    fn ws_url(&self) -> String {
        let base = self
            .base_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/api/websocket")
    }

    /// Open the WS, authenticate, and subscribe to `state_changed`.
    async fn connect_ws(&self) -> anyhow::Result<WsStream> {
        let (mut ws, _) = connect_async(self.ws_url()).await?;

        // 1. auth_required → 2. auth → 3. auth_ok
        let msg = next_json(&mut ws).await?;
        if msg.get("type").and_then(Value::as_str) != Some("auth_required") {
            anyhow::bail!("expected auth_required, got: {msg}");
        }
        ws.send(Message::text(
            json!({"type": "auth", "access_token": self.token}).to_string(),
        ))
        .await?;
        let msg = next_json(&mut ws).await?;
        if msg.get("type").and_then(Value::as_str) != Some("auth_ok") {
            anyhow::bail!("Home Assistant authentication failed: {msg}");
        }

        // 4. subscribe → 5. ack
        ws.send(Message::text(
            json!({"id": 1, "type": "subscribe_events", "event_type": "state_changed"}).to_string(),
        ))
        .await?;
        let msg = next_json(&mut ws).await?;
        if msg.get("success").and_then(Value::as_bool) != Some(true) {
            anyhow::bail!("Home Assistant event subscription failed: {msg}");
        }
        Ok(ws)
    }

    /// Read events until the socket drops or shutdown fires. Returns `true` when
    /// shutdown was requested (stop), `false` on disconnect (reconnect).
    async fn read_events(
        &self,
        mut ws: WsStream,
        dispatcher: &Arc<GatewayDispatcher>,
        shutdown: &mut watch::Receiver<bool>,
        cooldown: &mut HashMap<String, Instant>,
    ) -> bool {
        loop {
            let msg = tokio::select! {
                _ = shutdown.changed() => return true,
                msg = ws.next() => msg,
            };
            let text = match msg {
                Some(Ok(Message::Text(t))) => t,
                Some(Ok(Message::Close(_))) | None => return false,
                Some(Ok(_)) => continue, // ping/pong/binary/frame
                Some(Err(error)) => {
                    warn!(%error, "homeassistant websocket error");
                    return false;
                }
            };
            let Ok(event) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if event.get("type").and_then(Value::as_str) != Some("event") {
                continue;
            }
            self.dispatch_event(&event, dispatcher, cooldown).await;
        }
    }

    async fn dispatch_event(
        &self,
        event: &Value,
        dispatcher: &Arc<GatewayDispatcher>,
        cooldown: &mut HashMap<String, Instant>,
    ) {
        let Some(data) = event.get("event").and_then(|e| e.get("data")) else {
            return;
        };
        let Some(entity_id) = data.get("entity_id").and_then(Value::as_str) else {
            return;
        };
        if !self.filters.should_forward(entity_id) {
            return;
        }
        // Per-entity cooldown to avoid flooding the agent with rapid changes.
        let now = Instant::now();
        if let Some(last) = cooldown.get(entity_id)
            && now.duration_since(*last) < self.cooldown
        {
            return;
        }
        let Some(text) =
            format_state_change(entity_id, data.get("old_state"), data.get("new_state"))
        else {
            return;
        };
        cooldown.insert(entity_id.to_string(), now);

        info!(entity = %entity_id, "homeassistant event forwarded to agent");
        let sink: Arc<dyn crate::domain::gateway::ReplySink> = Arc::new(HomeAssistantReplySink {
            sender: self.sender.clone(),
        });
        dispatcher.handle(SESSION_ID, text, sink).await;
    }
}

#[async_trait]
impl Channel for HomeAssistantChannel {
    fn name(&self) -> &str {
        "homeassistant"
    }

    async fn serve(
        &self,
        dispatcher: Arc<GatewayDispatcher>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        if !self.filters.is_open() {
            warn!(
                "[homeassistant] no watch_domains/watch_entities set and watch_all is off — \
                 all events will be dropped"
            );
        }
        let mut cooldown: HashMap<String, Instant> = HashMap::new();
        let mut backoff_idx = 0;
        loop {
            let ws = tokio::select! {
                _ = shutdown.changed() => break,
                result = self.connect_ws() => match result {
                    Ok(ws) => ws,
                    Err(error) => {
                        warn!(%error, "homeassistant connect failed; retrying");
                        if sleep_or_shutdown(&mut shutdown, backoff_delay(backoff_idx)).await {
                            break;
                        }
                        backoff_idx += 1;
                        continue;
                    }
                }
            };
            info!(url = %self.base_url, "homeassistant channel connected");
            backoff_idx = 0;

            if self
                .read_events(ws, &dispatcher, &mut shutdown, &mut cooldown)
                .await
            {
                break;
            }
            warn!("homeassistant websocket disconnected; reconnecting");
            if sleep_or_shutdown(&mut shutdown, backoff_delay(backoff_idx)).await {
                break;
            }
            backoff_idx += 1;
        }
        info!("homeassistant channel stopped");
        Ok(())
    }
}

/// Read messages from the WS until a JSON text frame arrives, returning it
/// parsed. Skips ping/pong/binary control frames.
async fn next_json(ws: &mut WsStream) -> anyhow::Result<Value> {
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(t) => {
                return serde_json::from_str(t.as_str())
                    .map_err(|e| anyhow::anyhow!("invalid JSON from HA websocket: {e}"));
            }
            Message::Close(_) => anyhow::bail!("Home Assistant closed the connection"),
            _ => continue,
        }
    }
    anyhow::bail!("Home Assistant websocket ended unexpectedly")
}

/// Convert a `state_changed` event into a human-readable line, or `None` when
/// it should be skipped (no new state, or the state didn't actually change).
/// Domain-specific phrasing mirrors hermes' adapter.
fn format_state_change(
    entity_id: &str,
    old_state: Option<&Value>,
    new_state: Option<&Value>,
) -> Option<String> {
    let new_state = new_state.filter(|v| !v.is_null())?;
    let new_val = new_state
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let old_val = old_state
        .filter(|v| !v.is_null())
        .and_then(|o| o.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if old_val == new_val {
        return None;
    }
    let attrs = new_state.get("attributes");
    let friendly = attrs
        .and_then(|a| a.get("friendly_name"))
        .and_then(Value::as_str)
        .unwrap_or(entity_id);
    let domain = entity_id.split('.').next().unwrap_or("");

    let msg = match domain {
        "climate" => {
            let temp = attrs
                .and_then(|a| a.get("current_temperature"))
                .map(value_to_string)
                .unwrap_or_else(|| "?".to_string());
            let target = attrs
                .and_then(|a| a.get("temperature"))
                .map(value_to_string)
                .unwrap_or_else(|| "?".to_string());
            format!(
                "[Home Assistant] {friendly}: mode '{old_val}' → '{new_val}' \
                 (current {temp}, target {target})"
            )
        }
        "sensor" => {
            let unit = attrs
                .and_then(|a| a.get("unit_of_measurement"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("[Home Assistant] {friendly}: {old_val}{unit} → {new_val}{unit}")
        }
        "binary_sensor" => {
            let label = |v: &str| if v == "on" { "triggered" } else { "cleared" };
            format!(
                "[Home Assistant] {friendly}: {} (was {})",
                label(new_val),
                label(old_val)
            )
        }
        "light" | "switch" | "fan" => {
            format!(
                "[Home Assistant] {friendly}: turned {}",
                if new_val == "on" { "on" } else { "off" }
            )
        }
        "lock" => {
            format!("[Home Assistant] {friendly}: {new_val} (was {old_val})")
        }
        "alarm_control_panel" => {
            format!("[Home Assistant] {friendly}: alarm '{old_val}' → '{new_val}'")
        }
        _ => format!("[Home Assistant] {friendly} ({entity_id}): '{old_val}' → '{new_val}'"),
    };
    Some(msg)
}

/// Render a JSON scalar compactly (numbers without quotes/trailing `.0` noise).
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters() -> Filters {
        Filters {
            watch_domains: vec!["binary_sensor".to_string()],
            watch_entities: vec!["lock.front_door".to_string()],
            ignore_entities: vec!["binary_sensor.noisy".to_string()],
            watch_all: false,
        }
    }

    #[test]
    fn should_forward_honors_domain_entity_and_ignore_lists() {
        let f = filters();
        assert!(f.should_forward("binary_sensor.motion")); // domain match
        assert!(f.should_forward("lock.front_door")); // entity match
        assert!(!f.should_forward("light.kitchen")); // not watched
        assert!(!f.should_forward("binary_sensor.noisy")); // ignored wins
    }

    #[test]
    fn watch_all_forwards_everything_except_ignored() {
        let f = Filters {
            watch_all: true,
            ignore_entities: vec!["sensor.spam".to_string()],
            ..Default::default()
        };
        assert!(f.should_forward("light.kitchen"));
        assert!(!f.should_forward("sensor.spam"));
    }

    #[test]
    fn closed_by_default_drops_all() {
        let f = Filters::default();
        assert!(!f.is_open());
        assert!(!f.should_forward("light.kitchen"));
    }

    #[test]
    fn format_skips_unchanged_and_missing_new_state() {
        let same = json!({"state": "on"});
        assert!(format_state_change("light.k", Some(&same), Some(&same)).is_none());
        assert!(format_state_change("light.k", None, Some(&Value::Null)).is_none());
        assert!(format_state_change("light.k", None, None).is_none());
    }

    #[test]
    fn format_light_reads_naturally() {
        let old = json!({"state": "off"});
        let new = json!({"state": "on", "attributes": {"friendly_name": "Kitchen"}});
        let msg = format_state_change("light.kitchen", Some(&old), Some(&new)).unwrap();
        assert_eq!(msg, "[Home Assistant] Kitchen: turned on");
    }

    #[test]
    fn format_sensor_includes_unit_and_binary_sensor_labels() {
        let old = json!({"state": "20"});
        let new = json!({"state": "22",
            "attributes": {"friendly_name": "Temp", "unit_of_measurement": "°C"}});
        let msg = format_state_change("sensor.temp", Some(&old), Some(&new)).unwrap();
        assert_eq!(msg, "[Home Assistant] Temp: 20°C → 22°C");

        let old = json!({"state": "off"});
        let new = json!({"state": "on", "attributes": {"friendly_name": "Door"}});
        let msg = format_state_change("binary_sensor.door", Some(&old), Some(&new)).unwrap();
        assert_eq!(msg, "[Home Assistant] Door: triggered (was cleared)");
    }

    #[test]
    fn ws_url_swaps_scheme_and_appends_path() {
        let cfg = HomeAssistantChannelConfig {
            base_url: "http://homeassistant.local:8123".to_string(),
            token: "t".to_string(),
            watch_domains: vec![],
            watch_entities: vec![],
            ignore_entities: vec![],
            watch_all: false,
            cooldown_seconds: 30,
        };
        let ch = HomeAssistantChannel::new(&cfg);
        assert_eq!(ch.ws_url(), "ws://homeassistant.local:8123/api/websocket");
    }
}
