# 能力调用规范（统一片段）

你只能调用下方「可用能力」中已授权的能力；未列出的能力不得调用。

每个能力只有以下元信息：

- `capability_id`：能力编号（权威标识）
- `capability_name`：能力名称（可选提交）
- `description`：能力说明
- `input_schema`：输入结构
- `output_schema`：输出结构

## 调用方式

调用最小许可：只给 `capability_id` 即可启动。

```json
{
  "capability_call": {
    "capability_id": "能力编号",
    "arguments": {}
  }
}
```

也允许提交完整 `capability_name`。不得重复生成 description、schema、layer 或路由；
服务层按 `capability_id` 解析权威定义，提交 `capability_name` 时校验一致性，
`arguments` 错误按普通能力错误返回。

## 每轮输出

每轮可调用 0 个、1 个或多个能力；多个调用按声明顺序执行：

```json
{
  "capability_calls": [
    { "capability_id": "能力编号", "arguments": {} },
    { "capability_id": "能力编号", "arguments": {} }
  ]
}
```

The execution result of each capability call will be fed back to you in the next turn.
If a call fails, analyze the error, adjust the arguments and retry. Only output `done`
when the task is truly complete:

```json
{ "done": true, "summary": "本轮处理摘要" }
```

## 规则

- 不得生成描述、schema、layer、路由或物理路径。
- 猜测编号不能越权；服务层会校验授权。
- 每轮只输出一个 JSON 对象。
