# 执行文档：reminder 提醒系统 — 借鉴 hermes-agent cron 子系统

> 状态：待执行
> 参考：hermes-agent `cron/jobs.py`（任务存储）、`cron/scheduler.py`（gateway 每 60s tick）、
> `tools/cronjob_tools.py`（模型侧单一 action 工具）、平台 channel 投递。
> 背景：用户说"1分钟后提醒我"，模型假装倒计时但什么都没发生（能力幻觉）。

## 1. 目标与范围

**v1 做什么：**

1. 模型侧 `reminder` 工具（action 风格）：create / list / cancel，写入 `shion.db`。
2. gateway 每分钟扫描到期提醒并投递（macOS 桌面通知 + 日志）。
3. preamble 更新：告诉模型它**有**提醒能力（走 `reminder` 工具）、提醒由 gateway
   进程投递；没有 gateway 时如实告知用户。

**v1 明确不做（对照 hermes 裁剪）：**

- ❌ 周期任务（`every 30m` / cron 表达式）——表结构预留字段，逻辑只支持一次性
- ❌ 到期后 spawn agent 跑 prompt（hermes 的做法）——v1 提醒是**静态文本投递**，
  不经过 LLM，因此 hermes 的注入扫描、cron-agent 工具禁用先天不需要
- ❌ 多平台投递（飞书/Telegram）——等 config.toml 的 egress channel 声明落地
- ❌ tick 的跨进程文件锁——shion 单 gateway 实例（socket 单实例守卫已删，
  暂以文档约定单实例）

**与 hermes 的关键对应：**

| hermes | shion v1 |
|---|---|
| `~/.hermes/cron/jobs.json` | `shion.db` 新表 `ReminderRecord` |
| `cronjob` 工具（create/list/…） | `reminder` 工具（create/list/cancel） |
| gateway 后台线程每 60s `tick()` | `ReminderSweep`（新的 `Maintenance` 实现），schedule `* * * * *` |
| `ONESHOT_GRACE_SECONDS = 120` | 超期宽限 10 分钟，过期标记 `missed` 并仍投递一条迟到说明 |
| 投递到平台 home channel | `Notifier` trait，v1 实现 = macOS `osascript` 通知 |

## 2. 架构

```
chat 进程                          gateway 进程（常驻）
┌─────────────────┐               ┌──────────────────────────────┐
│ reminder 工具    │── 写入 ──▶    │ supervise(每分钟)             │
│ (create/list/   │   shion.db    │   └─ ReminderSweep            │
│  cancel)        │      ▲        │       ├─ 查 due reminders     │
└─────────────────┘      └────────│       ├─ Notifier.notify()    │
                                  │       └─ 标记 fired/missed    │
                                  │ supervise(config schedule)    │
                                  │   └─ ReviewSweep（已有）       │
                                  └──────────────────────────────┘
```

存（chat）与发（gateway）分离：chat 进程关闭提醒不丢，由 always-on gateway 到点投递。

## 3. 执行步骤

### Step 1 — domain：`Reminder` 值类型 + repository trait

`src/domain/reminder.rs`：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderStatus { Pending, Fired, Missed, Cancelled }

#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: String,          // uuid v7（沿用 session id 习惯）
    pub message: String,     // 投递的文本
    pub run_at: i64,         // unix 秒，到期时间
    pub status: ReminderStatus,
    pub created_at: i64,
}
```

`src/domain/repository.rs` 增加：

```rust
#[async_trait]
pub trait ReminderRepository: Send + Sync {
    async fn save(&self, reminder: &Reminder) -> anyhow::Result<()>;
    async fn list_pending(&self) -> anyhow::Result<Vec<Reminder>>;
    /// 状态流转（pending → fired/missed/cancelled）
    async fn set_status(&self, id: &str, status: ReminderStatus) -> anyhow::Result<()>;
}
```

### Step 2 — infra：`ReminderRecord` toasty 模型

`src/infra/db.rs` 增加（模式照抄 `MemoryRecord`）：

```rust
#[derive(Debug, toasty::Model)]
struct ReminderRecord {
    #[key]
    id: String,
    message: String,
    run_at: i64,
    status: String,          // "pending" | "fired" | "missed" | "cancelled"
    schedule: String,        // v1 恒为空串；预留给 v2 的 cron 表达式
    created_at: i64,
}
```

- 注册进 `Db::connect` 的 `toasty::models!(...)` 列表
- `Db` 实现 `ReminderRepository`（套 `SessionRepository` 的写法）

⚠️ **迁移说明**：`Db::connect` 只对**新建**的 db 文件执行 `push_schema`
（toasty 的 push_schema 不幂等）。升级后旧 `~/.shion/shion.db` 没有 reminders 表，
需要删库重建。AGENTS.md 已声明 shion.db 是 disposable developer state，v1 接受；
在 AGENTS.md 补一行"schema 变更后删除 ~/.shion/shion.db"。

### Step 3 — domain + infra：`Notifier` 投递抽象

`src/domain/notify.rs`：

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}
```

