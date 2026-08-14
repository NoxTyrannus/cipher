#!/usr/bin/env python3
"""Cipher v0.2.6 PTY 测试专用 mock LLM 服务器（OpenAI 兼容）。

- POST /chat/completions：按请求消息内容路由。
  - 思考引擎请求（流式 stream=true）：
      * 含 "[融合思考反思]"          -> kind=reflect
      * 含 "[memory echo]"/"[xxx echo]" -> kind=echo
      * 含 mode_unni/mode_keep/mode_loop 系统提示（无上述标记）-> kind=user
  - 平台请求（非流式 stream=false）：统一返回垃圾内容（各中台均有 fallback，
    仍会发送完成触发事件），用于确定性驱动触发链。
- 思考引擎响应按场景脚本（JSON 数组）顺序消费；脚本耗尽后走默认 say-only。

用法：python3 mock_llm.py <port> <scenario.json> <request.log>
  - port=0 表示自动分配（输出到 stdout 一行 `PORT <n>`）。
  - scenario.json：见 README（场景脚本格式）。
  - request.log：每请求一行 JSON 记录（供断言）。

返回格式：
  - 流式：SSE  data: {"choices":[{"delta":{"content":"..."}}]}   ... data: [DONE]
  - 非流式：{"choices":[{"message":{"content":"..."}}]}
"""

import json
import sys
import threading
import time
from http.client import HTTPConnection, HTTPSConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

MODE_MARKERS = {
    "unni": "Mode: UNNI",
    "keep": "Mode: KEEP",
    "loop": "Mode: LOOP",
}


class UpstreamRelay:
    """透明代理：把应用的请求转发到上游真实 LLM（如 minimax），并把响应原样流式转发回应用。

    仅用于真实 API 冒烟（--real）：mock 保持请求日志/分类不变，让原有结构化断言
    （次数、input 内容）继续工作，同时响应来自真实模型。
    """

    def __init__(self, base_url):
        p = urlparse(base_url)
        self.scheme = p.scheme
        self.host = p.netloc
        self.prefix = p.path.rstrip("/")

    def relay(self, path, headers, body):
        conn_cls = HTTPSConnection if self.scheme == "https" else HTTPConnection
        conn = conn_cls(self.host, timeout=240)
        try:
            fwd_path = (self.prefix + path) if self.prefix else path
            fwd_headers = {}
            for k in ("Authorization", "Content-Type"):
                v = headers.get(k)
                if v:
                    fwd_headers[k] = v
            conn.request("POST", fwd_path, body=body, headers=fwd_headers)
            resp = conn.getresponse()
            status = resp.status
            resp_headers = [
                (k, v)
                for k, v in resp.getheaders()
                if k.lower() not in ("transfer-encoding", "connection", "content-length")
            ]

            def reader():
                while True:
                    chunk = resp.read(65536)
                    if not chunk:
                        break
                    yield chunk

            return status, resp_headers, reader()
        except Exception as e:  # noqa: BLE001 —— 代理失败要回给应用可读错误
            return 502, [("Content-Type", "application/json")], iter(
                [
                    json.dumps(
                        {"error": {"message": f"proxy upstream failed: {e}"}}
                    ).encode("utf-8")
                ]
            )


