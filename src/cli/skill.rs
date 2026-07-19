//! Operator governance over the skill store (`~/.komo/skills`) — roadmap §9.
//!
//! All of these are pure file operations on the governed store, so they work
//! whether or not the gateway is running (no db lock involved). The runtime
//! `SkillRegistry` re-scans the skill dirs on every query, so changes that
//! affect the agent's catalog (install / promote / enable / disable) take
//! effect on its next `skill` list — no gateway restart.

use crate::{
    cli::inspect::local_time,
    infra::skills::FsSkillStore,
    services::operator_control::{OperatorControl, OperatorQuery, OperatorQueryResult},
};

fn store() -> FsSkillStore {
    FsSkillStore::new(FsSkillStore::default_root())
}

const RELOAD_HINT: &str = "Takes effect on the agent's next `skill` list (no restart needed).";

/// Install a skill from a git repo (`owner/repo[/subpath]`, a GitHub URL, or any
/// `*.git`/`git@…` URL) or a raw `SKILL.md` URL, straight into the active store.
/// Pure file+network ops — works while the gateway holds the db lock; the live
/// registry means the running agent sees it on its next `skill` list, no restart.
pub async fn install(source: &str) -> anyhow::Result<()> {
    let installed = crate::infra::skill_install::install(&store(), source).await?;
    let files = if installed.files == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", installed.files)
    };
    println!(
        "Installed `{}` ({files}) → {}",
        installed.name,
        installed.path.display()
    );
    if !installed.description.is_empty() {
        println!("  {}", installed.description);
    }
    println!("Active now; the agent picks it up on its next `skill` list (no restart needed).");
    Ok(())
}

/// Accept a reviewer candidate: move it into the active store (overwriting the
/// active skill of the same name, i.e. accepting an update proposal).
pub fn promote(name: &str) -> anyhow::Result<()> {
    let skill = store().promote(name)?;
    println!("Promoted `{}` to active. {RELOAD_HINT}", skill.name);
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
        println!(
            "Protected `{}` — the reviewer will no longer propose changes to it.",
            skill.name
        );
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
        println!(
            "Disabled `{}` — kept on disk, hidden from the agent. {RELOAD_HINT}",
            skill.name
        );
    } else {
        println!("Enabled `{}`. {RELOAD_HINT}", skill.name);
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
        println!(
            "history    {} prior version(s): {}",
            history.len(),
            history.join(", ")
        );
    }
    println!("audit      `komo skill audit {name}` shows which turns loaded it");
    println!("\n{}", skill.instructions);
    Ok(())
}

/// Which turns loaded this skill — derived from the run ledger (`skill view`
/// steps), so it needs the db, i.e. the operator surface.
pub async fn audit(control: &OperatorControl, name: &str) -> anyhow::Result<()> {
    let OperatorQueryResult::SkillAudit(invocations) = control
        .query(OperatorQuery::SkillAudit {
            name: name.to_string(),
        })
        .await?
    else {
        unreachable!("SkillAudit query answers with SkillAudit");
    };
    if invocations.is_empty() {
        println!("No recorded loads of `{name}` in the run ledger.");
        return Ok(());
    }
    for i in &invocations {
        let mark = if i.ok { "ok " } else { "ERR" };
        println!(
            "{}  {}  step #{}  {}",
            local_time(i.started_at),
            mark,
            i.seq,
            i.run_id
        );
    }
    println!("\n(`komo run inspect <id>` shows the full turn.)");
    Ok(())
}
