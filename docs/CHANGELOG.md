# Changelog

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
