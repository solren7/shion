# AGENTS.md

Guidance for coding agents (Claude Code and others) working in this repository.
`CLAUDE.md` is a symlink to this file — edit `AGENTS.md` only.

## Commands

```bash
cargo check                        # fast compile check
cargo build                        # build
cargo run -- chat                  # start interactive chat (db lives at ~/.shion/shion.db)
cargo run -- gateway               # always-on process: maintenance sweeps + ingress channels (feishu, telegram)
cargo test                         # run all tests
cargo test tools::time             # run a single test module
cargo fmt                          # format

shion gateway start                # install + start under launchd (auto-restart, login start)
shion gateway stop                 # stop and remove from launchd
shion gateway restart              # regenerate plist + restart (picks up a reinstalled binary)
shion gateway status               # launchd state (state/pid/last exit code)
```

`~/.shion/shion.db` is disposable developer state (sessions, messages, session
todos, skills, reminders, pairings, settings) — delete it freely to reset.
Durable cross-session **tasks live in a separate file, `~/.shion/kanban.db`**
(`infra/kanban.rs`), so resetting `shion.db` never wipes real task data.
After a schema change, delete the affected file — `push_schema` only runs for
newly created database files: a `TaskRecord` field change means deleting
`kanban.db`, any other model means `shion.db`.

Building requires `protoc` (`brew install protobuf`): the feishu channel's websocket
frames are protobuf, and `lark-websocket-protobuf` compiles its `.proto` at build time.

Runtime settings (provider/model/base_url/aux_model, maintenance `schedule`,
the opt-in daily `briefing_schedule`, the `[channels.*]` tables) live in
`~/.shion/config.toml`; credentials (API keys,
`FEISHU_APP_ID` / `FEISHU_APP_SECRET`, `TELEGRAM_BOT_TOKEN`) only in
`~/.shion/.env`. Priority: built-in defaults < config.toml < `SHION_*` env vars.
`SHION_HOME` relocates the whole directory.

Env management: dotenvy loads `.env` files into the process env (`main.rs`); envy
deserializes them into typed structs in `config.rs` (`ShionEnv` for `SHION_*`,
`ApiKeys` for provider keys, `FeishuEnv` for `FEISHU_*`, `TelegramEnv` for
`TELEGRAM_*`). Read env vars through those structs, not `std::env::var` — the
only exception is `SHION_HOME`, the bootstrap variable that locates `.env` itself.

Channel declarations follow hermes-agent's per-platform block shape — behavior
keys in the table, credentials in env:

```toml
[channels.feishu]
enabled = true
allow_from = ["ou_xxx"]   # pre-trusted sender open_ids (skip pairing)
require_mention = true     # group messages must carry an @mention (DMs bypass)
home_chat = "oc_xxx"      # optional: reminders go here instead of macOS notifications

[channels.telegram]
enabled = true
allow_from = ["123456789"]  # pre-trusted sender user-ids (skip pairing)
allowed_chats = ["-100123"]  # group chat-id allowlist (empty = any group; DMs always pass)
require_mention = true       # group messages must @mention the bot (DMs bypass)
home_chat = "123456789"     # optional: reminders go here instead of macOS notifications
```

When multiple channels set `home_chat`, feishu takes reminder delivery. The
config `home_chat` is only a fallback: the `/sethome` chat command sets the home
channel at runtime (persisted in the db), and that override wins. See the
`HomeNotifier` in the gateway section below.

Senders outside `allow_from` must pair before the agent talks to them: their
first message gets a pairing code as the only reply, and someone with shell
access to the host runs `shion pair approve <code>`. Pairing is hardened after
hermes' `pairing.py` (`domain/pairing.rs`): the code is stored only as a salted
SHA-256 hash (never plaintext, so `shion pair list` shows pending/approved but
not the code — get it from the sender), a sender is issued at most one fresh
code per 10 min (`PAIRING_RATE_LIMIT_SECS`; codes still expire after 1h), at
most 3 senders may await approval per platform (`MAX_PENDING_PER_PLATFORM`), and
the approve path locks for 1h after 5 wrong codes (`APPROVE_MAX_FAILURES`).
`shion pair revoke <id>` un-pairs. Approval is written to the shared db, so it
takes effect on the sender's next message without a gateway restart.

## Architecture

Personal Agent framework v0.1, implemented in Rust. The codebase follows a DDD-style layered architecture.

