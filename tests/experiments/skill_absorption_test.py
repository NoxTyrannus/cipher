#!/usr/bin/env python3
"""D6 自扩展最终验证：真实模型把 SKILL.md 转换为 capability.import JSON，
再用 capability_probe 导入并执行该能力，断言产物内容正确。

用法:
  MINIMAX_API_KEY=... python3 tests/experiments/skill_absorption_test.py
"""
import json
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
SKILL = ROOT / "data" / "seed" / "skills" / "count_errors_skill.md"
PROBE = ROOT / "target" / "debug" / "examples" / "capability_probe"

SYSTEM = """你是 cipher 的自扩展转换器。把 SKILL.md 转换为 capability.import 的 arguments JSON。
必须且只能输出如下 JSON 对象本身（不要包裹 capability.import，不要 Markdown）：
{
  "grant_to_agent": "agent",
  "composite_capabilities": [
    {
      "id": "user.skill.<skill名>",
      "name": "<技能名>",
      "description": "<描述>",
      "schema_in": {"type":"object","properties":{...},"required":[...]},
      "schema_out": {"type":"object","properties":{"capability_id":{"type":"string"}}},
      "dag": [
        {"id":"step1","base_capability":"shell.exec","args":{"command":"grep -R ERROR $input.path | wc -l"},"depends_on":[]},
        {"id":"step2","base_capability":"file.write","args":{"path":"$input.output","content":"${step1}.stdout"},"depends_on":["step1"]}
      ]
    }
  ]
}
规则：
- 只允许 builtin:file.read/write/list/delete/move/chunk_read、text.grep、shell.exec、powershell.exec、code.exec、db.*、memory.*。
- command 中用 $input.path 引用输入字段；用 ${step_id}.stdout 引用上一步输出。
- schema_in 必须覆盖 skill inputs。
- 确保 JSON 完整，不要截断。"""


def call_model(key, user):
    payload = {
        "model": "MiniMax-M3",
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": user},
        ],
        "stream": False,
        "thinking": {"type": "disabled"},
    }
    req = urllib.request.Request(
        "https://api.minimaxi.com/v1/chat/completions",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=180) as resp:
        body = json.loads(resp.read().decode())
    return body


def main():
    key = os.environ.get("MINIMAX_API_KEY", "")
    if not key:
        raise SystemExit("set MINIMAX_API_KEY")
    body = call_model(key, "SKILL.md:\n" + SKILL.read_text(encoding="utf-8"))
    content = body["choices"][0]["message"]["content"]
    if "<think" in content:
        content = content.split("</think>")[-1]
    m = re.search(r"```(?:json)?\s*(.*?)\s*```", content, re.S)
    if m:
        content = m.group(1)
    import_args = json.loads(content)
    assert import_args.get("grant_to_agent") == "agent"

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="cipher-skill-absorb-"))
    ws = tmp / "ws"
    logs = ws / "logs"
    logs.mkdir(parents=True)
    (logs / "a.log").write_text("ERROR\nERROR\n")
    (logs / "b.log").write_text("ERROR\n")
    import_file = tmp / "import.json"
    import_file.write_text(json.dumps(import_args, ensure_ascii=False, indent=2))
    proc = None
    result_text = ""
    try:
        proc = subprocess.run(
            [
                str(PROBE),
                "--data-dir",
                str(tmp / "data"),
                "--workspace",
                str(ws),
                "--import-file",
                str(import_file),
                "--capability",
                "user.skill.count-errors",
                "--arguments",
                json.dumps({"path": "logs", "output": "result.txt"}, ensure_ascii=False),
            ],
            text=True,
            capture_output=True,
            timeout=180,
        )
    finally:
        if proc is not None:
            print(proc.stdout)
            print(proc.stderr)
        result_text = (ws / "result.txt").read_text().strip() if (ws / "result.txt").exists() else ""
        print("total_tokens", body.get("usage", {}).get("total_tokens"))
        print("result", result_text)
        shutil.rmtree(tmp, ignore_errors=True)
    assert proc is not None and proc.returncode == 0
    assert result_text == "3"


if __name__ == "__main__":
    main()
