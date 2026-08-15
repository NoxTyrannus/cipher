# Cipher v0.3.0 版本报告

> 版本：v0.3.0 ｜ 分支：main ｜ 上一版本：v0.2.6

## 一、版本目标

在 v0.2.6 基础上完成：
1. 记忆读写/检索/淘汰能力化；
2. 注意力 agent 服务层能力调用式交错思维链；
3. 经验/偏好原始记忆查证实验并落地选型；
4. 认知图 `memory.cognitive.update` 能力；
5. 移除 WASM 层；
6. 出厂预装（五态认知 + 原子/分子/方法能力示例 + skill/plugin 转化方法）；
7. v0.3.1 提示词职责重构前移合并；
8. 代码优化（类型下沉 + 缓存 + 模块拆分）。

## 二、主要变更

### 1. WASM 层移除
- 删除 `wasmtime` / `wat` 依赖、`data/wasm/*.wat`、`src/logic/script/*`。
- 新增 `src/logic/builtin/`：原 host 安全逻辑改写为纯 Rust builtin ops，保留路径白名单、读写预算、危险命令/代码黑名单、30s 超时与结构化错误。
- 9 能力 executor 全部切换为 `builtin:*`；`capability.import` 会自动迁移旧 `wasm:*` 出厂行。

### 2. 记忆能力化
新增 9 个 `builtin:memory.*` 能力：
`memory.list`、`memory.retrieve`、`memory.delete`、`memory.attention.write`、
`memory.attention.retire`、`memory.experience.write`、`memory.preference.write`、
`memory.cognitive.update`、`memory.evidence.lookup`。

记忆 agent 全部入 `agent` 表：
- `attention-agent`
- `experience-agent`
- `preference-agent`
- `cognitive-agent`

### 3. 注意力 agent
- 沿用已验证的“服务层能力调用 + 文本 JSON 协议”，模型按 `tool_call` / `done` 交错调用能力。
- 真实 API 验证：模型实际执行 `memory.list` → `memory.attention.write` → `memory.attention.retire` → `done`。
- `source_refs` 原始 thought_id 索引写入注意力记忆，淘汰时保留供查证。

### 4. 经验/偏好查证实验（D12）
- 样本：8 个合成场景 × 3 方案 × 2 轮，真实 MiniMax-M3。
- 结果（确定性黄金事实评分）：
  - A（输入前准备）：recall 0.75，precision 1.0，parse_ok 1.0，平均 605 tokens。
  - B（agent 自查）：recall 0.50，precision 0.6875，parse_ok 0.9375，平均 680 tokens。
  - Hybrid：recall 0.6875，precision 1.0，parse_ok 1.0，平均 652 tokens。
- **选型：A（输入前准备）**。经验/偏好 agent 采用预取证据后单轮提取，写入走能力通道。
- 实验脚本：`tests/experiments/memory_verification_experiment.py`；摘要：`docs/experiments/memory_verification_experiment_summary.md`。

### 5. 出厂预装与自扩展（D6）
- 能力种子统一由 `ensure_default_capabilities + import_factory_defaults` 导入，不再使用硬编码 SQL。
- 出厂默认 agent 授权 9 个 builtin + `capability.import`。
- 五态认知 seed 保持并继续作为出厂记忆。
- 新增 `capability.import`：原子导入 base/composite/usage 定义，仅允许已知 builtin executor，可 `grant_to_agent`。
- 新增 `skill.convert` usage method 和 `data/seed/skills/count_errors_skill.md` 示例。
- **真实模型最终验证通过**：MiniMax-M3 将 SKILL.md 转为 capability.import JSON，`capability_probe` 导入并执行 `user.skill.count-errors`，对 3 条 ERROR 日志统计得到 `3`。
- 复现脚本：`tests/experiments/skill_absorption_test.py`。
- **真实外部 skill 实测**：`https://github.com/NoxTyrannus/Glanstia_System-skill` 的 `soul_guide` 组件。首两次模型直出 JSON 存在语法/schema 错误；用严格骨架修复提示后，导入 `user.skill.soul_guide` 并成功执行：2 个 `.soulmd` 被提取标签、写入 `souls.csv`、归档到 `Hades/`；`gno` 依赖未伪装成能力，以 `external_dependency=gno` 明确暴露。此问题已反哺 `skill.convert` usage method 的硬性规则。

### 6. 提示词职责重构（v0.3.1 前移）
- `system.md` 纯输出协议；`SOUL.md` 身份/语言/风格；三种 mode 文件补充协同关系与交流原则。
- `execution_platform.md` 强化一次完成与双范例；`insight_platform.md` 强化工具经验；memory 提示词对齐索引/查证/认知图协议。
- 旧版出厂提示词通过 legacy hash 自动升级，用户自定义内容保留。

### 7. 执行层健壮性包（F4 选定项）
- `dep_summary` 200→2000 字符。
- subagent 环境上下文注入工作区绝对路径。
- `SUBAGENT_MAX_TURNS` 6→8。

### 8. 代码优化（D14）
- 阶段一：`ThoughtId` 下沉 `common::types`，数据层不再反向依赖 agent。
- 阶段二：`PromptCache`（进程内缓存）、`ThoughtHistoryCache`（写穿透 + recover 缓存增量更新）。
- 阶段三：`thought_store` 拆分出 `atomic_io`；`context_assembler` 拆分出 `budget` / `reader`。
- 风险说明：`execution_platform` / `entry` 的进一步垂直拆分未在 v0.3.0 内强制执行，避免在已通过全量真实 API 验证后引入无收益重构风险；列为后续低风险工程项。

## 三、测试结果

### L1
- `cargo test --lib`：**673 passed / 0 failed / 2 ignored**。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `cargo fmt --all -- --check`：通过。

### L2 PTY mock
- V1–V12：**53/53 通过**（新增 V12 注意力 agent 工具调用链）。
- 报告：`tests/ptytest/PTY_TEST_REPORT.md`（运行时生成）。

### 真实 API 冒烟（MiniMax-M3）
- V1–V9 + V12：**40/40 通过**。
- 覆盖：统一触发链、Mix Thinking、KEEP 预算、配置面板、旧配置兼容、真实能力执行、
  **真实注意力 agent 工具调用**（`memory.list` / `memory.attention.write` / `memory.attention.retire` / `done`）。

### 自扩展最终验证
- SKILL.md → `capability.import` → 导入 → 执行 → 结果 `3`，全链路通过。

## 四、兼容性
- 旧 `memory_mode` 配置继续兼容启动。
- 旧 `wasm:*` executor 行启动时由工厂导入自动改写为 `builtin:*`。
- 用户自定义提示词与用户自定义 agent 行不被覆盖。
- 注意力条目新增 `source_refs` 字段为可选，旧数据兼容。

## 五、遗留项
1. `execution_platform` / `entry` 更深模块拆分（当前已有工具协议等新模块，但不做纯移动式重构）。
2. 真实 API 无法确定性制造 429/404，V10/V11 由 mock 覆盖。
3. 语义检索仍为过滤式检索，未引入 embedding。
