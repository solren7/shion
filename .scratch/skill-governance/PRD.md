# PRD: skill governance——inspect、启停/保护、triage、调用审计(roadmap §9 / 推荐顺序 #4)

Status: ready-for-human (design review)

## 背景与现状

连接器走 skill 后(roadmap §1),skill 的审查、保护、来源追溯就是连接器的质量线——治理从"锦上添花"升级为核心设施。现状盘点暴露了一个**结构性分裂**,它比任何单个治理功能都优先:

| 组件 | 位置 | 状态 |
|------|------|------|
| 运行时加载 | `services/skill_registry.rs`:启动时扫描 `SHION_SKILLS_PATH` / `<workspace>/skills` / `.claude/skills` / `~/.claude/skills` 的 `SKILL.md`,内存持有,不再刷新 | ✅ |
| 模型侧 | catalog 注入 system prompt(cap 30)+ `skill` tool `list`/`view`(渐进披露) | ✅ |
| reviewer 提取 | `agent/reviewer.rs`:写入 **shion.db** 的 `SkillRecord`(`SkillRepository::save`,同名 upsert) | ⚠️ **运行时永远读不到**——registry 只读文件系统 |
| CLI `shion skill list` | `cli/inspect.rs`,经 gateway `GET /api/skills` | ⚠️ 只读 db——**看不到任何文件 skill** |
| `protected` 标志 | `Skill`/`SkillRecord` 字段,list 显示 🔒,reviewer 更新时**保留标志值** | ❌ 从不 enforce——reviewer 照样覆写 protected skill 的 instructions(bug) |
| 状态/来源/时间戳/审计 | — | ❌ 全部缺失 |

即:**存在两套互不可见的 skill 存储**。reviewer 学到的 skill 写进 db 后既不被 agent 使用、也会随 disposable 的 `shion.db` 一起被删掉;用户手写的文件 skill 又不在 `shion skill list` 里。治理设计必须先收敛存储,再谈 triage/保护/审计。

对照物:memory 治理梯子(candidate→active 的 promote/reject、自动提取只进 candidate、操作面归 operator CLI)是本设计的直接模板;调用审计则复用 run ledger(`skill` tool 的调用已作为 RunStep 落账)。

## 缺口与设计

### G1: 双存储分裂——文件系统为唯一事实源

skill 是 durable 个人数据(和 memory/kanban 同级),不应活在 disposable `shion.db`;连接器 skill 要可编辑、可分享、可进版本管理,文件形态是生态前提(与 Claude Code / hermes 的 SKILL.md 惯例一致)。

**设计:**

- `~/.shion/skills/` 成为 shion 自有的 skill 主目录,加入 `cli/wiring.rs` 的 `skill_dirs`(排在 workspace 目录之后、`~/.claude/skills` 之前)。
- `SkillRepository` trait 保留为治理写路径的 seam,实现从 `Db` 换成文件后端 `FsSkillStore`(`infra/skills.rs`):`find`/`list`/`save` 操作 `~/.shion/skills/**/SKILL.md`。reviewer 持有的 `Arc<dyn SkillRepository>` 不需要改动调用方式。
- **一次性迁移**:启动时把 `shion.db` 里的存量 `SkillRecord` 导出为 candidate 文件(G2 的目录),写 `.exported` 哨兵防重复——与 memory 的 `import_legacy_markdown` 同一模式,方向相反。之后 `SkillRecord` 退役。
- `shion skill list` 及全部治理 CLI 改为直接文件操作。**附带收益:彻底绕开 Turso 跨进程锁**——gateway 在跑时所有 skill 治理照常可用,不需要 gateway 路由,也不进 `refuse_if_gateway_running` 名单。gateway 的 `GET /api/skills` 改由文件 store 供数(dashboard 用)。

不选的方案:db 为主、registry 改读 db——要同时解决锁路由和 durable 迁移(新 db 文件),换来的还是不可编辑/不可分享的封闭形态,两头成本买不来生态价值。

### G2: triage 流程——reviewer 提取只进 candidate(镜像 memory)

现状 reviewer 提取的 skill 直接 upsert 生效(若存储可见的话),违反治理原则"自动产出必须可追溯、可拒绝"。

**设计:**

- candidate 落 `~/.shion/skills/.candidates/<name>/SKILL.md`(点前缀目录,registry 的 `scan_dir` 天然不会误载;显式排除亦可)。
- frontmatter 扩展三个字段,`Skill::parse` 同步支持:

```yaml
---
name: feishu-calendar
description: 用 web_fetch 调飞书日历 API 查/建日程
source: reviewer          # reviewer | user(缺省 user,手写文件不用声明)
created_at: 2026-07-02T03:00:00Z
updated_at: 2026-07-02T03:00:00Z
---
```

- reviewer 写路径规则:目标名已存在 active 且 `protected` → **跳过**(连提案都不生成,见 G3);非 protected → 写/覆盖 candidate(作为"更新提案",不碰 active 文件);同名 candidate 已存在 → 覆盖前把旧版滚动到 `.candidates/<name>/.history/<ts>.md`(轻量修改历史,G5 消费)。
- CLI:
  - `shion skill list [--candidates]`——active 与 candidate 分列;
  - `shion skill promote <name>`——candidate 目录 → 主目录(同名 active 存在则覆盖,即接受更新提案);
  - `shion skill reject <name>`——删除 candidate(或挪 `.rejected/`,倾向直接删——skill 不像 memory 有召回信号需要留尸体)。