`src/infra/macos_notifier.rs`：用 `tokio::process::Command` 调
`osascript -e 'display notification "<body>" with title "<title>"'`。
body/title 进入 AppleScript 字符串前必须转义 `"` 和 `\`（防注入——
提醒文本是模型/用户可控的）。非 macOS 或 osascript 失败时降级为
`tracing::warn` 日志，**不让 sweep 失败**（对应 hermes "投递失败输出也落盘"）。

### Step 4 — tools：`reminder` 工具

`src/tools/reminder.rs`（action 模式照抄 `memory.rs` / `session.rs`）：

```rust
// parameters_schema：
// action: "create" | "list" | "cancel"   (required)
// message: string      — 提醒内容（create）
// after:   string      — 相对延时 "45s" / "5m" / "2h" / "1d"（create，二选一）
// at:      string      — RFC3339 绝对时间（create，二选一）
// id:      string      — 提醒 id（cancel）
```

- `after` 解析器是纯函数 `parse_after(s: &str) -> anyhow::Result<Duration>`，
  支持 `s/m/h/d` 后缀（对应 hermes `parse_schedule` 的 "30m" → once 路径）
- create 返回值附带提示：
  `"reminder <id> set for <rfc3339>. Delivered by the gateway process — make sure `shion gateway` is running."`
  ——把"需要 gateway"这个事实交给模型转述，而不是让它瞎承诺
- list 只列 pending；cancel 做 `set_status(Cancelled)`
- 注册：`cli/wiring.rs` `tools.register(Arc::new(ReminderTool::new(db.clone(), ...)))`
  （`Db` 实现 `ReminderRepository`，直接传 `db.clone()`）

### Step 5 — agent：`ReminderSweep`（新的 `Maintenance` 实现）

`src/agent/daemon.rs` 增加：

```rust
pub struct ReminderSweep {
    pub reminders: Arc<dyn ReminderRepository>,
    pub notifier: Arc<dyn Notifier>,
}

const REMINDER_GRACE_SECS: i64 = 600; // 超期 10 分钟内照常投递，再晚标记 missed

#[async_trait]
impl Maintenance for ReminderSweep {
    async fn run(&self) -> anyhow::Result<MaintenanceSummary> {
        let now = /* unix now */;
        for r in self.reminders.list_pending().await? {
            if r.run_at > now { continue; }
            let late = now - r.run_at;
            if late > REMINDER_GRACE_SECS {
                // gateway 宕机错过窗口：标记 missed，仍投递一条迟到说明
                self.notifier.notify("Shion (missed reminder)", &r.message).await.ok();
                self.reminders.set_status(&r.id, ReminderStatus::Missed).await?;
            } else {
                self.notifier.notify("Shion reminder", &r.message).await.ok();
                self.reminders.set_status(&r.id, ReminderStatus::Fired).await?;
            }
        }
        Ok(summary) // MaintenanceSummary 增加 reminders_fired: usize 字段
    }
}
```

单条投递失败隔离（`warn` + 继续），沿用 ReviewSweep "单点失败不崩整个 sweep" 的原则。
**先 notify 后 set_status**：宁可重复提醒一次，不可静默丢失。

### Step 6 — gateway：支持多个 MaintenanceService

`src/agent/gateway.rs`：

```rust
// Option<MaintenanceService> → Vec<MaintenanceService>
pub fn with_maintenance(mut self, service: MaintenanceService) -> Self {
    self.services.push(service);   // 方法名不变，可叠加调用
    self
}
// run() 里 for service in self.services { tokio::spawn(supervise(...)) }
```

`src/cli/gateway.rs` 接线：

```rust
let gateway = Gateway::new(handler)
    .with_maintenance(MaintenanceService { schedule: review_schedule, maintenance: review_sweep })
    .with_maintenance(MaintenanceService {
        schedule: Schedule::parse("* * * * *")?,   // 每分钟，硬编码
        maintenance: Arc::new(ReminderSweep { reminders: db.clone(), notifier }),
    });
