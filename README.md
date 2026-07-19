# Komo

<p align="center">
  <img src="docs/images/komo_logo.png" alt="Komo mascot, wordmark, and Light through your days slogan" width="520">
</p>

A personal agent framework in Rust. One binary gives you interactive LLM chat,
local tools, durable tasks and memories, scheduled reminders, and an always-on
gateway for chat channels and proactive background work. State lives locally
under `~/.komo`.

## Brand

**Komo** is inspired by the Japanese word *komorebi* (木漏れ日): sunlight
filtering through leaves. The image feels warm and clear, while suggesting how
small moments gather into something lasting—a natural fit for a personal agent
built around memory accumulating over time. The short, two-syllable name is
easy to say and remember, and adapts naturally to logos and domain names.

- **Candidate slogans:** 「记住每一缕光」 / 「陪你把日子攒成光」 / *Light through your days*
- **Visual language:** soft green, cream white, and sunlight yellow, with
  dappled-light shapes inspired by gaps between leaves
- **Personality:** a quiet friend beside you in the shade—warm and
  unobtrusive, attentive without being noisy, and able to remember the details
  entrusted to it

## Install

From GitHub release binaries (macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/solren7/komo/main/install.sh | bash
```

Or build from source:

```bash
cargo build --release
```

## Quick start

```bash
komo init                       # scaffold ~/.komo/config.toml + .env (never overwrites)
# then fill the DEEPSEEK_API_KEY= line in ~/.komo/.env

komo chat                       # interactive chat (full-screen TUI; needs a terminal)
komo model list                 # show current provider/model
komo model set anthropic        # switch provider (persists to config.toml)
```

Everything boots without a key — the gateway starts and channels serve — but
agent turns reply with a "key not set" pointer until one is configured.

Inside chat, `/new` (or `/clear` / `/reset`) starts a fresh session. History and
the run ledger are stored in `~/.komo/state.db`.

```bash
komo session list               # stored sessions with message counts
komo session clean              # delete empty sessions
komo cron list                  # pending reminders and next fire times
komo task list                  # open durable tasks
komo memory list                # memory candidates/active items
komo run list                   # recent agent turns (⟲ marks interrupted, resumable ones)
komo run resume                 # re-dispatch the last interrupted turn from the run ledger
komo skill list                 # governed skills + reviewer candidates awaiting triage
komo skill promote <name>       # accept a reviewer-proposed skill into the active store
```

## Gateway (always-on background process)

The gateway hosts chat/event ingress and scheduled maintenance:

- reflective review sweeps over stored sessions
- one-shot and recurring reminder delivery
- task due notifications
- optional daily briefing
- Feishu, Telegram, WeChat, and Home Assistant channels when configured

```bash
komo gateway start              # macOS only: install + start under launchd
komo gateway status             # macOS only: launchd state
komo gateway restart            # macOS only: pick up a reinstalled binary
komo gateway stop               # macOS only: stop and remove from launchd
```

Bare `komo gateway` runs in the foreground (this is what launchd
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
| `memory` | Govern long-term memories in `~/.komo/memory.db` |
| `homeassistant` | Read and control Home Assistant entities when configured |
| `session` | Look up past conversations |
| `delegate` | Hand a sub-task to a cheaper auxiliary model |
| `skill` | Load skills: workspace `skills/`·`.claude/skills/` dirs + the governed `~/.komo/skills` store |
| `time` | Current time (RFC 3339 UTC) |

## Data Layout

Everything lives in `~/.komo/` by default, or under `KOMO_HOME` when set.
During upgrades from the former `shion` name, an existing `~/.shion` directory
and `SHION_HOME` / `SHION_*` overrides remain compatibility fallbacks; any
`komo`-named path or variable takes precedence. `komo gateway start/restart`
also unloads the former launchd job before installing `com.komo.gateway`.

| File | Purpose |
|---|---|
| `state.db` | disposable session state: messages, todos, pairings, settings, reminders, run ledger |
| `kanban.db` | durable cross-session tasks |
| `memory.db` | durable long-term memories |
| `skills/` | durable governed skills (`SKILL.md` files; reviewer proposals in `skills/.candidates/`) |
| `config.toml` | provider/model/channel behavior |
| `.env` | API keys and channel credentials |

Delete `state.db` freely to reset development state. Do not delete `kanban.db`,
`memory.db`, or `skills/` unless you intend to wipe durable personal data.

## Configuration

Priority: built-in defaults < `config.toml` < `KOMO_*` env vars. API keys go
only in `~/.komo/.env`, never in `config.toml`.

`~/.komo/config.toml`:

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

Verify with `komo policy list` (resolved rules) and
`komo policy check <category> <target>` (dry-run one action, shows the
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

WeChat is QR-based: run `komo channel wechat login` on the host, or send `/wechat login`
from an already-working chat channel.

## Architecture

DDD-style layers with domain traits at the center:

```
CLI/channel → AgentRuntime ─ run_agent_loop ─┬→ LlmClient::begin_turn → TurnDriver (one rig completion / round)
                                             └→ ToolExecutor::execute_round → tools   (loop until Step::Final)
                          ↘ MessageRepository · RunRepository (ledger) → Response
```

komo owns the tool loop: `AgentRuntime::run_agent_loop` drives the model one
round at a time and hands each round of requested tool calls to the
`ToolExecutor`, where every call is isolated, retried on transient failures,
traced, and recorded in the run ledger.

### Project layout

```
src/
├── main.rs                # entry point + tracing setup
├── domain/                # pure traits and value types — no I/O, no external crates
│   ├── repository.rs · tool.rs · llm.rs      # the core trait seams
│   ├── message.rs · session.rs · run.rs      # value types + run-ledger model
│   ├── memory.rs · task.rs · todo.rs · skill.rs
│   └── policy.rs · approval.rs · pairing.rs · gateway.rs · …
├── agent/                 # application logic
│   ├── runtime.rs         # AgentRuntime: the in-house tool loop (run_agent_loop)
│   ├── gateway.rs · daemon.rs      # always-on gateway + scheduled sweeps
│   ├── interaction.rs     # GatewayDispatcher + chat approval
│   └── review_coordinator.rs · reviewer.rs · policy_approver.rs · system_prompt.rs
├── services/              # cross-cutting services
│   ├── tool_execution/    # ToolExecutor: retry / ledger / truncation pipeline
│   ├── operator_control/  # CLI operator actions, gateway/direct dual backend
│   ├── memory_enrichment.rs        # pinned + recall memory injection
│   └── skill_registry.rs  # live runtime view over the skill dirs
├── infra/                 # I/O implementations
│   ├── llm.rs · codex.rs · rig_tool.rs       # rig backend + Codex OAuth provider
│   ├── persistence/       # toasty/Turso: state.db + kanban.db
│   ├── memory/            # memory.db (+ legacy markdown import)
│   ├── messaging/         # feishu · telegram · wechat · homeassistant · api · notifiers
│   └── skills.rs · skill_install.rs · gateway_client.rs · rendezvous.rs · workday.rs
├── tools/                 # built-in tools (shell, file, web, task, memory, skill, …)
├── cli/                   # subcommands; wiring.rs assembles the AgentRuntime
├── config/                # one-shot resolution into ConfigSnapshot (sources → resolved → report)
└── tui/                   # full-screen chat TUI (ratatui): app · ui · markdown · approver
```

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

- `TaskRecord` changes: `~/.komo/kanban.db`
- `MemoryRecord` changes: `~/.komo/memory.db`
- other toasty models: `~/.komo/state.db`

## Roadmap

The only long-form docs file kept in this repository is the current roadmap:
[docs/personal-agent-roadmap.md](docs/personal-agent-roadmap.md).
