# shion

A personal agent framework in Rust. One binary gives you interactive LLM chat,
local tools, durable tasks and memories, scheduled reminders, and an always-on
gateway for chat channels and proactive background work. State lives locally
under `~/.shion`.

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

Inside chat, `/new` (or `/clear` / `/reset`) starts a fresh session. History and
the run ledger are stored in `~/.shion/shion.db`.

```bash
shion session list               # stored sessions with message counts
shion session clean              # delete empty sessions
shion cron list                  # pending reminders and next fire times
shion task list                  # open durable tasks
shion memory list                # memory candidates/active items
shion run list                   # recent agent turns
```

## Gateway (always-on background process)

The gateway hosts chat/event ingress and scheduled maintenance:

- reflective review sweeps over stored sessions
- one-shot and recurring reminder delivery
- task due notifications
- optional daily briefing
- Feishu, Telegram, WeChat, and Home Assistant channels when configured

```bash
shion gateway start              # macOS only: install + start under launchd
shion gateway status             # macOS only: launchd state
shion gateway restart            # macOS only: pick up a reinstalled binary
shion gateway stop               # macOS only: stop and remove from launchd
```

Bare `shion gateway` runs in the foreground (this is what launchd
invokes, and what Docker should run as the container process). In chat channels,
side-effecting tools can ask for approval in the conversation; reply `/approve`,
`/approve session`, or `/deny`.

## Built-in tools

The agent can call these during a chat turn:

| Tool | What it does |
|---|---|
| `shell` | Run shell commands — safe commands auto-approved, dangerous ones blocked, the rest prompt for approval (session-scoped) |
| `file` | Read/write files in the workspace |
| `web_fetch` / `web_search` | Fetch pages and search the web |
| `reminder` | Schedule one-shot and recurring reminders |
| `task` | Capture/list/update/complete durable cross-session tasks |
| `todo` | Maintain the current session's working focus list |
| `memory` | Govern long-term memories in `~/.shion/memory.db` |
| `homeassistant` | Read and control Home Assistant entities when configured |
| `session` | Look up past conversations |
| `delegate` | Hand a sub-task to a cheaper auxiliary model |
| `skill` | Run user-defined skills from `skills/` |
| `time` | Current time (RFC 3339 UTC) |

## Data Layout

Everything lives in `~/.shion/` by default, or under `SHION_HOME` when set.

| File | Purpose |
|---|---|
| `shion.db` | disposable session state: messages, todos, pairings, settings, reminders, run ledger |
| `kanban.db` | durable cross-session tasks |
| `memory.db` | durable long-term memories |
| `config.toml` | provider/model/channel behavior |
| `.env` | API keys and channel credentials |

Delete `shion.db` freely to reset development state. Do not delete `kanban.db`
or `memory.db` unless you intend to wipe durable personal data.

## Configuration

Priority: built-in defaults < `config.toml` < `SHION_*` env vars. API keys go
only in `~/.shion/.env`, never in `config.toml`.

`~/.shion/config.toml`:

```toml
provider = "deepseek"        # deepseek | openai | anthropic | openrouter
model = "deepseek-chat"      # optional; defaults per provider
base_url = "https://..."     # optional override for OpenAI-compatible endpoints
aux_model = "..."            # optional cheaper model for delegated sub-tasks
schedule = "0 * * * *"       # gateway maintenance cron (5-field, default hourly)
briefing_schedule = "0 8 * * *"      # optional daily briefing
briefing_workdays_only = true        # optional Chinese workday gate
max_turns = 30               # max tool-calling round-trips per user turn

[channels.telegram]
enabled = true
allow_from = ["123456789"]
home_chat = "123456789"

[channels.feishu]
enabled = true
allow_from = ["ou_xxx"]
home_chat = "oc_xxx"

[channels.wechat]
enabled = true
allow_from = ["wxid_xxx"]

[channels.homeassistant]
enabled = true
watch_domains = ["binary_sensor", "lock"]
cooldown_seconds = 30

# Permission policy: auto-allow / hard-deny side-effecting actions instead of
# prompting for each one. Deny beats allow; anything unmatched falls back to
# `default_normal` (ask). Read-only actions (web fetches, file reads) are
# deny-only: a deny rule can block them, nothing ever prompts for them.
[policy]
default_normal = "ask"       # ask | deny | allow — fallback for unmatched Normal actions

[[policy.rule]]              # let cargo/git run without prompting…
category = "shell"           # shell | file | network | homeassistant
match = "prefix"             # prefix | suffix | exact | contains
value = "cargo "
effect = "allow"

[[policy.rule]]              # …but never talk to the internal network
category = "network"
match = "suffix"             # network matches the URL host, on dot boundaries
value = "internal.corp"
effect = "deny"

[[policy.rule]]              # and keep key material unreadable even in-workspace
category = "file"
match = "contains"
value = ".ssh"
access = "read"              # file rules can scope to read | write
effect = "deny"
```

Verify with `shion policy list` (resolved rules) and
`shion policy check <category> <target>` (dry-run one action, shows the
matching rule). Rules can also scope to channels
(`channels = ["telegram"]`), and an allow rule only covers
`Risk::Dangerous` actions when it sets `include_dangerous = true`.

| Provider | API key env var |
|---|---|
| `deepseek` | `DEEPSEEK_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

Channel credentials live in `.env`, for example:

```bash
FEISHU_APP_ID=cli_xxx
FEISHU_APP_SECRET=xxx
TELEGRAM_BOT_TOKEN=xxx
HASS_TOKEN=xxx
HASS_URL=http://homeassistant.local:8123
```

WeChat is QR-based: run `shion wechat login` on the host, or send `/wechat login`
from an already-working chat channel.

## Architecture

DDD-style layers with domain traits at the center:

```
CLI/channel → AgentRuntime → LlmClient/rig → ToolRegistry → tool
                         ↘ repositories + run ledger → Response
```

- `domain/` — pure traits and value types, no I/O
- `agent/` — runtime, gateway, maintenance daemon, reviewer, system prompt
- `infra/` — SQLite via toasty, LLM via rig, messaging channels, notifiers
- `tools/` — built-in tools; `services/` — tool registry
- `cli/` — subcommand wiring

The LLM owns tool dispatch through rig's function-calling loop. Every tool call
funnels through `execute_isolated`, where it is isolated, traced, and recorded
in the run ledger when a turn is active.

## Development

```bash
cargo check          # fast compile check
cargo test           # run all tests
cargo fmt            # format
cargo run -- chat    # run from source
cargo run -- gateway # foreground gateway
```

Building requires `protoc` (`brew install protobuf`) because the Feishu websocket
dependency compiles protobuf frames at build time.

To reset after schema changes, delete the affected database file:

- `TaskRecord` changes: `~/.shion/kanban.db`
- `MemoryRecord` changes: `~/.shion/memory.db`
- other toasty models: `~/.shion/shion.db`

## Roadmap

The only long-form docs file kept in this repository is the current roadmap:
[docs/personal-agent-roadmap.md](docs/personal-agent-roadmap.md).
