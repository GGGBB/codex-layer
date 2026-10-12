#!/usr/bin/env python3
"""流式冒烟：验证 Responses SSE 的终结事件闭合、上游密钥注入、前缀探针事件。

假上游用 chunked 编码真流式回包，逐块 flush，确保代理是「边到边转」而不是攒完再发。
"""
import http.server, json, os, socket, subprocess, sys, tempfile, threading, time, traceback, urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.environ.get("CODEX_LAYER_BIN", os.path.join(REPO, "target", "release", "codex-layer"))
WORK = os.environ.get("SMOKE_WORK") or tempfile.mkdtemp(prefix="codex-layer-stream-")
BACKEND_PORT = 57832
PROXY_PORT = 57831
CONFIG = os.path.join(WORK, "config.toml")
LOG = os.path.join(WORK, "proxy.jsonl")
OUT = os.path.join(WORK, "stream-events.txt")

# 假上游按请求顺序回放这几段 chunk
def chunk_frame(payload: bytes) -> bytes:
    return b"%X\r\n" % len(payload) + payload + b"\r\n"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _auth_seen(self):
        # 落盘上游收到的 Authorization，证明代理注入了密钥
        with open(os.path.join(WORK, "backend-auth.txt"), "a", encoding="utf-8") as handle:
            handle.write(self.headers.get("authorization", "<none>") + "\n")

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", "0")))
        self._auth_seen()
        body = json.loads(raw)
        if not body.get("stream"):
            data = json.dumps({
                "id": "chatcmpl-fake", "object": "chat.completion", "model": body.get("model"),
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        # 手写 chunk 帧，因此必须显式声明分块传输编码
        self.send_header("transfer-encoding", "chunked")
        self.send_header("cache-control", "no-cache")
        self.end_headers()

        def emit(delta, finish=None):
            return b"data: " + json.dumps({
                "id": "chatcmpl-s", "object": "chat.completion.chunk", "model": body.get("model"),
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }).encode() + b"\n\n"

        # 首帧：role 增量（真流式：逐块 flush，验证代理边到边转）
        self.wfile.write(chunk_frame(emit({"role": "assistant"})))

        self.wfile.flush()
        # 上游按请求内容自判模式：跨进程改不了全局变量，只能把意图写进请求
        if b"TOOLMODE" in raw:
            payload = b"data: " + json.dumps({
                "id": "chatcmpl-s", "object": "chat.completion.chunk", "model": body.get("model"),
                "choices": [{"index": 0, "delta": {"content": None, "tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"a\"}"}}]},
                    "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10},
            }).encode() + b"\n\n"
            self.wfile.write(chunk_frame(payload))
        else:
            for piece in ("hel", "lo ", "world"):
                self.wfile.write(chunk_frame(emit({"content": piece})))
                self.wfile.flush()
                time.sleep(0.05)
            last = b"data: " + json.dumps({
                "id": "chatcmpl-s", "object": "chat.completion.chunk", "model": body.get("model"),
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10},
            }).encode() + b"\n\n"
            self.wfile.write(chunk_frame(last))
        self.wfile.write(chunk_frame(b"data: [DONE]\n\n"))
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def do_GET(self):
        data = b'{"object":"list","data":[{"id":"fake-model","object":"model"}]}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass

if len(sys.argv) > 1 and sys.argv[1] == "backend":
    # 作为假上游子进程运行，只服务不编排
    http.server.ThreadingHTTPServer(("127.0.0.1", BACKEND_PORT), Handler).serve_forever()
    raise SystemExit(0)


def wait_port(port, timeout=25.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket() as sock:
            if sock.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.2)
    return False


def write_config():
    with open(CONFIG, "w", encoding="utf-8") as handle:
        handle.write(
            '[server]\nhost = "127.0.0.1"\nport = %d\n\n' % PROXY_PORT
            + '[upstream]\nbase_url = "http://127.0.0.1:%d/v1"\napi_key = "sk-fake-injected"\nprotocol = "chat_completions"\n\n' % BACKEND_PORT
            + '[stabilizer]\nprompt_prefix = true\n\n'
            + '[log]\npath = "%s"\n' % LOG
        )


def request(payload, stream):
    req = urllib.request.Request(
        "http://127.0.0.1:%d/v1/responses" % PROXY_PORT,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=30) as resp:
        if not stream:
            return resp.status, resp.read()
        raw = resp.read()
    events, texts, completed = [], [], {}
    for line in raw.decode().splitlines():
        if not line.startswith("data:"):
            continue
        blob = line[5:].strip()
        if blob == "[DONE]":
            events.append("[DONE]")
            continue
        obj = json.loads(blob)
        events.append(obj.get("type"))
        if obj.get("type") == "response.output_text.delta":
            texts.append(obj.get("delta"))
        if obj.get("type") == "response.output_item.done":
            completed.setdefault("item_type", obj.get("item", {}).get("type"))
        if obj.get("type") == "response.completed":
            completed["usage"] = obj.get("response", {}).get("usage")
    return resp.status, {"events": events, "text": "".join(t or "" for t in texts), **completed}


