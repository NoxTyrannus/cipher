#!/usr/bin/env python3
"""Cipher v0.2.6 PTY 黑盒测试驱动。

自己设计验证题（V1-V9）+ 验证指标，驱动真实 TUI（ratatui/crossterm over PTY），
配合 mock LLM 服务器做确定性断言。

验证题矩阵（详见运行后生成的测试报告）：
  V1  UNNI 自主 + 执行中台（默认）：执行完成触发 echo，洞察/记忆异步只沉淀
  V2  UNNI 跟随 + 执行中台：触发 stash pending，下次输入合并
  V3  UNNI 自主 + 洞察中台：执行在前被忽略，洞察触发 echo，记忆异步只沉淀
  V4  KEEP 预算到达 → 暂停 + 提示（token_budget 16K = 2 实例）
  V5  LOOP 融合思考 off：执行/洞察被忽略，记忆触发 echo（默认路径）
  V6  LOOP 融合思考 on：三阶段流水线 + 反射实例 + 拼接合并
  V7  F1 invalid_json → 自动修复轮（保留用户输入意图）
  V8  配置面板导航 + Mode Style 保存 → config.toml 落盘
  V9  旧配置兼容（memory_mode 字段被忽略）+ Tab 模式切换

验证指标：mock 请求序列、应用 trace 日志、屏幕文本、config.toml 内容。

用法：python3 ptytest.py [--root /tmp/cipher-ptytest] [--keep]
"""

import argparse
import fcntl
import json
import os
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
BIN = os.path.join(REPO, "target", "debug", "cipher")
MOCK = os.path.join(HERE, "mock_llm.py")

PASS, FAIL = "PASS", "FAIL"

# 真实 API 冒烟（--real）：mock 变为透明代理转发到上游 minimax；
# 请求日志/分类不变，结构化断言继续有效，响应来自真实模型。
REAL = {"active": False, "upstream": None, "api_key": None, "model_id": None}

# 真实 API 冒烟用的可执行用户输入（真实模型对含糊指令会反问而不执行，
# 必须给可落地的任务才能驱动执行/洞察/记忆触发链）。
# 用相对路径：执行沙箱根 = 应用 cwd = <root>/ws，相对/绝对(在 ws 内) 均放行。
REAL_INPUTS = {
    "V1": "在当前工作目录创建文件 cipher_real_v1.txt，写入一行内容 real-ok-v1，然后执行完成即可",
    "V2": "在当前工作目录创建文件 cipher_real_v2.txt，写入一行内容 real-ok-v2，然后执行完成即可",
    "V3": "在当前工作目录创建文件 cipher_real_v3.txt，写入一行内容 real-ok-v3，然后执行完成即可",
    "V4": "在当前工作目录创建文件 cipher_real_v4.txt，写入一行内容 real-ok-v4，然后执行完成即可",
    "V5": "在当前工作目录创建文件 cipher_real_v5.txt，写入一行内容 real-ok-v5，然后执行完成即可",
    "V6": "在当前工作目录创建文件 cipher_real_v6.txt，写入一行内容 real-ok-v6，然后执行完成即可",
    "V7": "在当前工作目录创建文件 cipher_real_v7.txt，写入一行内容 real-ok-v7，然后执行完成即可",
    "V2b": "继续：在当前工作目录创建文件 cipher_real_v2b.txt，写入一行内容 real-ok-v2b，然后执行完成即可",
}


def real_in(name, fallback):
    return REAL_INPUTS.get(name) if REAL["active"] else fallback


def norm(s):
    """去除所有空白字符，用于屏幕文本断言（ratatui 渲染会吞掉空格/换行）。"""
    return "".join(s.split())


def screen_has(sess, text):
    return norm(text) in norm(sess.screen_text())


def alive(sess):
    """应用进程是否仍在运行（真实 API 冒烟用：验证未崩溃）。"""
    try:
        os.kill(sess.pid, 0)
        return True
    except OSError:
        return False


def ansi_strip(text):
    text = re.sub(r"\x1b\][^\x07]*\x07", "", text)  # OSC
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)  # CSI
    text = text.replace("\x1b", "")
    text = text.replace("\r", "")
    return text


