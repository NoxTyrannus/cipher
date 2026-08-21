# Changelog

## v0.3.2
- 上下文工程：三模式提示词最小化为纯增量 + context-engineering prompt 架构，洞察流式输出与异步工具记忆（tool_memory 按实际执行能力 P0 gate）
- 执行证据规则：done-evidence + START/OK 日志顺序，capability 输出契约与自扩展运行时修复
- 新增工具能力：path.exists / file.glob / json.validate / 递归 text.grep
- 执行中台替换为新生命周期管理器（tc-execution-platform），UNNI 飞轮收敛 + 中台消息结构（批次 1-4）
- 删除 provider 原生 tool calling 路径，capability 语义命名迁移 + capability_protocol 冻结共享契约
- 新增 subagent 模块：异步 subagent runtime（spawn/hook 桥接/能力循环/超时与有限重试/心跳/完成回调）与 AgentPool 心跳/权限/快照展示
- subagent 记忆模块 memory.json/last_output.json（追加 + 窗口裁剪 + 私有权限 + 原子写），UI 状态栏最小适配
- subagent 能力域：六分子能力核心模块 + CapabilityExecutor 接线 + 种子数据（7 分子能力、4 模板）
- 契约修订：SubagentLifecycle 补充 Sleeping 变体，AgentEntry 心跳字段 + prompts 按需加载
- subagent 失败收口校验 + 多轮重试闭环；失败证据落盘（RunFailure 携带 calls/logs）+ shell.exec dd 黑名单误伤修复

## v0.3.0
- 移除 WASM 层，9 能力直连 builtin host 安全逻辑
- 新增 9 个记忆能力与 4 个入表记忆 agent
- 注意力 agent 改为服务层能力调用式交错思维链
- 经验/偏好原始记忆查证实验，落地 A 方案
- 认知图更新走 memory.cognitive.update 能力
- 出厂预装五态认知 + 原子/分子/方法能力示例
- 新增 capability.import 自扩展闭环与 skill.convert 方法
- v0.3.1 提示词职责重构前移完成
- 执行层健壮性：dep_summary 2000、工作区绝对路径、subagent 8 轮
- 代码优化：ThoughtId 下沉、PromptCache、ThoughtHistoryCache、thought_store/context_assembler 拆分
