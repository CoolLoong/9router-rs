from contextlib import ExitStack
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import sqlite3
import tempfile
from urllib.parse import parse_qs


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
tokens = {"access":"synthetic-official-access", "fresh":"synthetic-official-renewed-access",
          "refresh":"synthetic-official-rotation", "rotated":"synthetic-official-refresh-new",
          "cline":"eyJfixture.cline-old.signature", "cline_new":"eyJfixture.cline-new.signature"}
routes = {
    ("api.anthropic.com", "/v1/oauth/token"): ("claude", "grant"),
    ("auth.openai.com", "/oauth/token"): ("codex", "grant"),
    ("oauth2.googleapis.com", "/token"): ("gemini-cli", "grant"),
    ("chatgpt.com", "/backend-api/codex/responses"): ("codex", "probe"),
    ("cloudcode-pa.googleapis.com", "/v1internal:loadCodeAssist"): ("gemini-cli", "probe"),
    ("api.cline.bot", "/api/v1/users/me"): ("cline", "probe"),
    ("api.cline.bot", "/api/v1/auth/refresh"): ("cline", "grant"),
}
hosts = tuple(dict.fromkeys(host for host, _ in routes))
calls, errors = [], []
status = {"codex":400,"gemini-cli":200,"cline":200}
expired = "2000-01-01T00:00:00Z"
current = "2099-01-01T00:00:00Z"


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def request(self):
        try:
            direct = self.server.server_port == 52989
            host = self.headers["Host"]
            if direct and self.path == "/cline/api/v1/auth/refresh":
                provider, kind = "cline", "grant"
            elif direct:
                _, prefix, provider, *suffix = self.path.split("/")
                assert prefix == "oauth-probes"
                kind = "grant" if suffix == ["token"] else "probe"
            else:
                provider, kind = routes[(host, self.path)]
            assert self.headers["Cookie"] is None and self.headers["Proxy-Authorization"] is None
            assert self.headers["X-Relay-Target"] is None and self.headers["X-9r-Cli-Token"] is None
            data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if kind == "grant":
                assert self.command == "POST"
                assert self.headers["Authorization"] is None
                assert self.headers["Accept"] == "application/json"
                if provider == "cline":
                    assert json.loads(data) == {"refreshToken":tokens["refresh"],
                                                "grantType":"refresh_token", "clientType":"extension"}
                    calls.append((direct, provider, kind, None))
                    return self.respond(200, {"data":{"accessToken":tokens["cline_new"],
                                                      "refreshToken":tokens["rotated"],"expiresAt":current}})
                if provider == "gemini-cli":
                    grant = dict((key, value[0]) for key, value in parse_qs(data.decode()).items())
                    assert grant.pop("client_secret", "")
                else:
                    grant = json.loads(data)
                assert grant.pop("client_id", "")
                assert grant == {"grant_type":"refresh_token","refresh_token":tokens["refresh"]}
                calls.append((direct, provider, kind, None))
                return self.respond(200, {"access_token":tokens["fresh"],"refresh_token":tokens["rotated"],"expires_in":86400})
            access = self.headers["Authorization"]
            if provider == "cline":
                assert self.command == "GET" and not data
                assert self.headers["X-Client-Type"] == "9router" and self.headers["Accept"] == "application/json"
                assert access in tuple("Bearer workos:" + tokens[name] for name in ("cline", "cline_new"))
                code = status[provider] if access == "Bearer workos:" + tokens["cline"] else 200
                calls.append((direct, provider, kind, code))
                return self.respond(code, {"error":tokens["cline"]})
            assert self.command == "POST"
            assert access in tuple("Bearer " + tokens[name] for name in ("access", "fresh"))
            renewed = access == "Bearer " + tokens["fresh"]
            if provider == "codex":
                assert self.headers["Originator"] == "codex_cli_rs"
                assert self.headers["User-Agent"].startswith("codex_cli_rs/")
                assert json.loads(data) == {"model":"gpt-5.3-codex","input":[],"stream":False,"store":False}
                code = 400 if renewed else status[provider]
            else:
                assert self.headers["User-Agent"].endswith("gemini-cli/0.34.0")
                assert json.loads(data) == {"metadata":{"ideType":"IDE_UNSPECIFIED",
                    "platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}
                code = 200 if renewed else status[provider]
            calls.append((direct, provider, kind, code))
            self.respond(code, {"error":{"message":"fixture " + tokens["access"]}})
        except (AssertionError, KeyError, ValueError) as error:
            errors.append((self.path, repr(error)))
            self.send_error(422)

    do_GET = request
    do_POST = request

    def respond(self, status, value):
        raw = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def save(db, provider, details=None, expiry=current, renew=True):
    access = " " + tokens["cline"] + " " if provider == "cline" else tokens["access"]
    data = {"accessToken":access,"expiresAt":expiry,"lastRefreshAt":current,
            "providerSpecificData":details or {}}
    if renew:
        data["refreshToken"] = tokens["refresh"]
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES (?,?,'oauth','Synthetic OAuth fixture',1,1,?,'2026-11-20','2026-11-20')""",
                    (provider, provider, json.dumps(data)))


def test(port, provider, refreshed=False, error=None):
    conn = HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request("POST", f"/api/providers/{provider}/test", headers=shared["ADMIN"])
        response = conn.getresponse()
        raw = response.read()
        assert all(token.encode() not in raw for token in tokens.values())
        assert response.status == 200 and json.loads(raw) == {
            "valid":error is None,"error":error,"refreshed":refreshed,
        }, (response.status, raw, errors)
    finally:
        conn.close()


def saved(db, provider):
    with sqlite3.connect(db) as sql:
        return json.loads(sql.execute("SELECT data FROM providerConnections WHERE id=?", (provider,)).fetchone()[0])


with tempfile.TemporaryDirectory(prefix="9router-official-oauth-test-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared["tls_services"](stack, scratch, Vendor, hosts)
    stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    connect, socks = [shared["Proxy"](("127.0.0.1", 0), shared["Tunnel"]) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        proxy.hosts, proxy.secrets = hosts, tuple(tokens.values())
        stack.enter_context(shared["serving"](proxy))
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{connect.server_address[1]}"}
    pool = {"proxyPoolId":"official-socks"}
    pools = [("official-socks", "socks5", f"socks5h://127.0.0.1:{socks.server_address[1]}")]
    with shared["gateway"](scratch, "official-oauth", ca, pools) as (port, db):
        save(db, "claude", {"proxyPoolId":"absent"})
        test(port, "claude")
        save(db, "claude", http, expired, renew=False)
        test(port, "claude", error="Token expired")
        assert not calls and not shared["TUNNELS"]
        save(db, "claude", http, expired)
        test(port, "claude", refreshed=True)
        assert calls == [(False,"claude","grant",None)]
        assert shared["TUNNELS"] == [("connect","api.anthropic.com",443)]
        assert saved(db,"claude")["accessToken"] == tokens["fresh"]

        save(db, "codex", pool)
        test(port, "codex")
        assert calls[-1] == (False,"codex","probe",400)
        status["codex"] = 401
        test(port, "codex", refreshed=True)
        assert calls[-3:] == [(False,"codex","probe",401),(False,"codex","grant",None),
                              (False,"codex","probe",400)]
        assert shared["TUNNELS"][-3:] == [("socks",hold,443) for hold in ("chatgpt.com","auth.openai.com","chatgpt.com")]
        assert saved(db,"codex")["refreshToken"] == tokens["rotated"]

        status["gemini-cli"] = 401
        save(db, "gemini-cli", {**http,"connectionNoProxy":"oauth2.googleapis.com"})
        test(port, "gemini-cli", refreshed=True)
        assert calls[-3:] == [(False,"gemini-cli","probe",401),(True,"gemini-cli","grant",None),
                              (False,"gemini-cli","probe",200)]
        assert shared["TUNNELS"][-2:] == [("connect","cloudcode-pa.googleapis.com",443)] * 2
        status["gemini-cli"] = 403
        save(db, "gemini-cli")
        test(port, "gemini-cli", error="fixture [redacted]")
        assert saved(db,"gemini-cli")["lastError"] == "fixture [redacted]"
        before = len(calls)
        save(db, "gemini-cli", http)
        connect.reject = True
        test(port, "gemini-cli", error="fetch failed")
        connect.reject = False
        assert len(calls) == before

        save(db, "cline", http)
        status["cline"] = 403
        test(port, "cline", error="Access denied")
        assert calls[-1] == (False,"cline","probe",403)
        status["cline"] = 401
        test(port, "cline", refreshed=True)
        assert calls[-3:] == [(False,"cline","probe",401),(False,"cline","grant",None),(False,"cline","probe",200)]
        assert shared["TUNNELS"][-3:] == [("connect","api.cline.bot",443)] * 3
        assert saved(db,"cline")["accessToken"] == tokens["cline_new"]
        assert saved(db,"cline")["refreshToken"] == tokens["rotated"]

        save(db, "cline", pool, expired)
        test(port, "cline", refreshed=True)
        assert calls[-2:] == [(False,"cline","grant",None),(False,"cline","probe",200)]
        assert shared["TUNNELS"][-2:] == [("socks","api.cline.bot",443)] * 2

        save(db, "cline", {**http,"connectionNoProxy":"api.cline.bot"})
        start = len(shared["TUNNELS"])
        test(port, "cline", refreshed=True)
        assert calls[-3:] == [(True,"cline","probe",401),(True,"cline","grant",None),(True,"cline","probe",200)]
        assert len(shared["TUNNELS"]) == start
    assert not errors and not shared["ERRORS"], (errors, shared["ERRORS"])
print("Claude, Codex, Gemini CLI and Cline OAuth Test passed protected refresh, bypass and error redaction.")
