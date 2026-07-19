# PRD: 权限策略产品化——补完(roadmap §3 / 新推荐顺序 #1)

Status: ready-for-human (design review)

## 背景与现状

roadmap §3 的主体**已经落地**(squash commit `3507938`),现状比 roadmap 描述的更完整:

| 组件 | 位置 | 状态 |
|------|------|------|
| 纯规则引擎 `Policy` | `domain/policy.rs` | ✅ 四类 action(shell/file/network/homeassistant)、四种 matcher(prefix/suffix/exact/contains)、channel scope、deny 优先、dangerous 需 `include_dangerous` 显式授权、`default_normal` 兜底,带完整测试 |
| `PolicyApprover` 装饰器 | `agent/policy_approver.rs` | ✅ 包在 `CliApprover`/`ChatApprover` 外层,`Ask` 才落到人;无 session(sweep/aux)时 Allow 不生效(不无人值守放行) |
| config 解析 | `config.rs` `[policy]` + `[[policy.rule]]` | ✅ 非法规则单条忽略(eprintln 警告),缺表退化为空策略(= 全 Ask,行为不变) |
| ActionRef 埋点 | `tools/shell.rs`、`tools/file.rs`(写)、`tools/homeassistant.rs` | ✅ |
| 网络类埋点 | `tools/web_fetch.rs` | ❌ **无**(见 G1) |
| 操作面 | CLI / doctor | ❌ **无**(见 G3) |
| 文档 | README / AGENTS.md | ❌ **无**(见 G4) |

分层原则(已实现,保持不变):policy 在各工具自己的 hardline 地板**之上**——shell 的 refuse 模式、HA 的 BLOCKED_DOMAINS 在工具内部先短路,任何 Allow 规则都解不开;policy 只能比地板更严,不能更松。

## 缺口与设计

### G1: `network` 类别是死配置——web_fetch 完全绕过审批

`web_fetch`(GET-only)和 `web_search` 今天不持有 approver,不发 `ActionRef::Network`。
后果:`[[policy.rule]] category = "network"` 写了也没工具会命中;更重要的是,
**untrusted 网页内容诱导 agent 向攻击者域名发带敏感参数的 GET(exfiltration)时,策略层看不见、拦不住**。

**设计:deny-only 评估,不改默认体验。**

- `web_fetch` 注入 approver,发 `Risk::Safe` + `ActionRef::Network { url }`。
- `PolicyApprover` 对 `Risk::Safe` 且带 action 的请求,只评估 **deny 规则**:命中 → 拒绝(工具返回错误,模型可见原因),未命中 → 放行,不问人、不看 allow 规则。
- 语义:网络 GET 保持"默认可用"(聊天里每次 fetch 都 `/approve` 不可接受;sweep/aux 子代理的 web 检索也不能断),但用户可以写黑名单:

```toml
[[policy.rule]]
category = "network"
matcher = "suffix"
value = "internal.corp.com"
effect = "deny"
```

- `web_search` 不动:它只打固定的 provider API host,没有可变目标。
- allow 规则对 network 无意义(本来就放行),`komo policy check` 对此给出提示(见 G3)。

不选的方案及原因:`web_fetch` 升 `Risk::Normal` + `default_network = "allow"`(类别级默认)——会让无 session 的 aux 子代理/sweep 的 fetch 落到 inner approver 被拒,破坏现有 web 检索;且多一个配置概念,换来的能力与 deny-only 等价。

**附带收益(同一机制,顺手做)**:`file` 读路径同样发 `Risk::Safe + ActionRef::File { write: false }`,
使 `category = "file", access = "read", effect = "deny"` 能封敏感路径(如 `~/.ssh`、`~/.komo/.env`)。
目前 file 读完全不经 approver,`access = "read"` 的 deny 规则同样是死配置。

### G2: 无人值守场景没有任何放行通道