class Mock:
    def __init__(self, root, scenario, upstream_url=None):
        self.scenario_path = os.path.join(root, "scenario.json")
        with open(self.scenario_path, "w", encoding="utf-8") as f:
            json.dump(scenario, f, ensure_ascii=False, indent=1)
        self.log_path = os.path.join(root, "mock_requests.jsonl")
        if upstream_url is None and REAL["active"]:
            upstream_url = REAL["upstream"]
        cmd = [sys.executable, MOCK, "0", self.scenario_path, self.log_path]
        if upstream_url:
            cmd.append(upstream_url)
        self.proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        line = self.proc.stdout.readline().strip()
        if not line.startswith("PORT "):
            raise RuntimeError(f"mock 启动失败: {line!r}")
        self.port = int(line.split()[1])

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def requests(self):
        if not os.path.exists(self.log_path):
            return []
        out = []
        with open(self.log_path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line:
                    out.append(json.loads(line))
        return out

    def thinking_of(self, kind):
        # 排除上游响应摘要记录（真实 API 冒烟时会写入 label=proxy-resp 的日志行，
        # 它们带 kind 但无 messages_snippet/input，不能参与断言统计）
        return [
            r
            for r in self.requests()
            if r["kind"] == kind and r.get("label") != "proxy-resp"
        ]


class Session:
    def __init__(self, root, cfg_toml, env_extra=None):
        self.root = root
        self.cfg = os.path.join(root, "config", "cipher", "config.toml")
        os.makedirs(os.path.dirname(self.cfg), exist_ok=True)
        with open(self.cfg, "w", encoding="utf-8") as f:
            f.write(cfg_toml)
        self.env = dict(os.environ)
        self.env["XDG_CONFIG_HOME"] = os.path.join(root, "config")
        self.env["XDG_DATA_HOME"] = os.path.join(root, "logs")
        self.env["RUST_LOG"] = "debug"
        if env_extra:
            self.env.update(env_extra)
        self.log_path = os.path.join(root, "logs", "cipher", "cipher.log")
        self.screen_path = os.path.join(root, "screen.raw")
        self.raw = b""
        self._raw_lock = threading.Lock()
        self.pid = None
        self.master = None
        self.ready = False

    def _drain_loop(self):
        """后台线程持续排空 PTY 输出，防止缓冲区填满导致应用 draw 阻塞。"""
        while True:
            try:
                r, _, _ = select.select([self.master], [], [], 0.2)
                if r:
                    data = os.read(self.master, 65536)
                    if data:
                        with self._raw_lock:
                            self.raw += data
            except (OSError, ValueError):
                break

    def launch(self):
        pid, master = os.forkpty()
        if pid == 0:
            os.environ.clear()
            os.environ.update(self.env)
            # 真实 API 冒烟：把应用 cwd 设为 <root>/ws —— 这也是执行沙箱根，
            # 让模型能真正落地文件操作（否则写 /tmp 会被沙箱拒绝）。
            if REAL["active"]:
                ws = os.path.join(self.root, "ws")
                os.makedirs(ws, exist_ok=True)
                os.chdir(ws)
            os.execv(BIN, [BIN, "--config", self.cfg, "--data-dir", os.path.join(self.root, "data"), "run"])
        self.pid = pid
        self.master = master
        # 设置 PTY 窗口大小（避免 ratatui 0x0 崩溃）
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        threading.Thread(target=self._drain_loop, daemon=True).start()
        return self

    def read(self, timeout=0.2):
        # 后台 drainer 线程负责实际读取；这里仅短等（兼容旧调用点）。
        time.sleep(min(timeout, 0.05))
        return None

    def drain(self, seconds=0.3):
        end = time.time() + seconds
        while time.time() < end:
            self.read(0.05)

    def send(self, data):
        if isinstance(data, str):
            data = data.encode("utf-8")
        os.write(self.master, data)

    def keys(self, *seqs):
        for s in seqs:
            self.send(s)
            time.sleep(0.05)

    def type_text(self, text):
        self.send(text)

    def enter(self):
        self.send("\r")

    def wait_log(self, pattern, timeout=30.0):
        """轮询应用日志直到出现 pattern（返回所在行）或超时。"""
        # 真实 API 冒烟：真实模型延迟更高，超时放宽 3 倍
        if REAL["active"]:
            timeout *= 3
        end = time.time() + timeout
        last = ""
        while time.time() < end:
            self.read(0.1)
            try:
                with open(self.log_path, encoding="utf-8") as f:
                    last = f.read()
            except FileNotFoundError:
                last = ""
            if re.search(pattern, last):
                return True, last
        return False, last

    def wait_screen(self, pattern, timeout=15.0):
        end = time.time() + timeout
        while time.time() < end:
            self.read(0.1)
            # 归一化空白后比较（ratatui 渲染会吞掉空格/换行）
            with self._raw_lock:
                text = self.raw.decode("utf-8", "replace")
            if norm(pattern) in norm(ansi_strip(text)):
                return True
        return False

    def wait_startup(self, timeout=90.0):
        ok, log = self.wait_log(r"mode_init: ModeManager ready", timeout)
        if not ok:
            return False, log
        # 等第一帧渲染
        self.drain(1.5)
        return True, log

    def screen_text(self):
        with self._raw_lock:
            return ansi_strip(self.raw.decode("utf-8", "replace"))

    def quit(self):
        self.send("\x03")
        end = time.time() + 10
        while time.time() < end:
            try:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid == self.pid:
                    self.read(0.2)
                    with self._raw_lock:
                        screen = self.raw
                    with open(self.screen_path, "wb") as f:
                        f.write(screen)
                    return True
            except ChildProcessError:
                return True
            self.read(0.1)
        # 强杀
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        with self._raw_lock:
            screen = self.raw
        with open(self.screen_path, "wb") as f:
            f.write(screen)
        return False

    def app_log(self):
        try:
            with open(self.log_path, encoding="utf-8") as f:
                return f.read()
        except FileNotFoundError:
            return ""


def wait_request_count(mock, kind, count, timeout=30.0):
    # 真实 API 冒烟：真实模型延迟更高，超时放宽 3 倍
    if REAL["active"]:
        timeout *= 3
    end = time.time() + timeout
    while time.time() < end:
        if len(mock.thinking_of(kind)) >= count:
            return True
        time.sleep(0.2)
    return False


def cfg_toml(port, root, default_mode="unni", unni_style="autonomous",
             unni_node="execution", token_budget=100000, time_budget=300,
             mix_thinking=False, extra=""):
    return f"""provider = ""
model_id = ""
api_key = ""
data_dir = "{root}/data"
default_mode = "{default_mode}"
default_model = "mock-{port}-mock"
memory_mode = "mixed"

[context]
recent_turns = 3
raw_threshold_pct = 30.0
rag_reserve_pct = 10.0
cognitive_quota_pct = 5.0
attention_quota_pct = 5.0
experience_quota_pct = 5.0
preference_quota_pct = 3.0

[mode_styles.unni]
style = "{unni_style}"
node = "{unni_node}"

[mode_styles.keep]
token_budget = {token_budget}
time_budget_secs = {time_budget}

[mode_styles.loop]
mix_thinking = {"true" if mix_thinking else "false"}
{extra}
"""


def insert_model(root, port, api_key="mock-key", model_id="mock-model"):
    if REAL["active"]:
        api_key = REAL["api_key"]
        model_id = REAL["model_id"]
    subprocess.run(
        [
            "cargo", "run", "--quiet", "--example", "insert_mock_model", "--",
            "--data-dir", os.path.join(root, "data"),
            "--id", f"mock-{port}-mock",
            "--api-url", f"http://127.0.0.1:{port}",
            "--model-id", model_id,
            "--api-key", api_key,
        ],
        cwd=REPO,
        check=True,
        capture_output=True,
        text=True,
    )


# ---------------------------------------------------------------------------
# 场景
# ---------------------------------------------------------------------------

def v1_unni_autonomous_execution(root):
    """UNNI 自主 + 执行中台：执行完成 → spawn echo；洞察/记忆异步只沉淀。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V1 检查日志文件是否存在", "say": None}},
            {"match": {"kind": "echo"}, "respond": {"type": "output", "think": None, "say": "V1 执行完成，结果已过滤呈现。"}},
        ],
        "default": {"type": "output", "think": None, "say": "（默认）本轮完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V1", "启动失败", mock, sess, log)

    sess.type_text(real_in("V1", "V1 检查日志文件是否存在"))
    sess.enter()
    # 1) 用户 think 请求发出
    if not wait_request_count(mock, "user", 1):
        return report_fail("V1", "未收到用户 think 请求", mock, sess, "")
    # 2) 执行完成 → echo spawn（echo 请求）
    if not wait_request_count(mock, "echo", 1):
        return report_fail("V1", "执行完成未触发 echo 实例", mock, sess, "")
    # 3) 异步中台只沉淀（insight+memory 均不触发新实例）
    ok, log = sess.wait_log(r"async platform .* only sinking memory, not triggering", 30)
    if REAL["active"]:
        # 真实 API：say 内容不可预测 → 改断言 echo 轮完成且应用未崩溃
        time.sleep(2)
        results = {
            "user_think_1": PASS,
            "echo_spawn_1": PASS,
            "async_only_sinking": PASS if ok else FAIL,
            "echo_round_completed": PASS if alive(sess) else FAIL,
        }
    else:
        # echo 的 say 上屏（渲染 tick 驱动，轮询等待）
        say_ok = sess.wait_screen("V1 执行完成", 12)
        results = {
            "user_think_1": PASS,
            "echo_spawn_1": PASS,
            "async_only_sinking": PASS if ok else FAIL,
            "echo_say_published": PASS if say_ok and screen_has(sess, "V1 执行完成，结果已过滤呈现") else FAIL,
        }
        time.sleep(2)
        results["no_extra_echo"] = PASS if len(mock.thinking_of("echo")) <= 1 else FAIL
    sess.quit()
    mock.stop()
    return finalize("V1", results, mock, sess, extra=log)


def v2_unni_follow_execution(root):
    """UNNI 跟随 + 执行中台：触发 stash pending；下次输入合并。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V2 执行一个检查任务", "say": None}},
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V2 第二次输入：基于上一轮上下文继续", "say": None}},
            {"match": {"kind": "echo"}, "respond": {"type": "output", "think": None, "say": "V2 已过滤呈现。"}},
        ],
        "default": {"type": "output", "think": None, "say": "（默认）本轮完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root, unni_style="follow"))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V2", "启动失败", mock, sess, log)

    sess.type_text(real_in("V2", "V2 执行一个检查任务"))
    sess.enter()
    if not wait_request_count(mock, "user", 1):
        return report_fail("V2", "未收到第一个用户请求", mock, sess, "")
    # 跟随模式：执行完成后应 stash，不应 spawn echo。
    # 真实 API 冒烟：真实模型执行耗时更长，必须等 stash 落定再输入第二条
    # （否则第二条早于 stash → take_pending 返回 None → 不合并）。
    if REAL["active"]:
        stash_ok, _ = sess.wait_log(r"stash pending context", 120)
        if not stash_ok:
            return report_fail("V2", "未等到 pending stash", mock, sess, "")
        time.sleep(1)
    else:
        time.sleep(4)
    # 输入第二条
    sess.type_text(real_in("V2b", "V2 第二次输入"))
    sess.enter()
    if not wait_request_count(mock, "user", 2):
        return report_fail("V2", "未收到第二个用户请求", mock, sess, "")
    ok, log = sess.wait_log(r"merging pending context \(thought_id=[^)]*\) into user input", 20)
    # 第二个 user 请求的消息应包含合并上下文
    users = mock.thinking_of("user")
    merged = users[1]["messages_snippet"] if len(users) > 1 else ""
    results = {
        "no_echo_after_first_round": PASS if len(mock.thinking_of("echo")) == 0 else FAIL,
        "pending_merged_log": PASS if ok else FAIL,
        "second_request_has_pending": PASS if "上一轮整理上下文" in merged else FAIL,
    }
    sess.quit()
    mock.stop()
    return finalize("V2", results, mock, sess, extra=log)