- 生效时机:registry 启动加载,promote 后需 `shion gateway restart`(沿用 permission-policy PRD 的先例:热加载不做,重启秒级)。

### G3: `protected` 语义明确化 + enforce(顺带修 bug)

现状 protected 只是被展示和被保留,从未被检查——reviewer 可以覆写 protected skill 的 instructions。

**定义:**

- `protected: true`(frontmatter)= **只有 operator 能改**(直接编辑文件或 CLI);
- reviewer 对 protected skill 不生成任何写入,**包括 candidate 提案**(避免诱导 operator 一键 promote 覆盖——保护要挡在提案生成处,不是挡在接受处);
- agent 无 skill 写路径(`skill` tool 保持只有 `list`/`view`,不加 save action)——"agent 能否自改"的答案是否,自动写入的唯一通道是 reviewer→candidate。
- CLI:`shion skill protect <name>` / `unprotect <name>`(改 frontmatter)。

### G4: 启停——disable 不删除

**设计:**

- frontmatter `disabled: true`:registry **加载但标记**——catalog 不列出、`view` 返回"该 skill 已被 operator 停用"(比静默不加载好:模型引用时得到明确答案而不是"not found",operator inspect 也仍能看到全文)。
- CLI:`shion skill enable <name>` / `disable <name>`。
- 典型场景:发现某 reviewer 提取的 skill 行为可疑,先 disable 观察,不销毁证据。

### G5: inspect——全文、来源、修改历史

- `shion skill inspect <name>`:全文 + frontmatter 元数据(source/created/updated/protected/disabled)+ 文件路径与来源目录(workspace 还是 `~/.shion`)+ `.history/` 版本列表 + 最近调用(G6)。
- 修改历史只覆盖 reviewer 写路径(G2 的 `.history/` 滚动);用户手编文件的历史交给用户自己的版本管理(`~/.shion/skills` 可自行 git init),core 不做通用 versioning。

### G6: 调用审计——从 run ledger 派生,不加新字段

事实:`skill` tool 的每次调用已经是一条 `RunStep`(`tool_name = "skill"`,args 含 `{"action":"view","name":"X"}`)——"某次 turn 用了哪个 skill"在 `shion run inspect` 里**已经可见**。缺的只是反向索引。

**设计:**

- `RunRepository` 新增查询 `skill_invocations(name, limit) -> Vec<(run_id, session_id, started_at)>`:SQL 过滤 `tool_name = 'skill'` + args LIKE 匹配 name(过滤下推,不全表拉回内存)。
- `shion skill audit <name>`:渲染最近调用的 run 列表,可接 `shion run inspect <id>` 下钻。这条**读 shion.db**,需要 gateway 路由:`GET /api/skills/{name}/audit`,gateway 不在时直连 db(与 `run list` 同款双路径)。
- **不加** `usage_count`/`last_used_at` 字段:审计可随时从 ledger 派生,加字段就是 dead field;若未来做 skill 版的 dreaming(按使用信号自动归档 candidate),再由那个消费者驱动字段落地。

## 实施顺序

| # | issue | 内容 | 规模 |
|---|-------|------|------|
| 01 | fs-source-of-truth | `~/.shion/skills` 主目录 + `FsSkillStore` + `skill list` 改读文件 + db 存量导出 + `SkillRecord` 退役 | 中 |
| 02 | candidate-triage | frontmatter 扩展(source/时间戳)+ reviewer 落 candidate + `.history/` 滚动 + promote/reject CLI | 中 |
| 03 | protect-disable | protected enforce(reviewer 跳过)+ disabled 语义(registry 标记加载)+ protect/unprotect/enable/disable CLI | 小 |
| 04 | inspect-audit | `skill inspect` + `skill_invocations` ledger 查询 + `skill audit` + `GET /api/skills/{name}/audit` | 中 |
| 05 | docs | README(skill 目录与治理命令)、AGENTS.md(架构条目重写:文件为唯一事实源)、roadmap §9 收敛 | 小 |

01 是其余一切的地基,必须先行;02→03→04 顺序做;05 收尾。

## 不做

- **skill 热加载**:promote/enable 后重启 gateway 生效,先例同 permission-policy PRD("规则热加载不做");等治理链路稳定后若确有痛点,再做 registry 的 `RwLock` + reload 端点。
- **usage 字段 / skill dreaming**:审计从 ledger 派生已覆盖可观察性;自动生命周期管理等有真实堆积(candidate 泛滥)再立项。
- **agent 自写 skill 的 tool action**:治理原则是自动产出只进 candidate、写路径唯 reviewer;给 agent 直接写入能力与本 PRD 的目的相反。
- **skill 签名 / 内容沙箱**:skill 是指导文本,实际执行仍逐调用过工具层审批 + policy 地板(§3 排第一的原因就在这);对文本本身做签名验证是包管理器课题,超出个人单机场景。
- **per-channel skill scope**:目前没有"某 skill 只对某 channel 可见"的真实需求;policy 层已能按 channel 限制 skill 驱动的具体动作。
