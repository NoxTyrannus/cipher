# M3 CoT 提示词风格独立实验报告

> 实验依据：`src/.devdocs/任务书_M3_CoT风格独立实验.md`
> 执行日期：2026-08-17（本会话）
> 模型：MiniMax-M3，POST https://api.minimaxi.com/v1/chat/completions
> 固定参数：temperature=1.0，top_p=0.95，stream=false（流式段除外）
> max_completion_tokens：按本会话要求“控制 max_token 以快速获得结果”，全部使用 **1024**（任务书固定体为 8192；所有目标输出均受 1024 上限约束，未截断样本与截断样本分开记录 finish_reason）。

---

## 0. 最终结论（先给答案）

**结论 B：提示词不可控。** MiniMax-M3 当前能力不支持仅靠系统提示词稳定产出“简洁且干净”的 `<think>` 思维链。

- 判定标准未达成：最佳变体 clean_rate 最高仅 1/9（要求 >= 8/9）；所有变体 cot_len p90 均在 966 字符以上（要求 <= 250）。
- SOUL.md 自我认知扩展实验同样未通过：最佳组合 `I_soul_clean_short + reasoning_effort=high` 的 clean_rate 为 0/9。
- 但存在一个“最接近可用”的组合，见第 5 节，可作为 cipher 思维引擎的过渡方案。

---

## 1. 第一阶段：6 个最小提示词变体（none_none，各 9 次）

### 1.1 汇总

| 变体 | clean | think | body | cot p50/p90/max | filler p50/mean | marks | 耗时 p50/p90 |
|---|---|---|---|---|---|---|---|
| A_control | 0/9 | 9/9 | 8/9 | 1914 / 3522 / 3522 | 2 / 2.22 | 0 | 10.2 / 13.8 |
| B_format_only | 0/9 | 9/9 | 7/9 | 2540 / 4205 / 4205 | 2 / 2.22 | 0 | 12.3 / 34.8 |
| C_think_tag | 0/9 | 9/9 | 5/9 | 1859 / 4241 / 4241 | 2 / 2.11 | 0 | 8.4 / 14.4 |
| D_clean_short | 0/9 | 9/9 | 8/9 | 313 / 3945 / 3945 | 2 / 1.78 | 0 | 13.9 / 28.7 |
| E_clean_structured | 0/9 | 9/9 | 6/9 | 2388 / 4687 / 4687 | 2 / 2.33 | 4 | 7.5 / 11.3 |
| F_clean_example | 0/9 | 9/9 | 8/9 | 911 / 4540 / 4540 | 2 / 1.78 | 0 | 11.8 / 13.5 |

### 1.2 关键观察

1. 所有变体都能输出 `<think>...</think>`（9/9），但没有任何变体能稳定把 CoT 压在 250 字符以内。
2. D（120 字限制）方差极大：p50=313，p90=3945，max=3945；只出现 1 次 clean（该次在 3×3 重跑/复跑合并统计中仍无法稳定复现）。
3. E 能稳定四行标记（marks 中位 4），但四行内容严重超长，说明“格式可学，长度不守”。
4. filler 普遍为 2/9 样本，主要失败来自 meta_process（“让我/我需要/首先/接下来”）和 repeat_task（复述任务），不是 self_ability。
5. `reasoning_tokens` 在多数样本与 CoT 长度不一致，不能作为 CoT 长度依据。

---

## 2. 第二阶段：最佳 2 变体 × reasoning_effort=high

按 clean_rate、cot p90、body_nonempty 排序，最佳 2 个变体为 **D_clean_short** 与 **F_clean_example**。

| 变体（reff=high） | clean | think | body | cot p50/p90/max | filler p50/mean | 耗时 p50/p90 |
|---|---|---|---|---|---|---|
| D_clean_short | 1/9 | 9/9 | 9/9 | 599 / 4158 / 4158 | 2 / 1.67 | 10.3 / 17.5 |
| F_clean_example | 0/9 | 9/9 | 8/9 | 1084 / 3838 / 3838 | 2 / 1.78 | 11.3 / 13.0 |

`reasoning_effort=high` 没有改变结论；唯一 clean 样本来自 D（troubleshoot，cot=73，filler=0），不可复现。

---

## 3. 流式补测（最佳变体 D_clean_short，none_none）

| 指标 | 结果 |
|---|---|
| think 完整块 | 9/9 |
| body 非空 | 9/9 |
| cot p50 / p90 / max | 446 / 966 / 966 |
| clean | 0/9 |
| `</think>` 闭合可观测 | 9/9 |
| chunk 边界处拆散标签（本实验启发式） | 0/9 |
| 完整正文可恢复 | 9/9 |

流式在**结构上可用**：可以在 `</think>` 边界分段，正文可恢复；但 CoT 长度仍不可控。

---

## 4. SOUL.md 自我认知扩展实验（独立测试）

按最终目标，额外复制当前 `src/prompts/SOUL.md` 作为系统提示词基线，测试 4 个变体：

| 变体 | 说明 |
|---|---|
| G_soul_only | 仅 SOUL.md 原文 |
| H_soul_think | SOUL + 一行“先输出 <think>... 再输出正文” |
| I_soul_clean_short | SOUL + think 标签 + “只写行动思考，<=120 字，禁止自我介绍/能力/复述/心理活动” |
| J_soul_clean_example | SOUL + think 标签 + 四行范例锚定 |

