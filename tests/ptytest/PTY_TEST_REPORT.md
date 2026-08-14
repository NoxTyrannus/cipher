# Cipher v0.2.6 PTY 黑盒验证报告

> 验证方式：用 PTY 驱动真实 `target/debug/cipher` TUI（ratatui/crossterm），
> 配本地 mock LLM 服务器（OpenAI 兼容接口），按脚本化请求/响应确定性驱动思考引擎与三中台，
> 依据 **mock 请求序列（含输入内容） / 应用 trace 日志 / config.toml 落盘 / 屏幕文本** 四类渠道断言。
>
> 运行：`python3 tests/ptytest/ptytest.py --root /tmp/cipher-ptytest`（mock 全量 V1–V11）；
> 真实 API 冒烟：`MINIMAX_API_KEY=<key> python3 tests/ptytest/ptytest.py --root <dir> --real`（V1–V9）。
> （每次场景独立沙箱 root，`XDG_CONFIG_HOME/XDG_DATA_HOME` 重定向，不污染用户环境）

---

## 一、验证题矩阵（V1–V11）

| 编号 | 验证题 | 覆盖点 | 指标数 |
|------|--------|--------|--------|
| V1 | UNNI 自主 + 执行中台 | 统一触发调度主路径、异步中台只沉淀、echo 上屏 | 5 |
| V2 | UNNI 跟随 + 执行中台 | 协同节点完成不触发、pending context 合并进下次输入 | 3 |
| V3 | UNNI 自主 + 洞察中台 | 协同节点前事件被忽略、洞察节点触发、只一次 echo | 4 |
| V4 | KEEP 预算暂停 | token 预算耗尽 → 暂停 + 屏幕提示 + 不再 spawn | 3 |
| V5 | LOOP off（记忆节点） | 执行/洞察被忽略、记忆节点触发、无反射实例 | 3 |
| V6 | LOOP on（Mix Thinking） | 三实例流水线、拼接合并（含实例1/2 反思）、反射不执行、多轮循环 | 9 |
| V7 | F1 自动修复 | invalid 输出 → 自动修复轮（保留用户意图）、修复后完整走链 | 3 |
| V8 | 配置面板 Mode Style | 菜单入口、5 项子菜单、节点选择、保存消息、config.toml 落盘 | 5 |
| V9 | 旧配置兼容 + Tab 切换 | `memory_mode` 旧字段兼容启动、Tab 三模式切换 | 3 |
| V10 | LOOP 限流重试（缺陷2 配套） | 429 → 指数退避重试（3s/6s）→ 成功落库 → final 拼接完整 | 6 |
| V11 | LOOP 永久错误降级（缺陷2 配套） | 404 → 暴露错误、不中断、final 缺段继续、链续跑 | 6 |
| **合计** | | | **50 项全部通过** |

---

## 二、验证指标明细（全通过 50/50）

### V1 UNNI 自主 + 执行中台（5/5）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| user_think_1 | mock 收到 1 条 user 思考请求 | PASS |
| echo_spawn_1 | 执行完成后 echo（自主）实例仅 1 个 | PASS |
| async_only_sinking | 洞察/记忆中台在协同节点后只沉淀不触发（`only sinking memory, not triggering`） | PASS |
| echo_say_published | echo 的 say 渲染上屏（消息气泡） | PASS |
| no_extra_echo | 全程无多余 echo 实例 | PASS |

### V2 UNNI 跟随 + 执行中台（3/3）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| no_echo_after_first_round | 首轮协同节点完成不 spawn（跟随语义） | PASS |
| pending_merged_log | `merging pending context (thought_id=...) into user input` | PASS |
| second_request_has_pending | 二次输入请求含 `上一轮整理上下文` 段 | PASS |

### V3 UNNI 自主 + 洞察中台（4/4）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| echo_spawn_on_insight | 洞察节点完成触发 echo | PASS |
| memory_only_sinking | 记忆中台异步只沉淀 | PASS |
| execution_before_node_ignored | 执行完成在协同节点前被忽略（`trigger ignored`） | PASS |
| no_extra_echo | 仅 1 次 echo | PASS |

