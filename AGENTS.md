# Repository Guidelines

## Project Structure & Module Organization
This repository is a small Rust CLI crate. Core application code lives in `src/`. The executable entrypoint is `src/main.rs`, and CLI wiring is under `src/cli/` (`cli.rs`, `init.rs`, `mod.rs`). Other directories such as `src/commands/`, `src/services/`, `src/tasks/`, `src/tools/`, `src/types/`, and `src/utils/` are reserved for feature expansion and should keep business logic separated by responsibility. Build artifacts go to `target/` and must not be committed. Local SQLite files such as `test.db` are ignored and should be treated as disposable developer state.

## Build, Test, and Development Commands
Run commands from the repository root:

- `cargo check` verifies the crate compiles quickly without producing a release binary.
- `cargo test` runs the test suite. At present, the crate builds and executes `0` tests, so new features should add coverage.
- `cargo run -- init` runs the sample initialization flow, creates `test.db`, pushes the schema, and seeds one user.
- `cargo fmt` formats the codebase with standard Rust style before review.

If `cargo run -- init` fails with a table-already-exists error, remove the local database and rerun because the current seed path is not idempotent.

## Coding Style & Naming Conventions
Use default Rust formatting with 4-space indentation and `snake_case` for modules, files, functions, and variables. Use `PascalCase` for structs and enums, and keep CLI subcommands short, verb-based, and explicit, for example `init`. Prefer small modules with one responsibility each, and keep async database code close to the CLI or service layer that owns it.

## Testing Guidelines
Place unit tests beside the code they cover with `#[cfg(test)] mod tests`, and use `#[tokio::test]` for async paths. Name tests by behavior, for example `init_creates_user_record`. Add at least one success-path and one failure-path test for new CLI or database behavior. Run `cargo test` before opening a PR.

## Commit & Pull Request Guidelines
The current history uses short imperative messages such as `init`. Keep commits focused and use the same style, for example `add user lookup` or `wire init command`. PRs should include a concise description, linked issue if applicable, commands run for verification, and terminal output or screenshots when CLI behavior changes.
