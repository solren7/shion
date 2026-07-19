# PRD: run resume——从 run ledger 恢复中断任务(roadmap §6 / 推荐顺序 #3)

Status: shipped (implemented 2026-07-03; see git log)

## 背景与现状

run ledger 的记录层已全部落地,resume 缺的是消费端:

| 组件 | 位置 | 状态 |
|------|------|------|
| `Run`/`RunStep`/`RunStatus` 域模型 | `domain/run.rs` | ✅ 每 turn 一条 Run,每次工具调用一条 RunStep(args 经 redaction、截 2000 字符) |
| `RunRepository` + toasty 实现 | `infra/persistence/db.rs` | ✅ start/append_step/finish/list/get/steps/prune/reconcile_interrupted |
| `reconcile_interrupted` | db.rs + `cli/gateway.rs` startup 调用 | ✅ 进程重启时把遗留 `Running` 翻成 `Failed` + `INTERRUPTED_ERROR`,注释里写明"是 resume 的第一块积木:它命名了可恢复集合" |
| `komo run list/inspect/prune` | `cli/inspect.rs`,经 gateway `GET /api/runs*` 路由 | ✅ |
| `recoverable` 字段 | — | ❌ 刻意未加(roadmap §6:"不提前加成死字段,等 resume 时由消费者驱动") |
| `resume` 动作 | — | ❌ 本 PRD |

**中断时刻的持久化状态**(`agent/runtime.rs::turn_body` 的写入顺序决定):

1. user message:**已落库**(agent loop 之前写)
2. 已完成的 RunStep:**已落库**(`execute_isolated` 逐条 append)
3. assistant message:**未落库**(loop 结束后才写)
4. Run:遗留 `Running`,由下次 startup 的 `reconcile_interrupted` 翻成 `Failed`/`interrupted (process restarted)`

## 核心判断:ledger 是审计账,不是 checkpoint——忠实断点回放不可行

三个事实决定了 resume 不能是"从第 N 轮继续驱动 `TurnDriver`":

1. 中间轮的 assistant tool-call 消息**不持久化**(`MessageRepository` 只存最终 user/assistant 消息,轮间历史活在 `RigTurnDriver` 内存里);
2. `RunStep.args` 经过 redaction(shell 擦密钥、file 丢写正文)且截断 `STEP_FIELD_CAP`=2000——不是忠实回放材料;
3. 各 provider 的 tool-call correlation id(`id`/`call_id`)是单次会话内的临时句柄,重启后无法向 LLM 重建半个 assistant turn。

所以 v1 的 resume 语义是:**重派一轮新 turn**——把中断 run 的原始输入 + 已完成步骤摘要组装成 priming 输入,dispatch 到原 session,让模型自己判断哪些副作用已生效、从哪里继续。诚实(不假装能精确续跑)、安全(新副作用照常走审批,不盲目重放任何工具调用,`Tool::idempotent` 无需介入)。

## 设计

### D1: `recoverable` 字段,消费者驱动落地

- `Run` 加 `recoverable: bool`(默认 false),`RunRecord` 同步加列。
- **置位**:`reconcile_interrupted` 翻 `Running`→`Failed` 时同时置 `recoverable = true`——它本来就是"命名可恢复集合"的地方,现在字段把这个集合持久化。
- **清零**:resume 成功派发新 turn 后置 `recoverable = false`(at-most-once,防止重复 resume;`RunRepository` 新增 `mark_resumed(id)`)。
- 只有进程中断产生 recoverable。普通 `Failed`(LLM/driver 错误)不置位——用户重发消息即可,没有"已完成一半的步骤"需要交接。
- 展示:`run list` 给 recoverable 的行加 `⟲` 标记;`run inspect` 显示 recoverable 状态。
- schema 变更:`RunRecord` 加列 = 删 `komo.db` 重建(AGENTS.md 既有约定,disposable db 不做 ALTER 迁移)。

### D2: resume 的组装与执行

`AgentRuntime` 新增 `resume_run(run: &Run, steps: &[RunStep])`(或 cli 层的纯组装函数 + 复用 `handle_input`):

