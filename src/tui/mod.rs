//! Full-screen chat TUI — `shion chat`'s interface (a terminal is required;
//! scripted access goes through the gateway's api channel instead). A ratatui
//! front end over two backends: a running gateway via [`GatewayClient::chat`]
//! (trusted loopback, approvals auto-granted server-side), or the in-process
//! [`AgentRuntime`] against the local db.
//!
//! Layout: scrollable transcript · status line (spinner while a turn runs) ·
//! bordered input box. In local mode, a side-effecting tool's approval request
//! arrives over a channel ([`TuiApprover`]) and renders as a modal — `y`/`s`/`n`
//! — with the same semantics as `cli/approver.rs`'s stdin prompt (still used
//! by `shion run resume`).
//!
//! Logs: `main.rs::init_tracing` routes tracing to `~/.shion/logs/chat-tui.log`
//! when it detects the TUI will run — stderr writes would tear the alternate
//! screen. `ratatui::init` installs a panic hook that restores the terminal.

mod app;
mod approver;
mod markdown;
mod ui;

use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    agent::runtime::AgentRuntime,
    cli::wiring,
    config::ConfigSnapshot,
    domain::{
        approval::Approver, gateway::ReplySink, repository::SessionRepository, session::Session,
    },
    infra::{
        gateway_client::GatewayClient,
        persistence::{db::Db, kanban::KanbanDb},
    },
    services::{
        clarify::ClarifyState,
        tool_execution::{SessionContext, with_session},
    },
};

use app::{Action, App, Role};
use approver::{ApprovalPrompt, TuiApprover};

/// How a turn reaches the agent. Cloneable (all `Arc`) so each turn can run on
/// its own task while the event loop keeps handling keys.
#[derive(Clone)]
enum Backend {
    /// A running gateway over loopback HTTP (trusted; server-side approvals).
    Remote(Arc<GatewayClient>),
    /// In-process agent against the local db (approvals via the TUI modal).
    Local {
        runtime: Arc<AgentRuntime>,
        db: Arc<Db>,
    },
}

impl Backend {
    /// Run one turn. In local mode, `ctx` carries an interactive session
    /// context (its sink feeds mid-turn messages — `ask_user` questions —
    /// into the transcript); remote turns are handled server-side.
    async fn turn(
        &self,
        session_id: &str,
        input: String,
        ctx: Option<SessionContext>,
    ) -> anyhow::Result<String> {
        match self {
            Backend::Remote(gw) => gw.chat(session_id, &input).await,
            Backend::Local { runtime, .. } => match ctx {
                Some(ctx) => with_session(ctx, runtime.handle_input(session_id, input)).await,
                None => runtime.handle_input(session_id, input).await,
            },
        }
    }
}

/// A [`ReplySink`] that feeds mid-turn agent messages (the `ask_user`
/// question) into the TUI event loop's channel for transcript rendering.
struct ChannelSink {
    tx: mpsc::UnboundedSender<String>,
}

#[async_trait::async_trait]
impl ReplySink for ChannelSink {
    async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.tx
            .send(text.to_string())
            .map_err(|_| anyhow::anyhow!("TUI sink closed"))
    }
}

/// Start the TUI on a fresh session: a running gateway holds the db lock, so
/// route turns to it; otherwise run in-process.
pub async fn run(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let connected = connect(config).await?;
    drive(connected, new_session_id(), false).await
}

/// Continue an existing session (`shion session resume <id>` on a TTY). Errors
/// if the session doesn't exist — resume never creates one.
pub async fn resume(config: &ConfigSnapshot, session_id: &str) -> anyhow::Result<()> {
    let connected = connect(config).await?;
    match &connected.backend {
        Backend::Remote(gw) => {
            let known = gw.sessions().await?.iter().any(|s| s.id == session_id);
            if !known {
                anyhow::bail!("no session with id `{session_id}` (see `shion session list`)");
            }
        }
        Backend::Local { db, .. } => {
            if SessionRepository::find(&**db, session_id).await?.is_none() {
                anyhow::bail!("no session with id `{session_id}` (see `shion session list`)");
            }
        }
    }
    drive(connected, session_id.to_string(), true).await
}

struct Connected {
    backend: Backend,
    /// Approval prompts from the in-process agent (local mode). In remote mode
    /// the sender half is parked in the struct so the channel never closes —
    /// a closed receiver would busy-loop the `select!`.
    approvals: (
        mpsc::UnboundedSender<ApprovalPrompt>,
        mpsc::UnboundedReceiver<ApprovalPrompt>,
    ),
    /// Mid-turn clarify state (local mode only): the `ask_user` tool waits on
    /// it, the event loop resolves the user's answer into it. Remote turns
    /// clarify server-side (where the gateway dispatcher owns routing).
    clarify: Option<Arc<ClarifyState>>,
}

