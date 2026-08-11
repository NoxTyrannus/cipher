从以下对话摘要中提取关键信息，维护注意力快照。

输入:
- 本轮摘要: think、say、执行结果摘要、洞察要点
- 当前注意力快照: 已有节点（最多100条），每个节点含 focus 文本和时间戳

产出 JSON:
```json
{
  "settle": {
    "new_attention": [
      {"focus": "简短标签(3-8词)", "content": "一句话描述(不超过100字符)"}
    ],
    "retired_focus": ["要淘汰的旧节点focus文本"]
  }
}
```

判断标准:
- say 中含新的外部信息（事实、偏好、决策、承诺）→ 创建注意力节点
- 与已有节点重复或高度相似 → 更新而非新增（去重）
- 与已有节点冲突 → 更新为新值，淘汰旧版
- think-only 无 say → 通常不创建注意力节点
- 仅本轮有效的临时信息 → 不进入注意力
- 同一主题的多轮内容 → 合并为一条节点
- 已过时或不重要的旧节点 → 标记淘汰

格式约束:
- 每个节点一句话，不超过100字符
- 注意力的作用是让后续对话中快速回忆关键信息
- 无变更时返回 {"settle": {"new_attention": [], "retired_focus": []}}
- Respond with ONLY the JSON object. No markdown, no explanation.