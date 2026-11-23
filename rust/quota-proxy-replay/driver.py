import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import socketserver
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS.pop(0)).resolve())
CASE, = ARGS
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
KEY = "fixture-provider-api-key"
GITHUB = "fixture-github-oauth-access"
COPILOT = "fixture-copilot-access"
REFRESH = "fixture-github-refresh-token"
PROXY = "http://127.0.0.1:52988"
RELAY = "http://127.0.0.1:52989/forward"
NETWORK = []
FAILURES = []
RELAY_CODES = []
PROVIDERS = {
    "clinepass": ("api.cline.bot", "/api/v1/users/me/plan/usage-limits", "Bearer " + KEY,
                  {"data": {"limits": [{"type": "weekly", "percentUsed": 24, "resetsAt": "2030-01-01T00:00:00Z"}]}}),
    "github": ("api.github.com", "/copilot_internal/user", "token " + GITHUB,
               {"copilot_plan": "free", "monthly_quotas": {"chat": 100}, "limited_user_quotas": {"chat": 12},
                "limited_user_reset_date": "2030-01-01T00:00:00Z"}),
    "deepseek": ("api.deepseek.com", "/user/balance", "Bearer " + KEY,
                 {"is_available": True, "balance_infos": [{"currency": "cny", "total_balance": "143.75"}]}),
    "synthetic": ("api.synthetic.new", "/v2/quotas", "Bearer " + KEY,
                  {"subscription": {"limit": 100, "requests": 24, "renewsAt": "2030-01-01T00:00:00Z"}}),
}
ADDITIONAL = {
    "groq": ("api.groq.com", "/openai/v1/models", "Bearer " + KEY, {"data": []}),
    "glm": ("api.z.ai", "/api/monitor/usage/quota/limit", "Bearer " + KEY,
            {"data": {"level": "PRO", "limits": [{"type": "TOKENS_LIMIT", "unit": 6, "percentage": 40}]}}),
    "glm-cn": ("open.bigmodel.cn", "/api/monitor/usage/quota/limit", "Bearer " + KEY,
               {"data": {"level": "PRO", "limits": [{"type": "TOKENS_LIMIT", "unit": 6, "percentage": 75}]}}),
    "vercel-ai-gateway": ("ai-gateway.vercel.sh", "/v1/credits", "Bearer " + KEY,
                          {"balance": "2.60", "total_used": "1.4"}),
    "opencode-go": ("opencode.ai", "/inference/go/v1/usage", "Bearer " + KEY,
                    {"usage": {"rolling": {"percent": 16, "resetsAt": "2030-01-01T00:00:00Z"}}}),
    "opencode-zen": ("opencode.ai", "/zen/v1/usage", "Bearer " + KEY,
                     {"usage": {"weekly": {"percent": 47, "resetsAt": "2030-01-01T00:00:00Z"}}}),
}
ALL_PROVIDERS = {**PROVIDERS, **ADDITIONAL}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, data, **headers):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in headers.items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        try:
            assert self.server.server_port == 52989 and self.path == "/forward", self.path
            target, path = self.headers.get("X-Relay-Target"), self.headers.get("X-Relay-Path")
            provider, = [provider for provider, (host, leaf, _, _) in ALL_PROVIDERS.items()
                         if target == "https://" + host and path == leaf]
            _, _, token, result = ALL_PROVIDERS[provider]
            assert self.headers.get_all("Authorization") == [token]
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("Content-Length") in {None, "0"}
            if provider == "github":
                assert self.headers.get("X-GitHub-Api-Version") == "2022-11-28"
                assert self.headers.get("User-Agent") == "GitHubCopilotChat/0.26.7"
                assert self.headers.get("Editor-Version") == "vscode/1.100.0"
                assert self.headers.get("Editor-Plugin-Version") == "copilot-chat/0.26.7"
            else:
                assert self.headers.get("X-Client-Type") is None
            NETWORK.append({"route": "relay", "method": "GET", "target": target, "path": path})
            status = RELAY_CODES.pop(0) if RELAY_CODES else 200
            match status:
                case 302:
                    self.reply(status, {}, Location="http://127.0.0.1:52988/other-origin")
                case 503:
                    self.reply(status, {"message": "fixture relay rejected " + KEY})
                case _ if provider == "groq":
                    self.reply(status, result, **{"X-Ratelimit-Limit-Requests": "100", "X-Ratelimit-Remaining-Requests": "40"})
                case _:
                    self.reply(status, result)
        except (AssertionError, ValueError) as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {})

    def do_CONNECT(self):
        try:
            assert self.server.server_port == 52988
            assert self.path == "api.cline.bot:443"
            assert self.headers.get("Authorization") is None
            assert self.headers.get("X-Relay-Target") is None
            NETWORK.append({"route": "http-proxy", "method": "CONNECT", "target": self.path})
            self.reply(502, {})
        except AssertionError as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {})

    def do_POST(self):
        FAILURES.append("Unexpected outgoing POST " + self.path)
        self.reply(422, {})


