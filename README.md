# shion

A personal agent framework in Rust. One binary gives you an interactive
LLM chat with tools (shell, files, web, reminders, memory) and an
always-on background gateway that runs scheduled maintenance and
delivers reminders — all stored locally under `~/.shion`.

## Install

From GitHub release binaries (macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/solren7/shion/main/install.sh | bash
```

Or build from source:

```bash
cargo build --release
```

## Quick start

```bash
# put an API key in ~/.shion/.env (created on first run)
echo 'DEEPSEEK_API_KEY=sk-...' >> ~/.shion/.env

shion chat                       # interactive chat
shion model list                 # show current provider/model
shion model set anthropic        # switch provider (persists to config.toml)
```

Inside chat, `/new` (or `/clear`) starts a fresh session. Every run
starts a new session; history is stored in `~/.shion/shion.db`.

```bash
shion session list               # stored sessions with message counts
shion session clean              # delete empty sessions
shion cron list                  # pending reminders and next fire times
```

## Gateway (always-on background process)

The gateway hosts scheduled maintenance — a reflective review sweep
over past sessions (on the configured cron, hourly by default) and a
reminder sweep that delivers due reminders via macOS notifications
every minute.

```bash
shion gateway start              # install + start under launchd (auto-restart, login start)
shion gateway status             # launchd state
shion gateway restart            # pick up a reinstalled binary
shion gateway stop               # stop and remove from launchd
```

Bare `shion gateway` runs in the foreground (this is what launchd
invokes). The gateway is non-interactive: side-effecting tools are
denied rather than blocking on a prompt.

## Built-in tools

The agent can call these during a chat turn:

| Tool | What it does |
|---|---|
| `shell` | Run shell commands — safe commands auto-approved, dangerous ones blocked, the rest prompt for approval (session-scoped) |
| `file` | Read/write files in the workspace |
| `web_fetch` / `web_search` | Fetch pages and search the web |
| `reminder` | Schedule one-shot and recurring reminders |
| `memory` | Persistent notes as markdown files under `~/.shion/memory` |
| `session` | Look up past conversations |
| `delegate` | Hand a sub-task to a cheaper auxiliary model |
| `skill` | Run user-defined skills from `skills/` |
| `time` | Current time (RFC 3339 UTC) |

## Configuration

Everything lives in `~/.shion/` (relocatable via `SHION_HOME`).
Priority: built-in defaults < `config.toml` < `SHION_*` env vars.
API keys go only in `~/.shion/.env`, never in `config.toml`.

`~/.shion/config.toml`:

```toml
provider = "deepseek"        # deepseek | openai | anthropic | openrouter
model = "deepseek-chat"      # optional; defaults per provider
base_url = "https://..."     # optional override for OpenAI-compatible endpoints
aux_model = "..."            # optional cheaper model for delegated sub-tasks
schedule = "0 * * * *"       # gateway maintenance cron (5-field, default hourly)
max_turns = 30               # max tool-calling round-trips per user turn
```

| Provider | API key env var |
|---|---|
| `deepseek` | `DEEPSEEK_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

## Architecture

DDD-style layers; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for detail.

```
CLI → AgentRuntime → Planner → ToolRegistry → MessageRepository → Response
```

- `domain/` — pure traits and value types, no I/O (`Planner`, `Tool`,
  `LlmClient`, `SessionRepository`, `MessageRepository`, …)
- `agent/` — application logic: `AgentRuntime` (session lifecycle),
  the gateway, the maintenance daemon, the reflective reviewer
- `infra/` — implementations: SQLite via toasty, LLM clients via
  `rig`, markdown memory, macOS notifier
- `tools/` — built-in tools; `services/` — tool registry
- `cli/` — subcommand wiring

Each layer depends only on `domain` traits, so the LLM provider,
persistence backend, planner, and tools are all swappable without
touching `agent/`.

## Development

```bash
cargo check          # fast compile check
cargo test           # run all tests
cargo fmt            # format
cargo run -- chat    # run from source
```

`~/.shion/shion.db` is disposable — delete it freely to reset, and
always delete it after a schema change (the schema is only pushed for
newly created database files).
