#!/usr/bin/env python3
"""D12 经验/偏好查证实验：A（输入前准备）/ B（agent 自查）/ 混合。

用法:
  MINIMAX_API_KEY=... python3 tests/experiments/memory_verification_experiment.py \
      --runs 2 --out /tmp/memory_verification_results.jsonl
"""
import argparse
import json
import os
import re
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
API_URL = "https://api.minimaxi.com/v1/chat/completions"
MODEL = "MiniMax-M3"

SCENARIOS = [
    {
        "id": "exp-retry-shell",
        "type": "experience",
        "prompt_file": "memory_experience.md",
        "retired": [
            {"focus": "shell路径错误修复", "source_refs": ["turn-shell-1"]},
            {"focus": "grep统计失败后改用find", "source_refs": ["turn-shell-2"]},
        ],
        "evidence": {
            "turn-shell-1": "用户要求统计 logs 下 ERROR 数。agent 先用 grep logs/*.log 失败，因为路径应为 logs/。改用 find logs -name '*.log' -exec grep -c ERROR {} + 后成功。",
            "turn-shell-2": "用户要求统计 CSV 销售额。agent 先 grep sales.csv 失败，因为文件在 data/sales.csv。改用 find data -name sales.csv 后定位成功。",
        },
        "gold": [
            ["grep", "路径", "find"],
            ["失败", "定位", "重试"],
        ],
    },
    {
        "id": "exp-write-atomic",
        "type": "experience",
        "prompt_file": "memory_experience.md",
        "retired": [
            {"focus": "大文件写入超限", "source_refs": ["turn-write-1"]},
        ],
        "evidence": {
            "turn-write-1": "用户要求生成 20MB 报告。agent 一次性 file.write 失败，因为超过 1MB 预算。后来用 shell 按行分块追加写入，成功生成报告。",
        },
        "gold": [["大文件", "分块"], ["写入", "预算"]],
    },
    {
        "id": "exp-schema-first",
        "type": "experience",
        "prompt_file": "memory_experience.md",
        "retired": [
            {"focus": "能力schema缺字段", "source_refs": ["turn-schema-1"]},
        ],
        "evidence": {
            "turn-schema-1": "用户要求写文件。agent 调 file.write 时只给了 path，缺少 content，schema 校验失败。补 content 后一次成功。",
        },
        "gold": [["schema", "必填"], ["参数", "失败"]],
    },
    {
        "id": "exp-composite-reuse",
        "type": "experience",
        "prompt_file": "memory_experience.md",
        "retired": [
            {"focus": "copy组合能力", "source_refs": ["turn-copy-1"]},
        ],
        "evidence": {
            "turn-copy-1": "用户需要复制文件。agent 先读再写分两步完成。之后把 read+write 组合成 file.copy 分子能力，后续一次调用完成。",
        },
        "gold": [["组合", "分子能力"], ["读", "写", "复制"]],
    },
    {
        "id": "pref-language-zh",
        "type": "preference",
        "prompt_file": "memory_preference.md",
        "retired": [
            {"focus": "语言偏好中文", "source_refs": ["turn-lang-1"]},
        ],
        "evidence": {
            "turn-lang-1": "用户说：以后都用中文回答，不用英文。agent 之后切换到中文。",
        },
        "gold": [["中文", "语言"], ["回答"]],
    },
    {
        "id": "pref-tool-shell",
        "type": "preference",
        "prompt_file": "memory_preference.md",
        "retired": [
            {"focus": "偏好shell而非python", "source_refs": ["turn-tool-1"]},
        ],
        "evidence": {
            "turn-tool-1": "用户说：统计类任务优先用 shell，不要一上来写 python 脚本。",
        },
        "gold": [["shell", "统计"], ["python", "优先"]],
    },
    {
        "id": "pref-correction",
        "type": "preference",
        "prompt_file": "memory_preference.md",
        "retired": [
            {"focus": "用户更正报告格式", "source_refs": ["turn-fmt-1"]},
        ],
        "evidence": {
            "turn-fmt-1": "用户说：不对，报告应该用 Markdown 表格，不要用 JSON 原文。",
        },
        "gold": [["Markdown", "表格"], ["JSON", "不要"]],
    },
    {
        "id": "pref-default-path",
        "type": "preference",
        "prompt_file": "memory_preference.md",
        "retired": [
            {"focus": "默认输出到reports目录", "source_refs": ["turn-path-1"]},
        ],
        "evidence": {
            "turn-path-1": "用户说：以后所有报告默认写到 reports/ 目录，不用每次都问。",
        },
        "gold": [["reports", "默认"], ["报告"]],
    },
]


def load_prompt(name):
    path = ROOT / "prompts" / name
    if path.exists():
        return path.read_text(encoding="utf-8")
    return ""


