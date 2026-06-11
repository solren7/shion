# 个人 Agent 能力缺口与路线图

本文档基于当前 shion 的实现状态，整理一个个人 agent 工具从"能聊天和调用工具"走向"能长期辅助个人事务"的能力缺口。

## 现状

被动应答这条链路已经成型：

- `AgentRuntime` 负责会话生命周期、消息持久化、planner 决策和回复写回
- LLM 通过 `rig` 接入，完整多轮对话历史已经发给模型（`infra/llm.rs`），并暴露内置工具给模型调用
- 已注册的工具包括 `time`、`file`、`shell`、`web_fetch`、`web_search`、`session`、`reminder`、`memory`、`delegate`、`skill`
- `reminder` 支持一次性和 cron 周期提醒，由 gateway 每分钟投递（macOS 通知）
- `gateway` 已经具备常驻进程（launchd 安装）、维护任务调度和非交互审批策略
- 反思 reviewer、markdown memory、skill registry 已经具备初步自学习闭环
- 已有基础 inspect：`shion cron list`、`shion session list/clean`

总体判断：**缺的不是再补一个零散工具，而是"主动协作"的闭环**——shion 能在用户发起对话时干活，但还不能在用户不开终端的时候帮上忙。主动性闭环 = 入口（消息能递进来）+ 任务模型（知道该做什么）+ 后台权限（能安全地做事）。

## 1. 真实的 ingress 入口（第一缺口）

`Channel` trait 已定义在 `agent/gateway.rs`，但目前没有任何实现。这意味着 gateway 虽然 24 小时跑着，却没有任何方式把消息"递"给它——shion 本质上还是一个 REPL。没有入口，后面所有"主动"功能都没有用武之地。

优先级建议：

1. 本地 unix socket channel：最简单的第一个实现，方便脚本、快捷指令、Raycast、Automator 调用
2. Lark channel：用户日常在飞书生态里，这个入口的实际价值远高于 Telegram/Slack；可以和第 5 节的飞书连接器共享一套鉴权基础设施
3. 剪贴板 / share sheet 风格入口，支持快速丢一段文本给 shion

入口不需要一开始做复杂，核心是把"打开终端聊天"变成"随时把一个事件交给 shion"。

## 2. 任务与承诺模型

当前唯一的事务性模型是 reminder，它只解决"到点通知"，回答不了"我现在该做什么""哪些事情卡住了""我答应过谁什么"。`src/tasks/` 目录已创建但还是空的。

建议新增一组一等领域模型：

- `Task`：标题、状态、优先级、截止时间、来源、所属项目、备注
- `InboxItem`：尚未归类的输入、想法、待处理事项
- `Commitment`：从对话、会议、邮件中提取出的承诺和待跟进事项
- `Project`：目标、状态、活跃任务、最近进展（可以最后加，先不引入）

对应工具先收敛为一个 `task` tool：

- `capture`：快速收集任务或 inbox item
- `list`：查看当前任务
- `update`：修改状态、截止时间、项目归属
- `complete`：完成任务
- `plan_today`：生成今日建议执行列表

这是最能把 shion 从聊天工具推进到个人 agent 的能力。

## 3. 后台权限策略（不只是 DenyApprover）

gateway 目前挂的是 `DenyApprover`：任何有副作用的工具一律拒绝。这在 v0.1 是对的，但它和"主动 agent"直接冲突——daily briefing 要读日历、整理 inbox 要写文件，全都会被挡掉。不解决这个，gateway 永远只能做扫描和提醒。

需要把权限从"交互式确认 vs 全拒"升级为显式可配置策略：

- `shell` 默认只读，写操作需要确认
- 特定目录允许自动读写
- 特定命令前缀允许自动执行
- 网络访问可按域名授权
- gateway 模式默认拒绝危险动作，但允许配置放行的安全动作
- 工具写入动作必须记录到 run ledger（见第 7 节）

权限策略应该是产品能力，不只是内部实现细节。`src/policy/` 目录已创建，可以作为落点。

## 4. 每日 / 每周主动摘要