class MockState:
    def __init__(self, scenario_path, log_path):
        if scenario_path and scenario_path != "-":
            with open(scenario_path, "r", encoding="utf-8") as f:
                raw = json.load(f)
        else:
            raw = []
        # 支持 {"default": {...}, "script": [...]} 或直接数组
        if isinstance(raw, dict):
            self.scenario = raw.get("script", [])
            self.default = raw.get("default")
        else:
            self.scenario = raw
            self.default = None
        self.log_path = log_path
        self.upstream = None
        self.cursor = 0  # 当前脚本条目
        self.used_in_entry = 0  # 当前条目已消费次数
        self.lock = threading.Lock()
        self.latency_ms = 0
        if isinstance(raw, dict):
            self.latency_ms = int(raw.get("latency_ms", 0) or 0)
        self.requests = 0
        self.thinking = 0

    def log(self, record):
        if not self.log_path:
            return
        with open(self.log_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    def match(self, entry, kind, mode, full_text):
        m = entry.get("match", {})
        if "kind" in m and m["kind"] != kind:
            return False
        if "mode" in m and m["mode"] != mode:
            return False
        if "content_contains" in m and m["content_contains"] not in full_text:
            return False
        return True

    def next_response(self, kind, mode, full_text):
        """返回 (kind_label, respond_dict or None)。None 表示用默认。

        respond 可为 dict 或 list：list 按匹配次数顺序取用（用于"先失败 N 次再成功"），
        超出列表长度后取最后一个。
        """
        with self.lock:
            while self.cursor < len(self.scenario):
                entry = self.scenario[self.cursor]
                if self.match(entry, kind, mode, full_text):
                    self.used_in_entry += 1
                    count = entry.get("count", 1)
                    used = self.used_in_entry
                    if used >= count:
                        self.cursor += 1
                        self.used_in_entry = 0
                    respond = entry.get("respond")
                    if isinstance(respond, list):
                        respond = respond[min(used - 1, len(respond) - 1)]
                    return entry.get("kind_label", "scripted"), respond
                self.cursor += 1
                self.used_in_entry = 0
            if self.default is not None:
                return "default", self.default
            return "default", None


def classify(messages, stream):
    """返回 (kind, mode)。

    思考引擎请求恒为流式（spawn_streaming → call_stream），平台请求恒为非流式。
    子类型（user/echo/reflect2/final）按当前输入文本内容判定：
      - 输入不以"既定目标:"开头 → 用户原始输入（user）
      - 含"记忆中台已整理上一轮" → 融合思考最终实例（final）—— 该标记只在
        memory_complete 阶段的摘要里出现（含缺段降级的 final，其可能没有"实例2 反思"）
      - 含"实例2 反思" → 融合思考最终实例（final，保底）
      - 含"实例1 反思" → 融合思考第2反思实例（reflect2）
      - 其余以"既定目标:"开头 → echo（含融合思考第1反思实例，脚本用 content_contains 区分）
    """
    text = "\n".join(
        m.get("content", "") or "" for m in messages if isinstance(m, dict)
    )
    mode = "unknown"
    for name, marker in MODE_MARKERS.items():
        if marker in text:
            mode = name
            break
    if not stream:
        return "platform", mode
    # 当前输入 = 最后一条消息（assembler 将当前输入 push 为最后一个 User 消息）
    last = ""
    for m in messages:
        if isinstance(m, dict) and m.get("role") == "user" and m.get("content"):
            last = m["content"]
    if "[融合思考反思]" in text:
        return "reflect", mode
    # final 的判定：记忆中台块 + 反思段同时出现（缺段降级的 final 也有 [实例1 反思]；
    # 纯记忆触发的 echo 虽有记忆中台块但无反思段，仍归 echo）
    if "记忆中台已整理上一轮" in last and (
        "[实例1 反思" in last or "[实例2 反思" in last
    ):
        return "final", mode
    if "实例2 反思" in last:
        return "final", mode
    if "实例1 反思" in last:
        return "reflect2", mode
    if last.startswith("既定目标"):
        return "echo", mode
    return "user", mode


def build_sse(content):
    chunks = []
    # 拆成多个 SSE 块以贴近真实流式（也验证增量拼接）
    step = 40
    for i in range(0, len(content), step):
        piece = content[i : i + step]
        chunks.append(
            "data: " + json.dumps({"choices": [{"delta": {"content": piece}}]}) + "\n\n"
        )
    chunks.append("data: [DONE]\n\n")
    return "".join(chunks).encode("utf-8")


def build_json(content):
    return json.dumps(
        {
            "id": "mock-1",
            "object": "chat.completion",
            "model": "mock",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": len(content), "total_tokens": 10 + len(content)},
        }
    ).encode("utf-8")


class Handler(BaseHTTPRequestHandler):
    state = None  # 类变量，由 serve 设置

    def log_message(self, fmt, *args):
        pass  # 静默

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b""
        req_id = time.time_ns()
        try:
            body = json.loads(raw.decode("utf-8")) if raw else {}
        except Exception as e:
            body = {"parse_error": str(e)}
        messages = body.get("messages", [])
        stream = bool(body.get("stream", False))
        kind, mode = classify(messages, stream)

        # 全量文本 + 当前输入（供脚本 content_contains 匹配与断言日志）
        text = "\n".join(
            (m.get("content", "") or "")[:4000] for m in messages if isinstance(m, dict)
        )
        last = ""
        for m in messages:
            if isinstance(m, dict) and m.get("role") == "user" and m.get("content"):
                last = m["content"]

        label = "platform"
        content = "not a valid structured output"
        status = 200

        if self.state.upstream is not None:
            # 真实 API 冒烟：跳过脚本，直接代理到上游（先记录请求，再转发）
            self.state.log(
                {
                    "req_id": req_id,
                    "kind": kind,
                    "mode": mode,
                    "label": "proxy",
                    "stream": stream,
                    "status": 200,
                    "content": "(proxy)",
                    "input": last[:4000],
                    "messages_snippet": text[:4000],
                }
            )
            # MiniMax-M3 是推理模型：代理注入 thinking: disabled，
            # 让输出严格遵循应用的 JSON 契约（think/say），不产生推理前缀文本。
            fwd_body = raw
            try:
                bj = json.loads(raw.decode("utf-8"))
                if isinstance(bj, dict):
                    bj.setdefault("thinking", {"type": "disabled"})
                    fwd_body = json.dumps(bj).encode("utf-8")
            except Exception:  # noqa: BLE001 —— 非 JSON 请求体按原样转发
                pass
            status, resp_headers, chunks = self.state.upstream.relay(
                self.path, self.headers, fwd_body
            )
            relayed = b""
            buf = []
            for c in chunks:
                relayed += c
                buf.append(c)
                if len(relayed) > 2000:
                    break
            with open(self.state.log_path, "a", encoding="utf-8") as f:
                f.write(
                    json.dumps(
                        {
                            "req_id": req_id,
                            "kind": kind,
                            "mode": mode,
                            "label": "proxy-resp",
                            "stream": stream,
                            "status": status,
                            "upstream": relayed[:2000].decode("utf-8", "replace"),
                        },
                        ensure_ascii=False,
                    )
                    + "\n"
                )
            self.send_response(status)
            for k, v in resp_headers:
                self.send_header(k, v)
            self.end_headers()
            try:
                for chunk in buf:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                for chunk in chunks:
                    self.wfile.write(chunk)
                    self.wfile.flush()
            except BrokenPipeError:
                pass
            return

        if kind == "platform":
            # 平台调用：返回垃圾（各中台 fallback 后仍会触发完成事件）
            respond = None
            # 若脚本想指定平台响应，也可支持
            if self.state.scenario:
                for entry in self.state.scenario:
                    if entry.get("match", {}).get("kind") == "platform":
                        respond = entry.get("respond")
                        break
            if respond:
                if respond.get("type") == "http_error":
                    status = int(respond.get("status", 500))
                    content = respond.get("content", "error")
                else:
                    content = respond.get("content", content)
            label = "platform"
        else:
            # 思考引擎
            self.state.thinking += 1
            label, respond = self.state.next_response(kind, mode, text)
            if respond is None:
                # 默认：say-only，结束当前轮
                content = json.dumps({"think": None, "say": "（默认）本轮已完成。"}, ensure_ascii=False)
            elif respond.get("type") == "http_error":
                status = int(respond.get("status", 500))
                content = respond.get("content", "error")
            elif respond.get("type") == "invalid":
                content = respond.get("content", "this is not valid agent output")
            elif respond.get("type") == "output":
                out = {
                    "think": respond.get("think"),
                    "say": respond.get("say"),
                }
                content = json.dumps(out, ensure_ascii=False)

        # 记录请求（含关键内容摘要，供断言）
        self.state.log(
            {
                "req_id": req_id,
                "kind": kind,
                "mode": mode,
                "label": label,
                "stream": stream,
                "status": status,
                "content": content[:400],
                "input": last[:4000],
                "messages_snippet": text[:4000],
            }
        )

        if self.state.latency_ms:
            time.sleep(self.state.latency_ms / 1000.0)

        if stream:
            payload = build_sse(content)
            ctype = "text/event-stream"
        else:
            payload = build_json(content)
            ctype = "application/json"

        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except BrokenPipeError:
            pass


def serve(port, scenario_path, log_path, upstream_url=None):
    Handler.state = MockState(scenario_path, log_path)
    if upstream_url:
        Handler.state.upstream = UpstreamRelay(upstream_url)
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    actual_port = server.server_address[1]
    print(f"PORT {actual_port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    scenario = sys.argv[2] if len(sys.argv) > 2 else None
    log_path = sys.argv[3] if len(sys.argv) > 3 else None
    upstream = sys.argv[4] if len(sys.argv) > 4 else None
    serve(port, scenario, log_path, upstream)
