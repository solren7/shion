# 执行文档：配置体系迁移 — `~/.shion` 分层配置 + config 模块上移顶层

> 状态：待执行
> 参考：hermes-agent 的 `~/.hermes/config.yaml + .env` 分层设计（见对话结论：不加密，靠权限 + 职责分离 + secret 不落盘）

## 1. 目标

1. **config 模块移到顶层**：`src/infra/config.rs` → `src/config.rs`。配置是纯解析/解析规则，不依赖 toasty/rig 等外部 I/O 框架，按本项目 DDD 分层约定不属于 `infra/`。
2. **引入 `~/.shion/` 配置目录**（借鉴 hermes 的 `~/.hermes/`）：
   ```
   ~/.shion/              # 0700
   ├── config.toml        # 非敏感设置：provider / model / base_url / aux_model
   └── .env               # 仅 secrets（API key），0600
   ```
3. **加载优先级**：内置默认 < `config.toml` < `SHION_*` 环境变量。API key 永远只从环境变量 / `.env` 读取，绝不写入 `config.toml`。

## 2. 依赖变更（Cargo.toml）

```toml
toml = "0.9"     # config.toml 解析，serde 已有
dirs = "6"       # home 目录解析（~零传递依赖）
```

不引入 config-rs / figment（配置面只有 ~6 个字段，分层合并用 `#[serde(default)]` 即可，见前期决策）。

## 3. 执行步骤

### Step 1 — 模块上移

- `git mv src/infra/config.rs src/config.rs`
- `src/main.rs`：增加 `mod config;`
- `src/infra/mod.rs`：删除 `pub mod config;`
- 更新三处引用：
  - `src/infra/llm.rs:21` → `crate::config::{ModelConfig, Provider}`
  - `src/cli/wiring.rs:19` → `crate::config::ModelConfig`（与 `infra::{db::Db, llm::build_llm}` 拆开）
  - `src/cli/daemon.rs:12` → 同上
- 验证：`cargo check` 通过，此步不改任何行为。**单独提交**：`move config to top-level module`。

### Step 2 — `shion_home()` 与目录初始化

在 `src/config.rs` 新增：

```rust
/// ~/.shion，可用 SHION_HOME 覆盖（对应 hermes 的 HERMES_HOME）。
pub fn shion_home() -> PathBuf {
    std::env::var("SHION_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().expect("no home dir").join(".shion"))
}

/// 确保 ~/.shion 存在且权限为 0700；.env 存在时收紧为 0600。
/// Windows / 设权限失败时静默跳过（hermes 同款 no-op 策略）。
pub fn ensure_shion_home() -> PathBuf { /* create_dir_all + set_permissions */ }
```

权限用 `std::os::unix::fs::PermissionsExt`，包在 `#[cfg(unix)]` 里。

### Step 3 — `FileConfig`：config.toml 解析

```rust
#[derive(Debug, Deserialize, Default)]
#[serde(default)]                 // 缺失字段 = 内置默认（hermes DEFAULT_CONFIG 合并语义）
pub struct FileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub aux_model: Option<String>,
}

impl FileConfig {
    /// 读 ~/.shion/config.toml。文件不存在 → Default。
    /// 解析失败 → eprintln 警告 + Default，**不得改写/覆盖用户文件**
    /// （hermes _backup_corrupt_config 的教训：损坏文件是用户唯一的副本）。
    pub fn load(home: &Path) -> FileConfig { ... }
}
```

### Step 4 — `ModelConfig::resolve()`：三层合并

把现有 `from_env()` 改造为：

```rust
pub fn resolve() -> anyhow::Result<Self> {
    let file = FileConfig::load(&shion_home());
    // 每个字段：env 优先，其次 config.toml，最后内置默认
    let provider = env_or(file.provider, "SHION_PROVIDER")  // → Provider::parse
        .unwrap_or(Provider::DeepSeek);
    ...
    // api_key 只从 env 读，逻辑不变
}
```

- 保留 `from_env()` 作为薄别名或直接更名 `resolve()`，调用点只有 `cli/wiring.rs` 与 `cli/daemon.rs` 两处。
- `Provider::parse` 的报错文案补充来源（"from config.toml" / "from SHION_PROVIDER"），方便排查。

### Step 5 — `.env` 加载点迁移（main.rs）

```rust
// 先 cwd（开发覆盖），后 ~/.shion/.env（dotenvy 不覆盖已存在变量 → 先加载者优先）
let _ = dotenvy::dotenv();
let _ = dotenvy::from_path(config::ensure_shion_home().join(".env"));
```

仓库根的 `.env` 继续可用于开发，不破坏现有工作流。

### Step 6 — api_key 防泄漏（Debug 遮蔽）

`ModelConfig` 当前 `derive(Debug)` 会把 `api_key` 打进日志。手动 impl Debug，key 显示为 `sk-…abcd`（前 3 后 4，对应 hermes redact 层的最小实现）。暂不引入 `secrecy` crate。

### Step 7 — 测试

`src/config.rs` 内 `#[cfg(test)] mod tests`，按行为命名：

| 测试 | 行为 |
|---|---|
| `file_config_missing_file_yields_defaults` | 无 config.toml → 全 None |
| `file_config_broken_toml_yields_defaults_and_keeps_file` | 损坏文件回退默认且文件内容不变 |
| `env_overrides_file_config` | `SHION_MODEL` 覆盖 config.toml 的 model |
| `shion_home_respects_env_override` | `SHION_HOME` 生效 |
| `debug_output_masks_api_key` | Debug 输出不含完整 key |

环境变量类测试用 `SHION_HOME=临时目录` 隔离，避免污染真实 `~/.shion`。

## 4. 验收标准

- [ ] `cargo check` / `cargo test` / `cargo fmt` 全部通过
- [ ] `src/infra/` 下不再有 config.rs；`grep -rn "infra::config" src/` 无结果
- [ ] 首次 `cargo run -- chat` 自动创建 `~/.shion/`（0700）
- [ ] `~/.shion/config.toml` 写入 `model = "deepseek-reasoner"` 后生效；再设 `SHION_MODEL` 时 env 胜出
- [ ] 故意写坏 config.toml：启动不崩、有警告、文件原样保留
- [ ] 日志/`{:?}` 输出中无完整 API key

## 5. 提交切分

1. `move config to top-level module`（Step 1，纯移动）
2. `add ~/.shion home with config.toml layering`（Step 2–5 + 测试）
3. `mask api_key in ModelConfig debug output`（Step 6）

## 6. 风险与回滚

- **行为变化**：原来只认环境变量的用户不受影响（env 优先级最高，三层合并向后兼容）。
- **Windows**：权限设置全部 `#[cfg(unix)]`，其余路径逻辑由 `dirs` 保证。
- 回滚：三个提交相互独立，`git revert` 任意一层即可。

## 7. 明确不做（本次范围外）

- config.toml 加密（hermes 结论：本地单用户场景加密密钥无处可放，靠 0600/0700 + secret 不进 config）
- 外部密钥管理（Bitwarden/Vault 集成）
- `secrecy` crate / 完整 redact 层
- daemon schedule 等更多字段进 config.toml（待配置面变大后再加）