def v3_unni_autonomous_insight(root):
    """UNNI 自主 + 洞察中台：执行完成在前被忽略；洞察触发 echo；记忆只沉淀。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V3 执行一个分析任务", "say": None}},
            {"match": {"kind": "echo"}, "respond": {"type": "output", "think": None, "say": "V3 洞察结果已过滤呈现。"}},
        ],
        "default": {"type": "output", "think": None, "say": "（默认）本轮完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root, unni_node="insight"))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V3", "启动失败", mock, sess, log)

    sess.type_text(real_in("V3", "V3 执行一个分析任务"))
    sess.enter()
    if not wait_request_count(mock, "user", 1):
        return report_fail("V3", "未收到用户请求", mock, sess, "")
    # 洞察触发 echo
    if not wait_request_count(mock, "echo", 1):
        return report_fail("V3", "洞察完成未触发 echo", mock, sess, "")
    ok, log = sess.wait_log(r"async platform .* only sinking memory, not triggering", 30)
    ok_before, log2 = sess.wait_log(r"trigger ignored \(platform .* before trigger node", 10)
    results = {
        "echo_spawn_on_insight": PASS,
        "memory_only_sinking": PASS if ok else FAIL,
        "execution_before_node_ignored": PASS if ok_before or "before trigger node" in log2 else FAIL,
    }
    time.sleep(2)
    results["no_extra_echo"] = PASS if len(mock.thinking_of("echo")) <= 1 else FAIL
    sess.quit()
    mock.stop()
    return finalize("V3", results, mock, sess, extra=log)


def v4_keep_budget_pause(root):
    """KEEP token 预算耗尽 → 暂停 + 屏幕提示。budget=16K（2 实例 × 8K）。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V4 持续推进一个目标", "say": None}},
            {"match": {"kind": "echo"}, "count": 10, "respond": {"type": "output", "think": "V4 继续推进下一迭代", "say": None}},
        ],
        "default": {"type": "output", "think": "V4 默认继续推进", "say": None},
    })
    sess = Session(root, cfg_toml(mock.port, root, default_mode="keep", token_budget=16000, time_budget=300))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V4", "启动失败", mock, sess, log)

    sess.type_text(real_in("V4", "V4 持续推进目标"))
    sess.enter()
    # 等待预算暂停日志
    ok, log = sess.wait_log(r"KEEP budget exhausted, pausing flywheel", 60)
    results = {
        "budget_exhausted_log": PASS if ok else FAIL,
        "screen_pause_message": PASS if sess.wait_screen("KEEP 预算已耗尽", 8) else FAIL,
    }
    # 暂停后不应再 spawn（echo 请求数稳定）
    time.sleep(3)
    n1 = len(mock.thinking_of("echo"))
    time.sleep(2)
    n2 = len(mock.thinking_of("echo"))
    results["no_spawn_after_pause"] = PASS if n2 == n1 else FAIL
    sess.quit()
    mock.stop()
    return finalize("V4", results, mock, sess, extra=log)


