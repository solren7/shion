# 执行文档：周期提醒（reminder v2）— 启用 schedule 字段

> 状态：待执行
> 前置：`docs/reminder-implementation-plan.md`（v1，已落地）
> 背景：v1 只支持一次性提醒（"1分钟后提醒我"）。真实需求里"每天早上9点吃药"
> "每周一提醒写周报"这类**墙钟对齐的周期提醒**必然出现。interval（`now + 24h`）
> 表达不了墙钟：每次 sweep 的延迟会累积漂移，夏令时切换直接错一小时——
> 这正是 cron 表达式的本职。v1 在 `ReminderRecord` 预留的 `schedule` 字段就是为此。

## 1. 目标与范围

**v2 做什么：**

1. `reminder` 工具 create 增加 `cron` 参数（5 字段表达式，**本地时区**语义）。
2. `ReminderSweep` 识别周期提醒：到期投递后用 croner 推算下一次 `run_at` 写回，
   状态保持 `pending`，而不是标记 `fired`。
3. 宕机补发策略：错过窗口的周期提醒**只投递一条迟到说明**，然后跳到未来的
   下一次——绝不逐次补发（gateway 停一周 ≠ 早上连响 7 次）。
4. 展示时间从 UTC 改为本地时区（v1 的确认文案是 UTC，对用户不友好）。

**v2 明确不做：**

- ❌ 到期 spawn agent 跑 prompt（hermes 式定时任务）——仍是静态文本投递，
  不经过 LLM；任务队列泛化另起文档
- ❌ 自然语言转 cron（"每天早上9点" → `0 9 * * *` 由模型完成，工具只收表达式）
- ❌ 秒级精度 / 6 字段 cron——croner 5 字段，分钟粒度，与 sweep 每分钟 tick 匹配
- ❌ 暂停/恢复周期提醒——cancel 即终止，要改就 cancel + 重建

**关键设计决策：**

| 决策 | 理由 |
|---|---|
| cron 表达式放在 reminder 行上，不放 supervise 层 | supervise（review/reminder sweep）是固定间隔，cron 的归宿是用户面的墙钟需求（gbrain 调研结论：调度字段 `delay_until` + 周期规则分离） |
| 下一次计算用 `chrono::Local` | "每天9点"指本地时间；daemon 的 supervise tick 保持 UTC 不动（系统作业无墙钟语义） |
| 投递后先算下一次再写回，失败则下个 tick 重试 | 与 v1 "先 notify 后 set_status、宁重勿丢" 同一原则 |
| 超期 > grace 只投一条 missed 说明并前跳 | 防补发风暴；用户需要知道"错过了"，不需要被轰炸 |

## 2. 架构（v1 基础上的增量）

```
ReminderSweep.run() 每分钟：
  for r in list_pending() where run_at <= now:
    ├─ r.schedule 为空     → v1 路径原样（fired / missed）
    └─ r.schedule 非空（周期）：
         ├─ late <= 600s → notify("Shion reminder", msg)
         ├─ late >  600s → notify("Shion (missed reminder)", msg)   // 仅一条
         └─ reschedule(id, next_occurrence_local(schedule, now))     // 状态仍 pending
```

无 schema 迁移：`ReminderRecord.schedule` 列 v1 已存在（恒空串），
旧 `~/.shion/shion.db` 不用删。

## 3. 执行步骤

### Step 1 — domain：`Reminder` 增加 schedule + repository 增加 reschedule

`src/domain/reminder.rs`：

```rust
pub struct Reminder {
    // ... 现有字段不变
    /// 5 字段 cron 表达式（本地时区）。空串 = 一次性提醒。
    pub schedule: String,
}

impl Reminder {
    pub fn new(message: String, run_at: i64) -> Self { /* schedule: String::new() */ }
    pub fn recurring(message: String, run_at: i64, schedule: String) -> Self { ... }
    pub fn is_recurring(&self) -> bool { !self.schedule.is_empty() }
}

#[async_trait]
pub trait ReminderRepository: Send + Sync {
    // ... 现有三个方法不变
    /// 周期提醒投递后推进到下一次（状态保持 pending）。
    async fn reschedule(&self, id: &str, next_run_at: i64) -> anyhow::Result<()>;
}
```

