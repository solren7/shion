//! Full-screen chat TUI (`shion chat` on a TTY) — a ratatui front end over the
//! same two backends the line REPL (`cli/chat.rs`) uses: a running gateway via
//! [`GatewayClient::chat`] (trusted loopback, approvals auto-granted
//! server-side), or the in-process [`AgentRuntime`] against the local db.
//! Neither backend changes; this module only replaces the read-line loop with
//! an event loop.
//!
//! Layout: scrollable transcript · status line (spinner while a turn runs) ·
//! bordered input box. In local mode, a side-effecting tool's approval request
//! arrives over a channel ([`TuiApprover`]) and renders as a modal — `y`/`s`/`n`
//! — instead of `CliApprover`'s raw stdin prompt.
//!
//! Logs: `main.rs::init_tracing` routes tracing to `~/.shion/logs/chat-tui.log`
//! when it detects the TUI will run — stderr writes would tear the alternate
//! screen. `ratatui::init` installs a panic hook that restores the terminal.

mod app;
mod approver;
mod ui;

use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    agent::runtime::AgentRuntime,
    cli::{gateway_client::GatewayClient, wiring},
    domain::{approval::Approver, repository::SessionRepository, session::Session},
    infra::persistence::{db::Db, kanban::KanbanDb},
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
    async fn turn(&self, session_id: &str, input: String) -> anyhow::Result<String> {
        match self {
            Backend::Remote(gw) => gw.chat(session_id, &input).await,
            Backend::Local { runtime, .. } => runtime.handle_input(session_id, input).await,
        }
    }
}

/// Start the TUI on a fresh session. Mirrors `cli::chat::run`: a running
/// gateway holds the db lock, so route turns to it; otherwise run in-process.
pub async fn run(db_url: &str, kanban_url: &str) -> anyhow::Result<()> {
    let Connected { backend, approvals } = connect(db_url, kanban_url).await?;
    drive(backend, approvals, new_session_id(), false).await
}

/// Continue an existing session (`shion session resume <id>` on a TTY). Errors
/// if the session doesn't exist — resume never creates one.
pub async fn resume(db_url: &str, kanban_url: &str, session_id: &str) -> anyhow::Result<()> {
    let Connected { backend, approvals } = connect(db_url, kanban_url).await?;
    match &backend {
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
    drive(backend, approvals, session_id.to_string(), true).await
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
}

async fn connect(db_url: &str, kanban_url: &str) -> anyhow::Result<Connected> {
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(gw) = GatewayClient::try_connect().await {
        return Ok(Connected {
            backend: Backend::Remote(Arc::new(gw)),
            approvals: (tx, rx),
        });
    }
    let db = Arc::new(Db::connect(db_url).await?);
    let kanban = Arc::new(KanbanDb::connect(kanban_url).await?);
    let approver: Arc<dyn Approver> = Arc::new(TuiApprover::new(tx.clone()));
    let runtime = Arc::new(wiring::build(db.clone(), kanban, approver).await?.runtime);
    Ok(Connected {
        backend: Backend::Local { runtime, db },
        approvals: (tx, rx),
    })
}

/// Set up the terminal, run the event loop, and always restore — including on
/// an error path (the panic path is covered by `ratatui::init`'s hook).
async fn drive(
    backend: Backend,
    approvals: (
        mpsc::UnboundedSender<ApprovalPrompt>,
        mpsc::UnboundedReceiver<ApprovalPrompt>,
    ),
    session: String,
    resuming: bool,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, backend, approvals, session, resuming).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    backend: Backend,
    approvals: (
        mpsc::UnboundedSender<ApprovalPrompt>,
        mpsc::UnboundedReceiver<ApprovalPrompt>,
    ),
    session: String,
    resuming: bool,
) -> anyhow::Result<()> {
    // Keep the sender alive for the whole loop (remote mode has no other
    // holder) so `approval_rx.recv()` pends instead of returning None forever.
    let (_approval_tx, mut approval_rx) = approvals;

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
                        tokio::spawn(async move {
                            let result = backend
                                .turn(&session_id, text)
                                .await
                                .map_err(|e| format!("{e:#}"));
                            let _ = turn_tx.send(result);
                        });
                    }
                    Some(Action::NewSession) => {
                        // Turns are keyed by session id, so an in-flight turn
                        // for the old id can finish and render harmlessly.
                        app.session_id = new_session_id();
                        if let Backend::Local { db, .. } = &backend {
                            ensure_session(db, &app.session_id).await?;
                        }
                        app.push(
                            Role::Info,
                            format!("Started new session `{}`", app.session_id),
                        );
                    }
                    Some(Action::Answered(_)) | None => {}
                }
            }
            Some(result) = turn_rx.recv() => {
                app.in_flight = false;
                match result {
                    Ok(reply) => app.push(Role::Agent, reply),
                    Err(error) => app.push(Role::Error, error),
                }
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
