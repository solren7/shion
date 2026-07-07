use clap::{Parser, Subcommand};

use super::{
    doctor, dream, gateway, inspect, journey, logs, memory, model, pair, policy, resume, service,
    skill, upgrade, wechat, workday,
};

#[derive(Parser)]
#[command(name = "shion", version, about = "Personal agent framework")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an interactive chat session (full-screen TUI; needs a terminal)
    Chat,
    /// Run the always-on gateway: maintenance scheduler (and, later,
    /// config-declared ingress channels). Maintenance cron comes from
    /// `schedule` in ~/.shion/config.toml (or SHION_SCHEDULE); default hourly.
    /// With no action, runs in the foreground (this is what launchd invokes).
    Gateway {
        #[command(subcommand)]
        action: Option<GatewayAction>,
    },
    /// Pull the latest source, rebuild + reinstall the binary, and restart the
    /// gateway so the new build goes live (shion's analog of `hermes update`)
    Upgrade {
        /// Rebuild and reinstall, but don't restart the gateway
        #[arg(long)]
        no_restart: bool,
    },
    /// Inspect scheduled reminders (recurring crons and one-shots)
    Cron {
        #[command(subcommand)]
        action: CronAction,
    },
    /// Inspect stored chat sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Inspect the durable task list
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Inspect the run ledger (every agent turn and its tool steps)
    Run {
        #[command(subcommand)]
        action: RunAction,
    },
    /// Inspect and govern the long-term memory library
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Run (or preview) usage-driven memory consolidation: promote well-recalled
    /// candidates to active, archive ones that never earned a recall
    Dream {
        /// Apply the cycle (mutate the store). Without it, this is a dry run.
        #[arg(long)]
        apply: bool,
    },
    /// Inspect registered skills
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Timeline of what shion has learned: memories (born/promoted/archived)
    /// and skills (proposed/activated), newest first
    Journey {
        /// Maximum number of events to show
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Only show events on or after this date (YYYY-MM-DD, local time)
        #[arg(long)]
        since: Option<String>,
    },
    /// Config & gateway health: model, schedules, channels, home, recent failures
    Doctor,
    /// Manage channel pairing: unknown senders must be approved from this
    /// host before the agent talks to them
    Pair {
        #[command(subcommand)]
        action: PairAction,
    },
    /// Inspect and dry-run the permission policy ([policy] in config.toml)
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Show or switch the active LLM provider and model
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Channel operator commands (per-platform provisioning/maintenance)
    Channel {
        #[command(subcommand)]
        action: ChannelAction,
    },
    /// Check the Chinese working-day calendar (statutory holidays + 调休).
    /// Reports whether a date is a workday, fetching+caching its year if needed.
    Workday {
        /// Date to check (YYYY-MM-DD); defaults to today
        date: Option<String>,
    },
    /// Print the gateway log (the launchd-captured tracing output)
    Logs {
        /// Number of trailing lines to print
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// Follow the log, streaming new lines until Ctrl-C
        #[arg(short, long)]
        follow: bool,
        /// Show the stdout log (`gateway.log`) instead of the tracing log
        #[arg(long)]
        stdout: bool,
    },
    /// Print the shion version
    Version,
}

#[derive(Subcommand)]
enum ChannelAction {
    /// WeChat (微信) channel operator commands
    Wechat {
        #[command(subcommand)]
        action: WechatAction,
    },
}

#[derive(Subcommand)]
enum WechatAction {
    /// Provision iLink credentials by scanning a login QR (run on the host)
    Login,
}

