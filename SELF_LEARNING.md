# 自学习机制设计（借鉴 Hermes `background_review`）

> 来源参考: `~/02-note/.../hermes-agent-自学习机制.md`（Nous Research Hermes Agent）
> 目标: 把 Hermes 的「每轮后自我反思 → 写回 memory + skill」回路,移植到 shion 的 DDD 分层上。
> 状态: 设计稿,未实现。

## 0. 范围裁剪

Hermes 文档里有两套独立机制,只借其一:

| 机制 | 用途 | shion 是否借鉴 |
|---|---|---|
| Trajectory（ShareGPT JSONL → 压缩 → 训练数据） | 训练下一代 tool-calling 模型 | ❌ 不借。shion 不训模型,纯属基础设施浪费 |
| `background_review.py` | 在线反思,写回当前用户 memory/skill,下个 session 生效 | ✅ 借这个 |

## 1. 核心抽象: memory vs skill

Hermes 的关键洞见(直接采纳):

> **memory** 答「用户是谁、当前状态如何」;**skill** 答「这类任务怎么为这个用户做」。

shion 现在两者都没有。这是干净的 domain 扩展,顺着现有 `SessionRepository` / `MessageRepository` 的 trait 模式(`src/domain/repository.rs`)。

### 1.1 归属规则(决定一条洞见写到哪)

| 信号 | 写到 |
|---|---|
| 用户披露的事实 / persona / 偏好身份 | **memory** |
| 用户纠正 style / tone / format / verbosity（"too verbose" / "just give me the answer"） | **skill**（不是 memory!） |
| 用户纠正 workflow / 步骤顺序 | **skill**（pitfall 或 explicit step） |
| 非平凡的技巧 / 修复 / workaround / 调试路径 | **skill** |
| 本次载入的某个 skill 被发现错了 / 漏了 / 过期 | **skill**（当场 patch） |

> 关键: style/format/workflow 抱怨**必须写进 skill**,不能只丢进 memory。这是 Hermes 反复强调的归属错误来源。

## 2. Domain 层新增(纯接口,无 I/O,无外部 crate)

遵循 CLAUDE.md: `domain/` 不出现 toasty 等任何 I/O。

```
src/domain/memory.rs
  pub struct Memory { id, kind: MemoryKind, content, created_at }
  pub enum MemoryKind { User, Feedback, Project, Reference }   // 对齐 Claude Code 4 类
  #[async_trait] pub trait MemoryRepository {
      async fn list(&self) -> Result<Vec<Memory>>;
      async fn save(&self, m: &Memory) -> Result<()>;
  }

src/domain/skill.rs
  pub struct Skill { name, body, protected: bool }   // protected = bundled,可改内容不可删
  #[async_trait] pub trait SkillRepository {
      async fn find(&self, name: &str) -> Result<Option<Skill>>;
      async fn list(&self) -> Result<Vec<Skill>>;
      async fn save(&self, s: &Skill) -> Result<()>;   // upsert = patch 或新建
  }

src/domain/reviewer.rs
  pub struct ReviewOutcome { memories_written: Vec<String>, skills_written: Vec<String> }
  #[async_trait] pub trait Reviewer: Send + Sync {
      async fn review(&self, session: &Session) -> Result<ReviewOutcome>;
  }
```

## 3. Infra 层(唯一碰 toasty 的地方)

照 `src/infra/db.rs` 现有套路:新增两个私有 model 结构 `MemoryRecord` / `SkillRecord`,`Db` 再 impl `MemoryRepository` + `SkillRepository`。

⚠️ 注意 `Db::connect` 的既有约束:`push_schema()` 非幂等,只对新库调用。加表后旧的 `shion.db` 需删除重建(本来就是 disposable 开发态)。

## 4. Agent 层: `ReflectiveReviewer`

```
src/agent/reviewer.rs
  pub struct ReflectiveReviewer {
      llm: Arc<dyn LlmClient>,            // 见 §6 前置依赖
      memories: Arc<dyn MemoryRepository>,
      skills: Arc<dyn SkillRepository>,
  }
  impl Reviewer for ReflectiveReviewer { ... }
```

**工具白名单天然实现**: reviewer 的依赖里**根本没有 `ToolRegistry`**——它结构上只能写 memory/skill,无需像 Hermes 那样在 dispatch 层硬拦(`set_thread_tool_whitelist()`)。这是 Rust 依赖注入比 Python fork 更干净的地方。

`review()` 内部流程:
1. 把 `session.messages` 快照拼成反思 prompt(见 §5)
2. 调 `llm.complete()` 得到结构化建议(JSON: 写哪条 memory / patch 哪个 skill)
3. 按 §1.1 归属规则 + §5 反模式清单过滤
4. 命中的 `memories.save` / `skills.save`
5. 返回 `ReviewOutcome`,供 CLI 打一行 `💾 Self-improvement: ...`

## 5. 反思 prompt(最值钱的部分,直接抄逻辑)

Hermes `_SKILL_REVIEW_PROMPT` 的判别逻辑,语言无关,作为 `src/agent/reviewer.rs` 的常量字符串移植。

