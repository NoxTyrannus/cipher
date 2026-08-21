#!/usr/bin/env python3
"""v0.3.1 自由探索：classic/dual × unni/keep/loop 基础链路 + 统一目录检查。"""
import os, sys, json, time, shutil
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ptytest

def scenario_for(scheme):
    if scheme == "dual":
        return {
            "script": [
                {"match": {"kind": "think"}, "respond": {"type": "output", "content": "think body"}},
                {"match": {"kind": "say"}, "respond": {"type": "output", "content": "say body"}},
            ],
            "default": {"type": "output", "content": "default body"},
        }
    else:
        return {
            "script": [
                {"match": {"kind": "user"}, "respond": {"type": "output", "think": "classic think", "say": "classic say"}},
            ],
            "default": {"type": "output", "think": None, "say": "default say"},
        }

def run_case(root, scheme, mode):
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root, exist_ok=True)
    mock = ptytest.Mock(root, scenario_for(scheme))
    extra = f'\n[thinking]\nscheme = "{scheme}"\n'
    cfg = ptytest.cfg_toml(mock.port, root, default_mode=mode, extra=extra)
    sess = ptytest.Session(root, cfg)
    ptytest.insert_model(root, mock.port)
    sess.launch()
    ok, log = sess.wait_startup(timeout=60)
    if not ok:
        sess.quit(); mock.stop()
        return {"scheme": scheme, "mode": mode, "ok": False, "reason": "startup", "requests": []}
    # 输入一个简单消息（KEEP/LOOP 也发，便于观察 Think/Say 是否按预期）
    sess.type_text(f"test {mode}")
    sess.enter()
    time.sleep(4)
    sess.quit()
    reqs = mock.requests()
    mock.stop()
    kinds = [r.get("kind") for r in reqs]
    home_manifest = os.path.exists(os.path.join(root, "home", ".cipher", "manifest.json"))
    return {
        "scheme": scheme,
        "mode": mode,
        "ok": True,
        "kinds": kinds,
        "manifest": home_manifest,
        "requests": reqs,
    }

def main():
    results = []
    for scheme in ["classic", "dual"]:
        for mode in ["unni", "keep", "loop"]:
            root = f"/tmp/cipher-explore-{scheme}-{mode}"
            r = run_case(root, scheme, mode)
            results.append(r)
            print(json.dumps(r, ensure_ascii=False))
    with open("/tmp/cipher-explore-results.json", "w", encoding="utf-8") as f:
        json.dump(results, f, ensure_ascii=False, indent=2)

if __name__ == "__main__":
    main()