#[derive(Subcommand)]
enum PolicyAction {
    /// Show the resolved rules (as the approver applies them) and defaults
    List,
    /// Dry-run one action: which verdict, and which rule decided it
    Check {
        /// Action category: shell | file | network | homeassistant
        category: String,
        /// The target: a command, path, URL, or `domain.service`
        target: String,
        /// Evaluate as this channel (feishu | telegram | wechat | cli | …)
        #[arg(long)]
        channel: Option<String>,
        /// Classify the action as Risk::Dangerous (shell)
        #[arg(long)]
        dangerous: bool,
        /// file only: check the write path instead of read
        #[arg(long)]
        write: bool,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// Show the current provider/model and list available providers
    List,
    /// Switch provider (and optionally model); persists to config.toml
    Set {
        /// Provider, or a Codex model shortcut such as gpt-5.5
        provider: String,
        /// Model id (defaults to the provider's default model)
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum CronAction {
    /// List pending reminders with their schedules and next fire times
    List,
}

#[derive(Subcommand)]
enum TaskAction {
    /// List open tasks (inbox / todo / waiting), grouped by status
    List,
}

#[derive(Subcommand)]
enum RunAction {
    /// List recent runs (most recent first)
    List {
        /// Maximum number of runs to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one run in full, including every tool step
    Inspect {
        /// Run id (as shown by `run list`)
        id: String,
    },
    /// Resume an interrupted run: re-dispatch its input in the original
    /// session, primed with the tool steps that had completed
    Resume {
        /// Run id (as shown by `run list`); defaults to the most recent
        /// recoverable run
        id: Option<String>,
    },
    /// Prune old runs (and their tool steps) from the ledger. Pass exactly one
    /// of --before or --keep.
    Prune {
        /// Delete runs started before this date (YYYY-MM-DD, local time)
        #[arg(long, conflicts_with = "keep")]
        before: Option<String>,
        /// Keep only the N most recent runs, deleting everything older
        #[arg(long, conflicts_with = "before")]
        keep: Option<usize>,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// List stored memories (optionally filter by status), grouped by status
    List {
        /// Only show this status: candidate | active | archived | rejected
        #[arg(long)]
        status: Option<String>,
    },
    /// Substring search across all memories
    Search {
        /// Text to match in memory content
        query: String,
    },
    /// Promote candidates to active, confirmed memories
    Promote {
        /// Memory ids (as shown by `memory list`)
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Reject candidates so they never surface
    Reject {
        /// Memory ids
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Pin a memory into the L1 per-turn profile (the manual, explicit path)
    Pin {
        /// Memory id
        id: String,
    },
    /// Interactively triage the candidate pile (oldest first): p=promote,
    /// r=reject, s=skip, q=quit
    Triage,
    /// Quality report: counts by status/confidence + piles needing triage
    Report,
}

#[derive(Subcommand)]
enum SkillAction {
    /// List the governed skill store: active skills, then reviewer candidates
    List,
    /// Install a skill from a git repo or a raw SKILL.md URL into the active
    /// store (owner/repo, owner/repo/subpath, a GitHub URL, a *.git/git@ URL,
    /// or a link straight to a SKILL.md)
    Install {
        /// Where to fetch the skill from
        source: String,
    },
    /// Accept a reviewer candidate into the active store
    Promote {
        /// Skill name (as shown under `candidates` in `skill list`)
        name: String,
    },
    /// Discard a reviewer candidate
    Reject {
        /// Skill name
        name: String,
    },
    /// Mark a skill operator-edit-only (the reviewer stops proposing changes)
    Protect {
        /// Skill name
        name: String,
    },
    /// Clear the protected flag
    Unprotect {
        /// Skill name
        name: String,
    },
    /// Re-enable a disabled skill
    Enable {
        /// Skill name
        name: String,
    },
    /// Hide a skill from the agent without deleting it
    Disable {
        /// Skill name
        name: String,
    },
    /// Show one skill in full: status, provenance, path, history, body
    Inspect {
        /// Skill name
        name: String,
    },
    /// Which turns loaded this skill (derived from the run ledger)
    Audit {
        /// Skill name
        name: String,
    },
}

#[derive(Subcommand)]
enum PairAction {
    /// List pending pairing requests (with codes) and approved senders
    List,
    /// Approve a pending request by its pairing code
    Approve {
        /// The code the bot sent to the unpaired chat
        code: String,
    },
    /// Remove a pairing by id (`platform:sender_id`, as shown by `pair list`)
    Revoke {
        /// Pairing id to remove
        id: String,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// List stored sessions with creation time and message counts
    List,
    /// Resume an existing session: reopen the chat TUI bound to its id, so its
    /// history is loaded and the conversation continues where it left off
    Resume {
        /// Session id (as shown by `session list`)
        id: String,
    },
    /// Delete sessions that contain no messages
    Clean,
}

#[derive(Subcommand)]
enum GatewayAction {
    /// macOS only: install and start the gateway under launchd
    Start,
    /// macOS only: stop the gateway and remove it from launchd
    Stop,
    /// macOS only: restart the launchd gateway
    Restart,
    /// macOS only: show launchd state for the gateway
    Status,
}

/// The chat TUI owns the terminal, so it needs a real one on both ends.
/// Piped/scripted invocations get a clear pointer to the api channel instead —
/// that is the scripting surface (roadmap §8), not an interactive chat.
/// Must stay in sync with `main.rs::will_run_tui`, which picks the tracing
/// writer before the CLI parses.
fn require_terminal() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }
    anyhow::bail!(
        "`shion chat` is a full-screen TUI and needs a terminal.\n\
         For scripted access, POST to the gateway's api channel instead \
         (`/v1/chat/completions`; address and key in ~/.shion/gateway.json)."
    )
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The database always lives in the config directory; use SHION_HOME to
    // point at a different home (e.g. for tests or a second instance).
    let db = crate::config::default_db_url();
    // Durable tasks live in a separate file so resetting `shion.db` (disposable
    // dev state) never wipes them.
    let kanban = crate::config::default_kanban_db_url();
    match cli.command {
        Commands::Chat => {
            require_terminal()?;
            crate::tui::run(&db, &kanban).await
        }
        Commands::Gateway { action } => match action {
            None => {
                let schedule = crate::config::maintenance_schedule();
                gateway::run(&db, &kanban, &schedule).await
            }
            Some(GatewayAction::Start) => service::start(),
            Some(GatewayAction::Stop) => service::stop(),
            Some(GatewayAction::Restart) => service::restart(),
            Some(GatewayAction::Status) => service::status(),
        },
        Commands::Upgrade { no_restart } => upgrade::run(no_restart),
        Commands::Cron { action } => match action {
            CronAction::List => inspect::cron_list(&db).await,
        },
        Commands::Session { action } => match action {
            SessionAction::List => inspect::session_list(&db).await,
            SessionAction::Resume { id } => {
                require_terminal()?;
                crate::tui::resume(&db, &kanban, &id).await
            }
            SessionAction::Clean => inspect::session_clean(&db).await,
        },
        Commands::Task { action } => match action {
            TaskAction::List => inspect::task_list(&kanban).await,
        },
        Commands::Run { action } => match action {
            RunAction::List { limit } => inspect::run_list(&db, limit).await,
            RunAction::Inspect { id } => inspect::run_inspect(&db, &id).await,
            RunAction::Resume { id } => resume::run(&db, &kanban, id).await,
            RunAction::Prune { before, keep } => run_prune(&db, before, keep).await,
        },
        Commands::Memory { action } => {
            let url = crate::config::default_memory_db_url();
            match action {
                MemoryAction::List { status } => memory::list(&url, status).await,
                MemoryAction::Search { query } => memory::search(&url, &query).await,
                MemoryAction::Promote { ids } => memory::promote(&url, &ids).await,
                MemoryAction::Reject { ids } => memory::reject(&url, &ids).await,
                MemoryAction::Pin { id } => memory::pin(&url, &id).await,
                MemoryAction::Triage => memory::triage(&url).await,
                MemoryAction::Report => memory::report(&url).await,
            }
        }
        Commands::Dream { apply } => {
            dream::run(&crate::config::default_memory_db_url(), apply).await
        }
        Commands::Skill { action } => match action {
            SkillAction::List => inspect::skill_list(),
            SkillAction::Install { source } => skill::install(&source).await,
            SkillAction::Promote { name } => skill::promote(&name),
            SkillAction::Reject { name } => skill::reject(&name),
            SkillAction::Protect { name } => skill::protect(&name, true),
            SkillAction::Unprotect { name } => skill::protect(&name, false),
            SkillAction::Enable { name } => skill::set_enabled(&name, true),
            SkillAction::Disable { name } => skill::set_enabled(&name, false),
            SkillAction::Inspect { name } => skill::inspect(&name),
            SkillAction::Audit { name } => skill::audit(&db, &name).await,
        },
        Commands::Journey { limit, since } => {
            journey::journey(&crate::config::default_memory_db_url(), limit, since).await
        }
        Commands::Doctor => doctor::doctor(&db).await,
        Commands::Pair { action } => match action {
            PairAction::List => pair::list(&db).await,
            PairAction::Approve { code } => pair::approve(&db, &code).await,
            PairAction::Revoke { id } => pair::revoke(&db, &id).await,
        },
        Commands::Policy { action } => match action {
            PolicyAction::List => policy::list(),
            PolicyAction::Check {
                category,
                target,
                channel,
                dangerous,
                write,
            } => policy::check(&category, &target, channel.as_deref(), dangerous, write),
        },
        Commands::Model { action } => match action {
            ModelAction::List => model::list().await,
            ModelAction::Set { provider, model } => model::set(&provider, model).await,
        },
        Commands::Channel { action } => match action {
            ChannelAction::Wechat { action } => match action {
                WechatAction::Login => wechat::login().await,
            },
        },
        Commands::Workday { date } => workday::check(date).await,
        Commands::Logs {
            lines,
            follow,
            stdout,
        } => logs::run(lines, follow, stdout),
        Commands::Version => {
            println!("shion {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// Resolve `run prune`'s `--before <date>` / `--keep N` into a cutoff timestamp,
/// then prune. Exactly one of the two must be given (clap enforces mutual
/// exclusion, but not presence).
async fn run_prune(db: &str, before: Option<String>, keep: Option<usize>) -> anyhow::Result<()> {
    // Route through a running gateway (which holds the db lock) when one is up,
    // else open the db directly. The `--keep N` cutoff needs the run list, which
    // also comes from the gateway when routed.
    let gw = crate::cli::gateway_client::GatewayClient::try_connect().await;
    let cutoff = match (before, keep) {
        (Some(date), None) => parse_local_date(&date)?,
        (None, Some(keep)) => {
            let cutoff = match &gw {
                Some(gw) => gw.runs(keep + 1).await?.get(keep).map(|r| r.started_at),
                None => inspect::run_keep_cutoff(db, keep).await?,
            };
            match cutoff {
                Some(cutoff) => cutoff,
                None => {
                    println!("Fewer than {} runs; nothing to prune.", keep + 1);
                    return Ok(());
                }
            }
        }
        _ => anyhow::bail!("pass exactly one of --before <YYYY-MM-DD> or --keep <N>"),
    };
    match &gw {
        Some(gw) => {
            let removed = gw.prune_runs(cutoff).await?;
            inspect::report_prune(removed, cutoff);
            Ok(())
        }
        None => inspect::run_prune(db, cutoff).await,
    }
}

/// Parse a `YYYY-MM-DD` date as local-time midnight, returning a unix timestamp.
pub(crate) fn parse_local_date(s: &str) -> anyhow::Result<i64> {
    use chrono::TimeZone;
    let date = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid date `{s}` (expected YYYY-MM-DD): {e}"))?;
    let midnight = date.and_hms_opt(0, 0, 0).expect("valid midnight");
    match chrono::Local.from_local_datetime(&midnight).single() {
        Some(dt) => Ok(dt.timestamp()),
        None => anyhow::bail!("ambiguous local time for `{s}`"),
    }
}
