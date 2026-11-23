from contextlib import ExitStack, contextmanager
import http.client
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = runpy.run_path(str(ROOT / "rust/reference/zed-cline-models-proxy-contract.py"))
ADMIN = FIXTURE["ADMIN"]
AUTH = FIXTURE["ZED_ACCESS"]
SYSTEM = FIXTURE["ZED_SYSTEM"]
MODEL = "fixture-live-zed"
SEND = "/api/translator/send"
CALLS, ERRORS = [], []
LEASES = FIXTURE["LEASES"]
LIVE = {"role": "user", "content": "Send through Translator 世界"}
BODY = {"model": MODEL, "stream": True, "messages": [LIVE], "thread_id": "test-thread", "prompt_id": "test-prompt"}
WIRE = {"id": "fixture-xai", "object": "chat.completion.chunk", "created": 1784419200,
        "model": MODEL, "choices": [{"index": 0, "delta": {"content": "fixture 世界"}, "finish_reason": "stop"}]}
FULL = {"id": "fixture-json", "object": "chat.completion", "created": 1784419200,
        "model": MODEL, "choices": [{"index": 0, "message": {"role":"assistant", "content":"fixture JSON 世界"},
                                    "finish_reason": "stop"}]}


class Vendor(FIXTURE["Vendor"]):
    def do_POST(self):
        if self.path not in ("/zed-fixture/completions", "/completions"):
            return super().do_POST()
        try:
            direct = self.server.server_port == 52989
            assert self.path == ("/zed-fixture/completions" if direct else "/completions")
            if not direct:
                assert self.headers["Host"] == "cloud.zed.dev"
            for name in ("Proxy-Authorization", "X-Relay-Target", "X-Relay-Path", "x-zed-system-id", "X-Session-Id"):
                assert self.headers[name] is None
            token = self.headers["Authorization"].removeprefix("Bearer ")
            assert token in LEASES
            assert self.headers["Accept"] == "application/x-ndjson, text/event-stream, */*"
            assert self.headers["User-Agent"] == "9router/zed"
            assert self.headers["x-zed-client-supports-status-messages"] == "true"
            assert self.headers["x-zed-client-supports-stream-ended-request-completion-status"] == "true"
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompt = data["provider_request"]["messages"][0]["content"]
            streamed = prompt != "single JSON"
            assert data == {"provider": "x_ai", "model": MODEL, "thread_id": "test-thread", "prompt_id": "test-prompt",
                            "provider_request": BODY | {"stream": streamed, "messages": [{"role": "user", "content": prompt}]}}, data
            CALLS.append((direct, prompt, token))
            if prompt == "echo error":
                return self.reply(403, {"error": {"message": f"upstream echoed {AUTH}, {SYSTEM}, and {token}"}})
            if prompt == "redirect":
                return self.reply(307, {"error": "redirected"}, "https://api.cline.bot/elsewhere")
            if prompt == "retry lease" and sum(call[1] == prompt for call in CALLS) == 1:
                return self.reply(401, {"error": "expired"})
            chunk = WIRE
            if prompt == "echo stream":
                chunk = WIRE | {"debug": f"{AUTH}/{SYSTEM}/{token}"}
            if prompt == "retry lease":
                older = next(old for seen_direct, label, old in CALLS if label == prompt and seen_direct == direct)
                assert older != token
                chunk = WIRE | {"debug": f"{older}/{token}"}
            if streamed:
                raw = (json.dumps({"event": chunk}, ensure_ascii=False) + '\n{"status":"stream_ended"}\n').encode()
            else:
                raw = json.dumps(FULL, ensure_ascii=False).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson" if streamed else "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            middle = len(raw) // 2
            self.wfile.write(raw[:middle])
            self.wfile.flush()
            self.wfile.write(raw[middle:])
        except (AssertionError, ValueError, KeyError) as error:
            ERRORS.append(repr(error))
            self.reply(422, {"error": "Unexpected fixture"})


def post(port, path, payload, headers=ADMIN):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        connection.request("POST", path, json.dumps(payload),
                           {"Content-Type":"application/json", "X-Session-Id":"do-not-bind", **headers})
        response = connection.getresponse()
        return response.status, response.headers["Content-Type"].split(";")[0], response.read()
    finally:
        connection.close()


def send(port, prompt=LIVE["content"], *, body=None, headers=ADMIN):
    if body is None:
        body = BODY | {"stream": prompt != "single JSON", "messages": [{"role": "user", "content": prompt}]}
    return post(port, SEND, {"provider": "zed", "model": MODEL, "body": body}, headers)