`PolicyApprover` 现在要求 Allow 必须有 session(有人、有 channel 可 scope)。这是对的默认,
但 roadmap §2 的方向(briefing 组装时调 skill 拉外部数据)需要一条**显式 opt-in** 的窄通道。

**设计:规则级 `unattended = true` 标志。**

- `Rule` 加 `unattended: bool`(默认 false)。
- `Policy::evaluate` 返回值改为携带命中规则引用(`Verdict` + `Option<&Rule>`,或新 `Decision` 结构)。
- `PolicyApprover`:`Verdict::Allow` 且无 session 时,仅当命中规则 `unattended = true` 才放行;
  `Risk::Dangerous` 永不无人值守放行(即 `unattended` 与 `include_dangerous` 不叠加生效)。
- 典型用法:

```toml
[[policy.rule]]           # 允许 briefing sweep 无人值守 fetch 日历 API
category = "network"      # (配合 G1 的 deny-only,这条只在 unattended 场景有意义)
matcher = "suffix"
value = "open.feishu.cn"
effect = "allow"
unattended = true
```

排序:此项**后置**——它的消费者(briefing-via-skill)还没出现,但设计先定,字段落地时不算 dead field(PolicyApprover 立即消费它)。

### G3: 零操作面——写了规则无法验证,doctor 不显示

**设计:`komo policy` 子命令 + doctor 段。**

- `komo policy list`:解析后的规则表(序号、channel scope、category、matcher、value、effect、标志)+ `default_normal` + 非法规则计数。纯本地 config 解析,不碰 db,不需要 gateway 路由。
- `komo policy check <category> <target> [--channel <c>] [--dangerous] [--write]`:dry-run 一条动作,输出 verdict + 命中的规则序号(未命中则显示落到的默认)。例:
  - `komo policy check shell "git push origin main"`
  - `komo policy check network "https://api.github.com/repos" --channel telegram`
  - 对 network 的 allow 规则命中时提示"network 默认放行,此 allow 仅对 unattended 有意义"。
- doctor 新增 `policy:` 段:规则数、非法规则数(>0 标 ✗)、`default_normal`。放在 sweeps 和 channels 之间。

### G4: 文档缺失

- README:`[policy]` 配置示例(常用三件套:放行项目目录写、放行 cargo/git 前缀、封内网域名)。
- AGENTS.md:架构节补 `domain/policy.rs` + `agent/policy_approver.rs` 条目(分层、deny-only、unattended 语义)。
- roadmap §3 重写为"已落地 + 剩余缺口指向本 PRD"。

## 审计(设计确认,不改代码)

- 策略判定已有结构化日志(`policy: denied` / `policy: auto-allowed`,带 summary + channel);G2 落地时在日志里附命中规则的序号/值。
- run ledger 不加字段:policy deny 的结果已经以工具错误形式进入 `RunStep`(模型和 `komo run inspect` 都可见),"no dead fields" 原则继续成立。

## 实施顺序

| # | issue | 内容 | 规模 |
|---|-------|------|------|
| 01 | deny-only-for-safe | `Policy`/`PolicyApprover` 支持 Safe+action 的 deny-only 评估;`web_fetch` 埋点;`file` 读路径埋点 | 中 |
| 02 | policy-cli | `komo policy list` / `komo policy check`;doctor `policy:` 段 | 中 |
| 03 | docs | README 示例、AGENTS.md 架构条目、roadmap §3 重写 | 小 |
| 04 | unattended-flag | `unattended` 规则标志 + `PolicyApprover` 窄通道(后置,等 briefing-via-skill 立项时一起) | 小 |

01→02→03 一次做完;04 挂起到有消费者。

## 不做

- 每 channel 的 `default_normal`(规则已可按 channel 写 deny/allow,再加维度是配置复杂度换不来表达力)。
- skill 维度的 category(skill 最终落到具体工具调用,每个调用已被现有类别覆盖)。
- 规则热加载(config 改动重启 gateway 即可,`komo gateway restart` 秒级)。