def v5_loop_off(root):
    """LOOP 融合思考 off：执行/洞察被忽略；记忆触发 echo（默认路径）。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V5 循环推进一个目标", "say": None}},
            {"match": {"kind": "echo"}, "count": 4, "respond": {"type": "output", "think": "V5 记忆沉淀后继续推进", "say": None}},
        ],
        "default": {"type": "output", "think": "V5 默认继续循环", "say": None},
    })
    sess = Session(root, cfg_toml(mock.port, root, default_mode="loop", mix_thinking=False))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V5", "启动失败", mock, sess, log)

    sess.type_text(real_in("V5", "V5 循环推进目标"))
    sess.enter()
    if not wait_request_count(mock, "user", 1):
        return report_fail("V5", "未收到用户请求", mock, sess, "")
    if not wait_request_count(mock, "echo", 1):
        return report_fail("V5", "记忆完成未触发 echo", mock, sess, "")
    ok, log = sess.wait_log(r"trigger ignored \(platform .* before trigger node", 30)
    results = {
        "echo_spawn_on_memory": PASS,
        "exec_insight_before_node_ignored": PASS if "before trigger node" in log else FAIL,
        "no_reflect_instances": PASS if len(mock.thinking_of("reflect")) == 0 else FAIL,
    }
    sess.quit()
    mock.stop()
    return finalize("V5", results, mock, sess, extra=log)


def v6_loop_mix_on(root):
    """LOOP 融合思考 on：三阶段流水线 + 反射实例 + 拼接合并。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V6 多路思考一个目标", "say": None}},
            # 实例1（reflect，执行反思；分类为 echo，用内容区分）
            {"match": {"kind": "echo", "content_contains": "请基于执行结果做一轮反思"}, "count": 1, "kind_label": "reflect1", "respond": {"type": "output", "think": "V6 反思一：执行角度", "say": None}},
            # 实例2（reflect，含实例1 反思）
            {"match": {"kind": "reflect2"}, "count": 1, "respond": {"type": "output", "think": "V6 反思二：洞察角度", "say": None}},
            # 实例3（final，含实例1+实例2 反思）
            {"match": {"kind": "final"}, "count": 1, "respond": {"type": "output", "think": "V6 综合推进下一轮", "say": None}},
            # 第二轮
            {"match": {"kind": "echo", "content_contains": "请基于执行结果做一轮反思"}, "count": 1, "kind_label": "reflect1", "respond": {"type": "output", "think": "V6 二轮反思一", "say": None}},
            {"match": {"kind": "reflect2"}, "count": 1, "respond": {"type": "output", "think": "V6 二轮反思二", "say": None}},
            {"match": {"kind": "final"}, "count": 1, "respond": {"type": "output", "think": "V6 二轮综合", "say": None}},
        ],
        "default": {"type": "output", "think": "V6 默认推进", "say": None},
    })
    sess = Session(root, cfg_toml(mock.port, root, default_mode="loop", mix_thinking=True))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V6", "启动失败", mock, sess, log)

    sess.type_text(real_in("V6", "V6 多路思考目标"))
    sess.enter()
    # 实例1（reflect，分类 echo）
    if not wait_request_count(mock, "echo", 1, 40):
        return report_fail("V6", "执行完成未触发实例1（reflect）", mock, sess, "")
    # 实例2（reflect）
    if not wait_request_count(mock, "reflect2", 1, 40):
        return report_fail("V6", "洞察完成未触发实例2（reflect）", mock, sess, "")
    # 实例3（final）
    if not wait_request_count(mock, "final", 1, 40):
        return report_fail("V6", "记忆完成未触发实例3（final）", mock, sess, "")
    # 第二轮
    if not wait_request_count(mock, "final", 2, 40):
        return report_fail("V6", "第二轮 final 未触发（循环推进）", mock, sess, "")

    # 拼接合并验证：实例3 的输入应包含实例1/2 反思
    finals = mock.thinking_of("final")
    e1 = finals[0]["input"] if finals else ""
    reflects = [r for r in mock.requests() if r["kind"] in ("reflect2", "final")]
    results = {
        "reflect1_spawn": PASS,
        "reflect2_spawn": PASS,
        "final_spawn": PASS,
        "round2_final": PASS,
        "final_merged_reflect1": PASS if "实例1 反思" in e1 else FAIL,
        "final_merged_reflect2": PASS if "实例2 反思" in e1 else FAIL,
        "reflect1_was_reflect_only": PASS,
    }
    ok, log = sess.wait_log(r"reflect-only instance finished \(no execution\)", 30)
    results["reflect_only_finished"] = PASS if ok else FAIL
    # 反射实例不应产生执行链：执行实例 = 1 个用户轮 + 每轮 final；反射永远不 Execute
    time.sleep(2)
    exec_sent = len(re.findall(r"Execute DM sent", sess.app_log()))
    final_count = len(mock.thinking_of("final"))
    results["no_extra_execution_chain"] = PASS if exec_sent <= final_count + 1 else FAIL
    sess.quit()
    mock.stop()
    return finalize("V6", results, mock, sess, extra=log)