**Request flow:**
```
CLI → AgentRuntime → Planner → ToolRegistry → MessageRepository → Response
```

**Layers and their responsibilities:**

`domain/` — pure interfaces, no I/O, no external crates
- `repository.rs` — `SessionRepository` (find/save) and `MessageRepository` (list_by_session/save); the two traits `AgentRuntime` depends on
- `planner.rs` — `Planner` trait + `Plan` enum (`RespondDirectly`, `CallTool`, `MultiStep`)
- `tool.rs` — `Tool` trait (name / description / execute)
- `message.rs`, `session.rs` — core value types

`infra/db.rs` + `infra/kanban.rs` — the only places toasty (SQLite ORM) appears
- `Db` (`infra/db.rs`) wraps `Arc<Mutex<toasty::Db>>` over `shion.db`; implements every repository trait *except* `TaskRepository` (sessions, messages, skills, reminders, session todos, pairings, settings)
- `KanbanDb` (`infra/kanban.rs`) is a second, independent connection over `kanban.db`; it holds only `TaskRecord` and implements `TaskRepository`. Separate file = durable tasks survive a `shion.db` reset
- both: `connect(url)` checks if the db file exists; calls `push_schema()` only for new databases (toasty's `push_schema` is not idempotent)
- toasty model structs are private to their file
- SQLite URL format: `sqlite:./path.db` (single colon, not `sqlite://`)

`agent/runtime.rs` — application logic
- `AgentRuntime` holds `Arc<dyn SessionRepository>` + `Arc<dyn MessageRepository>` — no knowledge of toasty
- `handle_input` owns the session lifecycle: load-or-create, append messages, dispatch plan, persist reply

`agent/planner.rs` — `KeywordPlanner`
- v0.1 rule-based: routes "time" / "now" / "时间" keywords to the `time` tool; everything else → `RespondDirectly` (answered by the LLM)

`domain/llm.rs` — `LlmClient` trait (`complete(&Session) -> String`); the abstraction `AgentRuntime` calls for `RespondDirectly`

`infra/llm.rs` — `DeepSeekClient`: `LlmClient` backed by the `rig` framework (`rig-core`, aliased as `rig`) against DeepSeek
- `from_env()` reads `DEEPSEEK_API_KEY`; model `deepseek-chat`
- sends the full session history: prior turns go through `with_history`, the latest user message is the prompt

`services/tool_registry.rs` — `HashMap<String, Box<dyn Tool>>` with `register` / `execute`

`tools/time.rs` — first built-in tool; returns RFC 3339 UTC timestamp

`domain/task.rs` + `tools/task.rs` — durable cross-session tasks (roadmap §2's "kanban layer", shaped after hermes-agent), persisted by `KanbanDb` in its own `kanban.db`
- single `Task` model: `status` (`inbox`→`todo`→`done`, plus `waiting`/`cancelled`), `waiting_on` (set = a commitment), optional `due_at`, `source`/`source_message_id` (origin session + dedup key for reviewer commitment extraction, see `ReviewSweep`), `board` (optional project/grouping label — a plain string, not a Project entity; the §2 escape hatch, as hermes does)
- `task` tool actions: `capture` (defaults to inbox) / `list` (filter by `status` and/or `board`) / `update` / `complete`; no `plan_today` — daily planning belongs to the briefing sweep
- operator view: `shion task list` (open tasks grouped by status, board shown as `#board`)
- deliberately NOT modeled: task-to-task dependency edges (`blockedBy`/`blocks`) or `owner` — those serve autonomous worker-swarm orchestration (hermes kanban's `task_links`, Claude Code's Task\* tools), which shion (single-turn personal assistant, no dispatcher) does not have. `waiting_on` covers personal-context blocking.

`domain/todo.rs` + `tools/todo.rs` — session-scoped working focus list (roadmap §2/§8; shaped after hermes `todo_tool` / Claude Code `TodoWrite`)
- `TodoItem { content, status: pending|in_progress|completed|cancelled, active_form }`; list order = priority; at most one `in_progress` (validated on write)
- distinct from `task`: a todo dies with the conversation. Persisted per session (`SessionTodoRecord`, keyed by session id) because shion reloads a session each turn, but it is disposable — the dispatcher clears it on `/new`
- `todo` tool: call with no args to read; pass `todos` to replace the whole list (full-list replace, no merge). Reads the current session from the ambient turn context (`current_session`); inert (no session) for aux sub-agents and sweeps
- the turn's session context is established for BOTH paths: the gateway dispatcher sets it (with a real `ReplySink`), and `AgentRuntime::handle_input` sets a *detached* context (no-op sink) when none exists, so the REPL gets `todo` too — see `SessionContext::detached`

`cli/chat.rs` — wires everything together; creates `Arc<Db>` and passes it as both repos
- Session ids are program-managed (uuid v7); every run starts a fresh session. `/new` and `/clear` are equivalent — both open a new session. There is no user-supplied session id and no `/session` subcommand.

`agent/daemon.rs` — background maintenance supervisor, hosted by the gateway (pattern borrowed from gbrain's `autopilot` supervisor)
- `Schedule` wraps `croner` (5-field Unix cron, e.g. `0 * * * *`); `Maintenance` trait is the scheduled unit of work
- `ReviewSweep` is the one fixed action: run the reflective reviewer over every stored session with ≥1 user turn. Beyond memories/skills, the reviewer also extracts commitments ("I'll do X", "waiting on Y") and captures them as `inbox` tasks tagged with the origin `source` + a content-derived `source_message_id` dedup key (`find_by_source_message_id` guards against re-capturing across sweeps). Auto-extracted tasks only ever land in `inbox`, never `todo` — the user triages them (same governance as auto-written memories).
- `ReminderSweep` delivers due reminders via `Notifier` every minute (10-min grace window; older ones are marked `missed`)
- `TaskSweep` notifies once when an open task comes due (the task stays open; `due_notified_at` is the at-most-once guard)
- `BriefingSweep` is the opt-in daily briefing (roadmap §4): it reads open tasks + recently-learned memories, lets the aux LLM compose a short digest (`briefing_prompt` is the pure, clock-injected prompt builder — returns `None` when there's nothing worth a ping), and delivers it through the same `Notifier`. Only scheduled when `briefing_schedule` is set (no default — proactive pings stay opt-in); wired in `cli/gateway.rs`.
- `supervise` is the loop: sleep to the next cron fire, run the cycle, isolate per-cycle failures, and trip a circuit breaker after 5 consecutive failures
- the OS-level supervisor install is `cli/service.rs` (`shion gateway start/stop/restart/status`, macOS launchd: `KeepAlive` auto-restart + `RunAtLoad`)

`agent/gateway.rs` — always-on gateway (pattern borrowed from hermes-agent's gateway: a persistent process hosting background services + ingress)
- `MessageHandler` (`domain/gateway.rs`) is the pure seam between a transport and the agent; `AgentRuntime` implements it (an inbound message is one session turn)
- `Channel` trait = a pluggable ingress; `Gateway` hosts N channels + N `MaintenanceService`s (the `daemon.rs` supervisor loop — review sweep on the config schedule, reminder + task sweeps every minute, optional daily briefing), all sharing one `watch` shutdown signal
- channels are declared in `~/.shion/config.toml` and constructed in `cli/gateway.rs`; `feishu` and `telegram` are the wired channels
- sender admission is two-layered: each channel's `admit` filters message shape (non-text, bot senders, group mention gate), then the shared `PairingGuard` (`agent/pairing.rs`, store in `domain/pairing.rs`) decides identity — config `allow_from` is pre-trusted, approved pairings pass, anyone else gets a pairing code (`shion pair approve <code>` on the host admits them; `cli/pair.rs`)
- `GatewayDispatcher` (`agent/interaction.rs`) is the front door between a channel and the agent: a channel builds a `ReplySink` (`domain/gateway.rs`) for the chat and hands it each inbound message; the dispatcher classifies chat control commands and otherwise runs a turn. Channels no longer await turns or send agent replies themselves — the dispatcher owns that, and runs each turn on a spawned task so the receive loop keeps polling (which is what lets an `/approve` reply arrive mid-turn). One turn at a time per session.
- chat control commands (any channel): `/new` (also `/clear`, `/reset`) rotates the session hermes-style (`SessionRepository::rotate` archives the old transcript under a fresh id, leaving the chat's session empty — the reviewer can still see it), clears approval state, and clears the session's working todo list; `/approve` (+ `/approve session`) and `/deny` resolve a pending approval; `/sethome` (also `/home`) makes the current chat the home channel for proactive output (persisted via `HomeRepository`, `domain/home.rs`)
- home channel + shutdown notice (hermes-borrowed): a single `HomeNotifier` (`infra/home_notifier.rs`) delivers all proactive output — reminders, task due notices, and the gateway's shutdown notice. It resolves the home at notify-time: the `/sethome` override (db, a `{platform}:{chat_id}` session id) wins over the config `home_chat` fallback (feishu first), degrading to the macOS notifier when no chat home resolves. On shutdown the gateway sends an "offline" notice through it (bounded by `SHUTDOWN_NOTICE_TIMEOUT`) before tearing down — only wired when a chat channel exists, so a foreground Ctrl-C with no channels stays quiet
- interactive tool approval over chat (ported from hermes' gateway approval): the gateway wires `ChatApprover` (`agent/interaction.rs`), not a deny-everything approver. When a side-effecting tool requests approval (`Risk::Normal`/`Dangerous`), the agent sends a prompt to the chat and the turn suspends on a `oneshot` registered in the shared `ApprovalState` (keyed by session, 5-min timeout); the user's `/approve`/`/deny` resolves it. `Risk::Safe` actions run without asking. With no chat session in context (maintenance sweeps, aux sub-agents) approval is denied. The turn's session context (id + `ReplySink`) reaches the approver via a task-local in `services::tool_registry` that `execute_isolated` re-establishes across its `tokio::spawn`.
- background install: `shion gateway start` (see `cli/service.rs`) runs it under launchd; bare `shion gateway` is the foreground process launchd invokes

`infra/feishu.rs` — the feishu integration: `FeishuChannel` (ingress), `FeishuSender` (outbound: cached tenant token + send; also a `TextSender` for the shared `HomeNotifier`)
- receives `im.message.receive_v1` over Feishu's WebSocket long connection (open-lark, no public callback URL needed); replies via the IM REST API with plain reqwest
- the ws connection runs on a dedicated thread with a current-thread runtime because open-lark's event dispatcher is not `Send`; events cross back over an mpsc channel
- `admit` filters message shape: `require_mention` for group chats, non-text and bot-sent messages dropped; sender identity goes through the shared `PairingGuard`
- session id is `feishu:{chat_id}`, so each chat is one continuous session; group @mention placeholders are stripped

`infra/telegram.rs` — the telegram integration: `TelegramChannel` (ingress), `TelegramSender` (outbound send; also a `TextSender` for the shared `HomeNotifier`)
- receives messages via `getUpdates` long polling (no public callback URL needed); plain reqwest against the Bot API, no SDK dependency
- `admit` mirrors the feishu policy: `require_mention` (group text must contain `@bot_username`, resolved via `getMe` at startup), non-text and bot-sent messages dropped; sender identity goes through the shared `PairingGuard`
- session id is `telegram:{chat_id}`; replies are sent with `parse_mode=Markdown` (rich formatting), falling back to plain chunked text when the API rejects the Markdown or the reply exceeds 4096 UTF-16 units

`cli/gateway.rs` — wires the `gateway` subcommand; `cli/wiring.rs` — shared `AgentRuntime` construction used by both chat and gateway (differ only in the `Approver`)

## Key extension points

- **Add a tool**: implement `Tool` in `src/tools/`, register it in `cli/chat.rs`
- **Swap LLM provider**: implement `LlmClient` (`domain/llm.rs`) for another backend and construct it in `cli/chat.rs`
- **Swap persistence**: implement `SessionRepository + MessageRepository` for a different backend; no changes needed in `agent/` or `domain/`
- **Upgrade planner**: replace `KeywordPlanner` with a model-based impl of `Planner`
- **Change the scheduled action**: implement `Maintenance` (`agent/daemon.rs`) and construct it in `cli/gateway.rs`
- **Add a gateway ingress**: implement `Channel` (`agent/gateway.rs`) for a new transport (TCP/HTTP/chat platform), `add_channel` it in `cli/gateway.rs`, gated by a `~/.shion/config.toml` declaration — `infra/feishu.rs` is the reference implementation

## Testing

Tests live beside the code with `#[cfg(test)] mod tests`. Use `#[tokio::test]` for async. Name tests by behavior (`time_tool_returns_non_empty_string`).

## Coding style

Default Rust formatting (`cargo fmt`), `snake_case` for modules/files/functions, `PascalCase` for structs and enums. CLI subcommands stay short and verb-based. Prefer small modules with one responsibility; keep async database code close to the layer that owns it.

## Commit & PR style

Short imperative commit messages: `add file tool`, `wire llm client`. PRs include a concise description, commands run for verification, and terminal output when CLI behavior changes.
