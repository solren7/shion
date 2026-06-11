# 个人 Agent 能力缺口与路线图

本文档基于当前 shion 的实现状态，整理一个个人 agent 工具从“能聊天和调用工具”走向“能长期辅助个人事务”的能力缺口。

当前代码已经具备不少基础设施：

- `AgentRuntime` 负责会话生命周期、消息持久化、planner 决策和回复写回
- LLM 通过 `rig` 接入，并暴露内置工具给模型调用
- 已注册的工具包括 `time`、`file`、`shell`、`web_fetch`、`web_search`、`session`、`reminder`、`memory`、`delegate`、`skill`
- `gateway` 已经具备常驻进程、维护任务、提醒投递和非交互审批策略
- 反思 reviewer、markdown memory、skill registry 已经具备初步自学习闭环

因此，下一阶段最缺的不是再补一个零散工具，而是补齐个人事务的长期闭环。

## 1. 首要缺口：任务与项目模型

`reminder` 只解决“到点通知”，但个人 agent 更核心的问题是“我现在该做什么”“哪些事情卡住了”“我昨天承诺了什么”。

建议新增一组一等领域模型：

- `Task`：标题、状态、优先级、截止时间、来源、所属项目、备注
- `Project`：目标、状态、活跃任务、最近进展
- `InboxItem`：尚未归类的输入、想法、待处理事项
- `Commitment`：从对话、会议、邮件中提取出的承诺和待跟进事项

对应工具可以先收敛为一个 `task` tool：

- `capture`：快速收集任务或 inbox item
- `list`：查看当前任务
- `update`：修改状态、截止时间、项目归属
- `complete`：完成任务
- `plan_today`：生成今日建议执行列表

这是最能把 shion 从聊天工具推进到个人 agent 的能力。

## 2. 常驻入口

`gateway` 已经是正确方向，但当前还缺真实 ingress。没有稳定入口，agent 仍然主要是一个 REPL。

优先级建议：

1. 本地 unix socket 或 HTTP ingress，方便其他脚本、快捷指令、Raycast、Automator 调用
2. 剪贴板 / share sheet 风格入口，支持快速丢一段文本给 shion
3. 一个聊天平台入口，例如 Lark、Telegram 或 Slack

入口不需要一开始做复杂，核心是把“打开终端聊天”变成“随时把一个事件交给 shion”。

## 3. 长期记忆检索与治理

当前已有 markdown memory 和 reflective reviewer，但还需要更明确的记忆分类、检索和治理机制。

建议至少区分以下类别：

- `fact`：关于用户、环境、项目的事实
- `preference`：用户偏好、输出格式、工作方式
- `project`：长期项目背景和当前状态
- `person`：人物、团队、协作关系
- `decision`：已经做出的决策及原因
- `open_loop`：尚未关闭的问题、承诺、等待事项

每条记忆最好带上：

- source session id / message id
- created_at / updated_at
- confidence
- optional expiry

这样可以减少 agent 自己污染记忆，也能在回答时追溯“为什么这么认为”。

## 4. 工作流执行记录

长任务需要可恢复、可审计。建议新增 `Run` / `RunStep` 模型，记录每次 agent 执行：

- 用户原始请求
- planner 产出的步骤
- 工具调用参数和结果摘要
- 失败原因
- 是否可恢复
- 最终输出

这会让以下能力变得自然：

- `shion run list`
- `shion run inspect <id>`
- `resume` 上一次中断任务
- 复盘 agent 为什么做了某个动作

没有 run ledger，个人 agent 越自动化，越难调试。

## 5. 更强的 planner / orchestrator

当前 planner 仍然是很薄的一层，主要依赖 LLM tool calling。下一步应该让 shion 自己拥有更明确的执行策略。

建议支持：

- 澄清问题：信息不足时先问，而不是盲目执行
- 多步计划：把复杂请求拆成步骤
- 工具失败重试：可重试错误和不可重试错误分开处理
- 预算限制：限制时间、token、工具调用次数
- 危险动作审批：结合现有 `Approver` 做更细粒度策略
- 中途产物保存：长任务可以先落地草稿或 run record

不需要一开始做复杂模型化 planner，但至少要把“一次请求一次回复”升级为“一个有状态执行单元”。

## 6. 个人数据连接器

个人 agent 的价值来自它能接触真实个人上下文。可以按使用频率逐步补：

- 日历：今天/本周日程、会议冲突、空闲时间
- 邮件：未读、待回复、重要线程
- 笔记：Obsidian / Notion / 本地 markdown 检索
- 任务系统：如果用户已有外部任务工具，先同步而不是替代
- 浏览器当前页：总结、保存、转任务
- 本地文件夹：最近文件、下载目录、项目目录

建议每个连接器都先做只读，再做写入；写入必须经过权限策略。

## 7. 每日 / 每周主动摘要

`gateway` 已经能跑 maintenance，所以可以自然接入 briefing。

每日摘要建议包含：

- 今天日程
- 到期任务
- 未关闭承诺
- 最近新增重要记忆
- 需要用户确认的事项

每周摘要建议包含：

- 本周完成事项
- 卡住的项目
- 被多次推迟的任务
- 新形成的偏好或工作流规则
- 建议清理的过期记忆

这类主动摘要会让 shion 更像一个长期协作者，而不是被动问答工具。

## 8. 权限策略

当前已有 `Approver`，这是好的基础。后续应该把权限策略显式配置化：

- `shell` 默认只读，写操作需要确认
- 特定目录允许自动读写
- 特定命令前缀允许自动执行
- 网络访问可按域名授权
- gateway 模式默认拒绝危险动作
- 工具写入动作必须记录到 run ledger

权限策略应该是产品能力，不只是内部实现细节。

## 9. 可观察性与 inspect

个人 agent 会积累越来越多状态，因此需要清晰的 inspect 能力。

建议补齐：

- `shion inspect sessions`
- `shion inspect tasks`
- `shion inspect reminders`
- `shion inspect memories`
- `shion inspect skills`
- `shion inspect runs`
- `shion inspect gateway`

目标是让用户能随时看懂 shion 当前知道什么、正在做什么、为什么这样做。

## 10. 文档同步

部分设计文档仍停留在更早的 v0.1 状态，而 README 和源码已经体现了更多能力。后续需要持续同步：

- README：面向用户的当前能力
- AGENTS.md：面向 coding agent 的真实架构和命令
- `docs/ARCHITECTURE.md`：长期架构设计
- 本文档：产品能力缺口和路线图

文档不同步会直接影响后续 agent 对仓库的判断。

## 推荐实现顺序

如果只选一条最有价值的路线，建议按下面顺序：

1. 新增 `Task` / `Project` / `InboxItem` 领域模型
2. 实现 `task` tool 和基础 CLI inspect
3. 在 gateway maintenance 中加入 daily briefing
4. 新增本地 socket 或 HTTP ingress
5. 为长任务加入 `Run` / `RunStep` 记录
6. 强化 memory 分类、来源和过期机制
7. 接入第一个个人数据连接器

这样 shion 会先获得“维护个人待办和承诺”的核心能力，再逐步扩展到更多入口和外部上下文。
