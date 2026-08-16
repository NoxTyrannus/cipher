# Cipher 能力输出合同（v0.3.1 基准模板）

所有可执行能力必须通过两层校验，缺一不可：
1. **语义校验**：输出中不得包含失败语义（`success=false`、非空 `error`、非零 `exit_code`）。
2. **结构校验**：输出必须满足 `schema_out.required`，即“成功输出必须包含证明字段”。

服务层入口统一执行：`CapabilityService::execute_base()` → `evaluate_capability_output()` → `validate_schema(schema_out)`。

## 模板一：动作类（file.write / file.list / file.delete / file.move）

```json
{
  "schema_out": {
    "type": "object",
    "properties": { "success": { "type": "boolean" } },
    "required": ["success"]
  }
}
```

成功输出示例：
```json
{"success": true}
```

失败时返回：
```json
{"success": false, "error": "原因"}
```

## 模板二：读取类（file.read / file.chunk_read）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "content": { "type": "string" },
      "size": { "type": "integer" }
    },
    "required": ["content", "size"]
  }
}
```

成功输出示例：
```json
{"content": "...", "size": 3}
```

## 模板三：进程类（shell.exec / powershell.exec / code.exec）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "stdout": { "type": "string" },
      "stderr": { "type": "string" },
      "exit_code": { "type": "integer" }
    },
    "required": ["stdout", "stderr", "exit_code"]
  }
}
```

规则：`exit_code` 必须为 `0`，否则服务层直接判定失败，不会进入成功路径。

## 模板四：列表/检索类（memory.list / memory.retrieve / memory.evidence.lookup）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "items": { "type": "array", "items": { "type": "object" } },
      "count": { "type": "integer" }
    },
    "required": ["items", "count"]
  }
}
```

## 模板五：写入/变更类（memory.attention.write / memory.experience.write / memory.preference.write）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "written": { "type": "integer" },
      "ids": { "type": "array", "items": { "type": "integer" } }
    },
    "required": ["written", "ids"]
  }
}
```

## 模板六：导入类（capability.import）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "success": { "type": "boolean" },
      "imported": { "type": "object" },
      "granted_to_agent": { "type": ["string", "null"] }
    },
    "required": ["success", "imported"]
  }
}
```

## 模板七：复合能力（composite）

```json
{
  "schema_out": {
    "type": "object",
    "properties": {
      "capability_id": { "type": "string" },
      "steps": { "type": "array" },
      "final": {}
    },
    "required": ["capability_id", "steps", "final"]
  }
}
```

复合能力内部每个节点仍按对应 base 能力合同校验；任一步失败，整个 composite 失败。