### V4 KEEP 预算暂停（3/3，token_budget=16000）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| budget_exhausted_log | `KEEP budget exhausted, pausing flywheel` | PASS |
| screen_pause_message | 屏幕 `KEEP 预算已耗尽` 提示 | PASS |
| no_spawn_after_pause | 暂停后无新实例 spawn | PASS |

### V5 LOOP off（3/3）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| echo_spawn_on_memory | 记忆节点完成触发 echo | PASS |
| exec_insight_before_node_ignored | 执行/洞察在协同节点前被忽略 | PASS |
| no_reflect_instances | 无反射实例（Mix off 不启用） | PASS |

### V6 LOOP on（9/9，Mix Thinking 融合思考）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| reflect1_spawn | 执行完成 → 实例1（执行反思） | PASS |
| reflect2_spawn | 洞察完成 → 实例2（洞察反思） | PASS |
| final_spawn | 记忆完成 → 实例3（综合=下一轮 think_0） | PASS |
| round2_final | 第二轮循环推进 | PASS |
| final_merged_reflect1 | final 输入含 `[实例1 反思 ...]`（拼接合并） | PASS |
| final_merged_reflect2 | final 输入含 `[实例2 反思 ...]`（拼接合并） | PASS |
| reflect1_was_reflect_only | 反射实例不执行 | PASS |
| reflect_only_finished | `reflect-only instance finished (no execution)` | PASS |
| no_extra_execution_chain | 反射永不 Execute（执行实例 = 用户轮 + 每轮 final） | PASS |

### V7 F1 自动修复（3/3）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| auto_repair_log | `auto-repair round 1/2 for user intent` | PASS |
| repair_request_with_intent | 修复请求保留用户输入原文意图 | PASS |
| repair_turn_completed | 修复轮走完整执行链并正常收敛 | PASS |

### V8 配置面板（5/5）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| menu_has_mode_style | 主菜单含 `协同模式风格 (Mode Style)` | PASS |
| submenu_shows_5_items | 子菜单 5 项（协同方式/节点/KEEP 预算/时间/LOOP 开关） | PASS |
| node_select_shown | UNNI 协同节点三选（执行/洞察/记忆） | PASS |
| save_message_shown | 保存后界面提示 `协同节点已切换` | PASS |
| node_saved_to_config | `config.toml` 落盘 `node = "insight"` | PASS |

### V9 旧配置兼容 + Tab（3/3）
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| startup_with_legacy_memory_mode | 含旧 `memory_mode` 字段配置正常启动 | PASS |
| tab_unni_to_keep | Tab → KEEP（`KEEP mode entered`） | PASS |
| tab_keep_to_loop | Tab → LOOP（`LOOP mode entered`） | PASS |

### V10 LOOP 限流重试（6/6，缺陷2 配套）
> 脚本：实例1（执行反思）前 2 次返回 429，第 3 次成功；其余按脚本/默认。
> 覆盖思考层指数退避重试（3s → 6s）与最终拼接完整性。
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| retry_log_attempt1 | `LLM call failed (retryable, attempt=1) ... backoff 3s` | PASS |
| retry_log_attempt2 | `attempt=2 ... backoff 6s`（指数序列正确） | PASS |
| retry_status_exposed | 屏幕错误条出现 `后重试`（`StreamChunk::Status` → 消息面板） | PASS |
| reflect1_succeeded_after_retry | 实例1 共请求 3 次（2 失败 + 1 成功） | PASS |
| final_merged_reflect1 | final 输入含 `[实例1 反思 ...]`（重试成功 → 拼接不缺失） | PASS |
| final_merged_reflect2 | final 输入含 `[实例2 反思 ...]` | PASS |