```

注意：两个 supervise 各自持有独立的熔断计数器（现状天然如此，无须改动）。

### Step 7 — preamble 更新（`src/infra/llm.rs`）

在现有 PREAMBLE 基础上追加：

```
You CAN schedule reminders: call the `reminder` tool (action=create) with a
message and a delay. Reminders are delivered as desktop notifications by the
`shion gateway` background process — you do NOT count down yourself, and you
must never pretend to track time in the conversation. If the user asks for a
reminder, create it with the tool and relay the tool's confirmation.
```

### Step 8 — 测试（行为命名，`#[cfg(test)]`）

| 测试 | 行为 |
|---|---|
| `parse_after_supports_s_m_h_d` | "45s"/"5m"/"2h"/"1d" 解析正确，"abc" 报错 |
| `reminder_tool_create_persists_pending` | FakeRepo 收到 pending 记录，回复含 id 和时间 |
| `reminder_tool_cancel_sets_status` | cancel 后状态为 Cancelled |
| `sweep_fires_due_reminder` | run_at 已过 → FakeNotifier 收到一次，状态 Fired |
| `sweep_skips_future_reminder` | run_at 未到 → 不投递，仍 pending |
| `sweep_marks_long_overdue_as_missed` | 超期 > 600s → 状态 Missed |
| `notifier_failure_does_not_abort_sweep` | FakeNotifier 报错 → 其余提醒照常处理 |
| `db_reminder_roundtrip`（infra/db.rs） | save → list_pending → set_status 全链路 |

## 4. 验收标准

- [ ] `cargo test` 全过；`cargo fmt`
- [ ] 删除旧 `~/.shion/shion.db` 后 `cargo install --path .`
- [ ] `shion gateway` 跑起来，另开终端 `shion chat` 说"1分钟后提醒我喝水"：
  模型调用 reminder 工具并转述确认（不再假装倒计时）
- [ ] ≤ 2 分钟内收到 macOS 桌面通知（每分钟 tick + 创建时机，最坏 ~2 分钟）
- [ ] gateway 未运行时创建提醒：模型如实告知需要 gateway
- [ ] 关掉 chat 进程，提醒仍到达（存/发分离验证）
- [ ] 把 gateway 停 15 分钟再启动：旧提醒收到 "missed" 通知且不再重复投递

## 5. 提交切分

1. `add reminder domain model and repository`（Step 1–2 + db 测试）
2. `add notifier with macos osascript backend`（Step 3）
3. `add reminder tool`（Step 4 + 测试）
4. `gateway hosts multiple maintenance services, add reminder sweep`（Step 5–6 + 测试）
5. `teach preamble about reminder capability`（Step 7）

## 6. 风险与边界

- **gateway 不在跑**：提醒静默滞留 → 工具确认文案已把事实交给模型转述；
  后续可在 chat 启动时检测 pending 且无 gateway 时提示（defer）
- **重复投递**：notify 成功但 set_status 前进程被杀 → 下个 tick 重发一次。
  接受（宁重勿丢），v2 可加 fired_at 幂等
- **AppleScript 注入**：消息文本转义 `"` / `\`（Step 3 已列为硬要求）
- **时区**：run_at 全程 unix 秒，展示时才格式化；croner tick 用 UTC（现状一致）

## 7. v2 展望（不在本次范围）

- 周期提醒：`schedule` 字段已预留，`ReminderSweep` 到期后按 cron 推算下一次而非标记 Fired
- hermes 式"定时 agent 任务"：到期 spawn 非交互 agent 跑 prompt——届时必须引入
  hermes 的两条安全规则：cron-spawned agent 禁用 `reminder` 工具（防自我复制）、
  prompt 注入扫描
- 投递走 config.toml 声明的 egress channel（飞书/Telegram），替换硬编码的 macOS 通知
