# 个人 Agent 能力缺口与路线图

本文档基于当前 shion 的实现状态，整理它从"能聊天和调用工具"继续走向"能长期辅助个人事务"还差什么。重点不是复述已落地能力，而是把下一阶段的产品/工程缺口排清楚。

## 现状

被动应答、常驻入口、任务/记忆/审计这几条主链路已经成型：

- `AgentRuntime` 负责会话生命周期、消息持久化、run ledger 开关和回复写回。
- LLM 通过 `rig` 接入，但只承担单次 completion：工具调用循环已收回 `AgentRuntime::run_agent_loop`，自带瞬时错误重试（幂等区分）、软工具调用预算和 `max_turns` 轮次预算。
- gateway 常驻一个 loopback api channel（OpenAI 兼容 + `/api/*` 只读视图），CLI 在 gateway 持有 db 锁时经它路由，不再直接开 db。
- 已注册工具包括 `time`、`file`、`shell`、`web_fetch`、`web_search`、`session`、`reminder`、`memory`、`delegate`、`skill`、`task`、`todo`、`homeassistant`。
- `reminder` 支持一次性提醒和 5 字段 cron 周期提醒，由 gateway 每分钟扫描投递。
- `gateway` 已具备常驻进程、launchd 安装、维护任务调度、chat 交互式审批和 proactive home channel。
- ingress channel 已落地：Feishu、Telegram、WeChat、Home Assistant 事件通道；聊天通道带 pairing / allowlist，HA 是受 `HASS_TOKEN` 保护的本地事件入口。
- durable task 存在独立 `~/.shion/kanban.db`；long-term memory 存在独立 `~/.shion/memory.db`；session/message/run/todo 等 disposable 状态仍在 `~/.shion/shion.db`。
- reflective reviewer 已能提取 candidate memories 和 commitments；承诺会进入 task inbox。
- memory 已有 L1 pinned profile、L2 governance tool、L3 active recall。
- run ledger 已记录每个 turn 和每次工具调用，CLI 可 `shion run list/inspect`。
- 已有 operator CLI：`cron`、`session`、`task`、`run`、`memory`、`pair`、`model`、`wechat`、`workday`、`logs`、`doctor`、`dream`、`upgrade`。

总体判断：**shion 是一个简洁的 agent core，能力扩展走 skill，不往 core 里堆连接器**。入口、任务、记忆和审计已经有骨架；下一阶段 core 只做四件事——权限策略产品化、memory 质量、run resume、skill governance。日历/邮件/笔记这类真实数据源由 skill 接入（用现有通用工具调 API），core 提供的是让 skill 安全好用的地基。

## 1. 真实个人数据：走 skill，不进 core

个人 agent 的价值来自真实上下文，但连接器**不编译进 shion**。日历、邮件、外部任务、Notion/Obsidian、本地文件夹这类数据源，用 skill（指导 agent 拿现有通用工具 `web_fetch`/`shell`/`file` 调 API 的知识文件）接入。core 不新增 per-service 工具，保持简洁。

这个取舍对 core 的真实要求，恰好就是本路线图的四个重点：

- **权限策略（§3）**：skill 驱动的 API 调用要靠可配置的域名/命令授权自动放行，而不是每次人工 `/approve`。
- **skill governance（§9）**：连接器逻辑活在 skill 里，skill 的审查、保护、来源追溯就是连接器的质量线。
- 凭证仍集中在 `~/.shion/.env`，skill 文本里不放 secrets。
- 原则不变：先只读后写入，写入必须过权限策略并进 run ledger。

## 2. 主动摘要升级

每日 briefing 已落地：`BriefingSweep` 读取 open tasks + 近期记忆，用 aux LLM 组织成摘要，经 `HomeNotifier` 投递；它是 opt-in 的，只有设置 `briefing_schedule` 才运行，也可通过 `briefing_workdays_only` 限制为中国工作日。

当前缺口：

- briefing 是 tool-less 的 aux LLM 一次性调用，接不了外部数据。要包含日历/邮件，走的路是"briefing 组装时允许调 skill/工具"，而不是往 core 加连接器——这依赖 §3 的权限策略先落地（无人值守的 sweep 只能执行策略明确放行的动作）。
- 每周摘要尚未实现，但框架已经够用：换 prompt、换 cron、读取完成任务和近期 run/memory 即可。

每日摘要最终应包含：

- 今天日程
- 到期任务
- 未关闭承诺
- 最近新增重要记忆
- 待确认的 memory / task candidate
- 邮件或消息里的待回复事项

每周摘要建议包含：

- 本周完成事项
- 卡住的项目
- 被多次推迟的任务
- 新形成的偏好或工作流规则
- 建议清理的过期/低置信记忆