### V11 LOOP 永久错误降级（6/6，缺陷2 配套）
> 脚本：实例2（洞察反思）返回 404（永久错误），实例1 正常。
> 覆盖永久错误暴露 + 不中断 + 摘要缺段继续 + 链续跑。
| 指标 | 断言渠道 | 结果 |
|------|----------|------|
| permanent_error_log | `mix dep reflect2 (...) permanent failed: ... HTTP 404` | PASS |
| error_exposed_screen | 屏幕错误条 `反思实例2 永久失败` | PASS |
| final_spawns_after_permanent | 永久失败后 final 仍 spawn（不中断） | PASS |
| final_has_reflect1 | final 输入含 `[实例1 反思 ...]`（可用段保留） | PASS |
| final_skips_reflect2 | final 输入**不含** `[实例2 反思]`（缺段继续） | PASS |
| chain_continues | 第二轮 final 仍正常 spawn（飞轮不因单点失败中断） | PASS |

---

## 三、测试中发现并修复的缺陷（2 个）

### 缺陷 1：模式名大小写不匹配导致 KEEP/LOOP 触发调度失效（v0.2.6 实装 bug）
- **现象**：V4（KEEP 预算）飞轮无限转、预算永不耗尽；V5/V6（LOOP）Mix Thinking 未按设计走。
- **根因**：`mode_manager.current_name()` 返回大写 `"KEEP"/"LOOP"/"UNNI"`，
  而统一触发调度（`entry.rs`）用 `style_for(mode_name)` 与 `mode_name == "keep"/"loop"` 比较小写；
  `style_for("KEEP")` 落入 `_ => self.unni` 兜底分支 → KEEP/LOOP 拿到的是 UNNI 风格。
  UNNI 恰好因兜底而“侥幸正确”，掩盖了该问题。
- **修复**：触发分支内 `let mode_name = current_name().to_ascii_lowercase();`
  （`src/startup/entry.rs`）。UI 展示处仍用大写，不受影响。
- **验证**：修复后 V4/V5/V6 全部通过。

### 缺陷 2：Mix Thinking 拼接 join 竞态——final 可能缺实例2 反思（v0.2.6 实装 bug）
- **现象**：V6 第二轮 final 实例的输入缺失 `[实例2 反思]` 段。
- **根因**：final 由 `memory_complete` 触发时直接 `mix_summary` 读取实例2 的 think；
  但反思实例（ReflectOnly）与各中台**并行**，其 TurnContext 落库可能晚于 `memory_complete`
  事件到达（实测第二轮 reflect2 于 20.572s 落库，而 memory_complete 于 20.556s 已处理）。
- **修复（两阶段）**：
  1. 初版临时方案 `wait_for_reflect_think`（有界轮询 60s/50ms）验证了方向，但被用户否决
     （不要超时兜底，要可重试与真实暴露）。
  2. **终版（事件驱动 + 重试，用户拍板）**：
     - **join 事件驱动**：`MixDepRegistry`（turn_id → Running/Ready/Permanent + `Notify`）
       登记反射实例；主循环在 `pool_rx` 收到实例结果时更新注册表并 `try_progress_mix()`
       推进 `PendingMix`（`AwaitReflect1` → 等实例1 → spawn 实例2；`AwaitFinal` → 等
       实例1+2 → spawn final）。依赖在跑（含重试中）时立即返回、不阻塞主循环、无轮询。
     - **思考层指数退避重试**：`spawn_streaming` 内对可重试错误（429/408/5xx/超时/网络）
       退避重试，3s 起、×2、60s 封顶、无总次数硬限（用户可随时退出）；每次重试经
       `StreamChunk::Status` 暴露到消息面板错误条。永久错误（401/403/404/400 等）不重试。
     - **错误分类**：`error.rs` 新增 `extract_http_status` / `is_retryable_status` /
       `is_retryable_llm_error` / `backoff_delay_secs`。
     - **降级语义**：永久失败 → 注册表 `Permanent(err)` → 暴露错误后摘要**缺段继续**，
       链不中断（符合“失败是学习机会、能用就有概率拿经验”的设计意图）。
- **验证**：V6（竞态）、V10（429 重试 → final 完整）、V11（404 降级 → final 缺段但链续跑）全部通过。

