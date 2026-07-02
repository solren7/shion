//! Operator governance over the skill store (`~/.shion/skills`) — roadmap §9.
//!
//! All of these are pure file operations on the governed store, so they work
//! whether or not the gateway is running (no db lock involved). The runtime
//! `SkillRegistry` loads at startup, so changes that affect the agent's
//! catalog (promote / enable / disable) take effect on the next
//! `shion gateway restart`.

use crate::{
    cli::{gateway_client::GatewayClient, inspect::local_time},
    domain::run::RunRepository,
    infra::{
        messaging::api::skill_invocations_from_steps,
        persistence::db::Db,
        skills::FsSkillStore,
    },
};

fn store() -> FsSkillStore {
    FsSkillStore::new(FsSkillStore::default_root())
}

const RESTART_HINT: &str = "Takes effect for the agent after `shion gateway restart`.";

/// Accept a reviewer candidate: move it into the active store (overwriting the
/// active skill of the same name, i.e. accepting an update proposal).
pub fn promote(name: &str) -> anyhow::Result<()> {
    let skill = store().promote(name)?;
    println!("Promoted `{}` to active. {RESTART_HINT}", skill.name);
    Ok(())
}

/// Discard a reviewer candidate.
pub fn reject(name: &str) -> anyhow::Result<()> {
    store().reject(name)?;
    println!("Rejected candidate `{name}` (deleted).");
    Ok(())
}

/// Set or clear `protected`: a protected skill is operator-edit-only — the
/// reviewer stops writing even candidate proposals for it.
pub fn protect(name: &str, on: bool) -> anyhow::Result<()> {
    let skill = store().set_protected(name, on)?;
    if skill.protected {
        println!("Protected `{}` — the reviewer will no longer propose changes to it.", skill.name);
    } else {
        println!("Unprotected `{}`.", skill.name);
    }
    Ok(())
}

/// Enable or disable an active skill without deleting it: disabled skills stay
/// on disk and inspectable but are hidden from the model's catalog.
pub fn set_enabled(name: &str, enabled: bool) -> anyhow::Result<()> {
    let skill = store().set_disabled(name, !enabled)?;
    if skill.disabled {
        println!("Disabled `{}` — kept on disk, hidden from the agent. {RESTART_HINT}", skill.name);
    } else {
        println!("Enabled `{}`. {RESTART_HINT}", skill.name);
    }
    Ok(())
}

/// One skill in full: status, provenance, file path, prior candidate versions,
/// and the instruction body.
pub fn inspect(name: &str) -> anyhow::Result<()> {
    let store = store();
    let (skill, status, path) = if let Some(s) = store.find_active(name) {
        (s, "active", store.active_path(name))
    } else if let Some(s) = store.find_candidate(name) {
        (s, "candidate", store.candidate_path(name))
    } else {
        anyhow::bail!("no skill named `{name}` in {}", store.root().display());
    };

    println!("skill      {}", skill.name);
    let mut state = status.to_string();
    if skill.protected {
        state.push_str(" 🔒 protected");
    }
    if skill.disabled {
        state.push_str(" [disabled]");
    }
    println!("status     {state}");
    println!("source     {}", skill.source);
    println!("path       {}", path.display());
    if !skill.description.is_empty() {
        println!("describes  {}", skill.description);
    }
    let history = store.candidate_history(name);
    if !history.is_empty() {
        println!("history    {} prior version(s): {}", history.len(), history.join(", "));
    }
    println!("audit      `shion skill audit {name}` shows which turns loaded it");
    println!("\n{}", skill.instructions);
    Ok(())
}

/// Which turns loaded this skill — derived from the run ledger (`skill view`
/// steps), so it needs the db: routed to the gateway when one is running.
pub async fn audit(db_url: &str, name: &str) -> anyhow::Result<()> {
    const SCAN: usize = 500;
    const CAP: usize = 50;
    let invocations = match GatewayClient::try_connect().await {
        Some(gw) => gw.skill_audit(name).await?,
        None => {
            let db = Db::connect(db_url).await?;
            let steps = RunRepository::steps_by_tool(&db, "skill", SCAN).await?;
            skill_invocations_from_steps(steps, name, CAP)
        }
    };
    if invocations.is_empty() {
        println!("No recorded loads of `{name}` in the run ledger.");
        return Ok(());
    }
    for i in &invocations {
        let mark = if i.ok { "ok " } else { "ERR" };
        println!("{}  {}  step #{}  {}", local_time(i.started_at), mark, i.seq, i.run_id);
    }
    println!("\n(`shion run inspect <id>` shows the full turn.)");
    Ok(())
}
