# 个人 Agent 框架 v0.1 架构设计稿

本文档描述一个使用 Rust 实现的个人 Agent 框架 v0.1 设计方案。目标是先构建一个本地可运行、易于扩展的最小可用版本，优先跑通单 Agent、单会话、基础工具调用和持久化能力，再逐步演进到更复杂的规划、记忆和多 Agent 协作能力。

## 1. 设计目标

v0.1 聚焦以下五项核心能力：

1. 接收用户输入并路由到对应能力
2. 维护短期上下文与会话状态
3. 调用工具执行外部动作
4. 支持简单任务编排
5. 为记忆、插件、日志和扩展预留接口

## 2. 非目标

当前版本暂不重点覆盖以下能力：

- 多 Agent 协作
- 复杂 GUI
- 分布式部署
- 高级长期记忆检索
- 完整权限沙箱

## 3. 架构原则

v0.1 采用“先收敛后扩展”的设计思路：

- 保持核心抽象稳定，具体实现可替换
- 优先保证请求处理主链路简单清晰
- 将业务模型与基础设施解耦
- 将工具调用视为统一协议，而不是零散功能
- 用 SQLite 满足初期状态持久化和可观察性需求

## 4. 总体架构

建议采用分层结构：

1. `interface`
   负责 CLI、后续 HTTP API 或 TUI 的输入输出
2. `application`
   负责 Agent 请求生命周期编排
3. `domain`
   负责核心抽象和业务模型
4. `infrastructure`
   负责模型接入、数据库、配置、日志、外部工具适配

请求流向如下：

```text
User
  -> Interface (CLI / API)
  -> Agent Runtime
  -> Planner / Router
  -> LLM Orchestrator
  -> Tool Executor
  -> Memory Store
  -> Response Renderer
  -> User
```

## 5. 推荐目录结构

结合当前仓库结构，建议逐步演进为：

```text
src/
  main.rs
  cli/
    mod.rs
    app.rs
  agent/
    mod.rs
    runtime.rs
    context.rs
    session.rs
    planner.rs
    executor.rs
  domain/
    mod.rs
    message.rs
    task.rs
    tool.rs
    memory.rs
    event.rs
    error.rs
  services/
    mod.rs
    llm.rs
    memory.rs
    tool_registry.rs
    workflow.rs
  tools/
    mod.rs
    shell.rs
    file.rs
    time.rs
  infra/
    mod.rs
    config.rs
    sqlite.rs
    logger.rs
    openai.rs
  utils/
    mod.rs
```

其中：

- `cli/` 负责命令行入口和参数解析
- `agent/` 负责 runtime、上下文、执行流程
- `domain/` 负责纯业务模型和 trait 抽象
- `services/` 负责 orchestration 与注册机制
- `tools/` 负责工具能力实现
- `infra/` 负责具体外部系统接入

## 6. 核心模块设计

### 6.1 AgentRuntime

`AgentRuntime` 是系统入口，负责一次用户请求的完整生命周期。

```rust
pub struct AgentRuntime {
    planner: Box<dyn Planner>,
    llm: Box<dyn LlmClient>,
    tools: ToolRegistry,
    memory: Box<dyn MemoryStore>,
}
```

职责：

- 加载会话上下文
- 记录用户输入
- 决定是否需要模型推理或工具调用
- 执行工具并回填结果
- 生成最终回复并持久化

### 6.2 Session

`Session` 表示连续对话或任务处理上下文。

```rust
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    pub metadata: SessionMetadata,
}
```

它承载：

- 会话唯一标识
- 历史消息
- 当前上下文元信息

### 6.3 Message

统一消息模型，覆盖系统、人类、模型和工具消息。

```rust
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: i64,
}
```

这个抽象要尽量稳定，因为它会影响：

- prompt 构造
- 历史记录存储
- 工具结果回填
- 后续 trace 和 replay

### 6.4 Planner

v0.1 不做复杂规划器，先采用轻量路由模型。

```rust
pub trait Planner: Send + Sync {
    fn plan(&self, session: &Session) -> Plan;
}
```

职责：

- 判断直接回答还是调用工具
- 判断是否进入简单多步流程
- 后续可扩展为 rule-based 与 model-based 混合规划

### 6.5 Plan

```rust
pub enum Plan {
    RespondDirectly,
    CallTool { tool_name: String, input: String },
    MultiStep { steps: Vec<Step> },
}
```

`Plan` 是 runtime 执行意图的统一表示。v0.1 重点先跑通：

- 直接回答
- 单工具调用
- 简单顺序式多步任务

### 6.6 Tool

