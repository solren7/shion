# PRD: memory 质量升级——recall 提纯与 candidate 治理(roadmap §5 / 推荐顺序 #2)

Status: shipped (01–05 全部落地；实现顺序表即交付顺序)

## 背景与现状

roadmap §5 的三层记忆与 usage-based 治理**已经落地**,本 PRD 只做质量升级,不加新入口:

| 组件 | 位置 | 状态 |
|------|------|------|
| L1 pinned 注入 | `domain/memory.rs::is_pinnable` + `system_prompt.rs::render_pinned_memory_block` | ✅ 手动 pin、confirmed-only、scope 过滤、≤800 字符 |
| L2 tool/governance | `tools/memory.rs` + `cli/memory.rs` | ✅ save/search/list/update/promote/reject/archive/pin |
| L3 active recall | `domain/memory.rs::recall_terms/recall_score` + `infra/llm.rs::assemble` | ✅ 词面重叠(ASCII 词 + CJK bigram),top-5 原文直注,`mark_used` 记使用信号 |
| dreaming | `domain/memory.rs::dream_verdict` + `agent/daemon.rs::DreamSweep` | ✅ candidate-only:recall_count ≥ 3 → promote,30 天零召回 → archive |
| 原地迁移机制 | `infra/memory/memory_db.rs::ensure_columns` | ✅ 加列走 additive `ALTER TABLE`,不丢数据(`recall_count` 即由此加入) |
| dreaming 的 query-diversity | — | ❌ 只有裸计数(见 G1) |
| candidate 批量 triage | — | ❌ 一次一个 id,且 gateway 持锁时 CLI 拒绝(见 G2) |
| recall 精度 / aux recall agent | — | ❌ 词面假阳性直接进 prompt,`mark_used` 计的是"撞上"不是"有用"(见 G3) |

三个缺口有一条主线:**`recall_count` 是 dreaming 的唯一晋升信号,但它现在既能被重复 query 泵大(G1),也把词面假阳性计进去(G3);而人肉纠错的通道又太窄(G2)。** 三项分别从判据、操作面、信号源三头收紧。

embedding / hybrid search 维持 roadmap 的"之后再说":G3 解决 precision(选得准),embedding 解决 recall(捞得全),前者便宜且不动存储,先做。

## 缺口与设计

### G1: dreaming 只看裸计数——同一问题连问三次就能把 candidate 泵成 active

`dream_verdict` 的 promote 判据是 `recall_count >= DREAM_MIN_RECALL_COUNT(3)`。
长会话里同一条 candidate 每 turn 被同样的上下文撞上、或用户把同一个问题换标点重问,
计数就无差别上涨——这正是 OpenClaw 用 `minUniqueQueries` 防的事(roadmap §5 也点名了)。

**设计:query 指纹集合 + 晋升判据加 diversity 下限。**

- `MemoryRecord` 加一列 `recall_query_hashes TEXT`(默认空串),存**去重后的 query 指纹**,
  逗号分隔,上限 `RECALL_QUERY_HASHES_CAP = 8`(满了不再追加——8 个不同问法足以证明 diversity,
  无限追加只是白占行宽)。domain 侧 `Memory` 加 `recall_query_hashes: Vec<String>`。
- 指纹 = 用户消息经 `recall_terms` 归一(分词 + 去停用词)后**排序去重**,拼接取 SHA-256 前 16 hex。
  同一问题换语序/标点 → 同一指纹;换了实质用词才算新 query。宁可轻微高估 diversity(改写句子
  换了词面),不做语义判重——那是 embedding 阶段的事。
- `mark_used(ids, now)` → `mark_used(ids, now, query_hash: &str)`:指纹在 `assemble` 里
  每 turn 算一次,随现有的 spawn 一起落库;写入时对已有集合去重、按 cap 截断。
- `dream_verdict` 的 promote 分支加一个与条件:`unique_queries >= DREAM_MIN_UNIQUE_QUERIES(2)`。
  archive 分支不变(零召回照旧归档,与 diversity 无关)。
- **不回填**:升级前已积累 `recall_count >= 3` 但指纹为空的 candidate,升级后要再攒 2 个
  不同指纹才能晋升。宁可晚 promote,不凭不可考的历史计数放行。
- 操作面:`komo dream`(预览)每行加 `queries=N`;`cli/memory.rs::line` 在 recall_count 后
  追加同款标注,`komo memory report` 不动。

迁移:`ensure_columns` 原地加列,memory.db 是 durable 数据,**用户不删库**。

### G2: candidate 堆清不动——单 id 操作 + gateway 锁把 CLI triage 堵死

`komo memory promote/reject/pin` 一次一个 id,且都套着 `refuse_if_gateway_running`——
gateway 常驻(常态)时唯一的 triage 通道是去 chat 里让 agent 调 memory tool,逐条对话式操作。
`komo memory report` 能看见堆,却清不了。

**设计:api channel 加 memory 写路由,CLI 走转发 + 批量 + 交互式清堆。**

- `infra/messaging/api.rs` 加写路由:`POST /api/memory/{id}/promote|reject|pin`。
  照抄现有 GET 路由的鉴权模式(loopback + auto-key);gateway 持有 db 连接,天然无锁冲突。
- `cli/memory.rs::mutate` 与 `load_all` 同构:`GatewayClient::try_connect` 可达 → HTTP 转发,
  不可达 → 直开 db。**memory 三个写命令的 `refuse_if_gateway_running` 就此删除**。
