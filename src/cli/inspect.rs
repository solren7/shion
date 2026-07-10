//! Operator subcommands (`shion cron list`, `shion session list/clean`).
//!
//! These query the database directly and print to stdout — no LLM, no agent
//! runtime. They are the operator's view into what the gateway will act on.

use crate::{
    domain::task::TaskStatus,
    services::operator_control::{
        OperatorCommand, OperatorCommandResult, OperatorControl, OperatorQuery, OperatorQueryResult,
    },
};

pub(crate) fn local_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

/// List pending reminders with their schedules (recurring ones show the cron
/// expression, one-shots are marked as such).
pub async fn cron_list(control: &OperatorControl) -> anyhow::Result<()> {
    let OperatorQueryResult::Reminders(mut pending) =
        control.query(OperatorQuery::Reminders).await?
    else {
        unreachable!("Reminders query answers with Reminders");
    };
    pending.sort_by_key(|r| r.run_at);

    if pending.is_empty() {
        println!("No pending reminders.");
        return Ok(());
    }
    for r in pending {
        if r.is_recurring() {
            println!(
                "{}  [{}]  next {}  {}",
                r.id,
                r.schedule,
                local_time(r.run_at),
                r.message
            );
        } else {
            println!(
                "{}  [one-shot]  due {}  {}",
                r.id,
                local_time(r.run_at),
                r.message
            );
        }
    }
    Ok(())
}

/// List open tasks grouped by status (inbox first — it needs triage).
pub async fn task_list(control: &OperatorControl) -> anyhow::Result<()> {
    let OperatorQueryResult::Tasks(open) = control.query(OperatorQuery::Tasks).await? else {
        unreachable!("Tasks query answers with Tasks");
    };

    if open.is_empty() {
        println!("No open tasks.");
        return Ok(());
    }
    for status in [TaskStatus::Inbox, TaskStatus::Todo, TaskStatus::Waiting] {
        let group: Vec<_> = open.iter().filter(|t| t.status == status).collect();
        if group.is_empty() {
            continue;
        }
        println!("{}:", status.as_str());
        for t in group {
            let mut line = format!("  {}  {}", t.id, t.title);
            if !t.board.is_empty() {
                line.push_str(&format!("  #{}", t.board));
            }
            if !t.waiting_on.is_empty() {
                line.push_str(&format!("  (waiting on: {})", t.waiting_on));
            }
            if let Some(due) = t.due_at {
                line.push_str(&format!("  due {}", local_time(due)));
            }
            println!("{line}");
        }
    }
    Ok(())
}