所有外部能力统一通过工具协议暴露。

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    async fn execute(&self, input: String) -> anyhow::Result<String>;
}
```

设计重点：

- 能力统一注册
- 调用协议统一
- 输出统一进入消息流

### 6.7 ToolRegistry

```rust
pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn Tool>>,
}
```

职责：

- 注册工具
- 查询工具
- 执行工具
- 控制可用工具集合

### 6.8 MemoryStore

v0.1 推荐将记忆分成两层：

- 短期记忆：当前 session message history
- 持久记忆：SQLite 中的会话、消息、工具调用和事件日志

```rust
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    async fn load_session(&self, session_id: &str) -> anyhow::Result<Session>;
    async fn save_message(&self, session_id: &str, msg: &Message) -> anyhow::Result<()>;
}
```

后续可逐步扩展：

- 摘要压缩
- 记忆提炼
- 检索增强

## 7. 请求处理流程

一次标准请求的处理链路建议如下：

1. Interface 接收用户输入
2. Runtime 加载或创建 Session
3. 写入用户消息
4. Planner 判断执行策略
5. 如需调用 LLM，则构造上下文并发起推理
6. 如模型要求调用工具，则交由 ToolExecutor 执行
7. 将工具结果写回 Session
8. 二次调用 LLM 生成最终答复
9. 持久化消息、工具调用和事件日志
10. 返回结果给用户

伪代码如下：

```rust
pub async fn handle_input(&self, session_id: &str, user_input: String) -> anyhow::Result<String> {
    let mut session = self.memory.load_session(session_id).await?;
    session.messages.push(Message::user(user_input));

    let plan = self.planner.plan(&session);

    let reply = match plan {
        Plan::RespondDirectly => {
            self.llm.complete(&session).await?
        }
        Plan::CallTool { tool_name, input } => {
            let tool_output = self.tools.execute(&tool_name, input).await?;
            session.messages.push(Message::tool(tool_output.clone()));
            self.llm.complete(&session).await?
        }
        Plan::MultiStep { steps } => {
            self.execute_steps(&mut session, steps).await?
        }
    };

    session.messages.push(Message::assistant(reply.clone()));
    self.memory
        .save_message(session_id, session.messages.last().unwrap())
        .await?;

    Ok(reply)
}
```

## 8. 技术选型建议

Rust 生态建议如下：

- CLI：`clap`
- Async Runtime：`tokio`
- 序列化：`serde`, `serde_json`
- 错误处理：`anyhow`, `thiserror`
- async trait：`async-trait`
- 数据库：`sqlx` + SQLite
- 日志：`tracing`, `tracing-subscriber`
- 配置：`config` 或 `figment`

## 9. 工具体系设计

v0.1 建议先支持三类工具：

### 9.1 time

返回当前时间、时区等信息。

### 9.2 file

读写工作目录中的文件，用于最基础的本地自动化。

### 9.3 shell

执行受限白名单命令。v0.1 不建议默认开放全部 shell 能力，应当通过显式配置开启。

初期建议统一使用字符串或 JSON 字符串作为输入输出格式，以降低复杂度；后续版本再逐步引入强类型 schema。

## 10. 数据存储设计

SQLite 足以支撑 v0.1。建议至少包含以下数据表：

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL
);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  timestamp INTEGER NOT NULL
);

CREATE TABLE tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  tool_name TEXT NOT NULL,
  input TEXT NOT NULL,
  output TEXT NOT NULL,
  timestamp INTEGER NOT NULL
);
```

后续可增加：

- `events`
- `memory_snapshots`
- `workflow_runs`

## 11. 配置设计

建议采用 `config.toml` + 环境变量覆盖。

示例：

```toml
[agent]
name = "shion"
model = "gpt-4.1-mini"
max_history = 20

[storage]
database_url = "sqlite://agent.db"

[tools]
enable_shell = false
enable_file = true
enable_time = true
```

建议配置项覆盖：

- Agent 名称
- 默认模型
- 历史消息窗口大小
- 数据库位置
- 工具开关
- 日志级别

## 12. 错误处理策略

建议按三层划分错误：

1. `domain error`
   业务错误，例如工具不存在、计划非法
2. `infra error`
   配置、数据库、网络、模型接入错误
3. `user-facing error`
   输出给用户的可理解错误信息

设计原则：

- 底层错误尽量保留上下文，便于调试
- 用户侧错误信息保持可读，不暴露过多内部细节
- 工具错误与模型错误分开记录，便于定位问题

## 13. 可扩展点

v0.1 需要提前为后续版本留出稳定边界：

- `LlmClient` 可替换不同模型服务商
- `Tool` 支持动态注册和按配置启停
- `Planner` 可从 rule-based 升级到 model-based
- `MemoryStore` 可从 SQLite 演进到混合检索
- `Workflow` 可从线性步骤扩展到状态机或 DAG

## 14. v0.1 里程碑建议

建议按以下顺序推进：

1. 建立基础 crate 结构
2. 实现 `Session`、`Message`、`Tool` 等核心抽象
3. 接入 CLI 输入循环
4. 实现 `time` 工具
5. 接入 SQLite 持久化
6. 跑通一次“用户输入 -> 工具调用 -> 模型回复”
7. 增加基础日志和错误处理

## 15. 最小可用版本定义

满足以下条件即可视为 v0.1：

- 可以通过 CLI 启动
- 能保存会话历史
- 至少支持 1 到 3 个工具
- 能完成单轮或简单多轮对话
- 出错时有明确日志和用户提示

## 16. 后续演进建议

完成 v0.1 后，可按优先级继续推进：

1. 会话摘要与上下文压缩
2. 工具参数 schema 化
3. 更稳定的工具调用协议
4. 任务状态机与可恢复执行
5. 长期记忆与检索增强
6. HTTP API / TUI / Web UI 接口
7. 多 Agent 协作机制

## 17. 与当前仓库的关系

当前仓库是一个小型 Rust CLI crate，已经具备基础命令行结构。本文档的设计方向与现有仓库组织兼容，适合作为后续演进蓝图：

- 保持 `src/main.rs` 作为入口
- 在 `src/cli/` 基础上继续扩展交互入口
- 增加 `agent/`、`domain/`、`services/`、`tools/`、`infra/` 等模块
- 用 SQLite 承接初期持久化需求

这意味着可以在不推翻现有代码结构的前提下，逐步把当前 CLI 项目演进成个人 Agent 框架。
