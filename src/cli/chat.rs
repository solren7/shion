use std::sync::Arc;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::{
    cli::{approver::CliApprover, gateway_client::GatewayClient, wiring},
    domain::{approval::Approver, repository::SessionRepository, session::Session},
    infra::persistence::{db::Db, kanban::KanbanDb},
};

/// Start the chat REPL. If a gateway is already running it holds the exclusive
/// db lock, so we can't open the db here — route the conversation to it over the
/// loopback api channel instead (trusted: side-effecting tools auto-approve).
/// Otherwise run the agent in-process against the db, exactly as before.
pub async fn run(db_url: &str, kanban_url: &str) -> anyhow::Result<()> {
    if let Some(gw) = GatewayClient::try_connect().await {
        return run_remote(gw).await;
    }
    run_local(db_url, kanban_url).await
}

/// REPL backed by a running gateway over HTTP. No db is opened here; session
/// history lives server-side keyed by the session id we send each turn.
async fn run_remote(gw: GatewayClient) -> anyhow::Result<()> {
    let mut current_session = new_session_id();
    println!(
        "Shion v0.1 — connected to the running gateway (session `{}`, trusted). Type /new to start a fresh session, Ctrl-C or Ctrl-D to quit.\n",
        current_session
    );
    let mut editor = DefaultEditor::new()?;
    loop {
        let (line, returned_editor) = tokio::task::spawn_blocking(move || {
            let line = editor.readline("->");
            (line, editor)
        })
        .await?;
        editor = returned_editor;

        let input = match line {
            Ok(line) => line.trim().to_string(),
            Err(ReadlineError::Eof) => break,
            Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(&input);

        // `/new` rotates the session id client-side; the gateway simply starts a
        // fresh transcript under the new id. (Other `/`-commands aren't routed —
        // use a chat channel for `/sethome` / `/pair`; approval is automatic here.)
        if input == "/new" || input == "/clear" {
            current_session = new_session_id();
            println!("Started new session `{}`.\n", current_session);
            continue;
        }

        match gw.chat(&current_session, &input).await {
            Ok(reply) => println!("Agent: {}\n", reply),
            Err(e) => eprintln!("Error: {e:#}\n"),
        }
    }
    Ok(())
}

/// REPL backed by an in-process agent over the local db (no gateway running).
async fn run_local(db_url: &str, kanban_url: &str) -> anyhow::Result<()> {
    let db = Arc::new(Db::connect(db_url).await?);
    let kanban = Arc::new(KanbanDb::connect(kanban_url).await?);
    // Session ids are program-managed: every run starts a fresh session.
    let mut current_session = new_session_id();

    // Interactive approval at the TTY for side-effecting tools.
    let approver: Arc<dyn Approver> = Arc::new(CliApprover::new());
    let runtime = wiring::build(db.clone(), kanban, approver).await?.runtime;

    ensure_session(&db, &current_session).await?;
    println!(
        "Shion v0.1 — session `{}`. Type /new (or /clear) to start a fresh session, Ctrl-C or Ctrl-D to quit.\n",
        current_session
    );

    // `rustyline` runs the terminal in raw mode for the duration of each
    // `readline` call only, decoding UTF-8 and tracking display width itself —
    // so backspace deletes whole multi-byte (CJK) characters instead of
    // corrupting them as the kernel's cooked-mode line discipline does. The
    // editor releases the terminal the moment it returns, so a tool's approval
    // gate (`CliApprover`) can still read stdin while a turn is in flight.
    let mut editor = DefaultEditor::new()?;

    loop {
        // `readline` blocks until the user submits a line; run it on tokio's
        // blocking thread pool so it never pins an async worker thread. The
        // editor moves into the closure and back out each iteration.
        let (line, returned_editor) = tokio::task::spawn_blocking(move || {
            let line = editor.readline("->");
            (line, editor)
        })
        .await?;
        editor = returned_editor;

        let input = match line {
            Ok(line) => line.trim().to_string(),
            Err(ReadlineError::Eof) => break,         // Ctrl-D
            Err(ReadlineError::Interrupted) => break, // Ctrl-C
            Err(e) => return Err(e.into()),
        };
        if input.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(&input);

        // `/new` and `/clear` are equivalent: both start a fresh, program-managed
        // session. There are no user-supplied session ids.
        if input == "/new" || input == "/clear" {
            current_session = new_session_id();
            ensure_session(&db, &current_session).await?;
            println!("Started new session `{}`.\n", current_session);
            continue;
        }

        // No need to echo the input — `rustyline` already left it on the prompt
        // line, so re-printing it would double every message. A failed turn
        // (tool panic, network error, …) is reported and the loop continues;
        // only readline/session errors above end the REPL.
        match runtime.handle_input(&current_session, input).await {
            Ok(reply) => println!("Agent: {}\n", reply),
            Err(e) => eprintln!("Error: {e:#}\n"),
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