def body(cache_key, stream, tail="问题一"):
    return {
        "model": "fake-model", "instructions": "core instructions text",
        "prompt_cache_key": cache_key,
        "input": [
            {"type": "message", "role": "developer",
             "content": [{"type": "input_text", "text": "<env>\n环境说明\n</env>"}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": tail}]},
        ],
        "stream": stream, "max_output_tokens": 8,
        "tools": [{"type": "function", "function": {"name": "lookup",
                  "description": "查", "parameters": {"type": "object", "properties": {}}}}],
    }


# 子进程必须继承 SMOKE_WORK：否则它自己 mkdtemp，父子各写各的目录
backend = subprocess.Popen([sys.executable, __file__, "backend"],
                           env=dict(os.environ, SMOKE_WORK=WORK),
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
proxy = []
results = []
auth_path = os.path.join(WORK, "backend-auth.txt")
for path in (LOG, OUT, auth_path):
    if os.path.exists(path):
        os.unlink(path)
try:
    if not wait_port(BACKEND_PORT):
        raise SystemExit("假上游没起来")
    write_config()
    proxy.append(subprocess.Popen([BIN, "--config", CONFIG], stdout=subprocess.DEVNULL,
                                   stderr=subprocess.STDOUT))
    if not wait_port(PROXY_PORT):
        raise SystemExit("代理没起来")

    # 用例1：非流式 + prompt_cache_key -> 应登记基线探针；同时检查密钥注入
    code, _ = request(body("thread-A", False), False)
    auth = open(auth_path).read().strip().splitlines()
    results.append(("非流式 200", code == 200))
    results.append(("上游收到注入的 Authorization", bool(auth) and auth[0] == "Bearer sk-fake-injected"))

    # 用例2：同键同内容再来一轮 -> 已见指纹，不应报 divergence
    code, _ = request(body("thread-A", False), False)
    # 用例3：流式 stop
    code, info = request(body("thread-A", True), True)
    with open(OUT, "w", encoding="utf-8") as handle:
        handle.write("\n".join(str(e) for e in info["events"]))
    need = ["response.created", "response.output_text.delta", "response.output_text.done",
            "response.content_part.done", "response.output_item.done", "response.completed", "[DONE]"]
    results.append(("流式 stop 状态 200", code == 200))
    results.append(("流式文本拼接正确", info["text"] == "hello world"))
    results.append(("终结事件齐全", all(e in info["events"] for e in need)))
    results.append(("completed 带 usage", bool(info.get("usage"))))

    # 用例4：流式 tool_calls（换独立会话键，避免与用例1 的前缀指纹互相干扰）
    code, info2 = request(body("thread-B", True, tail="TOOLMODE 请调用工具"), True)
    with open(OUT, "a", encoding="utf-8") as handle:
        handle.write("\n--- tool_calls ---\n" + "\n".join(str(e) for e in info2["events"]))
    results.append(("tool_calls 轮 item 为 function_call", info2.get("item_type") == "function_call"))
    results.append(("tool_calls 轮有 completed", "response.completed" in info2["events"]))
except Exception as exc:
    # 异常不当即抛出：先记下并打印 traceback，让断言结果能完整打印
    traceback.print_exc()
    results.append(("编排异常中断: %r" % (exc,), False))
finally:
    for proc in proxy + [backend]:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()

events_log = []
if os.path.exists(LOG):
    for line in open(LOG, encoding="utf-8"):
        try:
            events_log.append(json.loads(line).get("event"))
        except Exception:
            pass
from collections import Counter
print("== 代理诊断事件 ==")
for k, v in sorted(Counter(events_log).items()):
    print("  %2d %s" % (v, k))
results.append(("探针登记了 prefix_baseline", "protocol_proxy.prefix_stabilizer_prefix_baseline" in events_log))
results.append(("同键同内容（多轮重复）未误报 divergence",
                "protocol_proxy.prefix_stabilizer_prefix_divergence" not in events_log))

print("== 断言 ==")
failed = 0
for name, ok in results:
    print("  %s %s" % ("PASS" if ok else "FAIL", name))
    failed += 0 if ok else 1
print("结果:", "ALL PASS" if failed == 0 else "%d FAILED" % failed)
sys.exit(1 if failed else 0)