**4 类触发信号** = §1.1 表格后四行。

**写入优先级(earliest fits wins)**:
1. PATCH 本次会话载入过的 skill（"它在场了,它就该被改"）
2. PATCH 已有 umbrella skill
3. 给已有 umbrella 加 support 文件(references / templates / scripts + SKILL.md 加一行 pointer)
4. 新建 class-level umbrella skill —— 名字**必须类级别**,**绝不能**是 PR 号 / error 字符串 / 库名本身 / "fix-X" 这类一次性命名

**绝不写清单(团队踩坑总结,原样保留)**:
- 环境依赖型失败: `command not found` / 凭证没配 / 包没装 —— 用户能自己修,不是 durable 规则
- 对工具的负面断言: "X 工具坏了" —— **会硬化成 agent 几个月后还在引用的拒绝理由,哪怕问题早修了**
- session-specific 瞬时错误(重试就好)
- 一次性任务叙事("今天总结一下市场"不是一类工作)

> 这份清单是 §4.3 里「判别逻辑比触发机制重要 10 倍」的具体兑现,是整套机制里最该照抄的。

## 6. 前置依赖: `LlmClient`(当前缺失,阻塞反思)

shion v0.1 现状(必须先解决,否则反思无法真跑):
- `KeywordPlanner` 是关键词规则,没接模型
- `runtime.rs:46` 的回复是 `format!("(echo) {}")` 桩
- CLAUDE.md 已把 `LlmClient` trait 列为 TODO

→ 反思的本体是「用模型判断对话」。没有 `LlmClient`,§4/§5 只能搭空壳。**实现顺序见 §8。**

## 7. Runtime 触发(对应 Hermes §3.1/§3.2)

在 `src/agent/runtime.rs:handle_input` 末尾、持久化 reply 之后触发,**fire-and-forget 不阻塞主流程**:

```rust
// runtime.rs handle_input 尾部
self.messages.save(session_id, &Message::assistant(&reply)).await?;

if let Some(reviewer) = &self.reviewer {
    if session.user_turns() % self.review_interval == 0 {   // 默认 10,对齐 Hermes
        let reviewer = reviewer.clone();
        let snapshot = session.clone();
        tokio::spawn(async move {
            match reviewer.review(&snapshot).await {
                Ok(o) if !o.is_empty() => info!(?o, "self-improvement review"),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "review failed (non-fatal)"),
            }
        });
    }
}
Ok(reply)
```

需要 `AgentRuntime` 新增字段 `reviewer: Option<Arc<dyn Reviewer>>` + `review_interval: u64`,并在 `Session` 上加 `user_turns()` 辅助方法。

**触发节奏**: 每 N=10 个 user turn,不要每轮跑(Hermes 默认值,够用)。

## 8. 工程细节移植对照(Hermes §3.4)

| Hermes 细节 | shion 怎么对应 | 现在能做? |
|---|---|---|
| 主线程永不阻塞(daemon thread) | `tokio::spawn` fire-and-forget | ✅ 比 Hermes 更干净 |
| 工具白名单只允许 memory/skill | reviewer 依赖里无 `ToolRegistry` | ✅ 结构性保证 |
| 复用父 system prompt 命中 prefix cache(省 26%) | 反思与主对话共用同一 system prompt 前缀 | ⏳ 接 LLM 后 |
| `skip_memory` 防 fork 污染外部 memory provider | shion memory 是本地 toasty 表,无外部 provider | ➖ 不存在此问题 |
| 危险命令自动 deny | shion 无 shell 工具 | ➖ 不相关 |
| protected skill 可改不可删 | `Skill.protected` 字段,curator 删除时校验 | ✅ |

## 9. 实现顺序

1. **第 1 步(不依赖 LLM,可独立交付)**: §2 + §3 —— 落 `domain/memory.rs`、`domain/skill.rs`、`domain/reviewer.rs` 接口 + `infra/db.rs` 两张表 + §5 反模式清单常量。可编译、可写单测(repo 增删查)。
2. **第 2 步(前置)**: §6 —— 实现 `LlmClient` trait,替换 `runtime.rs` echo 桩,升级 `KeywordPlanner`。
3. **第 3 步**: §4 + §5 + §7 —— `ReflectiveReviewer` + runtime 每 N 轮触发 + CLI 提示行。

## 10. 用户感知(对应 Hermes §3.5)

反思跑完,扫 `ReviewOutcome`,去重后在 CLI 打一行,不污染主对话:

```
💾 Self-improvement: Memory updated · Skill foo-bar patched · references/baz.md added
```

## 11. takeaways(按重要性,源自 Hermes §4.3)

1. **判别逻辑 > 触发机制(10×)**。§5 的「绝不写」清单是整套机制的灵魂,先抄它。
2. **memory vs skill 是核心抽象**。shion 两条线都要建,别只建 memory。
3. **白名单靠依赖注入实现**,不靠运行时拦截——这是 shion 相对 Hermes 的结构优势。
4. **别每轮跑**,每 N=10 轮够了。
5. **反思必须非阻塞、失败不致命**(`tokio::spawn` + `warn!` 吞错)。
