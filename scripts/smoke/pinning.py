#!/usr/bin/env python3
"""冒烟：证明稳定层把「表层字节抖动」钉成同一份前缀，而真正的语义变化不被钉死。"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.environ.get("CODEX_LAYER_BIN", os.path.join(REPO, "target", "release", "codex-layer"))
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
# 默认在临时目录里跑，不污染仓库也不依赖固定的 /tmp 路径
WORK = os.environ.get("SMOKE_WORK") or tempfile.mkdtemp(prefix="codex-layer-smoke-")
BACKEND_PORT = 57812
PROXY_PORT = 57811
CAPTURE_ROOT = os.path.join(WORK, "capture")

def block(name, body):
    return "<%s>\n%s\n</%s>" % (name, body, name)

def developer(chunk):
    return {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": chunk}]}

def body_of(chunks, tail="问题一"):
    return {
        "model": "fake-model",
        "instructions": "core instructions text",
        "input": developer_messages(chunks) + [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": tail}]}
        ],
        "stream": False,
        "max_output_tokens": 8,
    }

def developer_messages(chunks):
    return [developer(c) for c in chunks]

def wait_port(port, timeout=20.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with __import__("socket").socket() as sock:
            if sock.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.2)
    return False

def write_config(path, prompt_prefix, port):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(
            "[server]\nhost = \"127.0.0.1\"\nport = %d\n\n" % port +
            "[upstream]\nbase_url = \"http://127.0.0.1:%d/v1\"\napi_key = \"sk-fake\"\nprotocol = \"chat_completions\"\n\n" % BACKEND_PORT +
            "[stabilizer]\nprompt_prefix = %s\n" % ("true" if prompt_prefix else "false") +
            "[log]\npath = \"%s\"\n" % (os.path.join(WORK, "proxy-%s.jsonl" % ("on" if prompt_prefix else "off"))),
        )

def post(path, payload):
    req = urllib.request.Request(
        "http://127.0.0.1:%d%s" % (PROXY_PORT, path),
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json", "user-agent": "codex-smoke/1.0"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return resp.status, resp.read()

backend_proc = []

def start_backend(tag):
    capture = os.path.join(CAPTURE_ROOT, tag)
    os.makedirs(capture, exist_ok=True)
    proc = subprocess.Popen([sys.executable, os.path.join(SCRIPT_DIR, "fake_backend.py"), capture, str(BACKEND_PORT)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    if not wait_port(BACKEND_PORT):
        raise SystemExit("假上游没起来")
    backend_proc.append(proc)
    return capture

def stop_backend():
    for proc in backend_proc:
        proc.terminate()
        proc.wait(timeout=10)
    backend_proc.clear()

def run_case(stabilize_on, requests):
    config = os.path.join(WORK, "config-%s.toml" % ("on" if stabilize_on else "off"))
    write_config(config, stabilize_on, PROXY_PORT)
    tag = "on" if stabilize_on else "off"
    env = dict(os.environ, TAG=tag, CODEX_LAYER_HOME=os.path.join(WORK, "home-%s" % tag))
    capture = start_backend("on" if stabilize_on else "off")
    proxy = subprocess.Popen([BIN, "--config", config], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        if not wait_port(PROXY_PORT):
            raise SystemExit("代理没起来: %s" % config)
        for payload in requests:
            status, raw = post("/v1/responses", payload)
            if status != 200:
                raise SystemExit("代理返回 %d: %s" % (status, raw[:200]))
    finally:
        proxy.send_signal(signal.SIGINT)
        proxy.wait(timeout=10)
        stop_backend()

def captured(index, tag):
    with open(os.path.join(CAPTURE_ROOT, tag, "req-%02d.json" % index), "rb") as handle:
        return json.load(handle)["messages"][0]["content"]

def main():
    os.makedirs(CAPTURE_ROOT, exist_ok=True)
    try:
        apps = block("apps_instructions", "apps body")
        collab = block("collaboration_mode", "collab body")
        multi = block("multi_agent_mode", "multi body")
        fresh = block("fresh_unknown_block", "fresh body")

        # 1) 同一块集合、不同到达顺序；2) 再换一种拆分边界；3) 真正的语义改动
        order_a = [multi, collab, apps, fresh]
        order_b = ["%s\n\n%s" % (apps, fresh), multi, collab]
        semantic = [multi, collab, apps, block("fresh_unknown_block", "完全不同的一段内容")]

        run_case(True, [body_of(order_a), body_of(order_b), body_of(semantic)])
        run_case(False, [body_of(order_a), body_of(order_b)])

        on_first = captured(1, "on")
        on_second = captured(2, "on")
        on_third = captured(3, "on")
        off_first = captured(1, "off")
        off_second = captured(2, "off")

        checks = [
            ("稳定层开启：块集合相同、到达顺序与拆分边界不同 -> 上游请求逐字节相同", on_first == on_second),
            ("稳定层开启：块内容真的变了 -> 上游请求必须跟着变（不钉死语义）", on_first != on_third),
            ("稳定层关闭：同一对请求输出不同（反证钉定来自稳定层）", off_first != off_second),
            ("规范序为块名字典序", on_first.index(apps) < on_first.index(collab) < on_first.index(fresh) < on_first.index(multi)),
        ]
        ok = True
        for label, passed in checks:
            print("%s %s" % ("PASS" if passed else "FAIL", label))
            ok = ok and passed
        if not ok:
            print("\n--- on#1 ---\n%s\n--- on#2 ---\n%s" % (on_first[:600], on_second[:600]))
            print("--- off#1 ---\n%s\n--- off#2 ---\n%s" % (off_first[:600], off_second[:600]))
        # 稳定层事件是否落进诊断日志
        log_on = os.path.join(WORK, "proxy-on.jsonl")
        events = []
        if os.path.isfile(log_on):
            with open(log_on, encoding="utf-8", errors="ignore") as handle:
                events = [json.loads(line)["event"] for line in handle if line.strip()]
        print("\n诊断日志事件（稳定层开启）:", sorted(set(events)))
        return 0 if ok else 1
    finally:
        stop_backend()

sys.exit(main())