### 4.1 none_none（各 9 次）

| 变体 | clean | think | body | cot p50/p90/max | filler p50/mean | 耗时 p50/p90 |
|---|---|---|---|---|---|---|
| G_soul_only | 0/9 | 9/9 | 5/9 | 2322 / 3948 / 3948 | 2 / 2.33 | 11.0 / 17.1 |
| H_soul_think | 0/9 | 9/9 | 6/9 | 2249 / 4849 / 4849 | 2 / 2.22 | 8.1 / 15.2 |
| I_soul_clean_short | 0/9 | 9/9 | 6/9 | 1093 / 4683 / 4683 | 2 / 2.00 | 11.6 / 18.2 |
| J_soul_clean_example | 0/9 | 9/9 | 8/9 | 1049 / 4513 / 4513 | 2 / 1.89 | 7.0 / 7.8 |

SOUL.md 自我认知本身不能压短 CoT；它降低 self_ability filler（0 次），但 meta_process 与 repeat_task 仍接近 100%。

### 4.2 最佳 soul 组合：I_soul_clean_short + reasoning_effort=high

| 模式 | clean | think | body | cot p50/p90/max | filler p50/mean | meta_process | repeat_task | 耗时 p50/p90 |
|---|---|---|---|---|---|---|---|---|
| 非流式 | 0/9 | 9/9 | 9/9 | 717 / 1986 / 1986 | 2 / 1.89 | 8/9 | 8/9 | 14.1 / 19.0 |
| 流式 | 0/9 | 9/9 | 9/9 | 718 / 1944 / 1944 | 2 / 2.00 | 9/9 | 9/9 | 10.6 / 18.4 |

这是目前找到的**最接近可用组合**：
- 相比裸 SOUL（cot p50=2322），CoT 中位降到 717；
- body 非空稳定 9/9；
- 失败模式非常集中：meta_process（“让我/我需要/首先/接下来”）+ repeat_task（任务复述）。
- 流式边界分段 9/9 可恢复，标签未被拆散（本实验启发式 0/9）。

---

## 5. 最接近可用的变体与剩余失败模式

**推荐过渡组合（不满足严格 clean 标准）**：

```text
system = <src/prompts/SOUL.md 原文>

## 输出
先输出 <think>...</think>，然后输出给用户看的正文。
<think> 内只写行动思考，不超过 120 字；不自我介绍、不讨论你的能力、不重复问题、不写心理活动。

参数：reasoning_effort=high（none_none 亦可，CoT 会再长一些）
```

剩余失败模式：
1. `meta_process`：CoT 中频繁出现“让我/我需要/首先/接下来”等过程词，且模型不因提示词禁止而消失。
2. `repeat_task`：CoT 开头大量复述任务，接近任务原文或关键词重复 >=3 次。
3. `json_code` 偶发（CoT 内出现 JSON/代码块）。
4. 长度重尾：p90 仍约 2000 字符，严格 clean_rate 为 0。

---

## 6. 对 cipher 思维引擎的建议（针对最终目标）

目标：让思维引擎降低对“提示词约束输出规范”的依赖。

1. **不要指望提示词完成长度/格式控制。** 本实验证明 M3 对“<=120 字 / 禁止过程词 / 禁止复述”等约束的服从率不足 1/9。继续加重提示词约束只会增加 token，不增加服从。
2. **把规范从提示词移到代码层。** 用极简提示词（第 5 节），然后：
   - 取第一个 `<think>...</think>` 作为思考引擎 think；
   - `</think>` 之后正文作为 say/交付正文；
   - 对 CoT 做代码级压缩或只保留前 N 字；
   - 若需要 JSON 协议，继续走现有恢复链，不依赖模型裸 JSON。
3. **参数选择**：
   - 需要正文稳定：`reasoning_effort=high`（body 9/9，CoT p50 717）；
   - 想要更低延迟/更少 token：`none_none`（CoT 更长但简单任务可能更短，需另行按模式测量）；
   - 不使用 `reasoning_effort=low/medium`（历史实验显示 low 会跑满 max_tokens、medium 协议合法率下降）。
4. **如果必须达到 <=250 字符的干净 CoT**：接受任务书结论 B，转入“CoT 作为原始反思，压缩行动意图单独生成”的两段式路线。

---

## 7. 产物清单

- 任务书 6 变体实验：
  - 原始响应：`/tmp/m3_cot_isolation/raw/*.json`（81 个）
  - 汇总：`/tmp/m3_cot_isolation/summary.json`、`/tmp/m3_cot_isolation/summary.csv`
- SOUL.md 扩展实验：
  - 原始响应：`/tmp/m3_cot_isolation_soul/raw/*.json`（63 个）
  - 汇总：`/tmp/m3_cot_isolation_soul/summary.json`、`/tmp/m3_cot_isolation_soul/summary.csv`
- 实验脚本：
  - `/tmp/m3_cot_isolation_runner.py`
  - `/tmp/m3_soul_experiment_runner.py`
- 本报告：`src/docs/m3_cot_prompt_isolation_report.md`

> 备注：任务书固定体要求 `max_completion_tokens=8192`；本会话为快速出结果改为 1024。因此部分样本以 `finish_reason=length` 结束。该上限只会让“短 CoT”更容易通过，所有变体仍全部失败，故结论 B 不受放宽上限影响（若放宽至 8192，CoT 只会更长）。