## 3. 权限策略产品化

**已落地**（设计与缺口分析见 `.scratch/permission-policy/PRD.md`）：

- 独立 policy 层：`domain/policy.rs` 纯规则引擎 + `agent/policy_approver.rs` 装饰器（包在 `CliApprover`/`ChatApprover` 外层），config.toml `[policy]` + `[[policy.rule]]` 配置。
- 规则维度：category（shell 命令前缀 / file 目录+读写 / network 域名 / homeassistant 服务）、matcher、channel scope、`include_dangerous`、`default_normal` 兜底。deny 永远压过 allow；无 session（sweep/aux）时 Allow 不无人值守生效。
- 读操作 deny-only：`web_fetch` 和 `file` 读以 `Risk::Safe` 过策略，deny 规则可封域名/路径（exfiltration 防线），未命中不打扰、不升级。
- 操作面：`shion policy list` / `shion policy check`（dry-run 并指出命中规则）、doctor `policy:` 段。
- 分层不变式：policy 在各工具 hardline 地板之上，只能收紧不能放松；策略判定进 tracing 日志，deny 结果以工具错误进 run ledger。

剩余缺口：

- `unattended = true` 规则标志：给无人值守 sweep（briefing-via-skill 拉外部数据）一条显式 opt-in 的窄放行通道——挂起到消费者出现（PRD issue 04）。
- 审批交互里提示"可以把这个动作加进 policy"（从临时审批到规则的引导路径）。

## 4. 任务与承诺模型

这块已经基本落地，但仍有少量产品缺口。

已实现：

- durable `Task` 单表：`inbox` / `todo` / `waiting` / `done` / `cancelled`。
- `waiting_on` 覆盖个人语境里的承诺/等待关系。
- `board` 字符串覆盖轻量项目分组，不建 Project 实体。
- `task` tool 支持 capture/list/update/complete。
- `TaskSweep` 每分钟投递到期任务，at-most-once。
- reflective reviewer 会从会话中提取 commitments，自动进入 inbox，用户再 triage。
- session-scoped `todo` tool 已落地，用于当前会话的工作焦点列表。

刻意不做：

- task dependency graph
- owner / worker claim
- multi-agent swarm 调度字段

原因仍然成立：shion 是单 turn 个人助理，没有 worker 群；这些结构暂时没有消费者。

剩余缺口：

- 更好的 task triage 操作体验，例如批量 promote/reject/board 分类。
- task 来源投递更细化：从哪个 channel 来，最好回哪个 channel。
- 与外部任务系统同步。

## 5. 长期记忆召回与治理

memory 三层已经落地：

- L1 pinned profile：手动 pin、active、confirmed/user_written、scope 过滤后注入。
- L2 memory tool/governance：save/search/list/update/promote/reject/archive/pin。
- L3 active recall：基于用户当前输入做 token-overlap recall，注入 top memories，并记录 `last_used_at`。

usage-based 治理也已落地：dreaming（`DreamSweep`，默认每晚 3 点，`shion dream [--apply]` 可预览/手动跑）按 recall 使用信号决定 candidate 的去向——常被召回的 promote 到 active，长期没人用的 archive；只动 candidate，pin 仍是手动专属。

下一步是质量升级，而不是再加一个粗糙入口：

- aux recall agent：从候选 hits 中选择、压缩、解释相关 facts。
- embedding / hybrid search：作为召回信号，不能绕过 scope/status/expiry/confidence 过滤。
- dreaming 的 query-diversity 信号（OpenClaw 的 `minUniqueQueries`）：目前只有 recall 计数，没有按 query 的 provenance。
- reviewer 防自噬规则继续保持：不能从注入块再提取记忆。

治理原则不变：自动提取只能进 candidate，不能自动 pin；影响长期行为的内容必须可追溯、可拒绝、可归档。

## 6. 工作流执行记录与恢复

run ledger 的记录层已经落地：

- 一次 turn = 一条 `Run`。
- 每次工具调用 = 一条 `RunStep`。
- 埋点在 `services/tool_registry.rs::execute_isolated`，覆盖 LLM function-calling 路径。
- step args 可由工具自行 redaction；`shell` 会擦掉疑似密钥，`file` 会丢掉写入正文。
- `shion run list [--limit N]` / `shion run inspect <id>` 可查看。

剪枝已有 operator 动作：`shion run prune --before <date>|--keep <N>`。

尚未做：

- `resume` 上一次中断任务。
- 与 resume 配套的 `recoverable` 字段。
- 基于 run ledger 的权限审计视图。

`recoverable` 不应提前加成死字段。等真正做 resume 时，再由消费者驱动模型变化。

