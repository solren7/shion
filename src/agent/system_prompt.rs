//! Tiered system-prompt assembly, ported from hermes-agent's
//! `agent/system_prompt.py`.
//!
//! The prompt is built in three cache-ordered tiers and joined into one
//! string (stable → context → volatile):
//!
//!   * **stable**   — identity/persona, tool-aware behavioral guidance (only
//!     for tools that are actually loaded), and the skills catalog. Never
//!     changes for the life of the process.
//!   * **context**  — project instruction files (`AGENTS.md` / `CLAUDE.md` /
//!     `.cursorrules`) found in the working directory. Stable within a session.
//!   * **volatile** — day-precision date, model, provider. The only part that
//!     drifts, kept last so the stable+context prefix stays byte-identical and
//!     upstream prompt caches stay warm.
//!
//! Hermes builds this once per session and caches it; komo builds it once at
//! agent construction (the chat REPL is one sitting = one session; the gateway
//! shares one agent identity across sessions). The date line is **day**
//! precision on purpose — byte-stable for the whole day, so a rebuild never
//! invalidates the prefix cache mid-day. The model queries the exact
//! wall-clock moment via the `time` tool when it actually needs it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::Local;

use crate::config::{ModelConfig, komo_home};

/// Base persona, used when no `~/.komo/SOUL.md` override is present.
const IDENTITY: &str = "You are Komo, a concise and helpful personal agent. \
    When a request needs live information or an action, call one of your tools \
    instead of guessing.";

/// Gated on the `time` tool.
const TIME_GUIDANCE: &str = "Use the `time` tool when you need the exact current \
    date and time; never invent a timestamp.";

/// Gated on any of the state-backed tools (`session` / `memory` / `skill`).
const STATE_GUIDANCE: &str = "Questions about your own state — your sessions, \
    conversation history, memories, or skills — refer to Komo's database, not the \
    operating system: answer them with the `session`, `memory`, or `skill` tools, \
    never with shell commands like `tmux ls` or `who`.";

/// Gated on the `reminder` tool.
const REMINDER_GUIDANCE: &str = "You CAN schedule reminders: call the `reminder` tool \
    (action=create) with a message and a delay. Reminders are delivered as desktop \
    notifications by the `komo gateway` background process — you do NOT count down \
    yourself, and you must never pretend to track time in the conversation. If the \
    user asks for a reminder, create it with the tool and relay the tool's \
    confirmation. For recurring reminders (\"every day at 9am\"), pass a 5-field cron \
    expression via the `cron` parameter (e.g. \"0 9 * * *\"); times are the user's \
    local timezone. One-shot reminders use `after` or `at` as before.";

/// Gated on the `ask_user` tool.
const CLARIFY_GUIDANCE: &str = "When a key parameter is ambiguous, the target of an \
    action is unclear, or an irreversible action's intent is uncertain, ask first: \
    call `ask_user` with one specific question (mid-task — your progress is kept) \
    instead of guessing. Do NOT ask about things you can safely infer, look up with \
    your tools, or that barely matter — never interrogate.";

/// Project instruction files searched in the working directory, first found wins.
const CONTEXT_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// Cap on an included context file, mirroring hermes' 20k-char head truncation.
const CONTEXT_FILE_CAP: usize = 20_000;

/// Assembles komo's system prompt from cache-ordered tiers.
///
/// Built via chained setters, then `build()`:
///
/// ```ignore
/// let prompt = SystemPromptBuilder::new(&config)
///     .tools(tool_names)
///     .skills_note(skills_note)
///     .workspace_root(Some(root))
///     .build();
/// ```
pub struct SystemPromptBuilder {
    tool_names: Vec<String>,
    skills_note: Option<String>,
    workspace_root: Option<PathBuf>,
    model: String,
    provider: &'static str,
    home: PathBuf,
    /// Memoized stable+context render, keyed on the mtimes of the files it reads
    /// (`SOUL.md` + the project instruction files). The gateway is long-lived
    /// and rebuilds the prompt every turn, but those files change rarely — so we
    /// re-read them only when an mtime moves, keeping the per-turn hot path off
    /// several blocking `std::fs` reads while still picking up an in-place edit.
    cache: Mutex<Option<StableCache>>,
}

/// The cached stable+context string and the file mtimes it was rendered from.
struct StableCache {
    fingerprint: Vec<Option<SystemTime>>,
    stable_context: String,
}

