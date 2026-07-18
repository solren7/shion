# PRD: 澄清而不是盲干——ask_user 哨兵工具(roadmap §7 剩余项)

Status: draft (needs-triage)

## 背景与现状

roadmap §7 已落地的控制点:`todo` 步骤列表、工具失败重试、三层预算(`max_turns` 轮次 / 每轮工具调用数 / 结果字节)。仍缺的第一项是**澄清**:信息不足时先问用户,而不是猜着执行。

现状的两个层次的缺口:

| 层次 | 现状 | 缺口 |
|------|------|------|
| turn 末尾问 | 模型可以用 `Step::Final("你指哪个文件?")` 结束 turn 来提问 | 行为上没有引导;更重要的是**轮间上下文丢失**——中间轮的 assistant tool-call 历史只活在 `RigTurnDriver` 内存里,turn 结束即丢,用户回答后模型要重做所有已完成的工具调用 |
| turn 中途问 | 无机制 | 模型跑到第 3 个工具调用发现歧义,只能硬猜或放弃整轮 |

## 核心判断:用哨兵工具,不动 `Step`/`TurnDriver` seam

roadmap 原文给了两条路:"新的 `Step` 变体或哨兵工具"。选**哨兵工具**:

1. **零 seam 改动**——`LlmClient`/`TurnDriver`/`run_agent_loop` 全部不动。`ask_user` 作为一个普通 `Tool` 注册,模型通过 function calling 自然发现;它在工具执行期间挂起、拿到答案后把答案作为 tool result 返回,**turn 不结束,driver 的轮间历史完整保留**——这正是 turn 末尾问做不到的。
2. **挂起机制现成**——gateway 的 `ChatApprover` 已经实现了同构的东西:turn 挂在 per-session 的 `oneshot` 上等 `/approve`,dispatcher 收到回复后 resolve。clarify 就是"resolve 的载荷从 `Decision` 换成自由文本"。
3. Claude Code 的 `AskUserQuestion`、hermes 的同类工具都验证过这个形态。

## 设计

### D1: `ClarifyState` + dispatcher 路由(镜像 `ApprovalState`)

- `agent/interaction.rs` 加 `ClarifyState`:`pending: Mutex<HashMap<session_id, oneshot::Sender<String>>>`,`wait(session, question)` / `resolve(session, text) -> bool` / `clear(session)`。
- **dispatcher 路由变更**(唯一的行为改动点):inbound 消息先查 `ClarifyState`——有挂起的 clarify 时,消息作为**答案** resolve 它,不再排队成新 turn。chat 控制命令(`/new`、`/approve`…)优先级更高:`/new` 取消挂起的 clarify(drop sender,waiter 读到"用户重开会话")。
- 与 approval 并存:一个 turn 同一时刻只会挂一个(工具串行等待),`ApprovalState` 查询在前(`/approve`/`/deny` 是显式命令,不会被误吃)。

### D2: `tools/ask_user.rs`——会话内挂起的哨兵工具

- `Tool` 实现:`name = "ask_user"`,参数 `{ question: string, options?: string[] }`(options 渲染成编号列表,回答数字或原文均可)。
- `execute`:从 ambient `SessionContext` 拿 `ReplySink` → 发送问题 → `ClarifyState::wait` 挂起 → 答案作为工具结果返回给模型。
- **超时**:默认 10 分钟(比 approval 的 5 分钟长——澄清可能需要用户查东西)。超时/无会话(sweep、HA 事件、detached)→ 返回 "user did not answer; proceed with your best assumption, stating it explicitly, or conclude the turn"——模型收尾而不是报错。
- **预算**:每 turn 最多 2 次 clarify(executor 的 per-tool 计数,防"审讯式"对话);超出→工具返回"clarify budget exhausted"。
- Risk::Safe——它不产生副作用,不进 policy/approval;但要计入 run ledger(RunStep 自然记录,审计里可见问了什么、答了什么)。

### D3: 提示词引导(system prompt 一段)

`agent/system_prompt.rs` 工具感知段(gated on `ask_user` 注册)加一条:关键参数歧义、目标文件/对象不明、动作不可逆且意图不确定时,先 `ask_user`;不要为可自行推断的小事打断用户。

### D4: TUI 路径

TUI 的 `TuiApprover` 已有模态队列;clarify 复用同一 UI 形态:问题显示在输入框上方,下一条输入即答案(`tui/app.rs` 加一个 `pending_clarify` 状态位,不弹模态——自由文本输入本来就是主交互)。

## 实施顺序

1. `ClarifyState` + dispatcher 路由 + 单测(resolve/超时/被 `/new` 取消/与 approval 并存)。
2. `ask_user` 工具 + executor 预算 + wiring 注册(gateway 与 chat 两路)。
3. system prompt 引导段。
4. TUI 支持。

## 不做

- **`Step::Clarify` 变体**——哨兵工具已覆盖;driver seam 留给真正需要轮级语义的控制点(如 token 预算)。
- **跨 turn 的 clarify 持久化**——挂起是进程内的;gateway 重启丢 clarify 等价于丢一个未回答的问题,用户重发即可,不值得进 ledger resume 语义。
- **多问题表单**——`options` 数组够用;复杂表单是 TUI 产品化方向,不是 core 语义。