`src/infra/db.rs`：

- `save` 写 `reminder.schedule`（替换 v1 硬编码的 `String::new()`）
- `reminder_from_record` 映射 `schedule`
- 实现 `reschedule`：`record.update().run_at(next_run_at).exec()`

### Step 2 — cron 推算：时区泛型纯函数

放 `src/agent/daemon.rs`（croner 已在此层，domain 不允许外部 crate）：

```rust
/// 时区泛型，便于用 FixedOffset 写确定性测试；生产走 Local 包装。
pub fn next_occurrence_in<Tz: chrono::TimeZone>(
    expr: &str,
    after: chrono::DateTime<Tz>,
) -> anyhow::Result<chrono::DateTime<Tz>> {
    let cron = expr.parse::<croner::Cron>()
        .map_err(|e| anyhow::anyhow!("invalid cron expression `{expr}`: {e}"))?;
    Ok(cron.find_next_occurrence(&after, false)?)
}

/// 生产入口：本地时区推算下一次，返回 unix 秒。
pub fn next_occurrence_local(expr: &str, after_unix: i64) -> anyhow::Result<i64> { ... }
```

工具层的 create 校验与 sweep 的推进共用这两个函数（tool 同时拿到
"下一次触发时间"用于确认文案）。

### Step 3 — tools：`reminder` 工具增加 `cron` 参数

`src/tools/reminder.rs`：

```rust
// parameters_schema 增加：
// cron: string — 5 字段 cron 表达式，本地时区，如 "0 9 * * *" = 每天早上9点
//                （create 时 after / at / cron 三选一）
```

- `cron` 路径：`next_occurrence_local(&cron, now)` 算首次 `run_at`，
  `Reminder::recurring(...)` 持久化；表达式非法在 create 时即报错
- 确认文案带周期信息：
  `"Recurring reminder <id> set: <expr> (next at <本地时间>). Delivered by the gateway process …"`
- list 对周期提醒显示 `(repeats: <expr>)`
- cancel 不变（周期提醒 cancel 即永久终止）
- **展示时区修正**：v1 用 `chrono::DateTime::from_timestamp(..).to_rfc3339()`（UTC），
  统一改为 `.with_timezone(&chrono::Local)` 后格式化

### Step 4 — agent：`ReminderSweep` 周期分支

`src/agent/daemon.rs` 的 `Maintenance for ReminderSweep`：

```rust
for r in self.reminders.list_pending().await? {
    if r.run_at > now { continue; }
    let late = now - r.run_at;

    if r.is_recurring() {
        let title = if late > REMINDER_GRACE_SECS { "Shion (missed reminder)" }
                    else { "Shion reminder" };
        self.notifier.notify(title, &r.message).await.ok();
        // 从 now（而非 run_at）推算：宕机数日也只前跳到未来的下一次，无补发风暴
        match next_occurrence_local(&r.schedule, now) {
            Ok(next) => self.reminders.reschedule(&r.id, next).await?,
            Err(e) => {
                // 表达式损坏（理论上 create 时已校验）：降级为 missed 终止，
                // 否则每分钟报错刷屏
                warn!(error = %e, id = %r.id, "broken schedule; marking missed");
                self.reminders.set_status(&r.id, ReminderStatus::Missed).await?;
            }
        }
        summary.reminders_fired += 1;
    } else {
        // v1 一次性路径，原样不动
    }
}
```

注意保持 v1 的两条原则：先 notify 后写库（宁重勿丢）；单条失败隔离不崩 sweep。

### Step 5 — preamble 更新（`src/infra/llm.rs`）

reminder 段落追加：

```
For recurring reminders ("every day at 9am"), pass a 5-field cron expression
via the `cron` parameter (e.g. "0 9 * * *"); times are the user's local
timezone. One-shot reminders use `after` or `at` as before.
```

