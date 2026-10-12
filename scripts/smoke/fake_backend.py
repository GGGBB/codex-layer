#!/usr/bin/env python3
"""假上游：把收到的请求体逐条落盘，回一个最小 Chat Completions 响应。"""
import http.server, json, os, sys, threading

CAPTURE_DIR = sys.argv[1]
PORT = int(sys.argv[2])
os.makedirs(CAPTURE_DIR, exist_ok=True)
counter = {"n": 0}
lock = threading.Lock()

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        with lock:
            counter["n"] += 1
            index = counter["n"]
        with open(os.path.join(CAPTURE_DIR, "req-%02d%s.json" % (index, os.environ.get("TAG", ""))), "wb") as handle:
            handle.write(raw)
        body = json.dumps({
            "id": "chatcmpl-fake", "object": "chat.completion", "model": "fake-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        body = b'{"object":"list","data":[{"id":"fake-model","object":"model"}]}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