### 缺陷 3：能力注册表加载时序——真实能力执行全部失效（v0.1.0 起，真实 API 冒烟暴露）
- **现象**：真实 API 冒烟中模型正确设计出 `echo 'real-ok-v1' > /tmp/...` 的执行方案，
  但 shell.exec 执行报 `not found: capability actor agent: agent`，文件未创建。
- **根因**：`run_normal` 里 `app_state.registry`（能力注册表）在 `bootstrap()` 时加载，
  **早于** agent/能力种子写入（entry.rs 的 7 条 base_capability + `UPDATE agent SET tool_caps`）。
  于是执行平台与模式管理器拿到的注册表：新目录无 `agent` 行，setup 过的目录 `tool_caps` 为 NULL
  （init_flow 只插行不设 tool_caps）→ `execute_for_agent("agent", ...)` 必失败，
  **shell.exec / file.\* 等全部真实能力执行从未生效**。git 历史确认自 v0.1.0 起存在。
- **为什么 mock 测试没暴露**：mock 场景的执行设计是垃圾内容 → 各中台 fallback →
  从没走到真实节点执行。这正是真实 API 全链路测试的价值。
- **修复**（用户拍板后落地）：
  1. `src/startup/entry.rs`：能力种子块结束后重载
     `app_state.registry = load_all_into_memory(&app_state.duckdb)?`（一行），
     与配置面板改动后重载注册表的既有模式一致。
  2. `examples/insert_mock_model.rs`：模拟 `cipher setup` 的 init_flow 补插 `agent` 行
     （tool_caps 留 NULL，与 init_flow 一致；否则 bootstrap 校验报 unknown capability）。
- **验证**：真实 API 全量重跑，file.write/file.read/shell.exec 均 `prefilled OK`，
  各场景工作区实际落盘文件（如 `cipher_real_v1.txt` 内容 `real-ok-v1`）。

---

## 四、真实 API 冒烟（--real，minimax MiniMax-M3）

> 方式：mock LLM 变**透明代理**——请求日志/分类不变（结构化断言继续有效），
> 响应来自真实模型。代理对上游请求注入 `thinking: {type: disabled}`
> （MiniMax-M3 是推理模型，不注入会在 content 里输出推理前缀文本，破坏 JSON 契约）。
> 模型行 `api_url` 仍指向本地 mock，`api_key` 为真实 minimax key；测试沙箱独立配置。
>
> 运行：`MINIMAX_API_KEY=<key> python3 tests/ptytest/ptytest.py --root <dir> --real`
> （V1–V9 全量；V10/V11 属错误注入场景，仅对 mock 有意义，真实冒烟跳过）。

| 场景 | 指标 | 结果 |
|------|------|------|
| V1 | user_think / echo_spawn / 异步只沉淀 / echo 轮完成 | 4/4 |
| V2 | 跟随不 echo / pending 合并日志 / 二次输入含上一轮上下文 | 3/3 |
| V3 | 洞察触发 echo / 记忆只沉淀 / 节点前忽略 / 无多余 echo | 4/4 |
| V4 | 预算耗尽日志 / 屏幕暂停提示 / 暂停后不 spawn | 3/3 |
| V5 | 记忆触发 echo / 节点前忽略 / 无反射实例 | 3/3 |
| V6 | 三实例流水线 / 拼接含实例1+2 反思 / 反射不执行 / 两轮循环 | 9/9 |
| V7 | 真实 think 发出 / 链走到 echo / 应用存活 | 3/3 |
| V8 | 配置面板 5 项 + 保存落盘 | 5/5 |
| V9 | 旧配置兼容启动 + Tab 切换 | 3/3 |
| **合计** | | **37/37 全部通过** |

真实执行核验：各场景工作区（`<root>/<V>/ws`，即执行沙箱根）实际落盘文件
（`cipher_real_v1.txt` 内容 `real-ok-v1` 等）；日志 `flow node prefilled OK file.write / file.read / shell.exec`。

---