maintenance 框架（`Schedule` + `Maintenance` trait）是现成的，加一个 briefing sweep 成本很低。但它的价值取决于第 2 节的任务模型和第 5 节的日历数据——没有这些，briefing 只能说"你没有到期提醒"。所以顺序上放在任务模型之后。

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

## 5. 个人数据连接器（飞书优先）

个人 agent 的价值来自它能接触真实个人上下文。对当前用户而言，第一个连接器几乎肯定是飞书：日历、待办、邮件都在那里，且可以和 Lark ingress channel 共享鉴权。

按使用频率逐步补：

- 日历：今天/本周日程、会议冲突、空闲时间
- 邮件：未读、待回复、重要线程
- 任务系统：如果用户已有外部任务工具，先同步而不是替代
- 笔记：Obsidian / Notion / 本地 markdown 检索
- 浏览器当前页：总结、保存、转任务
- 本地文件夹：最近文件、下载目录、项目目录

每个连接器都先做只读，再做写入；写入必须经过第 3 节的权限策略。

## 6. 长期记忆检索与治理

当前已有 markdown memory 和 reflective reviewer，但缺分类、来源追溯和过期机制。reviewer 自动写记忆的情况下，没有治理机制会慢慢污染记忆库。重要，但记忆量还小的时候问题不显，紧迫性低于前几节。

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

这样可以减少 agent 自己污染记忆，也能在回答时追溯"为什么这么认为"。

## 7. 工作流执行记录（run ledger）

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

没有 run ledger，个人 agent 越自动化，越难调试。不过当前 planner 还薄、自动化程度低，调试痛感尚未出现——等 gateway 真的开始自主做事（第 1、3、4 节落地后）再补不迟。第 3 节的权限审计也依赖它。

## 8. 更强的 planner / orchestrator

当前 planner 仍然是很薄的一层，主要依赖 LLM tool calling。下一步应该让 shion 自己拥有更明确的执行策略。

建议支持：

- 澄清问题：信息不足时先问，而不是盲目执行
- 多步计划：把复杂请求拆成步骤
- 工具失败重试：可重试错误和不可重试错误分开处理
- 预算限制：限制时间、token、工具调用次数
- 危险动作审批：结合现有 `Approver` 做更细粒度策略
- 中途产物保存：长任务可以先落地草稿或 run record

不需要一开始做复杂模型化 planner，但至少要把"一次请求一次回复"升级为"一个有状态执行单元"。

## 9. 可观察性与 inspect

`shion cron list`、`shion session list/clean` 已经存在。随着状态增多，继续补齐：

- `shion inspect tasks`
- `shion inspect memories`
- `shion inspect skills`
- `shion inspect runs`
- `shion inspect gateway`

目标是让用户能随时看懂 shion 当前知道什么、正在做什么、为什么这样做。

## 10. 文档与代码同步

文档落后于源码会直接影响后续 agent 对仓库的判断（曾出现过的例子：AGENTS.md 声称"只发最后一条用户消息"而实际已发送完整历史、一批空占位目录误导代码导航——均已修复）。需要持续同步：

- README：面向用户的当前能力
- AGENTS.md：面向 coding agent 的真实架构和命令
- `docs/ARCHITECTURE.md`：长期架构设计
- 本文档：产品能力缺口和路线图

## 推荐实现顺序

如果只选一条最有价值的路线，建议按下面顺序：

1. unix socket ingress channel（第 1 节）——让 gateway 真正可达
2. `Task` / `InboxItem` 领域模型 + `task` tool + 基础 inspect（第 2 节）
3. 后台权限策略，替换 gateway 的一刀切 DenyApprover（第 3 节）
4. gateway maintenance 中加入 daily briefing（第 4 节）
5. Lark channel + 飞书日历只读连接器，共享鉴权（第 1、5 节）
6. 强化 memory 分类、来源和过期机制（第 6 节）
7. 为长任务加入 `Run` / `RunStep` 记录（第 7 节）

前三步合起来才构成从"聊天工具"到"个人 agent"的那次跨越；其余都可以在这个闭环跑起来之后逐步加。