impl SystemPromptBuilder {
    /// Start from a model config; no tools, skills, or workspace context yet.
    pub fn new(config: &ModelConfig) -> Self {
        Self {
            tool_names: Vec::new(),
            skills_note: None,
            workspace_root: None,
            model: config.model.clone(),
            provider: config.provider.name(),
            home: komo_home(),
            cache: Mutex::new(None),
        }
    }

    /// Names of the tools loaded into the agent; gates the tool-aware guidance
    /// blocks so the prompt only mentions tools that actually exist.
    pub fn tools(mut self, names: Vec<String>) -> Self {
        self.tool_names = names;
        self
    }

    /// The skills catalog note (appended to the stable tier), if any.
    pub fn skills_note(mut self, note: Option<String>) -> Self {
        self.skills_note = note;
        self
    }

    /// Working directory to scan for project instruction files (context tier).
    pub fn workspace_root(mut self, root: Option<PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// Override the home directory used to look up `SOUL.md` (tests).
    #[cfg(test)]
    fn home(mut self, home: PathBuf) -> Self {
        self.home = home;
        self
    }

    fn has(&self, tool: &str) -> bool {
        self.tool_names.iter().any(|n| n == tool)
    }

    /// Stable tier: persona + tool-aware guidance + skills catalog. Cache-friendly.
    fn stable(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        // Persona: an operator-supplied ~/.komo/SOUL.md wins (hermes' SOUL.md
        // analog); otherwise the built-in identity.
        let persona = std::fs::read_to_string(self.home.join("SOUL.md"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| IDENTITY.to_string());
        parts.push(persona);

        // Tool-aware guidance: only inject when the tool is loaded.
        if self.has("time") {
            parts.push(TIME_GUIDANCE.to_string());
        }
        if self.has("session") || self.has("memory") || self.has("skill") {
            parts.push(STATE_GUIDANCE.to_string());
        }
        if self.has("reminder") {
            parts.push(REMINDER_GUIDANCE.to_string());
        }
        if self.has("ask_user") {
            parts.push(CLARIFY_GUIDANCE.to_string());
        }

        if let Some(note) = &self.skills_note {
            parts.push(note.clone());
        }

        join(parts)
    }

    /// Context tier: first project instruction file found in the workspace,
    /// head-truncated. Stable within a session, may differ session-to-session.
    fn context(&self) -> String {
        let Some(root) = &self.workspace_root else {
            return String::new();
        };
        for name in CONTEXT_FILES {
            let Ok(content) = std::fs::read_to_string(root.join(name)) else {
                continue;
            };
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            let body = cap(trimmed, CONTEXT_FILE_CAP);
            return format!(
                "The following are project instructions from `{name}` in the working directory:\n\n{body}"
            );
        }
        String::new()
    }

    /// Volatile tier: day-precision date + model + provider. Kept last so the
    /// stable+context prefix stays byte-identical across the day.
    fn volatile(&self) -> String {
        // Day precision (no time-of-day): byte-stable for the whole day so a
        // rebuild doesn't bust the prefix cache. Local date — the model asks
        // the `time` tool for the exact moment when it needs it.
        let today = Local::now().format("%A, %B %-d, %Y");
        format!(
            "Today's date is {today}.\nModel: {model}\nProvider: {provider}",
            model = self.model,
            provider = self.provider,
        )
    }

    /// mtimes of every file the stable+context tiers read, in a fixed order, so
    /// a cached render can be invalidated when any is edited, created, or
    /// removed. A missing file is `None` (creating it flips `None`→`Some`, so
    /// adding a higher-priority context file also busts the cache).
    fn dependency_fingerprint(&self) -> Vec<Option<SystemTime>> {
        fn mtime(path: &Path) -> Option<SystemTime> {
            std::fs::metadata(path).and_then(|m| m.modified()).ok()
        }
        let mut fp = vec![mtime(&self.home.join("SOUL.md"))];
        if let Some(root) = &self.workspace_root {
            for name in CONTEXT_FILES {
                fp.push(mtime(&root.join(name)));
            }
        }
        fp
    }

    /// Assemble the three tiers into the final system prompt. The stable+context
    /// prefix is memoized and re-rendered only when a source file's mtime moves;
    /// the volatile tier (date/model/provider — no I/O) is rebuilt every call.
    pub fn build(&self) -> String {
        let fingerprint = self.dependency_fingerprint();
        let stable_context = {
            let mut cache = self.cache.lock().unwrap();
            match cache.as_ref() {
                Some(c) if c.fingerprint == fingerprint => c.stable_context.clone(),
                _ => {
                    let rendered = join(vec![self.stable(), self.context()]);
                    *cache = Some(StableCache {
                        fingerprint,
                        stable_context: rendered.clone(),
                    });
                    rendered
                }
            }
        };
        join(vec![stable_context, self.volatile()])
    }
}

/// Join non-empty parts with a blank line between them.
fn join(parts: Vec<String>) -> String {
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Head-truncate `s` to at most `max` chars (on a char boundary), appending a
/// marker when truncated.
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n\n[... truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_MAX_TURNS, Provider};

    fn config() -> ModelConfig {
        ModelConfig {
            provider: Provider::DeepSeek,
            model: "deepseek-chat".into(),
            api_key: "sk-test".into(),
            base_url: None,
            aux_model: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_tool_result_bytes: crate::config::DEFAULT_MAX_TOOL_RESULT_BYTES,
            max_history_messages: crate::config::DEFAULT_MAX_HISTORY_MESSAGES,
        }
    }

    fn tmp(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_sysprompt_test_{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn minimal_prompt_has_identity_and_volatile_only() {
        let p = SystemPromptBuilder::new(&config())
            .home(tmp("minimal"))
            .build();
        assert!(p.contains("You are Komo"));
        assert!(p.contains("Model: deepseek-chat"));
        assert!(p.contains("Provider: deepseek"));
        // No tools → no tool-aware guidance.
        assert!(!p.contains("reminder"));
        assert!(!p.contains("tmux ls"));
    }

    #[test]
    fn tool_guidance_is_gated_on_loaded_tools() {
        let p = SystemPromptBuilder::new(&config())
            .home(tmp("gated"))
            .tools(vec!["reminder".into(), "memory".into(), "time".into()])
            .build();
        assert!(p.contains("schedule reminders"));
        assert!(p.contains("tmux ls")); // state guidance, via `memory`
        assert!(p.contains("`time` tool"));
    }

    #[test]
    fn stable_tier_precedes_volatile_tier() {
        let p = SystemPromptBuilder::new(&config())
            .home(tmp("order"))
            .build();
        let identity_at = p.find("You are Komo").unwrap();
        let date_at = p.find("Today's date is").unwrap();
        assert!(
            identity_at < date_at,
            "stable identity must precede volatile date"
        );
    }

    #[test]
    fn skills_note_lands_in_stable_tier() {
        let p = SystemPromptBuilder::new(&config())
            .home(tmp("skills"))
            .skills_note(Some("You have skills: foo, bar".into()))
            .build();
        let note_at = p.find("You have skills").unwrap();
        let date_at = p.find("Today's date is").unwrap();
        assert!(
            note_at < date_at,
            "skills note belongs to the stable prefix"
        );
    }

    #[test]
    fn context_file_is_included_and_labeled() {
        let home = tmp("ctx_home");
        let root = tmp("ctx_root");
        std::fs::write(root.join("AGENTS.md"), "Be terse. Prefer bullet points.").unwrap();
        let p = SystemPromptBuilder::new(&config())
            .home(home)
            .workspace_root(Some(root))
            .build();
        assert!(p.contains("project instructions from `AGENTS.md`"));
        assert!(p.contains("Prefer bullet points."));
    }

    #[test]
    fn persona_override_replaces_builtin_identity() {
        let home = tmp("persona");
        std::fs::write(home.join("SOUL.md"), "You are Nyx, a terse oracle.").unwrap();
        let p = SystemPromptBuilder::new(&config()).home(home).build();
        assert!(p.contains("You are Nyx, a terse oracle."));
        assert!(!p.contains("You are Komo"));
    }

    #[test]
    fn cached_prompt_picks_up_a_newly_created_context_file() {
        let home = tmp("hot_home");
        let root = tmp("hot_root");
        let builder = SystemPromptBuilder::new(&config())
            .home(home)
            .workspace_root(Some(root.clone()));
        // First build: no context file, so none is mentioned (this seeds cache).
        let first = builder.build();
        assert!(!first.contains("project instructions"));
        // Create one out-of-band — the mtime fingerprint (None→Some) must bust
        // the cache so the next build reflects it, no restart needed.
        std::fs::write(root.join("AGENTS.md"), "Be terse.").unwrap();
        let second = builder.build();
        assert!(second.contains("project instructions from `AGENTS.md`"));
        assert!(second.contains("Be terse."));
    }
}