async fn connect(config: &ConfigSnapshot) -> anyhow::Result<Connected> {
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(gw) = GatewayClient::try_connect().await {
        return Ok(Connected {
            backend: Backend::Remote(Arc::new(gw)),
            approvals: (tx, rx),
            clarify: None,
        });
    }
    let db = Arc::new(Db::connect(&config.runtime.db_url).await?);
    let kanban = Arc::new(KanbanDb::connect(&config.runtime.kanban_db_url).await?);
    let approver: Arc<dyn Approver> = Arc::new(TuiApprover::new(tx.clone()));
    let wired = wiring::build(config, db.clone(), kanban, approver).await?;
    Ok(Connected {
        backend: Backend::Local {
            runtime: Arc::new(wired.runtime),
            db,
        },
        approvals: (tx, rx),
        clarify: Some(wired.clarify),
    })
}

/// Set up the terminal, run the event loop, and always restore — including on
/// an error path (the panic path is covered by `ratatui::init`'s hook).
async fn drive(connected: Connected, session: String, resuming: bool) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, connected, session, resuming).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    connected: Connected,
    session: String,
    resuming: bool,
) -> anyhow::Result<()> {
    let Connected {
        backend,
        approvals,
        clarify,
    } = connected;
    // Keep the sender alive for the whole loop (remote mode has no other
    // holder) so `approval_rx.recv()` pends instead of returning None forever.
    let (_approval_tx, mut approval_rx) = approvals;
    // Mid-turn agent messages (the `ask_user` question) from the local turn's
    // sink; the sender is also parked so the arm pends in remote mode.
    let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<String>();

    let mut app = App::new(session);
    let mode = match &backend {
        Backend::Remote(_) => "connected to the running gateway (trusted)",
        Backend::Local { .. } => "in-process (no gateway)",
    };
    app.push(
        Role::Info,
        format!(
            "Shion v0.1 — {mode}, {} `{}`",
            if resuming {
                "resumed session"
            } else {
                "session"
            },
            app.session_id
        ),
    );
    if let Backend::Local { db, .. } = &backend
        && !resuming
    {
        ensure_session(db, &app.session_id).await?;
    }

    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            maybe_event = events.next() => {
                let Some(event) = maybe_event.transpose()? else { break };
                let Event::Key(key) = event else { continue };
                // kitty-protocol terminals also send Release/Repeat.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.on_key(key) {
                    Some(Action::Quit) => break,
                    Some(Action::Submit(text)) => {
                        app.push(Role::You, text.clone());
                        app.in_flight = true;
                        let backend = backend.clone();
                        let session_id = app.session_id.clone();
                        let turn_tx = turn_tx.clone();
                        // Local mode: an interactive context whose sink feeds
                        // mid-turn messages into this loop, so `ask_user` can
                        // question the user; fresh clarify budget per turn.
                        let ctx = clarify.as_ref().map(|cl| {
                            cl.begin_turn(&session_id);
                            SessionContext {
                                session_id: session_id.clone(),
                                sink: Arc::new(ChannelSink { tx: sink_tx.clone() }),
                                interactive: true,
                                auto_approve: false,
                            }
                        });
                        tokio::spawn(async move {
                            let result = backend
                                .turn(&session_id, text, ctx)
                                .await
                                .map_err(|e| format!("{e:#}"));
                            let _ = turn_tx.send(result);
                        });
                    }
                    Some(Action::NewSession) => {
                        // Turns are keyed by session id, so an in-flight turn
                        // for the old id can finish and render harmlessly.
                        if let Some(cl) = &clarify {
                            // A pending question belongs to the old session.
                            cl.clear(&app.session_id);
                        }
                        app.awaiting_answer = false;
                        app.session_id = new_session_id();
                        if let Backend::Local { db, .. } = &backend {
                            ensure_session(db, &app.session_id).await?;
                        }
                        app.push(
                            Role::Info,
                            format!("Started new session `{}`", app.session_id),
                        );
                    }
                    Some(Action::Answer(text)) => {
                        if let Some(cl) = &clarify
                            && cl.resolve(&app.session_id, &text)
                        {
                            app.push(Role::You, text);
                        } else {
                            app.push(Role::Info, "问题已失效（超时或会话已重置）。".to_string());
                        }
                    }
                    Some(Action::Answered(_)) | None => {}
                }
            }
            Some(result) = turn_rx.recv() => {
                app.in_flight = false;
                // A question the turn never resolved dies with it.
                app.awaiting_answer = false;
                match result {
                    Ok(reply) => app.push(Role::Agent, reply),
                    Err(error) => app.push(Role::Error, error),
                }
            }
            // Mid-turn agent message (local mode): render it, and if the turn
            // is now waiting on `ask_user`, unlock the input as its answer.
            // The tool registers before sending, so this check can't race.
            Some(text) = sink_rx.recv() => {
                app.push(Role::Agent, text);
                app.awaiting_answer = clarify
                    .as_ref()
                    .is_some_and(|cl| cl.has_pending(&app.session_id));
            }
            // Show one approval at a time; further prompts wait in the channel
            // until the current modal is answered.
            Some(prompt) = approval_rx.recv(), if app.modal.is_none() => {
                app.modal = Some(prompt);
            }
            _ = tick.tick() => {
                if app.in_flight {
                    app.spinner = app.spinner.wrapping_add(1);
                }
            }
        }
    }
    Ok(())
}

async fn ensure_session(db: &Db, session_id: &str) -> anyhow::Result<()> {
    if SessionRepository::find(db, session_id).await?.is_none() {
        SessionRepository::save(db, &Session::new(session_id)).await?;
    }
    Ok(())
}

fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