@contextmanager
def pinned(scratch):
    home = scratch / "reference"
    home.mkdir()
    data = home / "data"
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True,
                   env={"PATH": os.environ["PATH"], "HOME": str(home)}, stderr=subprocess.DEVNULL)
    FIXTURE["save"](data / "db/data.sqlite", "zed")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"DATA_DIR":str(data), "PORT":str(port), "HOSTNAME":"127.0.0.1", "HOME":str(home),
           "PATH":os.environ["PATH"], "ENABLE_TRANSLATOR":"true", "MODEL_CATALOG_SYNC":"off"}
    command = ["node", "--import", "./rust/reference/mock-zed-runtime.mjs", ".next/standalone/custom-server.js"]
    with subprocess.Popen(command, env=env, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if FIXTURE["get"](port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise ValueError("Pinned Translator did not start")
            yield port
        finally:
            process.terminate()
            process.wait(timeout=7)


def raw_frames(port):
    results = [send(port), send(port,"single JSON")]
    for reply, expected in zip(results, (WIRE, FULL)):
        status, mime, raw = reply
        assert (status, mime) == (200,"text/event-stream"), reply
        frames = [line[6:] for line in raw.splitlines() if line.startswith(b"data: ")]
        assert frames[-1] == b"[DONE]" and len(frames) == 2 and json.loads(frames[0]) == expected, reply
    return results


def protected_cases(port, db, address, proxy, wrong_host):
    details = {"connectionProxyEnabled":True, "connectionProxyUrl":address}
    FIXTURE["save"](db,"zed",details)
    before = len(CALLS)
    assert send(port, headers={})[0] == 401
    assert send(port, body={"provider":"x_ai", "provider_request":BODY})[0] == 400
    assert len(CALLS) == before
    status, _, raw = send(port,"echo error")
    assert status == 403 and json.loads(json.loads(raw)["details"]) == {
        "error":{"message":"upstream echoed [redacted], [redacted], and [redacted]"},
    }
    status, _, raw = send(port,"echo stream")
    assert status == 200 and b"[redacted]/[redacted]/[redacted]" in raw
    if proxy.mode == "connect":
        before = len(CALLS)
        status, _, raw = send(port,"retry lease")
        assert status == 200 and b"[redacted]/[redacted]" in raw and len(CALLS) == before + 2
        assert all(secret.encode() not in raw for secret in LEASES)
    status, _, raw = send(port,"redirect")
    assert status == 502 and b"redirected the request" in raw, (status, raw, ERRORS)
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            SELECT 'direct-backup',provider,authType,name,2,1,json_remove(data,'$.providerSpecificData.connectionProxyEnabled',
            '$.providerSpecificData.connectionProxyUrl'),createdAt,updatedAt FROM providerConnections WHERE id='zed'""")
    before = (len(CALLS), len(FIXTURE["TUNNELS"]))
    proxy.reject = True
    assert send(port)[0] == 502
    assert (len(CALLS), len(FIXTURE["TUNNELS"])) == (before[0], before[1] + 1)
    proxy.reject, proxy.target = False, wrong_host
    assert send(port)[0] == 502 and len(CALLS) == before[0]
    before_tunnels = len(FIXTURE["TUNNELS"])
    for pool in ("missing-pool", "fixture-relay"):
        FIXTURE["save"](db,"zed",{"proxyPoolId":pool})
        assert send(port)[0] == 502
    assert len(CALLS) == before[0] and len(FIXTURE["TUNNELS"]) == before_tunnels


with tempfile.TemporaryDirectory(prefix="9router-zed-translator-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    root_ca, upstream, wrong_host = FIXTURE["tls_services"](stack, scratch, Vendor)
    stack.enter_context(FIXTURE["serving"](ThreadingHTTPServer(("127.0.0.1",52989), Vendor)))
    with pinned(scratch) as port:
        reference = raw_frames(port)
        status, _, raw = post(port, "/api/translator/translate", {"step":3,"body":BODY | {"provider":"zed"}})
        assert status == 500 and b"baseUrls" in raw, (status, raw)
    with FIXTURE["gateway"](scratch,"native-direct",None,[]) as (port, db):
        FIXTURE["save"](db,"zed")
        assert raw_frames(port) == reference
    for mode in ("connect","socks5h"):
        proxy = FIXTURE["Proxy"](("127.0.0.1",0),FIXTURE["Tunnel"])
        proxy.mode, scheme = ("connect","http") if mode == "connect" else ("socks","socks5h")
        proxy.reject, proxy.target = False, upstream
        stack.enter_context(FIXTURE["serving"](proxy))
        address = f"{scheme}://127.0.0.1:{proxy.server_address[1]}"
        before = len(CALLS)
        with FIXTURE["gateway"](scratch,mode,root_ca,[("fixture-relay","cloudflare",address)]) as (port, db):
            FIXTURE["save"](db,"zed",{"connectionProxyEnabled":True,"connectionProxyUrl":address})
            assert raw_frames(port) == reference
            protected_cases(port, db, address, proxy, wrong_host)
        assert all(not direct for direct, _, _ in CALLS[before:])
    assert all(host == "cloud.zed.dev" for host in FIXTURE["HELLOS"])
    assert not ERRORS and not FIXTURE["ERRORS"], (ERRORS, FIXTURE["ERRORS"])
print("Zed Translator Send matched pinned raw executor frames over direct, CONNECT and SOCKS5h; failures stayed on the selected route.")
