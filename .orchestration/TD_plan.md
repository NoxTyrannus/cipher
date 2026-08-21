# TD 集成测试计划（v0.3.1 执行中台 subagent 体系）

前置：TA/TB/TC 各分支单测与全量门禁全绿；合并到 integration 分支。

集成测试（mock provider / 进程内，不包含 PTY 与真实 API）：
1. 执行中台输出 → subagent.create + run → 返回 accepted → 当轮不等待
2. 异步 runtime 完成 → 下一轮 ThinkingInput 出现 Subagent Status + last_output
3. 六个生命周期状态机端到端
4. attempt/total timeout 与 max_retries 端到端
5. 洞察读取最近 invocation 日志 → usage_method.observe 写回
6. AgentPool 心跳可观测；四组核心不可操作

收口：
- 全量四门禁通过
- 生成执行报告 .devdocs/执行报告_执行中台_v0.3.1_subagent体系.md
- 用户给出报告路径后 code review；review 通过前不开始 PTY mock / 真实 API 模拟测试