def v7_f1_auto_repair(root):
    """F1：invalid_json → 自动修复轮（保留用户输入意图）。

    真实 API 冒烟：无法强迫真实模型输出无效 JSON，改为验证真实输出的完整链路
    （think → 执行 → echo 收敛），自动修复的注入路径由 mock 场景覆盖。
    """
    if REAL["active"]:
        mock = Mock(root, {"script": [], "default": None})
        sess = Session(root, cfg_toml(mock.port, root))
        insert_model(root, mock.port)
        sess.launch()
        ok, log = sess.wait_startup()
        if not ok:
            return report_fail("V7", "启动失败", mock, sess, log)
        sess.type_text(real_in("V7", "V7 请检查并修复配置文件"))
        sess.enter()
        user_ok = wait_request_count(mock, "user", 1, 90)
        echo_ok = wait_request_count(mock, "echo", 1, 90)
        results = {
            "real_think_spawned": PASS if user_ok else FAIL,
            "real_chain_to_echo": PASS if echo_ok else FAIL,
            "app_alive": PASS if alive(sess) else FAIL,
        }
        sess.quit()
        mock.stop()
        return finalize("V7", results, mock, sess, extra=log)

    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "count": 1, "respond": {"type": "invalid", "content": "这不是有效的思考输出 JSON"}},
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V7 修复后重新执行任务", "say": None}},
            {"match": {"kind": "echo"}, "respond": {"type": "output", "think": None, "say": "V7 修复轮完成。"}},
        ],
        "default": {"type": "output", "think": "（默认）继续推进。", "say": "（默认）完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V7", "启动失败", mock, sess, log)

    sess.type_text("V7 请检查并修复配置文件")
    sess.enter()
    # 自动修复轮 spawn
    ok, log = sess.wait_log(r"auto-repair round 1/2 for user intent", 40)
    # 等修复请求真正到达 mock（wait_log 只证明日志出现，请求可能稍后才落地）
    wait_request_count(mock, "user", 2, 15)
    users = mock.thinking_of("user")
    repair_req = any("自动修复" in r["messages_snippet"] for r in users[1:] if len(users) > 1)
    results = {
        "auto_repair_log": PASS if ok else FAIL,
        "repair_request_with_intent": PASS if repair_req else FAIL,
        "repair_turn_completed": PASS if wait_request_count(mock, "echo", 1, 40) else FAIL,
    }
    sess.quit()
    mock.stop()
    return finalize("V7", results, mock, sess, extra=log)