class Direct(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        header = self.request.recv(5, socket.MSG_WAITALL)
        if len(header) != 5 or header[:2] != b"\x16\x03":
            FAILURES.append("Direct fallback did not start TLS")
            return
        hello = self.request.recv(int.from_bytes(header[3:], "big"), socket.MSG_WAITALL)
        if b"api.cline.bot" not in hello or KEY.encode() in hello:
            FAILURES.append("TLS destination or credential exposure mismatch")
            return
        NETWORK.append({"route": "direct", "method": "TLS", "target": "api.cline.bot:443"})
        self.request.sendall(b"\x15\x03\x03\x00\x02\x02\x28")


class DirectServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True


def seed(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], cwd=ROOT, check=True,
                   env={"PATH": os.environ["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
    database = data / "db/data.sqlite"
    relay = CASE in {"relay", "additional-relays", "strict"}
    with sqlite3.connect(database) as sql:
        sql.execute("""INSERT INTO proxyPools(id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES ('quota-fixture',?,'active',?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                    (CASE != "legacy", json.dumps({"type": "cloudflare" if relay else "http",
                     "proxyUrl": RELAY if relay else PROXY, "strictProxy": True,
                     "noProxy": ".cline.bot" if CASE == "no-proxy" else ""})))
        for provider in ALL_PROVIDERS:
            auth = "oauth" if provider == "github" else "api_key" if provider == "clinepass" else "apikey"
            flags = {"proxyPoolId": "quota-fixture", "connectionProxyEnabled": True, "connectionProxyUrl": PROXY}
            fields = {"apiKey": " " + KEY + " " if provider == "clinepass" else KEY, "providerSpecificData": flags}
            if auth == "oauth":
                flags.update(copilotToken=COPILOT, copilotTokenExpiresAt=4102444800)
                fields = {"accessToken": GITHUB, "refreshToken": REFRESH,
                          "expiresAt": "2099-12-31T00:00:00Z", "providerSpecificData": flags}
            sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES (?,?,?,'Proxy quota fixture',1,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        ("proxy-read-" + provider, provider, auth, json.dumps(fields)))
    return database


def request(port, path, headers=ADMIN):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=9)
    try:
        connection.request("GET", path, headers=headers)
        response = connection.getresponse()
        raw = response.read().decode()
        return response.status, json.loads(raw) if "json" in response.getheader("Content-Type", "") else raw
    finally:
        connection.close()


def wait(app, port):
    deadline = time.monotonic() + 12
    while app.poll() is None and time.monotonic() < deadline:
        if REFERENCE and select.select([app.stdout], [], [], .25)[0]:
            line = app.stdout.readline().decode().strip()
            if line.startswith("ready "):
                return int(line.split()[1])
        if not REFERENCE:
            try:
                if request(port, "/api/health")[0] == 200:
                    return port
            except OSError:
                time.sleep(.025)
    raise RuntimeError("Usage proxy fixture process did not start")


def saved(database):
    with sqlite3.connect(database) as sql:
        return list(sql.execute("SELECT id,data,updatedAt FROM providerConnections WHERE id LIKE 'proxy-read-%' ORDER BY id"))


def exercise(port, database):
    before = saved(database)
    base = "/api/usage/proxy-read-"
    if CASE in {"relay", "additional-relays"}:
        providers = PROVIDERS if CASE == "relay" else ADDITIONAL
        result = {provider: request(port, base + provider + "?force=1") for provider in providers}
        assert all(status == 200 and usage.get("quotas") for status, usage in result.values()), (result, FAILURES)
        assert [item["route"] for item in NETWORK] == ["relay"] * len(providers), NETWORK
    elif CASE == "strict":
        assert request(port, base + "clinepass", {}) == (401, {"error": "Unauthorized"})
        assert NETWORK == []
        RELAY_CODES.extend([503, 302])
        status, failure = request(port, base + "clinepass")
        assert status == 200 and "503" in failure["message"] and "[redacted]" in failure["message"] and KEY not in str(failure)
        status, redirected = request(port, base + "clinepass")
        assert status == 200 and "302" in redirected["message"]
        assert [item["route"] for item in NETWORK] == ["relay", "relay"], NETWORK
        with sqlite3.connect(database) as sql:
            (raw,), = sql.execute("SELECT data FROM providerConnections WHERE id='proxy-read-github'")
            fields = json.loads(raw)
            fields["expiresAt"] = "2000-01-01T00:00:00Z"
            sql.execute("UPDATE providerConnections SET data=? WHERE id='proxy-read-github'", (json.dumps(fields),))
        before = saved(database)
        assert request(port, base + "github") == (503, {"error": "Legacy runtime unavailable for this route"})
        assert len(NETWORK) == 2
        result = "Relay returned HTTP errors without retry or credential redirection. Due GitHub OAuth remained excluded."
    else:
        result = request(port, base + "clinepass")
        expected = ["direct"] if CASE == "no-proxy" else ["http-proxy", "direct"]
        assert [item["route"] for item in NETWORK] == expected, NETWORK
        assert result == (200, {"message": "ClinePass usage error: fetch failed"}), result
    assert saved(database) == before, "A read-only usage request changed connection credentials"
    assert not FAILURES, FAILURES
    if CASE == "strict":
        print(result)
    else:
        print(json.dumps({"responses": result, "network": NETWORK}, sort_keys=True))


with tempfile.TemporaryDirectory(prefix="9router-quota-proxy-") as directory:
    scratch = Path(directory)
    home, data, empty = (scratch / name for name in ("home", "data", "empty-path"))
    home.mkdir()
    empty.mkdir()
    database = seed(data, home)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": os.environ["PATH"] if REFERENCE else str(empty), "HOME": str(home), "DATA_DIR": str(data),
           "PORT": str(port), "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallbacks")}
    services = [ThreadingHTTPServer(("127.0.0.1", port), Handler) for port in (52988, 52989)]
    services.append(DirectServer(("127.0.0.1", 443), Direct))
    for service in services:
        threading.Thread(target=service.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    command = ["node", "--no-warnings", str(Path(__file__).with_name("original.mjs")), str(data)] if REFERENCE else [BINARY, "--no-browser"]
    with (scratch / "app.log").open("w+") as logs:
        app = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=logs)
        try:
            exercise(wait(app, port), database)
            if not REFERENCE:
                fallbacks = (scratch / "fallbacks").read_text().splitlines()
                assert fallbacks == (["GET /api/usage/proxy-read-github"] if CASE == "strict" else []), fallbacks
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            raise
        finally:
            app.terminate()
            try:
                app.wait(timeout=4)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait(timeout=2)
            for service in services:
                service.shutdown()
                service.server_close()
