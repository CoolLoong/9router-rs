import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event, Thread
import time

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
routes = ("/api/v1/api/chat", "/v1/api/chat", "/v1/v1/api/chat")
model = "fixture-live/gpt-5.6-sol"
ledger = []
next_chunk = Event()
usage = {"prompt_tokens":13, "completion_tokens":4, "total_tokens":17}


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, mime, raw):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        try:
            assert self.path == "/ollama-public-fixture/v1/chat/completions", self.path
            assert self.headers["Authorization"] == "Bearer offline-models-provider"
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert payload["model"] == "gpt-5.6-sol", payload
            label = payload["messages"][-1]["content"]
            ledger.append(label)
            if label == "http-error":
                self.send(422, "application/json", b'{"error":{"message":"synthetic HTTP failure"}}')
                return
            if label == "non-stream":
                assert payload["stream"] is False, payload
                self.send(200, "application/json", json.dumps({
                    "id":"chatcmpl-fixture-static","object":"chat.completion","created":1770000000,
                    "model":"upstream-name",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"ignored non-stream reply"},
                                "finish_reason":"stop"}],
                    "usage":usage,
                }).encode())
                return
            assert payload["stream"] is True, payload
            if label == "tools":
                choices = [
                    ({"role":"assistant","tool_calls":[
                        {"index":2,"id":"call_second","type":"function","function":{"name":"second","arguments":"invalid"}},
                        {"index":0,"id":"call_first","type":"function","function":{"name":"first_","arguments":'{"q":'}},
                    ]}, None),
                    ({"tool_calls":[{"index":0,"function":{"name":"tool","arguments":'"ok"}'}}]}, None),
                    ({}, "tool_calls"),
                ]
            else:
                assert label in ("stream", "length", "legacy", "live-stream"), label
                choices = [
                    ({"role":"assistant","reasoning_content":"ignored reasoning"}, None),
                    ({"content":"fixture 文"}, None),
                    ({"content":" response"}, None),
                    ({}, "length" if label == "length" else "stop"),
                ]
            frames = [
                "data: " + json.dumps({
                    "id":"chatcmpl-fixture-stream","object":"chat.completion.chunk","created":1770000000,
                    "model":"upstream-name",
                    "choices":[{"index":0,"delta":delta,"finish_reason":reason}],
                    **({"usage":usage} if reason else {}),
                }, ensure_ascii=False)
                for delta, reason in choices
            ]
            frames.insert(0, ": chat keepalive")
            frames.append("data: [DONE]")
            raw = ("\n\n".join(frames) + "\n\n").encode()
            if label == "live-stream":
                first = ("\n\n".join(frames[:3]) + "\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(first)
                self.wfile.flush()
                if next_chunk.wait(timeout=6):
                    self.wfile.write(raw[len(first):])
                return
            self.send(200, "text/event-stream", raw)
        except AssertionError as failure:
            self.send(422, "application/json", json.dumps({"error":{"message":str(failure)}}).encode())


def invoke(port, path, method="POST", body=None, key="offline-chat-key", extra=None):
    headers = {"Content-Type":"application/json"} | (extra or {})
    if key:
        headers["Authorization"] = "Bearer " + key
    if body is not None and not isinstance(body, bytes):
        body = json.dumps(body)
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        client.request(method, path, body=body, headers=headers)
        reply = client.getresponse()
        return reply.status, reply.headers, reply.read()
    finally:
        client.close()


def prompt(label, stream=True, requested=model):
    return {"model":requested, "stream":stream, "messages":[{"role":"user","content":label}]}


def frame(content="", done=True, requested=model, tools=None):
    message = {"role":"assistant","content":content}
    if tools is not None:
        message["tool_calls"] = tools
    return {"model":requested,"message":message,"done":done}


def scenario(port):
    snapshots = {}

    def call(name, path, body, expected, key="offline-chat-key", extra=None):
        status, headers, raw = invoke(port, path, body=body, key=key, extra=extra)
        assert (status, headers["Content-Type"], headers["Access-Control-Allow-Origin"]) == (
            200, "application/x-ndjson", "*"
        ), (name, status, headers, raw)
        assert raw.endswith(b"\n")
        rows = [json.loads(line) for line in raw.splitlines()]
        assert rows == expected, (name, rows, expected)
        assert headers["Retry-After"] is None and headers["Cache-Control"] is None
        snapshots[name] = rows

    for path in routes:
        status, headers, body = invoke(port, path, method="OPTIONS", key=None)
        assert (status, body) == (200, b"")
        assert headers["Access-Control-Allow-Origin"] == "*"
        assert headers["Access-Control-Allow-Methods"] == "GET, POST, OPTIONS"
        assert headers["Access-Control-Allow-Headers"] == "*"

    tagged = model + "[1m]"
    call("stream", routes[0], prompt("stream", requested=tagged), [
        frame("fixture 文", False, tagged), frame(" response", False, tagged),
        frame(requested=tagged), frame(requested=tagged), frame(requested=tagged),
    ])
    call("tool-deltas", routes[1], prompt("tools"), [
        frame(tools=[{"function":{"name":"first_tool","arguments":{"q":"ok"}}},
                     {"function":{"name":"second","arguments":{}}}]),
        frame(), frame(),
    ])
    call("alternative-finish", routes[2], prompt("length"), [
        frame("fixture 文", False), frame(" response", False), frame(), frame(),
    ])
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        client.request("POST", routes[0], json.dumps(prompt("live-stream")),
                       {"Content-Type":"application/json", "Authorization":"Bearer offline-chat-key"})
        response = client.getresponse()
        assert response.status == 200 and response.headers["Content-Type"] == "application/x-ndjson"
        first = json.loads(response.readline())
        assert first == frame("fixture 文", False)
        next_chunk.set()
        remaining = [json.loads(line) for line in response.read().splitlines()]
        assert remaining == [frame(" response", False), frame(), frame(), frame()]
        snapshots["first-frame-before-upstream-finishes"] = [first, *remaining]
    finally:
        next_chunk.set()
        client.close()
    call("legacy-response", routes[2], prompt("legacy"), [
        frame("fixture 文", False), frame(" response", False), frame(), frame(), frame(),
    ], extra={"X-App":"cli"})
    call("non-stream", routes[0], prompt("non-stream", False), [frame()])
    call("malformed-body", routes[0], b"{", [frame(requested="llama3.2")])
    call("missing-model", routes[0], {"messages":[]}, [frame(requested="llama3.2")])
    call("missing-key", routes[1], prompt("unexpected"), [frame()], key=None)
    call("invalid-key", routes[2], prompt("unexpected"), [frame()], key="offline-invalid-key")
    call("ungranted-model", routes[1], prompt("unexpected"), [frame()], key="offline-restricted-key")
    call("upstream-error", routes[0], prompt("http-error", False), [frame()])
    status, headers, raw = invoke(
        port, routes[1], body=prompt("unexpected"), key=None,
        extra={"Origin":"https://ollama-fixture.invalid"},
    )
    assert (status, headers["Content-Type"], json.loads(raw)) == (
        401, "application/json", {"error":"API key required for remote API access"}
    )
    snapshots["remote-denied"] = json.loads(raw)
    assert ledger == ["stream", "tools", "length", "live-stream", "legacy", "non-stream", "http-error"], ledger
    snapshots["upstream-requests"] = list(ledger)
    return snapshots


def seed(data, upstream):
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], cwd=root,
                   env=dict(os.environ, NODE_NO_WARNINGS="1"), check=True)
    with sqlite3.connect(data / "db/data.sqlite") as database:
        node_id = "openai-compatible-chat-fixture-live"
        url = f"http://127.0.0.1:{upstream}/ollama-public-fixture/v1"
        database.execute("UPDATE providerNodes SET data=json_set(data,'$.baseUrl',?) WHERE id=?", (url, node_id))
        database.execute("""UPDATE providerConnections SET isActive=1,data=json_set(data,'$.providerSpecificData.baseUrl',?)
                            WHERE id='fixture-live-connection' AND provider=?""", (url, node_id))
        database.execute("""INSERT INTO settings(id,data) VALUES(1,'{"requireApiKey":true}')
                            ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.requireApiKey',json('true'))""")