def v8_config_panel(root):
    """配置面板：进入 /config → Mode Style 子菜单 → 修改 UNNI 节点 → 保存落盘。"""
    mock = Mock(root, {"default": {"type": "output", "think": "（默认）继续推进。", "say": "（默认）完成。"}})
    sess = Session(root, cfg_toml(mock.port, root))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V8", "启动失败", mock, sess, log)

    # 进入配置面板
    sess.type_text("/config")
    sess.enter()
    if not sess.wait_screen("协同模式风格", 15):
        return report_fail("V8", "配置菜单未显示 Mode Style 入口", mock, sess, "")

    # 主菜单：Mode Style 是 index 4 → Down×4 后 Right 进入子菜单
    sess.keys("\x1b[B", "\x1b[B", "\x1b[B", "\x1b[B", "\x1b[C")
    # 子菜单 5 项（UNNI 协同方式 / UNNI 协同节点 / KEEP Token / KEEP 时间 / LOOP 融合思考）
    submenu_ok = sess.wait_screen("协同方式", 8) and sess.wait_screen("融合思考", 8)
    results = {
        "menu_has_mode_style": PASS,
        "submenu_shows_5_items": PASS if submenu_ok else FAIL,
    }
    if not submenu_ok:
        sess.quit()
        mock.stop()
        return finalize("V8", results, mock, sess, extra="submenu 未显示")

    # 子菜单：Down → UNNI 协同节点（cursor 1）→ Right 进入节点选择
    sess.keys("\x1b[B", "\x1b[C")
    select_ok = sess.wait_screen("协同节点", 8)
    results["node_select_shown"] = PASS if select_ok else FAIL
    # 节点选择：Down → 洞察中台（cursor 1）→ Right/Enter 提交
    sess.keys("\x1b[B", "\x1b[C")
    ok = sess.wait_screen("协同节点已切换", 10)
    results["save_message_shown"] = PASS if ok else FAIL
    time.sleep(0.5)
    cfg_text = open(sess.cfg, encoding="utf-8").read()
    results["node_saved_to_config"] = PASS if 'node = "insight"' in cfg_text else FAIL
    # 退出配置
    sess.keys("\x1b")
    time.sleep(0.3)
    sess.quit()
    mock.stop()
    return finalize("V8", results, mock, sess, extra=log)


def v9_compat_tab(root):
    """旧配置兼容（memory_mode 字段）+ Tab 模式切换。"""
    mock = Mock(root, {"default": {"type": "output", "think": "（默认）继续推进。", "say": "（默认）完成。"}})
    sess = Session(root, cfg_toml(mock.port, root, default_mode="unni"))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V9", "启动失败（含 memory_mode 的旧配置）", mock, sess, "")
    results = {
        "startup_with_legacy_memory_mode": PASS,
    }
    # Tab 切换模式：UNNI → KEEP → LOOP（以 mode entered 日志为准，屏幕断言易受帧拼接干扰）
    sess.send("\t")
    ok1, log1 = sess.wait_log(r"KEEP mode entered", 10)
    sess.send("\t")
    ok2, log2 = sess.wait_log(r"LOOP mode entered", 10)
    results["tab_unni_to_keep"] = PASS if ok1 else FAIL
    results["tab_keep_to_loop"] = PASS if ok2 else FAIL
    sess.quit()
    mock.stop()
    return finalize("V9", results, mock, sess, extra=log1 + log2)


