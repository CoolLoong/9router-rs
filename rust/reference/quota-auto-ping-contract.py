import datetime
import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


binary = str(Path(sys.argv[1]).resolve())
repository = Path(__file__).resolve().parents[2]
tokens = {
    "claude-ready": "claude",
    "claude-exhausted": "claude",
    "claude-inactive": "claude",
    "codex-schedule": "codex",
    "codex-reactive": "codex",
    "codex-blocked": "codex",
    "codex-failed": "codex",
    "codex-proxied": "codex",
    "codex-recent": "codex",
    "codex-disabled": "codex",
}


def iso(offset=0):
    now = datetime.datetime.now(datetime.timezone.utc)
    return (now + datetime.timedelta(seconds=offset)).isoformat(timespec="milliseconds").replace("+00:00", "Z")


requests = Counter()
ping_inputs = {}
codex_draining = threading.Event()
release_codex = threading.Event()
disable_during_probe = threading.Event()
release_probe = threading.Event()
initial_reset = iso(-15)


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def account(self, action):
        tag = self.headers["Authorization"].removeprefix("Bearer synthetic-")
        assert self.path == "/quota-autoping/" + tokens[tag] + "/" + action
        requests[(action, tag)] += 1
        return tag

    def send(self, status, payload, content_type="application/json"):
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        tag = self.account("usage")
        if tag == "codex-disabled":
            disable_during_probe.set()
            assert release_probe.wait(10)
        key = "session (5h)" if tokens[tag] == "claude" else "session"
        reset_at = initial_reset
        if tokens[tag] == "codex":
            reset_at = iso(7200 + requests[("usage", tag)] * 60)
        remaining = 0 if tag == "claude-exhausted" else 99
        weekly = 0 if tag in {"codex-blocked", "claude-ready"} else 99
        result = {"quotas": {
            key: {"remaining": remaining, "resetAt": reset_at},
            "weekly": {"used": 100 - weekly, "total": 100},
        }}
        self.send(200, json.dumps(result).encode())

    def do_POST(self):
        tag = self.account("ping")
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        ping_inputs[tag] = body
        if tag == "codex-failed":
            self.send(429, b'{"fixture":"rejected"}')
            return
        if tokens[tag] == "claude":
            self.send(200, b'{"fixture":"accepted"}')
            return
        payload = b'data: {"type":"response.completed"}\n\n'
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload[:10])
        self.wfile.flush()
        if tag == "codex-schedule":
            codex_draining.set()
            assert release_codex.wait(10)
        self.wfile.write(payload[10:])


def call(port, method, path, data=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        headers = dict(headers or {})
        if data is not None:
            headers["content-type"] = "application/json"
        connection.request(method, path, json.dumps(data) if data is not None else None, headers)
        response = connection.getresponse()
        return response.status, response.headers, response.read()
    finally:
        connection.close()


def until(check, label, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.025)
    raise AssertionError(label)


with tempfile.TemporaryDirectory(prefix="9router-quota-ping-") as temporary:
    home = Path(temporary)
    commands = home / "path"
    commands.mkdir()
    data = home / "data"
    fallback = home / "fallbacks"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "PATH": str(commands), "HOME": str(home), "PORT": str(port), "DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
    }
    process = subprocess.Popen([binary, "--no-browser"], env=env, cwd=repository,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    mock = ThreadingHTTPServer(("127.0.0.1", 52989), Upstream)
    worker = threading.Thread(target=mock.serve_forever, daemon=True)
    worker.start()
    try:
        def ready():
            try:
                return call(port, "GET", "/api/health")[0] == 200
            except OSError:
                return False

        until(ready, "native server did not start")
        status, headers, body = call(port, "POST", "/api/auth/login", {"password": "123456"})
        assert status == 200, body
        auth = {"cookie": headers["set-cookie"].split(";", 1)[0]}
        store = data / "db/data.sqlite"
        with sqlite3.connect(store) as db:
            for priority, (name, provider) in enumerate(tokens.items(), 1):
                account = {"accessToken": "synthetic-" + name}
                if name == "codex-proxied":
                    account["providerSpecificData"] = {
                        "connectionProxyEnabled": True,
                        "connectionProxyUrl": "http://127.0.0.1:1",
                    }
                if name == "codex-recent":
                    account["lastPingAt"] = iso(-60)
                db.execute("""INSERT INTO providerConnections
                    (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                    VALUES (?,?,'oauth',?,?,?,?,?,?)""",
                    (name, provider, name, priority, name != "claude-inactive", json.dumps(account), iso(), iso()))

        def load(name):
            with sqlite3.connect(store) as db:
                raw, = db.execute("SELECT data FROM providerConnections WHERE id=?", (name,)).fetchone()
            return json.loads(raw)

        clock = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=2)
        schedule = {"enabled": True, "anchor": clock.strftime("%H:%M"), "timezone": "UTC",
                    "windowHours": 5, "slotsPerDay": 1}
        slot_key = f"{clock:%Y-%m-%d}#0"
        keys = {"claudeAutoPing": {"connections": {
            "claude-ready": True, "claude-exhausted": True, "claude-inactive": True,
        }}, "codexAutoPing": {"connections": {"codex-reactive": True},
            "schedules": {name: schedule for name in tokens if name.startswith("codex-") and name != "codex-reactive"},
        }}
        assert call(port, "PATCH", "/api/settings", keys, auth)[0] == 200
        assert codex_draining.wait(8), requests
        assert "lastPingAt" not in load("codex-schedule")
        assert load("claude-ready")["lastPingedResetKey"].endswith(":00.000Z")
        release_codex.set()
        assert disable_during_probe.wait(8), requests
        del keys["codexAutoPing"]["schedules"]["codex-disabled"]
        assert call(port, "PATCH", "/api/settings", keys, auth)[0] == 200
        release_probe.set()
        until(lambda: "lastPingAt" in load("codex-reactive"), "sliding reset did not auto-ping on second sample")
        assert load("codex-schedule")["lastPingedSlotKey"] == slot_key
        assert requests[("ping", "claude-ready")] == requests[("ping", "codex-schedule")] == 1
        assert requests[("ping", "codex-reactive")] == requests[("ping", "codex-failed")] == 1
        for tag in ["claude-exhausted", "claude-inactive", "codex-blocked",
                    "codex-failed", "codex-proxied", "codex-disabled"]:
            assert "lastPingAt" not in load(tag), tag
        assert requests[("ping", "codex-disabled")] == requests[("ping", "codex-recent")] == 0
        assert requests[("usage", "codex-proxied")] == requests[("usage", "claude-inactive")] == 0
        assert ping_inputs["claude-ready"] == {"model": "claude-haiku-4-5-20251001",
            "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}
        for tag in ("codex-schedule", "codex-reactive"):
            assert ping_inputs[tag]["model"] == "gpt-5.6-terra"
            assert ping_inputs[tag]["input"][0]["content"][0]["text"] == "hi"
            assert ping_inputs[tag]["stream"] is True
            assert ping_inputs[tag]["store"] is False
        assert fallback.read_text() == ""
        print("Quota auto-ping checked ten accounts, stream completion, cooldowns, sliding/anchored schedules and opt-out without Node")
    finally:
        release_codex.set()
        release_probe.set()
        process.terminate()
        process.wait(timeout=10)
        mock.shutdown()
        mock.server_close()