## 7. 更强的 planner / orchestrator

架构前提已经完成：工具循环收回了 `AgentRuntime::run_agent_loop`，rig 只承担单次 completion（`LlmClient::begin_turn` / `TurnDriver` 是扩展的 seam）。控制点加在循环的轮与轮之间，不再受 rig 约束。

已落地的控制点：

- `todo`：模型可维护当前会话的步骤列表，最多一个 `in_progress`。
- 工具失败重试（`execute_isolated`）：连接级错误任何工具都重试，歧义错误（超时/5xx/429）只重试声明 `idempotent` 的只读工具，终结错误和 panic 不重试。
- 预算：`max_turns` 轮次预算（超限强制收尾）+ `MAX_TOOL_CALLS_PER_TURN` 软工具调用预算 + `cap_tool_result` 结果字节上限。

仍缺：

- 澄清问题：信息不足时先问，而不是盲目执行（新的 `Step` 变体或哨兵工具，循环里识别）。
- 时间 / token / 风险等级维度的预算。
- 中途产物保存：长任务先落草稿或 run artifact。
- 取消/暂停/恢复长任务。

## 8. 本地快捷入口

api channel 的 loopback HTTP 已经覆盖这个需求的大半：脚本、Raycast、Automator、快捷指令都可以直接 `POST /v1/chat/completions`（`~/.shion/gateway.json` 里有地址和 key）。不再计划新的本地 channel；剪贴板 / share sheet / 浏览器页入口如果要做，是调 api channel 的外围小工具，不进 core。

## 9. 可观察性与 inspect

已有 operator 视图：

- `shion cron list`
- `shion session list/clean`
- `shion task list`
- `shion run list/inspect`
- `shion memory list/search/promote/reject/pin`
- `shion pair list/approve/revoke`
- `shion logs`
- `shion workday`
- `shion doctor`：config health + gateway 存活 + channel/凭证状态 + home channel + 最近失败 run（gateway 状态聚合与 config health 都归它）
- `shion memory report`：memory quality report（状态/置信分布、待 triage 的堆）
- `shion run prune`
- `shion dream`：dreaming 预览/手动执行

还可以补——**skill governance**（连接器走 skill 后，这从"锦上添花"升级为核心设施）：

- skill inspect：单个 skill 的全文、来源（用户写入 / reviewer 提取）、修改历史。
- 启停与保护：disable 一个可疑 skill 而不删除；`protected` 语义明确化（谁能改、agent 能否自改）。
- 来源追溯与审查：reviewer 自动提取的 skill 应像 memory candidate 一样有 triage 流程，而不是直接生效。
- skill 调用审计：run ledger 里能看到某次 turn 用了哪个 skill。

目标是让用户随时看懂 shion 当前知道什么、正在做什么、为什么这样做。

## 10. 文档与代码同步

文档落后会直接误导后续 agent。当前仓库刻意只保留一份长期路线图：

- README：面向用户的当前能力、安装、配置和运行方式。
- AGENTS.md：面向 coding agent 的真实架构、命令和工程约束。
- `docs/personal-agent-roadmap.md`：产品能力缺口和推荐实现顺序。

历史设计稿和已落地执行计划已删除，避免继续引用过期事实。以后新增长文档前先判断是否应该并入 README、AGENTS 或本 roadmap。

## 推荐实现顺序

core 只做四件事，按依赖顺序：

1. **权限策略配置化（§3）**：目录、命令前缀、网络域名、channel/session scope 的可配置放行，独立 policy 层。它是 skill 生态和无人值守 sweep 的地基，排第一。
2. **memory 质量（§5）**：aux recall agent、candidate 批量 triage 体验、dreaming 的 query-diversity 信号；embedding/hybrid search 之后再说。
3. **run resume（§6）**：从已有 run ledger 恢复中断任务，`recoverable` 字段由它驱动加入。
4. **skill governance（§9）**：inspect、启停/保护、triage 流程、调用审计——连接器走 skill 的前提设施。

明确不做进 core 的：日历/邮件/笔记等数据连接器（走 skill）、briefing 的连接器化增强（等 §3 落地后由 skill 提供数据）、本地快捷入口的新 channel（api channel 的 loopback HTTP 已可承接脚本/Raycast）。

已经完成的里程碑：ingress、durable task、commitment extraction、ChatApprover、daily briefing、memory L1/L2/L3 + dreaming、run ledger + prune、recurring reminders、WeChat、Home Assistant、in-house tool loop（重试/预算）、api channel + CLI 路由、operator health（doctor / memory report）。下一阶段的主题是：**core 收敛，生态外放**——把权限、记忆、恢复、治理做扎实，让 skill 安全地承接一切具体能力。