/// Truncate a string to `n` chars for a single-line summary, collapsing newlines.
fn oneline(s: &str, n: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= n {
        flat
    } else {
        let mut out: String = flat.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// List recent runs (most recent first), one per line: id, status, time, plan,
/// and a snippet of the input. The run ledger (roadmap §7).
pub async fn run_list(control: &OperatorControl, limit: usize) -> anyhow::Result<()> {
    let OperatorQueryResult::Runs(runs) = control.query(OperatorQuery::Runs { limit }).await?
    else {
        unreachable!("Runs query answers with Runs");
    };

    if runs.is_empty() {
        println!("No runs recorded.");
        return Ok(());
    }
    for r in runs {
        println!(
            "{}  [{}]{}  {}  {}  {}",
            r.id,
            r.status.as_str(),
            if r.recoverable { " ⟲" } else { "" },
            local_time(r.started_at),
            if r.plan.is_empty() { "-" } else { &r.plan },
            oneline(&r.input, 60),
        );
    }
    Ok(())
}

/// Show one run in full: its input, plan, outcome, and every tool step in order.
pub async fn run_inspect(control: &OperatorControl, id: &str) -> anyhow::Result<()> {
    let OperatorQueryResult::Run(fetched) = control
        .query(OperatorQuery::Run { id: id.to_string() })
        .await?
    else {
        unreachable!("Run query answers with Run");
    };
    let Some((run, steps)) = fetched else {
        println!("No run with id `{id}`.");
        return Ok(());
    };

    println!("run     {}", run.id);
    println!("session {}", run.session_id);
    println!("status  {}", run.status.as_str());
    println!("started {}", local_time(run.started_at));
    if let Some(ended) = run.ended_at {
        println!("ended   {}", local_time(ended));
    }
    if !run.plan.is_empty() {
        println!("plan    {}", run.plan);
    }
    println!("input   {}", oneline(&run.input, 200));
    if !run.error.is_empty() {
        println!("error   {}", run.error);
    }
    if run.recoverable {
        println!("resume  recoverable — `shion run resume {}`", run.id);
    }
    if !run.final_output.is_empty() {
        println!("output  {}", oneline(&run.final_output, 200));
    }

    if steps.is_empty() {
        println!("\n(no tool steps)");
        return Ok(());
    }
    println!("\nsteps:");
    for s in steps {
        let mark = if s.ok { "ok " } else { "ERR" };
        println!("  #{}  {}  {}", s.seq, mark, s.tool_name);
        println!("      args   {}", oneline(&s.args, 120));
        if s.ok {
            println!("      result {}", oneline(&s.result, 120));
        } else {
            println!("      error  {}", oneline(&s.error, 120));
        }
    }
    Ok(())
}

/// Prune the run ledger: delete runs (and their tool steps) started before
/// `cutoff` (unix seconds). The ledger accumulates like messages, so this is the
/// operator's manual trim — `run prune` resolves either `--before` or `--keep`
/// into a cutoff before calling this.
pub async fn run_prune(control: &OperatorControl, cutoff: i64) -> anyhow::Result<()> {
    let OperatorCommandResult::RunsPruned { removed } = control
        .command(OperatorCommand::PruneRuns { cutoff })
        .await?
    else {
        unreachable!("PruneRuns answers with RunsPruned");
    };
    if removed == 0 {
        println!("No runs older than {}; nothing pruned.", local_time(cutoff));
    } else {
        println!(
            "Pruned {removed} run(s) started before {}.",
            local_time(cutoff)
        );
    }
    Ok(())
}

/// Resolve the `--keep N` form to a cutoff timestamp: keep the N most recent
/// runs, returning the `started_at` of the first run to drop (everything older
/// is pruned). `None` = fewer than N+1 runs exist, so there's nothing to prune.
pub async fn run_keep_cutoff(
    control: &OperatorControl,
    keep: usize,
) -> anyhow::Result<Option<i64>> {
    // `Runs` already returns most-recent-first; ask for one more than we keep so
    // the (keep+1)-th run's start time becomes the cutoff.
    let OperatorQueryResult::Runs(runs) = control
        .query(OperatorQuery::Runs { limit: keep + 1 })
        .await?
    else {
        unreachable!("Runs query answers with Runs");
    };
    Ok(runs.get(keep).map(|r| r.started_at))
}

/// List the governed skill store (`~/.shion/skills`): active skills first,
/// then reviewer candidates awaiting triage. Pure file reads — works whether
/// or not the gateway is running (no db lock involved). Workspace-local skill
/// dirs are per-repo and listed by the agent's own `skill` tool instead.
pub fn skill_list() -> anyhow::Result<()> {
    let store =
        crate::infra::skills::FsSkillStore::new(crate::infra::skills::FsSkillStore::default_root());
    let active = store.list_active();
    let candidates = store.list_candidates();
    if active.is_empty() && candidates.is_empty() {
        println!("No skills in {}.", store.root().display());
        return Ok(());
    }
    for s in &active {
        let lock = if s.protected { " 🔒" } else { "" };
        let off = if s.disabled { " [disabled]" } else { "" };
        println!("{}{}{}  {}", s.name, lock, off, oneline(&s.description, 80));
    }
    if !candidates.is_empty() {
        println!("\ncandidates (`shion skill promote|reject <name>`):");
        for s in &candidates {
            println!(
                "  {}  [{}]  {}",
                s.name,
                s.source,
                oneline(&s.description, 80)
            );
        }
    }
    Ok(())
}

/// List stored sessions with creation time and message counts.
pub async fn session_list(control: &OperatorControl) -> anyhow::Result<()> {
    let OperatorQueryResult::Sessions(sessions) = control.query(OperatorQuery::Sessions).await?
    else {
        unreachable!("Sessions query answers with Sessions");
    };

    if sessions.is_empty() {
        println!("No sessions.");
        return Ok(());
    }
    for s in sessions {
        println!(
            "{}  created {}  {} messages ({} user turns)",
            s.id,
            local_time(s.created_at),
            s.messages,
            s.user_turns
        );
    }
    Ok(())
}

/// Delete every session with zero messages. An operator action — run it by
/// hand or from an external scheduler (launchd/cron), e.g. daily at 4am.
pub async fn session_clean(control: &OperatorControl) -> anyhow::Result<()> {
    let OperatorCommandResult::SessionsCleaned { removed } =
        control.command(OperatorCommand::CleanSessions).await?
    else {
        unreachable!("CleanSessions answers with SessionsCleaned");
    };
    println!("Removed {removed} empty session(s).");
    Ok(())
}
