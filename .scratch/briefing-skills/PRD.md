# PRD: briefing 接入真实数据——工具化 sweep + policy 的 unattended 窄通道(roadmap §2 + §3 收尾)

Status: shipped except D3 weekly (implemented 2026-07-18) — weekly deferred: TaskRepository has no completed-since query yet, needs a repo method first

## 背景与现状

`BriefingSweep` 已落地:读 open tasks + 近期记忆 → `briefing_prompt`(纯函数)→ **tool-less 的 `aux_llm.complete` 一次性调用** → `HomeNotifier` 投递,opt-in(`briefing_schedule`)+ 工作日门控。

缺口(roadmap §2 原文):briefing 接不了外部数据——日程、邮件、家里的传感器。路线也定了:**"briefing 组装时允许调 skill/工具",而不是往 core 加连接器**;前置依赖是 §3 的 unattended 窄通道(无人值守的 sweep 只能执行策略明确放行的动作)。

关键现状盘点:

| 事实 | 含义 |
|------|------|
| `Risk::Safe` 动作(web_fetch、file 读、HA 读)本来就不进审批,只受 deny 规则约束 | **只读数据源今天就能无人值守跑**——缺的不是权限,是"briefing 是 tool-less 的" |
| `PolicyApprover` 的 Allow 要求 session 在 scope("no unattended grants") | `Risk::Normal` 动作(shell、HA call_service)在 sweep 里永远被拒——这是需要窄通道打开的部分 |
| sweep/aux 的工具调用不进 run ledger(无 `RunContext`) | 工具化后 briefing 的执行没有审计——需要补 |

## 核心判断

1. **briefing 从"一次 completion"升级为"一个受限的 agent turn"**,复用 `AgentRuntime::run_agent_loop`,不再造第二个循环。做法:wiring 里构造第二个小号 `AgentRuntime`(aux LLM + 一个**只读工具集**的 `ToolExecutor` + 同一批 db handles),session 固定 `briefing:sweep`。副产品:briefing 的每次执行天然进 run ledger(`komo run list` 可审计),解掉上表第三行。
2. **unattended 是 policy 规则的显式 opt-in 字段,不是新审批器**。默认没有任何规则带它 ⇒ 行为与今天完全一致(Normal 动作在 sweep 里被拒),永不静默放宽——延续 policy 层"只收紧"的原则。
3. **数据源活在 skill 里**。日历 = 一个用 web_fetch 调 CalDAV/API 的 skill;邮件同理。core 只保证:briefing agent 能 `skill view` 加载它们、能执行它们用到的(被策略放行的)工具。

## 设计

### D1: 只读工具集的 briefing runtime

- wiring 新增 `build_briefing_runtime`:工具白名单 `time / task(list) / memory(search,list) / skill(list,view) / web_fetch / homeassistant(读三动作)`——全部 `Risk::Safe` 或只读;不注册 shell、file、reminder、session、todo。
- approver 链:`PolicyApprover::wrap(policy, DenyAll)`——Normal+ 默认全拒,除非 D2 的 unattended 规则放行。
- `BriefingSweep` 改为:组装引导 prompt(今天日期、open tasks、近期记忆、"你可以调用工具/加载 briefing 相关 skill 补充外部数据,拿不到就跳过该节")→ `runtime.handle_input("briefing:sweep", prompt)` → 产出经 `HomeNotifier` 投递。失败降级:agent turn 出错时回落到现有 tool-less `briefing_prompt` 路径,简报永远发得出去。
- 轮次预算独立收紧(`max_turns` = 4 左右):简报是聚合读,不该长跑。

### D2: policy 的 `unattended` 字段(§3 收尾)

- `[[policy.rule]]` 加可选 `unattended = true`:该 Allow 规则在**无 session** 上下文里也生效。Deny 语义不变(本来就无条件)。
- 生效面收窄:只对 D1 这类显式传入"unattended 允许"的 approver 链生效——交互路径(chat/channel)完全不受影响。
- 硬底线不动:shell 拒绝清单、HA `BLOCKED_DOMAINS` 仍在工具内部短路,unattended 规则解不开。
- 操作面:`komo policy list` 标注 unattended 规则;`komo policy check --unattended` 干跑。
- 文档姿态:示例只给只读场景(如 `network suffix your-caldav-host.com allow unattended`);shell 的 unattended 放行写明"你在为无人值守进程开 shell,风险自负"。

### D3: 每周摘要(顺手收割)

- `weekly_schedule` 配置(opt-in,同 briefing 形态),prompt 换成周视角(完成事项、卡住的项目、多次推迟的任务、新增偏好、建议清理的低置信记忆)——复用 D1 全部机件,只是另一个 `Maintenance` 实例。

## 实施顺序

1. D2 policy unattended 字段 + 单测(默认无规则=现状、deny 优先、交互路径不受影响)——独立可合。
2. D1 briefing runtime + 降级路径 + 单测(工具白名单、失败回落、ledger 有记录)。
3. 一个参考 skill(如 workday/天气/CalDAV 只读)验证端到端。
4. D3 weekly。

## 不做

- **日历/邮件连接器进 core**——roadmap 明确走 skill。
- **给 sweep 发交互审批**——无人值守没有人回 `/approve`;能做的只有"策略预先放行"或"不做"。
- **briefing 里写操作**(建 task、发消息给第三方)——聚合读定位;写操作等有真实需求再议,且必然要求 unattended 规则显式覆盖。
