use std::{
    io::{self, BufRead, Write},
    sync::Arc,
};

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
        "Shion v0.1 — session `{}`. Type /new (or /clear) to start a fresh session, Ctrl-D to quit.\n",
        current_session
    );

    // Read one line at a time without holding the stdin lock across a turn, so
    // tools (e.g. the shell approval gate) can read stdin while a turn is in
    // flight.
    loop {
        // Read raw bytes and decode lossily: `read_line` aborts the whole
        // program on any non-UTF-8 byte ("stream did not contain valid UTF-8"),
        // which is too brittle for interactive input.
        let mut buf = Vec::new();
        let bytes = io::stdin().lock().read_until(b'\n', &mut buf)?;
        if bytes == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = String::from_utf8_lossy(&buf).trim().to_string();
        if input.is_empty() {
            continue;
        }
        // `/new` and `/clear` are equivalent: both start a fresh, program-managed
        // session. There are no user-supplied session ids.
        if input == "/new" || input == "/clear" {
            current_session = new_session_id();
            ensure_session(&db, &current_session).await?;
            println!("Started new session `{}`.\n", current_session);
            io::stdout().flush()?;
            continue;
        }

        println!("You [{}]: {}", current_session, input);
        let reply = runtime.handle_input(&current_session, input).await?;
        println!("Agent: {}\n", reply);
        io::stdout().flush()?;
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