### Step 6 — 测试（行为命名）

| 测试 | 行为 |
|---|---|
| `next_occurrence_in_computes_strictly_future_fire` | FixedOffset(+8) 下 `0 9 * * *`，08:00 → 当天 09:00，09:00 → 次日 09:00（严格未来，防 next == run_at 死循环） |
| `next_occurrence_in_rejects_invalid_expr` | `"not a cron"` 报错 |
| `reminder_tool_create_with_cron_persists_schedule` | FakeRepo 收到 schedule 非空、run_at 在未来的 pending 记录，回复含表达式 |
| `reminder_tool_rejects_invalid_cron` | create 即报错，不落库 |
| `sweep_advances_recurring_reminder` | 到期周期提醒 → notify 一次，状态仍 pending，run_at 推进到未来 |
| `sweep_recurring_overdue_fires_once_and_skips_catchup` | 超期 3 天的每日提醒 → 仅一条 missed 通知，run_at 跳到未来下一次 |
| `sweep_marks_recurring_with_broken_schedule_missed` | schedule 损坏 → 状态 Missed，不再重试 |
| `db_reminder_schedule_roundtrip`（infra/db.rs） | save(recurring) → list_pending 带 schedule → reschedule 后 run_at 更新且仍 pending |

FakeRepo / FakeNotifier 复用 v1 测试已有实现（补 `reschedule`）。

## 4. 验收标准

- [ ] `cargo test` 全过；`cargo fmt`
- [ ] 无需删库：旧 `~/.shion/shion.db` 直接兼容（schedule 列已存在）
- [ ] `cargo install --path .` + `shion gateway restart`
- [ ] `shion chat` 说"每天早上9点提醒我喝水"：模型传 `cron: "0 9 * * *"`，
      确认文案显示本地时间的下一次触发
- [ ] 把一条周期提醒的 run_at 手动改到 2 分钟后（或建 `* * * * *` 测试提醒）：
      到点收到通知，`reminder list` 显示 run_at 已推进、仍 pending
- [ ] gateway 停 15 分钟跨过触发点再启动：收到一条 missed 通知，
      下一次时间在未来，无重复轰炸
- [ ] cancel 周期提醒后不再触发

## 5. 提交切分

1. `add schedule to reminder domain model, repository reschedule`（Step 1 + db 测试）
2. `add timezone-aware cron occurrence helpers`（Step 2 + 测试）
3. `reminder tool accepts cron expressions, show local times`（Step 3 + 测试）
4. `reminder sweep advances recurring reminders`（Step 4 + 测试）
5. `teach preamble about recurring reminders`（Step 5）

## 6. 风险与边界

- **时区**：存储仍是 unix 秒（无歧义）；只有两处涉及时区——cron 推算用 `Local`，
  展示格式化用 `Local`。DST 切换由 croner+chrono 处理（不存在的本地时间自动跳过）
- **补发风暴**：从 `now` 而非 `run_at` 推算下一次，结构上杜绝
- **死循环**：`find_next_occurrence(.., false)` 严格未来，next 恒 > now
- **表达式损坏**（绕过工具直接写库）：sweep 降级标记 missed，不会每分钟刷错
- **模型写错表达式**（语义错但语法对，如想要9点写成 `9 0 * * *`）：
  确认文案回显"下一次触发的本地时间"，用户一眼能发现
- **机器时区变更**：下一次触发已按旧时区固化为 unix 秒，变更后的首次触发偏移一次，
  之后自愈（每次都按当前 Local 重算）。接受

## 7. v3 展望（不在本次范围）

- 任务队列泛化：`reminder` → `task` 工具（gbrain minions 蓝图：`delay_until` +
  handler 注册表 + agent 型任务带 `allowed_tools` 白名单），reminder 折叠为
  "投递静态文本"的 task 特例
- 周期提醒的暂停/恢复、"工作日"语义（`0 9 * * 1-5` 已可表达，无需额外建模）
- 投递走 config.toml 声明的 egress channel（飞书/Telegram）