def call_llm(messages, key, thinking_disabled=True):
    payload = {"model": MODEL, "messages": messages, "stream": False}
    if thinking_disabled:
        payload["thinking"] = {"type": "disabled"}
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        API_URL,
        data=data,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=180) as resp:
            body = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"HTTP {e.code}: {e.read()[:300]!r}", "latency_ms": (time.time() - started) * 1000}
    except Exception as e:
        return {"ok": False, "error": str(e), "latency_ms": (time.time() - started) * 1000}
    msg = body.get("choices", [{}])[0].get("message", {})
    usage = body.get("usage", {})
    return {
        "ok": True,
        "content": msg.get("content") or "",
        "latency_ms": (time.time() - started) * 1000,
        "prompt_tokens": usage.get("prompt_tokens", 0),
        "completion_tokens": usage.get("completion_tokens", 0),
        "total_tokens": usage.get("total_tokens", 0),
    }


def extract_json_array(content):
    content = content.strip()
    if content.startswith("<think"):
        idx = content.rfind("</think>")
        if idx != -1:
            content = content[idx + len("</think>"):].strip()
    try:
        v = json.loads(content)
        if isinstance(v, list):
            return v
        if isinstance(v, dict) and isinstance(v.get("settle"), dict):
            return v["settle"].get("new_attention", [])
        return []
    except Exception:
        m = re.search(r"```(?:json)?\s*(.*?)\s*```", content, re.S)
        if m:
            try:
                v = json.loads(m.group(1))
                if isinstance(v, list):
                    return v
            except Exception:
                pass
        m = re.search(r"\[.*\]", content, re.S)
        if m:
            try:
                v = json.loads(m.group(0))
                if isinstance(v, list):
                    return v
            except Exception:
                pass
    return []


def parse_tool_call(content):
    try:
        v = json.loads(content)
    except Exception:
        m = re.search(r"\{.*\}", content, re.S)
        if not m:
            return None
        try:
            v = json.loads(m.group(0))
        except Exception:
            return None
    tc = v.get("tool_call") if isinstance(v, dict) else None
    if not isinstance(tc, dict):
        return None
    name = tc.get("name")
    args = tc.get("arguments")
    if isinstance(name, str) and isinstance(args, dict):
        return name, args
    return None


def evidence_for(refs, evidence):
    out = {}
    for ref in refs:
        if ref in evidence:
            out[ref] = evidence[ref]
    return out


def make_retired_text(scenario):
    return "\n".join(
        f"- {r['focus']}" + (f" (source_refs: {', '.join(r['source_refs'])})" if r.get("source_refs") else "")
        for r in scenario["retired"]
    )


def make_evidence_text(scenario):
    parts = []
    for focus in scenario["retired"]:
        refs = focus.get("source_refs", [])
        ev = evidence_for(refs, scenario["evidence"])
        if ev:
            parts.append(f"## Evidence for {focus['focus']}\n" + "\n".join(ev.values()))
    return "\n\n".join(parts) if parts else "No original evidence available."


def final_instructions(scenario):
    if scenario["type"] == "experience":
        return "Extract experience memories. Output a JSON array of objects with title and summary. Only JSON."
    return "Extract preference memories. Output a JSON array of objects with key and value. Only JSON."


def mode_a(scenario, key, run):
    base = load_prompt(scenario["prompt_file"])
    system = (
        f"{base}\n\n## Retired Attention Entries\n{make_retired_text(scenario)}\n\n"
        f"## Original Evidence\n{make_evidence_text(scenario)}\n\n## Task\n{final_instructions(scenario)}"
    )
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": "Process the retired attention entries. Output ONLY JSON."},
    ]
    return call_llm(messages, key)


def mode_b(scenario, key, run):
    base = load_prompt(scenario["prompt_file"])
    tool_catalog = (
        "- memory.evidence.lookup: 按 source_refs 查原始对话证据；输入 {\"source_refs\":[...]}\n"
        "- memory.list: 列出记忆；输入 {\"memory_type\":\"attention\"}\n"
    )
    system = (
        f"{base}\n\n## Retired Attention Entries\n{make_retired_text(scenario)}\n\n"
        f"## 可用能力\n{tool_catalog}\n\n"
        "## 输出协议\n"
        "你可以先调用工具查证，再输出最终 JSON 数组。\n"
        "调用工具: {\"tool_call\":{\"name\":\"memory.evidence.lookup\",\"arguments\":{\"source_refs\":[...]}}}\n"
        f"最终回答: 只输出 JSON 数组（{final_instructions(scenario)}）。\n"
    )
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": "开始查证并提取记忆。"},
    ]
    return run_tool_protocol(messages, scenario, key)


