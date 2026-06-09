# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo check                        # fast compile check
cargo build                        # build
cargo run -- chat                  # start interactive chat (creates shion.db)
cargo run -- chat --db sqlite:./my.db --session foo  # custom db / session
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

## Key extension points

- **Add a tool**: implement `Tool` in `src/tools/`, register it in `cli/chat.rs`
- **Swap LLM provider**: implement `LlmClient` (`domain/llm.rs`) for another backend and construct it in `cli/chat.rs`
- **Swap persistence**: implement `SessionRepository + MessageRepository` for a different backend; no changes needed in `agent/` or `domain/`
- **Upgrade planner**: replace `KeywordPlanner` with a model-based impl of `Planner`

## Testing

Tests live beside the code with `#[cfg(test)] mod tests`. Use `#[tokio::test]` for async. Name tests by behavior (`time_tool_returns_non_empty_string`).

## Commit style

Short imperative messages: `add file tool`, `wire llm client`.