- `promote` / `reject` 接受变参:`komo memory promote <id>...`,逐条报结果。
- 新增 `komo memory triage`:交互式过 candidate 堆(排序与 report 一致,最老优先——
  最老的最接近 30 天 archive 线,先救),逐条显示内容 + kind/scope/source + recall/queries 计数,
  按键 `p`(promote)/ `r`(reject)/ `s`(跳过)/ `q`(退出),结束打一行汇总。
- chat 侧 `/memory triage` **后置**(见「不做」)。

### G3: recall 假阳性直进 prompt,且污染 dreaming 信号源——aux recall agent

L3 recall 是纯词面重叠,中文 bigram 撞词率不低;top-5 原文直接进 system prompt,
假阳性既占 2000 字符预算,又被 `mark_used` 计成"被使用"——G1 收紧了判据,
但信号源本身仍是"词面撞上"而非"真的相关"。roadmap §5 点名的解法即 aux recall agent。

**设计:宽取窄注——候选取宽,超阈值才过 aux 筛选,只给留下的记使用信号。**

- `assemble` 里候选取宽:`recall(ctx, prompt, RECALL_FETCH = 15)`。
  命中 ≤ `RECALL_LIMIT(5)` → 走今天的直注路径,**零新增延迟**(多数 turn 落在这里);
  命中 > 5 → 调 aux agent 筛选。
- aux 合约(输入:用户消息 + 编号候选列表,每条带 id/kind/confidence/内容):
  输出**严格 JSON**——`{"keep": [{"id": "...", "line": "≤120 字符压缩"}]}`,最多 5 条,
  `line` 可缺省(缺省则用原文)。komo 侧校验:id 必须 ∈ 候选集(伪造即丢弃),
  解析失败 / 超时 / keep 为空 → **回落到按分数 top-5 直注**,与现有 recall 失败契约一致
  (非致命、warn 日志)。
- **防注入边界不破**:aux 输出不作为自由文本进 prompt——选择结果仍走
  `render_recalled_memory_block` 渲染(source 标签、untrusted caveat、`<!-- komo:memory:recall -->`
  标记全保留);压缩行派生自 memory 原文,与原文同级 untrusted。reviewer 防自噬规则
  (不从注入块提取记忆)不受影响,块标记未变。
- **`mark_used` 只记 aux 留下的 id**(直注路径照旧记全部注入条目)。这是与 G1 的联动:
  recall_count 从"词面撞上"升级为"经过相关性筛选",diversity 判据消费的才是干净信号。
- 延迟账:aux 路径在回复热路径上多一次 LLM 往返。三重控制:阈值门(≤5 不走)、
  `tokio::time::timeout`(建议 4s,超时即回落)、aux 用的是 `aux_model`(便宜、快)。
- 接线:`RigLlm` 加 `aux: Option<Arc<dyn LlmClient>>`,`build_llm` 签名加一个参数;
  `cli/wiring.rs` 先建 aux_llm(此参传 `None`,兼防递归),主 agent 传 `Some(aux_llm)`。
  aux/delegate/reviewer 自身不变(它们本就拿不到 memory repo)。
- roadmap 提的「解释」(每条为何相关)**v1 不做**:多占注入预算,主模型并不需要理由,
  选择 + 压缩已覆盖收益。

## 实施顺序

与 roadmap 列举顺序相反,按风险/依赖排——G1 是纯 domain 小改,先堵判据的洞;
G3 的信号提纯要等 G1 的 diversity 判据在位才被正确消费:

| # | issue | 内容 | 规模 |
|---|-------|------|------|
| 01 | query-diversity | `recall_query_hashes` 列(原地迁移)+ 指纹计算 + `mark_used` 签名 + `dream_verdict` 判据 + dream 预览/`memory list` 显示 | 小 |
| 02 | memory-api-writes | api channel 三条 POST 路由;`mutate` 走 GatewayClient;删 `refuse_if_gateway_running`;promote/reject 变参 | 中 |
| 03 | memory-triage-cli | `komo memory triage` 交互式清堆(依赖 02) | 小 |
| 04 | aux-recall-agent | 宽取窄注 + aux 合约 + 回落 + `mark_used` 提纯 + `RigLlm.aux` 接线 | 中大 |
| 05 | docs | AGENTS.md memory 节 + roadmap §5 同步为"已落地 + 指向本 PRD" | 小 |

01→02→03 可连做;04 独立但建议在 01 之后;05 随最后一项收尾。

## 不做

- **embedding / hybrid search**:roadmap 已定靠后;G3 落地后先观察 precision 提升,再评估是否还需要。
- **chat 侧 `/memory triage` 命令**:CLI(02+03)先够用,chat 里已可通过 memory tool 逐条操作;确有需求再立项。
- **per-query 完整 provenance 表**(哪条 query 何时召回了哪条 memory):dreaming 只需要 distinct 计数,hash 集合够用;完整明细是审计需求,run ledger 已部分覆盖。
- **语义级 query 判重**:指纹是词面归一,不做 embedding 判重——高估 diversity 的代价(偶尔早一轮 promote)远小于引入向量依赖。
- **aux 的「解释」字段**:见 G3 末尾。
- **动 active/user-saved 记忆的自动治理**:dreaming 只动 candidate 的原则不变,active 的退役仍归操作员(`komo memory report`)。
