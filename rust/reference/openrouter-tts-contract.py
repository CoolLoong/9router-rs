import base64
from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "openrouter_speech_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "openrouter.ai"
ENDPOINT = "/api/v1/chat/completions"
KEY = "offline-openrouter-speech-primary"
RELAY = "https://speech-fixture.vercel.app/api/relay"
RELAY_SECRET = "g" * 43
DEFAULT = "openai/gpt-4o-mini-tts"
AUDIO = b"RIFF synthetic split OpenRouter WAV data"
ENCODED = base64.b64encode(AUDIO).decode()
shared.ZED_HOST, shared.CLINE_HOST = HOST, "test-tts.invalid"
shared.SECRETS = (KEY, RELAY_SECRET)
calls, problems = [], []
behavior = "success"


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, payload, content_type="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        for offset in range(0, len(payload), 17):
            self.wfile.write(payload[offset:offset + 17])
            self.wfile.flush()

    def do_POST(self):
        try:
            if self.path == "/vercel-relay/speech-fixture.vercel.app":
                lane = "relay"
                assert self.headers["X-Relay-Auth"] == RELAY_SECRET
                assert self.headers["X-Relay-Target"] == "https://" + HOST
                assert self.headers["X-Relay-Path"] == ENDPOINT
            elif self.server.server_port == 52989:
                lane = "direct"
                assert self.path == "/speech-openrouter" + ENDPOINT
            else:
                lane = "tls"
                assert self.headers["Host"] == HOST and self.path == ENDPOINT
            assert self.headers["Authorization"] == "Bearer " + KEY
            assert self.headers["HTTP-Referer"] == "https://endpoint-proxy.local"
            assert self.headers["X-Title"] == "Endpoint Proxy"
            assert self.headers["Cookie"] is None and self.headers["Proxy-Authorization"] is None
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            text = body["messages"][0]["content"]
            choices = {
                "custom model": ("acme/fixture-voice", "echo"),
                "legacy voice": (DEFAULT, "alias/voice"),
                "no audio": (DEFAULT, "no-audio"),
                "upstream error": (DEFAULT, "upstream-error"),
            }
            model, voice = choices.get(text, (DEFAULT, "alloy"))
            assert body == {"model": model, "modalities": ["text", "audio"],
                            "audio": {"voice": voice, "format": "wav"}, "stream": True,
                            "messages": [{"role": "user", "content": text}]}, body
            calls.append((lane, text))
            if behavior == "reject" or text == "upstream error":
                message = "OpenRouter fixture error"
                if behavior == "reject":
                    message += " " + KEY + (" " + RELAY_SECRET if lane == "relay" else "")
                return self.reply(403, json.dumps({"error": {"message": message}}).encode())
            if behavior == "redirect":
                return self.reply(307, b"{}", location="https://test-tts.invalid/no-follow")
            if text == "no audio":
                return self.reply(200, b'data: {"choices":[]}\n\ndata: [DONE]\n', "text/event-stream")
            def event(part):
                return "data: " + json.dumps({"choices": [{"delta": {"audio": {"data": part}}}]})
            content = "\n".join([": fixture comment", event(ENCODED[:9]), "data: invalid event",
                                  event(ENCODED[9:]), "data: [DONE]", event("ignored incomplete line")])
            return self.reply(200, content.encode(), "text/event-stream")
        except (AssertionError, KeyError, ValueError) as error:
            problems.append(str(error) or self.path)
            self.reply(422, b'{"error":{"message":"Unexpected speech fixture request"}}')