## 五、测试基建要点（可复用的确定性驱动方法）

1. **确定性触发链**：mock 对三中台平台请求统一返回垃圾（`not a valid structured output`），
   各中台 fallback 后仍发送完成事件 → 触发链完全由脚本化的 think/echo/reflect 输出驱动，
   不依赖任何真实 LLM。
2. **请求分类**（`mock_llm.py::classify`）：
   - 非流式请求 = 中台平台调用；流式请求 = 思考引擎；
   - 按最后一条 User 消息内容判定：`既定目标:` → echo/反射；
     `记忆中台已整理上一轮` + 反思段（或含 `实例2 反思`）→ final（含缺段降级 final）；
     `实例1 反思` → reflect2；其余 → 用户原始输入。
   - 脚本支持 `respond` 为**列表**（按匹配次数顺序取用，如 `[429, 429, 成功]`），
     `http_error` 可指定 `status`（如 404/429），用于失败注入与重试验证。
3. **断言渠道优先级**：trace 日志（`RUST_LOG=debug`）> mock 请求序列（含 input 全文）>
   `config.toml` 落盘 > 屏幕文本。
   屏幕文本断言一律做空白归一化（ratatui 渲染会吞空格/换行，如 `V1 执行完成` 渲染为 `V1执行完成`）。
4. **PTY 前置**：先 `TIOCSWINSZ` 设置窗口（40×120）防 0x0 崩溃；
   模式切换（Tab）断言用 `mode entered` 日志而非屏幕（帧拼接不可靠）。
5. **PTY 持续排空**：后台线程持续读取 PTY 输出，防止 60fps 渲染把 PTY 缓冲区填满、
   阻塞应用 `draw()`（V10 长退避期间曾因此冻结 40s，属测试基建问题）。
6. **隔离**：每场景独立 `XDG_CONFIG_HOME/XDG_DATA_HOME`，不污染用户真实配置/数据。
7. **真实 API（--real）**：
   - mock 变透明代理：日志/分类不变，`thinking: {type: disabled}` 注入保证 JSON 契约；
     断言仍走原四渠道，V1–V9 结构化指标原样复用（V10/V11 错误注入场景跳过）。
   - 执行沙箱根 = 应用 cwd = `<root>/<V>/ws`（场景创建），任务用相对路径，
     真实文件操作才能落地（写 `/tmp` 会被沙箱拒绝）。
   - 真实模型延迟更高：`wait_request_count` / `wait_log` 超时自动放宽 3 倍；
     关键时序断言改为“等到日志出现”再输入（如 V2 等 stash 落定再发第二条）。
   - 断言统计需过滤 `proxy-resp` 日志行（仅诊断用，不含请求输入）。

---

## 六、结论

- v0.2.6 的三模式协同语义（统一触发调度、协同节点后的中台只沉淀不触发、跟随暂存合并）、
  KEEP 预算暂停、LOOP Mix Thinking 三实例流水线与拼接合并、F1 自动修复、配置面板 Mode Style、
  旧配置兼容，**全部 50 项 mock 黑盒指标通过**（V1–V11，含缺陷2 配套的限流重试与永久错误降级场景）。
- **真实 API 冒烟（minimax MiniMax-M3）V1–V9 全量 37/37 通过**，且真实能力执行落地
  （file.write/file.read/shell.exec 均 OK，工作区实际生成文件）——证明思考引擎、三中台、
  触发链、Mix 拼接在真实 LLM 下端到端可用。
- 测试共发现并修复 3 个真实缺陷：
  1. 模式名大小写（v0.2.6 实装 bug）；
  2. Mix join 竞态（按用户拍板设计落地为事件驱动 join + 指数退避重试 + 状态码分类
     + 错误暴露不中断 + 永久错误缺段继续）；
  3. **能力注册表加载时序**（v0.1.0 起，真实 API 冒烟暴露——mock 测试从未走到真实能力执行）。
- 若非 PTY 黑盒全链路验证（尤其真实 API 冒烟），这 3 个核心链路缺陷难以暴露。
