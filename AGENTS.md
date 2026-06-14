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

`~/.shion/shion.db` is disposable developer state — delete it freely to reset.
After a schema change (new toasty model/field), delete `~/.shion/shion.db` —
`push_schema` only runs for newly created database files.

Building requires `protoc` (`brew install protobuf`): the feishu channel's websocket
frames are protobuf, and `lark-websocket-protobuf` compiles its `.proto` at build time.

Runtime settings (provider/model/base_url/aux_model, maintenance `schedule`, the
`[channels.*]` tables) live in `~/.shion/config.toml`; credentials (API keys,
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

When multiple channels set `home_chat`, feishu takes reminder delivery.

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

`infra/db.rs` — the only place toasty (SQLite ORM) appears
- `Db` struct wraps `Arc<Mutex<toasty::Db>>`
- implements both `SessionRepository` and `MessageRepository`
- `Db::connect(url)` checks if the db file exists; calls `push_schema()` only for new databases (toasty's `push_schema` is not idempotent)
- toasty model structs (`SessionRecord`, `MessageRecord`) are private to this file
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

`domain/task.rs` + `tools/task.rs` — durable cross-session tasks (roadmap §2's "kanban layer", shaped after hermes-agent)
- single `Task` model: `status` (`inbox`→`todo`→`done`, plus `waiting`/`cancelled`), `waiting_on` (set = a commitment), optional `due_at`, `source`/`source_message_id` (origin session + dedup key for future reviewer extraction)
- `task` tool actions: `capture` (defaults to inbox) / `list` / `update` / `complete`; no `plan_today` — daily planning belongs to a future briefing sweep
- operator view: `shion task list` (open tasks grouped by status)

`cli/chat.rs` — wires everything together; creates `Arc<Db>` and passes it as both repos
- Session ids are program-managed (uuid v7); every run starts a fresh session. `/new` and `/clear` are equivalent — both open a new session. There is no user-supplied session id and no `/session` subcommand.

`agent/daemon.rs` — background maintenance supervisor, hosted by the gateway (pattern borrowed from gbrain's `autopilot` supervisor)
- `Schedule` wraps `croner` (5-field Unix cron, e.g. `0 * * * *`); `Maintenance` trait is the scheduled unit of work
- `ReviewSweep` is the one fixed action: run the reflective reviewer over every stored session with ≥1 user turn
- `ReminderSweep` delivers due reminders via `Notifier` every minute (10-min grace window; older ones are marked `missed`)
- `TaskSweep` notifies once when an open task comes due (the task stays open; `due_notified_at` is the at-most-once guard)
- `supervise` is the loop: sleep to the next cron fire, run the cycle, isolate per-cycle failures, and trip a circuit breaker after 5 consecutive failures
- the OS-level supervisor install is `cli/service.rs` (`shion gateway start/stop/restart/status`, macOS launchd: `KeepAlive` auto-restart + `RunAtLoad`)

`agent/gateway.rs` — always-on gateway (pattern borrowed from hermes-agent's gateway: a persistent process hosting background services + ingress)
- `MessageHandler` (`domain/gateway.rs`) is the pure seam between a transport and the agent; `AgentRuntime` implements it (an inbound message is one session turn)
- `Channel` trait = a pluggable ingress; `Gateway` hosts N channels + N `MaintenanceService`s (the `daemon.rs` supervisor loop — review sweep on the config schedule, reminder sweep every minute), all sharing one `watch` shutdown signal
- channels are declared in `~/.shion/config.toml` and constructed in `cli/gateway.rs`; `feishu` and `telegram` are the wired channels
- sender admission is two-layered: each channel's `admit` filters message shape (non-text, bot senders, group mention gate), then the shared `PairingGuard` (`agent/pairing.rs`, store in `domain/pairing.rs`) decides identity — config `allow_from` is pre-trusted, approved pairings pass, anyone else gets a pairing code (`shion pair approve <code>` on the host admits them; `cli/pair.rs`)
- `GatewayDispatcher` (`agent/interaction.rs`) is the front door between a channel and the agent: a channel builds a `ReplySink` (`domain/gateway.rs`) for the chat and hands it each inbound message; the dispatcher classifies chat control commands and otherwise runs a turn. Channels no longer await turns or send agent replies themselves — the dispatcher owns that, and runs each turn on a spawned task so the receive loop keeps polling (which is what lets an `/approve` reply arrive mid-turn). One turn at a time per session.
- chat control commands (any channel): `/new` (also `/clear`, `/reset`) wipes the session's context (`MessageRepository::clear_session`) and approval state; `/approve` (+ `/approve session`) and `/deny` resolve a pending approval
- interactive tool approval over chat (ported from hermes' gateway approval): the gateway wires `ChatApprover` (`agent/interaction.rs`), not a deny-everything approver. When a side-effecting tool requests approval (`Risk::Normal`/`Dangerous`), the agent sends a prompt to the chat and the turn suspends on a `oneshot` registered in the shared `ApprovalState` (keyed by session, 5-min timeout); the user's `/approve`/`/deny` resolves it. `Risk::Safe` actions run without asking. With no chat session in context (maintenance sweeps, aux sub-agents) approval is denied. The turn's session context (id + `ReplySink`) reaches the approver via a task-local in `services::tool_registry` that `execute_isolated` re-establishes across its `tokio::spawn`.
- background install: `shion gateway start` (see `cli/service.rs`) runs it under launchd; bare `shion gateway` is the foreground process launchd invokes

`infra/feishu.rs` — the feishu integration: `FeishuChannel` (ingress), `FeishuSender` (outbound: cached tenant token + send), `FeishuNotifier` (reminders → `home_chat`)
- receives `im.message.receive_v1` over Feishu's WebSocket long connection (open-lark, no public callback URL needed); replies via the IM REST API with plain reqwest
- the ws connection runs on a dedicated thread with a current-thread runtime because open-lark's event dispatcher is not `Send`; events cross back over an mpsc channel
- `admit` filters message shape: `require_mention` for group chats, non-text and bot-sent messages dropped; sender identity goes through the shared `PairingGuard`
- session id is `feishu:{chat_id}`, so each chat is one continuous session; group @mention placeholders are stripped

`infra/telegram.rs` — the telegram integration: `TelegramChannel` (ingress), `TelegramSender` (outbound send), `TelegramNotifier` (reminders → `home_chat`)
- receives messages via `getUpdates` long polling (no public callback URL needed); plain reqwest against the Bot API, no SDK dependency
- `admit` mirrors the feishu policy: `require_mention` (group text must contain `@bot_username`, resolved via `getMe` at startup), non-text and bot-sent messages dropped; sender identity goes through the shared `PairingGuard`
- session id is `telegram:{chat_id}`; replies over 4096 UTF-16 units are split into consecutive messages

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
