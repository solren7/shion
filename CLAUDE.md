# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo check                        # fast compile check
cargo build                        # build
cargo run -- chat                  # start interactive chat (creates shion.db)
cargo run -- chat --db sqlite:./my.db  # custom db
cargo run -- daemon                # background maintenance on a cron schedule (hourly default)
cargo run -- daemon --schedule "*/30 * * * *"  # custom 5-field cron expression
cargo run -- gateway               # always-on process: maintenance + unix-socket ingress
cargo test                         # run all tests
cargo test tools::time             # run a single test module
cargo fmt                          # format
```

`shion.db` is disposable developer state — delete it freely to reset.

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
- v0.1 sends only the latest user message (multi-turn history wiring is TODO)

`services/tool_registry.rs` — `HashMap<String, Box<dyn Tool>>` with `register` / `execute`

`tools/time.rs` — first built-in tool; returns RFC 3339 UTC timestamp

`cli/chat.rs` — wires everything together; creates `Arc<Db>` and passes it as both repos
- Session ids are program-managed (uuid v7); every run starts a fresh session. `/new` and `/clear` are equivalent — both open a new session. There is no user-supplied session id and no `/session` subcommand.

`agent/daemon.rs` — background maintenance daemon (pattern borrowed from gbrain's `autopilot` supervisor)
- `Schedule` wraps `croner` (5-field Unix cron, e.g. `0 * * * *`); `Maintenance` trait is the scheduled unit of work
- `ReviewSweep` is the one fixed action: run the reflective reviewer over every stored session with ≥1 user turn
- `supervise` is the loop: sleep to the next cron fire, run the cycle, isolate per-cycle failures, and trip a circuit breaker after 5 consecutive failures
- in-process only; the OS-level supervisor install (launchd/systemd/crontab) gbrain also ships is intentionally deferred

`cli/daemon.rs` — wires the `daemon` subcommand: builds the aux-model reviewer and runs `supervise` until Ctrl-C

`agent/gateway.rs` — always-on gateway (pattern borrowed from hermes-agent's gateway: a persistent process hosting background services + ingress)
- `MessageHandler` (`domain/gateway.rs`) is the pure seam between a transport and the agent; `AgentRuntime` implements it (an inbound message is one session turn)
- `Channel` trait = a pluggable ingress; `Gateway` hosts N channels + an optional `MaintenanceService` (the `daemon.rs` supervisor loop), all sharing one `watch` shutdown signal
- `infra/unix_channel.rs` — the first `Channel`: newline-delimited JSON over a unix socket (`{"input","session?"}` → `{"reply"}`/`{"error"}`). The socket file doubles as a single-instance guard (live socket → refuse; stale → replace) and is removed on `Drop`
- non-interactive: the gateway wires `DenyApprover` so side-effecting tools are refused rather than blocking on a stdin prompt (mirrors hermes disabling interactive toolsets in cron/gateway context)
- in-process only; OS-level supervisor install (launchd/systemd) still deferred

`cli/gateway.rs` — wires the `gateway` subcommand; `cli/wiring.rs` — shared `AgentRuntime` construction used by both chat and gateway (differ only in the `Approver`)

## Key extension points

- **Add a tool**: implement `Tool` in `src/tools/`, register it in `cli/chat.rs`
- **Swap LLM provider**: implement `LlmClient` (`domain/llm.rs`) for another backend and construct it in `cli/chat.rs`
- **Swap persistence**: implement `SessionRepository + MessageRepository` for a different backend; no changes needed in `agent/` or `domain/`
- **Upgrade planner**: replace `KeywordPlanner` with a model-based impl of `Planner`
- **Change the scheduled action**: implement `Maintenance` (`agent/daemon.rs`) and construct it in `cli/daemon.rs`
- **Add a gateway ingress**: implement `Channel` (`agent/gateway.rs`) for a new transport (TCP/HTTP/chat platform) and `add_channel` it in `cli/gateway.rs`

## Testing

Tests live beside the code with `#[cfg(test)] mod tests`. Use `#[tokio::test]` for async. Name tests by behavior (`time_tool_returns_non_empty_string`).

## Commit style

Short imperative messages: `add file tool`, `wire llm client`.
