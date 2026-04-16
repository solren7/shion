# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo check          # verify compilation without building
cargo build          # build the project
cargo run -- init    # run the init command (creates test.db, pushes schema, seeds one user)
cargo test           # run all tests
cargo fmt            # format code
```

`cargo run -- init` is not idempotent — if `test.db` already exists with the schema, it will fail with a table-already-exists error. Delete `test.db` and rerun.

## Architecture

This is an early-stage personal Agent framework in Rust. The current code implements a minimal CLI skeleton with SQLite persistence via the `toasty` ORM.

**Request flow (target architecture):**
```
CLI -> AgentRuntime -> Planner/Router -> LLM Orchestrator -> Tool Executor -> MemoryStore -> Response
```

**Current source layout** (`src/`):
- `main.rs` — async entry point, delegates to `cli::run()`
- `cli/cli.rs` — clap command parsing, subcommand dispatch
- `cli/init.rs` — `init` subcommand: connects to SQLite, pushes schema, seeds a `User`

**Planned module expansion** (from `ARCHITECTURE.md`):
- `agent/` — `AgentRuntime`, `Session`, `Planner`, `Executor`
- `domain/` — core trait abstractions: `Message`, `Task`, `Tool`, `MemoryStore`
- `services/` — LLM client, tool registry, workflow orchestration
- `tools/` — built-in tools: `time`, `file`, `shell` (shell disabled by default)
- `infra/` — SQLite, config, logging, model provider adapters

**Key design constraints from `ARCHITECTURE.md`:**
- `LlmClient`, `Tool`, `Planner`, and `MemoryStore` are all trait-based for substitutability
- Tool inputs/outputs are plain strings in v0.1; typed schemas come later
- SQLite is the only persistence backend for v0.1
- Configuration via `config.toml` + env var overrides (not yet implemented)

## Testing

Place unit tests with `#[cfg(test)] mod tests` beside the code they cover. Use `#[tokio::test]` for async code. Name tests by behavior (e.g., `init_creates_user_record`). Each new CLI or database behavior needs at least one success-path and one failure-path test.

## Commit Style

Short imperative messages matching the existing history (e.g., `add user lookup`, `wire init command`).