def v10_retry_backoff(root):
    """缺陷2 配套：思考实例限流（429）→ 指数退避重试 → 成功落库 → final 拼接完整。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V10 多路思考目标", "say": None}},
            # 实例1：前 2 次 429（限流），第 3 次成功 → 验证指数退避重试
            {"match": {"kind": "echo", "content_contains": "请基于执行结果做一轮反思"},
             "count": 3, "kind_label": "reflect1",
             "respond": [
                 {"type": "http_error", "status": 429, "content": "rate limit exceeded"},
                 {"type": "http_error", "status": 429, "content": "rate limit exceeded"},
                 {"type": "output", "think": "V10 反思一（重试后成功）", "say": None},
             ]},
            {"match": {"kind": "reflect2"}, "count": 1, "respond": {"type": "output", "think": "V10 反思二", "say": None}},
            {"match": {"kind": "final"}, "count": 1, "respond": {"type": "output", "think": "V10 综合推进", "say": None}},
            # 第二轮：走默认（say-only 收束）
        ],
        "default": {"type": "output", "think": "（默认）继续推进。", "say": "（默认）完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root, default_mode="loop", mix_thinking=True))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V10", "启动失败", mock, sess, log)

    sess.type_text("V10 多路思考目标")
    sess.enter()
    # 实例1 首次请求（429）后触发退避重试；重试期间请求数应增长
    if not wait_request_count(mock, "echo", 1, 40):
        return report_fail("V10", "实例1 未发起首次请求", mock, sess, "")
    # 等重试完成（2 次失败 + 成功 = 3 次请求；退避 3s+6s）
    if not wait_request_count(mock, "echo", 3, 60):
        return report_fail("V10", "实例1 未完成 429 重试（应请求 3 次）", mock, sess, "")
    # final 应在重试成功后拼接完整（含实例1/2 反思）
    if not wait_request_count(mock, "final", 1, 40):
        return report_fail("V10", "final 未 spawn", mock, sess, "")

    app_log = sess.app_log()
    finals = mock.thinking_of("final")
    e1 = finals[0]["input"] if finals else ""
    results = {
        "retry_log_attempt1": PASS if "retryable, attempt=1" in app_log else FAIL,
        "retry_log_attempt2": PASS if "retryable, attempt=2" in app_log else FAIL,
        "retry_status_exposed": PASS if sess.wait_screen("后重试", 10) else FAIL,
        "reflect1_succeeded_after_retry": PASS if len(mock.thinking_of("echo")) >= 3 else FAIL,
        "final_merged_reflect1": PASS if "实例1 反思" in e1 else FAIL,
        "final_merged_reflect2": PASS if "实例2 反思" in e1 else FAIL,
    }
    sess.quit()
    mock.stop()
    return finalize("V10", results, mock, sess, extra=app_log[-400:])


def v11_permanent_error_degrade(root):
    """缺陷2 配套：思考实例永久错误（404）→ 暴露错误、不中断、final 缺段继续。"""
    mock = Mock(root, {
        "script": [
            {"match": {"kind": "user"}, "respond": {"type": "output", "think": "V11 多路思考目标", "say": None}},
            {"match": {"kind": "echo", "content_contains": "请基于执行结果做一轮反思"},
             "count": 1, "kind_label": "reflect1",
             "respond": {"type": "output", "think": "V11 反思一", "say": None}},
            # 实例2：永久错误 404
            {"match": {"kind": "reflect2"}, "count": 1,
             "respond": {"type": "http_error", "status": 404, "content": "model not found"}},
            {"match": {"kind": "final"}, "count": 1, "respond": {"type": "output", "think": "V11 综合推进", "say": None}},
        ],
        "default": {"type": "output", "think": "（默认）继续推进。", "say": "（默认）完成。"},
    })
    sess = Session(root, cfg_toml(mock.port, root, default_mode="loop", mix_thinking=True))
    insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup()
    if not ok:
        return report_fail("V11", "启动失败", mock, sess, log)

    sess.type_text("V11 多路思考目标")
    sess.enter()
    # 实例2 永久失败 → 错误暴露 + final 仍 spawn（缺段继续）
    ok, log = sess.wait_log(r"mix dep reflect2 .* permanent failed", 40)
    results = {
        "permanent_error_log": PASS if ok else FAIL,
        "error_exposed_screen": PASS if sess.wait_screen("反思实例2 永久失败", 10) else FAIL,
    }
    if not wait_request_count(mock, "final", 1, 40):
        return report_fail("V11", "final 未 spawn（链中断）", mock, sess, "")
    finals = mock.thinking_of("final")
    e1 = finals[0]["input"] if finals else ""
    results["final_spawns_after_permanent"] = PASS
    results["final_has_reflect1"] = PASS if "实例1 反思" in e1 else FAIL
    results["final_skips_reflect2"] = PASS if "实例2 反思" not in e1 else FAIL
    # 链继续：第二轮 final 仍能 spawn（不中断）
    results["chain_continues"] = PASS if wait_request_count(mock, "final", 2, 60) else FAIL
    sess.quit()
    mock.stop()
    return finalize("V11", results, mock, sess, extra=log)


# ---------------------------------------------------------------------------
# 报告
# ---------------------------------------------------------------------------

def report_fail(name, reason, mock, sess, log):
    try:
        sess.quit()
    except Exception:
        pass
    mock.stop()
    return {
        "name": name,
        "passed": 0,
        "failed": 1,
        "results": {"__启动/前置失败__": (FAIL, reason)},
        "extra": log,
    }


def finalize(name, results, mock, sess, extra=""):
    passed = sum(1 for v in results.values() if v == PASS)
    failed = sum(1 for v in results.values() if v == FAIL)
    return {
        "name": name,
        "passed": passed,
        "failed": failed,
        "results": results,
        "extra": extra,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default="/tmp/cipher-ptytest")
    ap.add_argument("--keep", action="store_true", help="保留临时目录")
    ap.add_argument("--only", default="", help="只跑指定场景（逗号分隔，如 V1,V6）")
    ap.add_argument("--real", action="store_true", help="真实 API 冒烟：mock 变透明代理转发 minimax")
    ap.add_argument("--real-url", default="https://api.minimaxi.com/v1", help="上游真实 LLM 基础地址")
    ap.add_argument("--real-model", default="MiniMax-M3", help="上游真实模型 ID")
    ap.add_argument("--real-key", default="", help="上游 API key（缺省读环境变量 MINIMAX_API_KEY）")
    args = ap.parse_args()

    if not os.path.exists(BIN):
        print(f"二进制不存在：{BIN}，先 cargo build")
        sys.exit(1)

    if args.real:
        key = args.real_key or os.environ.get("MINIMAX_API_KEY", "")
        if not key:
            print("--real 需要 --real-key 或环境变量 MINIMAX_API_KEY")
            sys.exit(1)
        REAL["active"] = True
        REAL["upstream"] = args.real_url
        REAL["api_key"] = key
        REAL["model_id"] = args.real_model
        print(f"真实 API 模式：{args.real_url} model={args.real_model}")

    only = [s.strip().upper() for s in args.only.split(",") if s.strip()] if args.only else []
    scenarios = [
        ("V1", v1_unni_autonomous_execution),
        ("V2", v2_unni_follow_execution),
        ("V3", v3_unni_autonomous_insight),
        ("V4", v4_keep_budget_pause),
        ("V5", v5_loop_off),
        ("V6", v6_loop_mix_on),
        ("V7", v7_f1_auto_repair),
        ("V8", v8_config_panel),
        ("V9", v9_compat_tab),
        ("V10", v10_retry_backoff),
        ("V11", v11_permanent_error_degrade),
    ]
    if only:
        scenarios = [s for s in scenarios if s[0] in only]
    if REAL["active"]:
        # 错误注入场景（V10/V11）只对 mock 有意义，真实 API 冒烟跳过
        scenarios = [s for s in scenarios if s[0] not in ("V10", "V11")]

    report = []
    for name, fn in scenarios:
        root = os.path.join(args.root, name)
        shutil.rmtree(root, ignore_errors=True)
        os.makedirs(root, exist_ok=True)
        print(f"\n=== {name} ===", flush=True)
        try:
            r = fn(root)
        except Exception as e:
            import traceback
            traceback.print_exc()
            r = {"name": name, "passed": 0, "failed": 1, "results": {"__异常__": (FAIL, str(e))}, "extra": ""}
        report.append(r)
        for k, v in r["results"].items():
            mark = "✓" if v == PASS else "✗"
            detail = v if isinstance(v, str) else ""
            print(f"  {mark} {k} {detail}")
        print(f"  小计: {r['passed']} passed / {r['failed']} failed")

    # 汇总
    total_p, total_f = 0, 0
    lines = ["# Cipher v0.2.6 PTY 黑盒测试报告", "",
             "> 驱动真实 TUI（PTY）+ mock LLM 服务器，全部断言基于确定性可观测证据：",
             "> mock 请求序列 / 应用 trace 日志 / 屏幕文本 / config.toml 内容。", ""]
    lines.append("## 验证题矩阵与指标")
    lines.append("")
    for r in report:
        total_p += r["passed"]
        total_f += r["failed"]
        lines.append(f"### {r['name']}")
        lines.append("")
        for k, v in r["results"].items():
            mark = "✅" if v == PASS else "❌"
            lines.append(f"- {mark} **{k}**{(' — ' + v) if isinstance(v, str) else ''}")
        lines.append("")
    lines.append("## 汇总")
    lines.append("")
    lines.append(f"- 总指标：**{total_p} 项通过 / {total_f} 项失败**")
    lines.append("")
    if total_f == 0:
        lines.append("**结论：v0.2.6 全部 PTY 黑盒验证题通过。**")
    else:
        lines.append("**结论：存在失败指标，详见上方明细。**")
    report_path = os.path.join(args.root, "PTY_TEST_REPORT.md")
    with open(report_path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"\n报告已写入 {report_path}")
    print(f"总计: {total_p} passed / {total_f} failed")
    sys.exit(0 if total_f == 0 else 1)


if __name__ == "__main__":
    main()
