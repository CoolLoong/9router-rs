from http.client import HTTPConnection
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib

server = str(Path(sys.argv[1]).resolve())
root = Path(__file__).resolve().parents[2]


def free():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(port, method, path, headers=None, data=None):
    client = HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = headers or {}
        if data is not None:
            data = json.dumps(data).encode()
            headers["content-type"] = "application/json"
        client.request(method, path, data, headers)
        response = client.getresponse()
        return response.status, response.headers, response.read()
    finally:
        client.close()


def start(port, env):
    return subprocess.Popen([server, "--no-browser", "--port", str(port)], cwd=root, env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def ready(process, port):
    deadline = time.monotonic() + 18
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if request(port, "GET", "/api/health")[0] == 200:
                return
        except OSError:
            time.sleep(.04)
    raise RuntimeError("launcher did not become ready")


def stop(proc):
    if proc.poll() is None:
        proc.terminate()
        proc.wait(timeout=10)


with tempfile.TemporaryDirectory(prefix="9router-native-launcher-") as temporary:
    env = {**os.environ, "HOME": temporary}
    for key in ["RUST_ANYTHING_FALLBACK_LOG","NINEROUTER_MIGRATION_FIXTURE",
                "NINEROUTER_MIGRATION_DATA_DIR","DATA_DIR","JWT_SECRET","INITIAL_PASSWORD","NODE_ENV"]:
        env.pop(key, None)
    directory = Path(temporary, ".9router")
    record = directory / "launcher.pid"
    port = free()
    previous = start(port, env)
    replacement = None
    try:
        ready(previous, port)
        assert record.is_file() and (directory / "db/data.sqlite").is_file()
        login = request(port, "POST", "/api/auth/login", data={"password":"123456"})
        assert login[0] == 200
        auth = {"cookie": login[1]["set-cookie"].split(";", 1)[0]}
        assert request(port, "PATCH", "/api/settings", auth, {"rtkEnabled": False})[0] == 200
        replacement = start(port, env)
        assert previous.wait(timeout=20) == 0
        ready(replacement, port)
        assert request(port, "GET", "/api/settings", auth)[0] == 200
        with sqlite3.connect(directory / "db/data.sqlite") as db:
            settings, = db.execute("SELECT data FROM settings WHERE id=1").fetchone()
        assert json.loads(settings)["rtkEnabled"] is False

        with socket.socket() as stranger:
            stranger.bind(("127.0.0.1", 0))
            stranger.listen()
            unrelated = start(stranger.getsockname()[1], env)
            assert unrelated.wait(timeout=10) == 1
        assert replacement.poll() is None and request(port, "GET", "/api/health")[0] == 200

        assert request(port, "POST", "/api/version/shutdown")[0] == 401
        status, _, body = request(port, "POST", "/api/shutdown", auth)
        assert status == 403 and json.loads(body) == {"success":False,"message":"Not allowed in production"}
        usage = HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            usage.request("GET", "/api/usage/stream", headers=auth)
            live = usage.getresponse()
            assert live.status == 200 and live.readline().startswith(b"data: ")
            status, _, body = request(port, "POST", "/api/version/shutdown", auth)
            assert status == 200 and json.loads(body) == {"success":True,"message":"Shutting down for manual update..."}
            assert live.read().strip() == b""
            replacement.wait(timeout=10)
        finally:
            usage.close()
        assert replacement.returncode == 0 and not record.exists()
        empty_path = Path(temporary, "empty-path")
        empty_path.mkdir()
        standalone = start(port, {**env, "PATH": str(empty_path)})
        try:
            ready(standalone, port)
            assert request(port, "GET", "/api/settings/require-login")[0] == 200
            status, _, body = request(port, "PATCH", "/api/settings", auth, {
                "outboundProxyEnabled":False, "outboundProxyUrl":"http://127.0.0.1:9",
                "comboStrategy":"round-robin", "comboStickyRoundRobinLimit":2,
                "currentPassword":"synthetic-unused-value",
            })
            saved = json.loads(body)
            assert status == 200 and saved["comboStrategy"] == "round-robin" and "currentPassword" not in saved
            with sqlite3.connect(directory / "db/data.sqlite") as db:
                raw, = db.execute("SELECT data FROM settings WHERE id=1").fetchone()
                assert "currentPassword" not in json.loads(raw)
            status, _, body = request(port, "POST", "/api/combos", auth, {
                "name":"standalone-round", "models":["gemini/fixture"],
            })
            assert status == 201
            path = "/api/combos/" + json.loads(body)["id"]
            assert request(port, "PUT", path, auth, {"models":["gemini/fixture-b"]})[0] == 200
            assert request(port, "DELETE", path, auth)[0] == 200
            assert request(port, "PATCH", "/api/settings", auth, {"comboStrategy":"fallback"})[0] == 200
            status, _, body = request(port, "GET", "/__9router-ui/cli-status", auth)
            states = json.loads(body)
            assert status == 200 and isinstance(states["claude"]["installed"], bool)
            assert isinstance(states["codex"]["installed"], bool) and states["cowork"]["installed"] is False
            devin = empty_path / "devin"
            devin.write_text("#!/bin/sh\nprintf '%s\n' 'devin fixture-1' 'ignored line'\n")
            devin.chmod(0o700)
            status, _, body = request(port, "GET", "/api/cli-tools/devin-settings", auth)
            assert status == 200 and json.loads(body)["source"] == "path"
            assert json.loads(body)["version"] == "devin fixture-1"
            grok = Path(temporary) / ".grok/config.toml"
            grok.parent.mkdir()
            grok.write_text('# retained\n[models]\ndefault = "custom-grok"\n\n[subagents.models]\nexplore = "old-explore"\n\n[user]\ncolor = "green"\n')
            route = "/api/cli-tools/grok-build-settings"
            settings = {"model":"fixture/main","baseUrl":"http://localhost:20001","apiKey":"synthetic-grok-key",
                        "contextWindow":133700,"subagentModels":{"explore":{"model":"fixture/explore","contextWindow":64000}}}
            assert request(port, "POST", route, auth, settings)[0] == 200
            content = tomllib.loads(grok.read_text())
            assert content["models"]["default"] == "9router"
            assert content["model"]["9router"]["context_window"] == 133700
            assert content["model"]["9router-explore"]["context_window"] == 64000
            assert request(port, "POST", route, auth, {key:value for key,value in settings.items() if key != "subagentModels"})[0] == 200
            assert tomllib.loads(grok.read_text())["subagents"]["models"]["explore"] == "9router-explore"
            status, _, body = request(port, "GET", route, auth)
            assert status == 200 and b"synthetic-grok-key" not in body
            assert request(port, "DELETE", route, auth)[0] == 200
            restored = tomllib.loads(grok.read_text())
            assert restored == {"models":{"default":"custom-grok"}, "subagents":{"models":{"explore":"old-explore"}}, "user":{"color":"green"}}
            assert "# retained" in grok.read_text()
            copilot = Path(temporary, ".config/Code/User/chatLanguageModels.json")
            copilot.parent.mkdir(parents=True)
            copilot.write_text('[{"name":"Other","models":[{"id":"fixture/direct",},],},]')
            route = "/api/cli-tools/copilot-settings"
            assert request(port, "GET", route, auth)[0] == 200
            config = {"baseUrl":"http://localhost:20002/v1","models":["fixture/one","fixture/two"],
                      "apiKey":"synthetic-copilot-key"}
            assert request(port, "POST", route, auth, config)[0] == 200
            written = json.loads(copilot.read_text())
            assert written[0] == {"name":"Other","models":[{"id":"fixture/direct"}]}
            assert written[1]["apiKey"] == "synthetic-copilot-key"
            status, _, content = request(port, "GET", route, auth)
            assert status == 200 and b"synthetic-copilot-key" not in content
            assert json.loads(content)["currentUrl"] == "http://localhost:20002/v1/chat/completions#models.ai.azure.com"
            assert request(port, "DELETE", route, auth)[0] == 200
            assert json.loads(copilot.read_text()) == written[:1]
            copilot.write_text("{ malformed")
            assert request(port, "POST", route, auth, config)[0] == 500
            assert copilot.read_text() == "{ malformed"

            deepseek = Path(temporary, ".deepseek/config.toml")
            deepseek.parent.mkdir()
            deepseek.write_text('provider = "deepseek"\n\n# Keep unrelated values\n[ui]\ncolor = true\n')
            route = "/api/cli-tools/deepseek-tui-settings"
            assert request(port, "POST", route, auth, {
                "baseUrl":"http://localhost:20003","model":'quoted"model',
                "apiKey":"synthetic-deepseek-key",
            })[0] == 200
            written = tomllib.loads(deepseek.read_text())
            assert written["providers"]["openai"] == {
                "base_url":"http://localhost:20003/v1","model":'quoted"model',
                "api_key":"synthetic-deepseek-key",
            }
            status, _, content = request(port, "GET", route, auth)
            assert status == 200 and b"synthetic-deepseek-key" not in content
            assert json.loads(content)["has9Router"] is True
            status, headers, content = request(port, "GET", "/api/cli-tools/all-statuses", auth)
            assert status == 200 and headers["cache-control"] == "no-store"
            states = json.loads(content)
            assert len(states) == 19 and states["deepseek-tui"]["has9Router"] is True
            assert b"synthetic-deepseek-key" not in content
            assert request(port, "DELETE", route, auth)[0] == 200
            assert tomllib.loads(deepseek.read_text()) == {"provider":"deepseek", "ui":{"color":True}}
            assert "# Keep unrelated values" in deepseek.read_text()
            status, _, body = request(port, "GET", "/legacy-only-fixture")
            assert status == 503 and json.loads(body) == {"error":"Legacy runtime unavailable for this route"}
        finally:
            stop(standalone)
        assert not record.exists()

        legacy_path = Path(temporary, "legacy-fixture-path")
        legacy_path.mkdir()
        legacy_calls = Path(temporary, "legacy-fixture-calls.txt")
        shell = legacy_path / "sh"
        shell.write_text(f"#!{sys.executable}\n" + '''
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path

calls = Path(os.environ["LEGACY_FIXTURE_CALLS"])
class Legacy(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_PATCH(self):
        if self.headers.get("Transfer-Encoding") == "chunked":
            raw = bytearray()
            while count := int(self.rfile.readline().strip(), 16):
                raw.extend(self.rfile.read(count))
                self.rfile.readline()
            self.rfile.readline()
        else:
            raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw)
        mode = body["comboStrategy"]
        with calls.open("a") as log:
            log.write(mode + "\\n")
        attempts = calls.read_text().splitlines().count(mode)
        if mode == "round-robin" and attempts == 3:
            status, result = 200, {"comboStrategy": mode}
        elif mode == "fusion":
            status, result = 500, {"error": "synthetic other error"}
        else:
            status, result = 500, {"error": "database is locked"}
        raw = json.dumps(result).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Legacy).serve_forever()
''')
        shell.chmod(0o700)
        with_legacy = start(port, {**env, "PATH": str(legacy_path), "LEGACY_FIXTURE_CALLS": str(legacy_calls)})
        try:
            ready(with_legacy, port)
            assert request(port, "PATCH", "/api/settings", auth, {"comboStrategy":"round-robin"})[0] == 200
            status, _, body = request(port, "PATCH", "/api/settings", auth, {
                "comboStrategy":"fusion", "outboundProxyEnabled":False,
            })
            assert status == 500 and json.loads(body)["error"] == "synthetic other error"
            status, _, body = request(port, "PATCH", "/api/settings", auth, {"comboStrategy":"smart"})
            assert status == 500 and json.loads(body)["error"] == "database is locked"
            assert legacy_calls.read_text().splitlines() == ["round-robin"] * 3 + ["fusion"] + ["smart"] * 3
            status, _, body = request(port, "PATCH", "/api/settings", auth, {
                "comboStrategies":{"fixture-fusion":{"fallbackStrategy":"fusion"}},
            })
            assert status == 200 and json.loads(body)["comboStrategies"]["fixture-fusion"]["fallbackStrategy"] == "fusion"
            assert legacy_calls.read_text().splitlines() == ["round-robin"] * 3 + ["fusion"] + ["smart"] * 3
        finally:
            stop(with_legacy)
        assert not record.exists()
        print("Rust launcher preserved ownership, drained an authenticated live feed and served native routes without Node or a shell")
    finally:
        stop(previous)
        if replacement is not None:
            stop(replacement)