def mode_hybrid(scenario, key, run):
    base = load_prompt(scenario["prompt_file"])
    tool_catalog = (
        "- memory.evidence.lookup: 按 source_refs 查原始对话证据；输入 {\"source_refs\":[...]}\n"
    )
    system = (
        f"{base}\n\n## Retired Attention Entries\n{make_retired_text(scenario)}\n\n"
        f"## Original Evidence (preloaded)\n{make_evidence_text(scenario)}\n\n"
        f"## 可用能力（仍可自查）\n{tool_catalog}\n\n"
        "证据不足时可以调用 memory.evidence.lookup 继续查证。\n"
        f"最终回答: 只输出 JSON 数组（{final_instructions(scenario)}）。"
    )
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": "开始查证并提取记忆。"},
    ]
    return run_tool_protocol(messages, scenario, key)


def run_tool_protocol(messages, scenario, key):
    calls = []
    for _ in range(6):
        r = call_llm(messages, key)
        if not r["ok"]:
            return r
        calls.append(r)
        content = r["content"]
        arr = extract_json_array(content)
        if arr:
            r["content"] = json.dumps(arr, ensure_ascii=False)
            r["tool_calls"] = calls[:-1]
            return r
        tc = parse_tool_call(content)
        if tc:
            name, args = tc
            messages.append({"role": "assistant", "content": content})
            if name == "memory.evidence.lookup":
                refs = args.get("source_refs", [])
                ev = evidence_for(refs, scenario["evidence"])
                result = {"items": [{"thought_id": k, "evidence": v} for k, v in ev.items()], "count": len(ev)}
                messages.append({"role": "user", "content": f"工具结果: {json.dumps(result, ensure_ascii=False)}"})
            else:
                messages.append({"role": "user", "content": "工具不存在或参数错误，继续或输出最终 JSON。"})
            continue
        messages.append({"role": "assistant", "content": content})
        messages.append({"role": "user", "content": "格式无法解析。只输出 JSON 数组，或先输出一个 tool_call JSON。"})
    r = calls[-1] if calls else call_llm(messages, key)
    r["tool_calls"] = calls[:-1]
    return r


def entry_text(entry, typ):
    if typ == "experience":
        return f"{entry.get('title','')} {entry.get('summary','')}"
    return f"{entry.get('key','')} {entry.get('value','')}"


def evaluate(scenario, result):
    if not result.get("ok"):
        return {"parse_ok": False, "recall": 0.0, "precision": 0.0, "entries": []}
    arr = extract_json_array(result.get("content", ""))
    texts = [entry_text(e, scenario["type"]).lower() for e in arr if isinstance(e, dict)]
    gold = scenario["gold"]
    hit = 0
    for fact in gold:
        if any(all(k.lower() in t for k in fact) for t in texts):
            hit += 1
    # precision：有多少输出条目至少命中一条 gold 事实（宽松判定）
    precise = 0
    for t in texts:
        if any(any(k.lower() in t for k in fact) for fact in gold):
            precise += 1
    recall = hit / len(gold) if gold else 1.0
    precision = precise / len(texts) if texts else 0.0
    return {
        "parse_ok": True,
        "recall": recall,
        "precision": precision,
        "entries": arr,
        "entry_count": len(arr),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--key", default=os.environ.get("MINIMAX_API_KEY", ""))
    ap.add_argument("--runs", type=int, default=2)
    ap.add_argument("--out", default="/tmp/memory_verification_results.jsonl")
    args = ap.parse_args()
    if not args.key:
        raise SystemExit("set MINIMAX_API_KEY or pass --key")
    modes = {"A": mode_a, "B": mode_b, "hybrid": mode_hybrid}
    with open(args.out, "w", encoding="utf-8") as out:
        for scenario in SCENARIOS:
            for run in range(1, args.runs + 1):
                for mode_name, mode_fn in modes.items():
                    r = mode_fn(scenario, args.key, run)
                    ev = evaluate(scenario, r)
                    rec = {
                        "scenario": scenario["id"],
                        "type": scenario["type"],
                        "run": run,
                        "mode": mode_name,
                        "ok": r.get("ok"),
                        "latency_ms": r.get("latency_ms"),
                        "total_tokens": r.get("total_tokens", 0),
                        "prompt_tokens": r.get("prompt_tokens", 0),
                        "completion_tokens": r.get("completion_tokens", 0),
                        "tool_calls": len(r.get("tool_calls", [])),
                        "error": r.get("error"),
                        **ev,
                    }
                    out.write(json.dumps(rec, ensure_ascii=False) + "\n")
                    out.flush()
                    print(json.dumps(rec, ensure_ascii=False))
    print("wrote", args.out)


if __name__ == "__main__":
    main()
