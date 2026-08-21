# MiniMax-M3 CoT 参数矩阵探测报告（第 2 轮）

- 接口：POST https://api.minimaxi.com/v1/chat/completions
- 模型：MiniMax-M3
- 固定：temperature=1.0, top_p=0.95, max_completion_tokens=8192
- 组合：6 种参数组合 × 3 prompt（LOOP-Glanstia / UNNI / KEEP）× 3 rep = 54 次非流式
- 追加：none_none 与 reff_high 的 stream=true（stream_options.include_usage=true）各 3 prompt × 3 rep = 18 次
- 另外有 18 次无 usage 的流式复测（旧批次）用于内容/合法性佐证
- 原始响应：/tmp/m3_cot_matrix/raw/ 与 /tmp/m3_cot_matrix_stream_usage/raw/
- 汇总：/tmp/m3_cot_matrix/consolidated.csv

## 主要结果（3×3 非流式）

| 组合 | 恢复后协议合法率 | strict 合法率 | content 长度中位(范围) | 嵌入<think>中位(范围) | completion_tokens 中位(范围) | reasoning_tokens 中位(范围) | 耗时中位(范围) |
|---|---|---|---|---|---|---|---|
| disabled + json_object（基线） | 9/9 | 8/9 | 356 (69-803) | 0 | 150 (26-476) | 0 | 3.6s (1.6-6.1) |
| disabled 无 response_format | 9/9 | 7/9 | 876 (163-1147) | 0 | 400 (82-672) | 0 | 5.5s (1.8-11.7) |
| 无 thinking 无 response_format | 9/9 | 0/9 | 3373 (518-17657) | 2208 (414-15862) | 1030 (165-4885) | 539 (0-4274) | 11.4s (2.4-42.0) |
| reasoning_effort=high | 9/9 | 0/9 | 2367 (910-6142) | 2112 (758-4694) | 638 (210-1842) | 176 (0-1312) | 7.8s (2.5-17.8) |
| reasoning_effort=medium | 7/9 | 0/9 | 2110 (821-5490) | 1469 (615-4695) | 887 (197-1606) | 630 (0-1132) | 10.7s (2.6-15.2) |
| reasoning_effort=low | 8/9 | 0/9 | 5603 (1324-31841) | 4691 (1180-31824) | 1874 (320-8192) | 894 (0-8192) | 16.0s (2.8-70.5) |

## 分 prompt 细节（3×3 非流式）

| 组合 | prompt | 恢复合法 | <think>长度中位(min-max) | completion_tokens 中位 | reasoning_tokens 中位 | 耗时中位 |
|---|---|---|---|---|---|---|
| none_none | LOOP | 3/3 | 2666 (1159-15862) | 1030 | 594 | 12.7s |
| none_none | UNNI | 3/3 | 697 (414-1090) | 233 | 0 | 3.3s |
| none_none | KEEP | 3/3 | 2503 (2208-3232) | 1369 | 650 | 15.1s |
| reff_high | LOOP | 3/3 | 1818 (811-4533) | 638 | 416 | 6.5s |
| reff_high | UNNI | 3/3 | 2112 (758-2183) | 584 | 0 | 6.2s |
| reff_high | KEEP | 3/3 | 2624 (2098-4694) | 1111 | 0 | 17.0s |
| reff_medium | LOOP | 1/3 | 1469 (664-4695) | 887 | 630 | 10.7s |
| reff_medium | UNNI | 3/3 | 1103 (615-1274) | 362 | 0 | 3.1s |
| reff_medium | KEEP | 3/3 | 2700 (2292-3754) | 1319 | 826 | 13.6s |
| reff_low | LOOP | 2/3 | 10870 (4048-31824) | 2976 | 2335 | 35.2s |
| reff_low | UNNI | 3/3 | 1181 (1180-1260) | 380 | 0 | 3.3s |
| reff_low | KEEP | 3/3 | 5929 (4691-9610) | 2032 | 1470 | 21.0s |

## stream=true 复测（include_usage=true，3×3）

| 组合 | prompt | 恢复合法 | <think>长度中位 | completion_tokens 中位 | reasoning_tokens 中位 | 耗时中位 |
|---|---|---|---|---|---|---|
| none_none | LOOP | 3/3 | 5283 | 1371 | 1144 | 14.3s |
| none_none | UNNI | 3/3 | 1057 | 444 | 360 | 4.1s |
| none_none | KEEP | 3/3 | 2133 | 1016 | 553 | 10.8s |
| reff_high | LOOP | 3/3 | 1799 | 1069 | 636 | 10.4s |
| reff_high | UNNI | 3/3 | 960 | 335 | 0 | 3.5s |
| reff_high | KEEP | 3/3 | 4896 | 1847 | 0 | 21.2s |

注：另一批 18 次流式复测（无 usage）中 none_none 恢复合法 7/9、reff_high 8/9；
失败为 LOOP 混入 say、或 KEEP/LOOP 的 JSON 转义错误，不是 HTTP 错误。

## 字段形态观察

- 所有 CoT 组合（none_none / reff_high / medium / low）的 content 一律以 `<think>` 开头；disabled 组合没有。
- 多数响应 message keys 只有 content, role；偶尔出现 `reasoning` 字段（KEEP 较多），
  但该字段与 content 内的 `<think>...</think>` 内容完全一致，属于重复，不可作为稳定独立通道。
- 从未出现 `reasoning_content` / `reasoning_details`（未开启 reasoning_split）。
- reasoning_tokens 报告不稳定：多个含长 <think> 的响应返回 0 或缺失，不能用于判断 CoT 长度。
- 所有成功样本均为 1 个闭合 `<think>...</think>` 块；唯一 length 截断样本也闭合了 think，
  但 think 后没有任何 JSON（协议输出失败）。

## 结论

1. 稳定解锁更长 CoT：`none_none`（不传 thinking，不传 response_format）与 `reasoning_effort=high`。
   - none_none：主批次与追加非流式共 18/18 通过 cipher 恢复解析；中位 CoT 约 2.1k-3.7k 字符（复杂任务），
     波动较大（cv≈1.18），简单 UNNI 自然较短。
   - reff_high：主批次 9/9，追加非流式 17/18；长度分布更集中（cv≈0.77），
     但简单任务也强制较长 think，KEEP/LOOP 偶发 JSON 转义失败。
   - 不推荐 medium / low：medium LOOP 合法性 1/3；low 会跑到 max_completion_tokens=8192、
     70s、且可能完全不给 JSON。
2. CoT 不是稳定独立字段，而是 content 里 `<think>...</think>`；偶发 `message.reasoning` 只是副本。
3. 可以低成本提取：先取第一个 `<think>...</think>` 作为思考引擎 think，尾部交给 cipher 现有
   strip_think_blocks/extract_first_json_object 恢复链解析协议 JSON。主批次 none_none/reff_high
   尾部解析全部可恢复；流式下需先缓冲到 `</think>` 再解析 JSON。
4. 建议：思考引擎在 LOOP/KEEP 切到 `none_none`（去掉 thinking disabled 与 response_format）；
   UNNI 如需最低延迟可保留 disabled，否则 none_none 的额外成本也较小（约 +1-2s、数百 token）。
   `reasoning_effort=high` 可作为候选，但属于未在 OpenAI SDK 文档列出的扩展参数，且可靠性不优于 none_none。