def write_account(db, **details):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('openrouter-tts','openrouter','apikey','Fixture OpenRouter',1,1,?,
                    '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps({"apiKey": KEY, "providerSpecificData": details}),))


@contextmanager
def original(scratch):
    home = scratch / "original"
    home.mkdir()
    data = home / "data"
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "DATA_DIR": str(data), "NODE_NO_WARNINGS": "1",
           "HOSTNAME": "127.0.0.1", "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
    subprocess.run([sys.executable, str(shared.ROOT / "rust/reference/seed.py"), str(data)], env=env, check=True)
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env["PORT"] = str(port)
    standalone = shared.ROOT / ".next/standalone/server.js"
    rewrite = "import { createRequire } from 'node:module';\nconst require = createRequire(" \
        + json.dumps(str(standalone)) + ");\n" + """
const route = request => (input, init) => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
  if (url === 'https://openrouter.ai/api/v1/chat/completions') {
    const target = 'http://127.0.0.1:52989/speech-openrouter/api/v1/chat/completions';
    return request(input instanceof Request ? new Request(target, input) : target, init);
  }
  return request(input, init);
};
globalThis.fetch = route(globalThis.fetch.bind(globalThis));
const undici = require('undici');
undici.fetch = route(undici.fetch);
"""
    command = ["node", "--import", "data:text/javascript," + quote(rewrite),
               str(shared.ROOT / ".next/standalone/custom-server.js")]
    process = subprocess.Popen(command, cwd=home, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and process.poll() is None:
            try:
                if shared.get(port, "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.03)
        else:
            raise RuntimeError("Original speech fixture did not start")
        yield port, db
    finally:
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def voice(port, text="default speech", model="alloy", format="json"):
    with closing(HTTPConnection("127.0.0.1", port, timeout=7)) as request:
        request.request("POST", f"/v1/audio/speech?response_format={format}", json.dumps({
            "model": "openrouter/" + model, "input": text,
        }), {"Content-Type": "application/json", "Authorization": "Bearer offline-chat-key", "Cookie": "local-only=1"})
        response = request.getresponse()
        content_type = response.headers["Content-Type"]
        data = response.read()
        assert all(secret.encode() not in data for secret in shared.SECRETS)
        payload = json.loads(data) if "application/json" in content_type else base64.b64encode(data).decode()
        return response.status, content_type, payload


def direct(port, db):
    write_account(db)
    options = [("default speech", "alloy", "json"), ("custom model", "acme/fixture-voice/echo", "wav"),
               ("legacy voice", "alias/voice", "mp3"), ("no audio", "no-audio", "json"),
               ("upstream error", "upstream-error", "json")]
    results = [voice(port, *case) for case in options]
    assert results[0] == (200, "application/json", {"audio": ENCODED, "format": "wav"}), results
    assert results[1] == results[2] == (200, "audio/wav", ENCODED), results
    assert results[3][0] == results[4][0] == 502, results
    assert "no audio data" in results[3][2]["error"]["message"], results
    assert "fixture error" in results[4][2]["error"]["message"], results
    assert [lane for lane, _ in calls] == ["direct"] * len(options), (calls, problems)
    return results


def protected(port, db, stack, tls, wrong_name, baseline):
    global behavior
    proxies = {}
    for kind, scheme in [("connect", "http"), ("socks", "socks5h")]:
        proxy = shared.Proxy(("127.0.0.1", 0), shared.Tunnel)
        proxy.mode, proxy.reject, proxy.target = kind, False, tls
        stack.enter_context(shared.serving(proxy))
        address = f"{scheme}://127.0.0.1:{proxy.server_address[1]}"
        write_account(db, connectionProxyEnabled=True, connectionProxyUrl=address)
        assert voice(port) == baseline and shared.TUNNELS[-1] == (kind, HOST, 443)
        proxies[kind] = (proxy, address)
    connect, address = proxies["connect"]
    write_account(db, connectionProxyEnabled=True, connectionProxyUrl=address, connectionNoProxy=HOST)
    before = len(shared.TUNNELS)
    assert voice(port) == baseline and calls[-1] == ("direct", "default speech")
    assert len(shared.TUNNELS) == before

    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES ('relay',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps({"type": "vercel", "proxyUrl": RELAY,
                                 "_managedVercel": {"url": RELAY, "token": RELAY_SECRET}}),))
    write_account(db, proxyPoolId="relay")
    assert voice(port) == baseline and calls[-1] == ("relay", "default speech")
    behavior = "reject"
    status, _, error = voice(port)
    assert status == 502 and error["error"]["message"].count("[redacted]") == 2, error

    behavior = "redirect"
    write_account(db, connectionProxyEnabled=True, connectionProxyUrl=address)
    before = len(calls), len(shared.TUNNELS)
    assert voice(port)[0] == 502
    connect.target = wrong_name
    assert voice(port)[0] == 502
    connect.reject = True
    assert voice(port)[0] == 502
    assert (len(calls), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 3)


with tempfile.TemporaryDirectory(prefix="9router-openrouter-tts-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    if sys.argv[2] == "reference":
        with original(scratch) as (port, db):
            results = direct(port, db)
    else:
        ca, tls, wrong_name = shared.tls_services(stack, scratch, Vendor)
        with shared.gateway(scratch, "native", ca, []) as (port, db):
            results = direct(port, db)
            protected(port, db, stack, tls, wrong_name, results[0])
    assert not problems and not shared.ERRORS, (problems, shared.ERRORS)
    print(json.dumps(results, sort_keys=True, separators=(",", ":")))