with tempfile.TemporaryDirectory(prefix="9router-ollama-public-") as directory:
    scratch = Path(directory)
    home = scratch / "home"
    data = scratch / "data"
    home.mkdir()
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    Thread(target=upstream.serve_forever, daemon=True).start()
    seed(data, upstream.server_port)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME":str(home), "PATH":os.environ["PATH"], "HOSTNAME":"127.0.0.1", "PORT":str(port),
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_DATA_DIR":str(data), "MODEL_CATALOG_SYNC":"off",
        "NEXT_TELEMETRY_DISABLED":"1", "NODE_NO_WARNINGS":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallback.log"),
    }
    command = [str(native)] if implementation == "rust" else [shutil.which("node"), ".next/standalone/custom-server.js"]
    with (scratch / "server.log").open("w") as log:
        process = subprocess.Popen(command, cwd=root, env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            deadline = time.monotonic() + 15
            while True:
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.15):
                        break
                except OSError:
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError("fixture server did not start")
                    time.sleep(.025)
            result = scenario(port)
            if implementation == "rust":
                assert (scratch / "fallback.log").read_text().splitlines() == [f"POST {routes[2]}"]
            print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        except Exception:
            print((scratch / "server.log").read_text(), file=sys.stderr)
            raise
        finally:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=2)
            upstream.shutdown()
            upstream.server_close()