1. 校验:run 存在且 `recoverable == true`,否则报错("已恢复过"/"不是中断 run")。
2. 组装 priming 输入(persisted 为一条正常 user message,transcript 里透明可见):

```
[resume run-1730000000000000000]
上一轮任务在执行中被中断(进程重启)。原始请求:

<run.input>

中断前已完成 3 个工具调用:
1. shell({"command":"cargo check"}) → ok: Finished dev profile...
2. file({"path":"src/main.rs","write":true,...}) → ok
3. web_fetch({"url":"https://..."}) → error: timeout

请检查哪些效果已经生效,从中断处继续完成任务;已生效的副作用不要重复执行。
```

   步骤摘要来自 ledger(`tool_name` + redacted args + ok/结果截断),整体受 `RUN_FIELD_CAP` 级别的长度控制。
3. dispatch 到 `run.session_id` 跑一轮**正常 turn**(新 Run 正常记账;新 run 的 input 里含旧 run id,`run inspect`/`list` 可追链路——不另加 `resumed_by` 字段)。
4. 成功后 `mark_resumed(旧 run)`。

Session 被 `/new` rotate 过的情况:原 session id 仍在(rotate 是转移历史),resume 照常派发,priming 里的步骤摘要自带上下文;CLI 输出提示"该会话此后已被重置"。

### D3: 入口与路由(Turso 锁约束)

resume 是**写路径**(跑 turn + 改 run 行),gateway 持锁时 CLI 不能直接开 db:

- **gateway 在跑**:新端点 `POST /api/runs/{id}/resume`(loopback + trusted,同 CLI chat——侧效工具自动放行),gateway 内完成校验→组装→turn→mark_resumed,返回最终回复;`GatewayClient` 加 `resume(id)`。
- **gateway 不在**:CLI 直接开 db + `cli/wiring.rs` 构建 runtime 本地跑(与 `komo chat` 同构)。
- CLI:`komo run resume [<id>]`——缺省取最新一条 recoverable(`run list` 顺序);找不到时提示"没有可恢复的 run"。

### D4(后置): 中断的主动可见性

- gateway startup 的 reconcile 发现 n>0 时,经 `HomeNotifier` 提示"上次重启中断了 n 个任务,`/resume` 可继续"。
- chat 命令 `/resume`:恢复**当前 session** 最新的 recoverable run(session-scoped,和 `/approve` 一样由 `GatewayDispatcher` 路由)。

后置理由:D1–D3 先证明 resume 语义可用;主动提示是体验增量,不阻塞核心链路。

## 实施顺序

| # | issue | 内容 | 规模 |
|---|-------|------|------|
| 01 | recoverable-field | `Run`/`RunRecord` 加字段;`reconcile_interrupted` 置位;`mark_resumed`;list/inspect 展示 | 小 |
| 02 | resume-core | priming 组装(纯函数,可测)+ 校验 + 本地直连路径 + `komo run resume` | 中 |
| 03 | gateway-route | `POST /api/runs/{id}/resume` + `GatewayClient::resume` + CLI 路由切换 | 中 |
| 04 | proactive-resume | startup 中断提示经 HomeNotifier + `/resume` chat 命令(后置) | 小 |

01→02→03 一次做完;04 等核心链路验证后再上。

## 不做

- **忠实断点回放 / checkpoint**:需要持久化中间轮 assistant 消息和未脱敏 args,与 redaction 原则冲突,且各 provider 的 correlation id 不可跨进程重建。等 §7 的"中途产物保存"立项时再评估,而不是把 ledger 偷改成 checkpoint。
- **自动 resume**(startup 自动重跑中断 run):无人在场时重放半完成任务的副作用风险不可接受;resume 必须是 operator/用户显式动作。
- **非中断 Failed run 的 resume**:重发消息已覆盖,无步骤交接价值。
- **pause / cancel**:roadmap §7 的独立课题,不搭车。
- **`resumed_by` 反向链字段**:新 run 的 input 已含旧 run id,链路可见;字段等有独立消费者(如审计视图)再加。
