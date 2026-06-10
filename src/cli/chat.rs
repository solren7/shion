use std::sync::Arc;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::{
    cli::{approver::CliApprover, wiring},
    domain::{approval::Approver, repository::SessionRepository, session::Session},
    infra::db::Db,
};

pub async fn run(db_url: &str) -> anyhow::Result<()> {
    let db = Arc::new(Db::connect(db_url).await?);
    // Session ids are program-managed: every run starts a fresh session.
    let mut current_session = new_session_id();

    // Interactive approval at the TTY for side-effecting tools.
    let approver: Arc<dyn Approver> = Arc::new(CliApprover);
    let runtime = wiring::build(db.clone(), approver).await?.runtime;

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
        let input = match editor.readline("->") {
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
        // line, so re-printing it would double every message.
        let reply = runtime.handle_input(&current_session, input).await?;
        println!("Agent: {}\n", reply);
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
