from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import socketserver
import sqlite3
import sys
import tempfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "quota_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

CODEX, CLAUDE = "chatgpt.com", "api.anthropic.com"
USAGE, CREDITS = "/backend-api/wham/usage", "/backend-api/wham/rate-limit-reset-credits"
OAUTH, SETTINGS, ORG = "/api/oauth/usage", "/v1/settings", "/v1/organizations/org-offline/usage"
CLAUDE_VERSION = "2023-06-01"
TOKENS = {"codex": "offline-protected-usage-codex", "claude": "offline-protected-usage-claude"}
REFRESH = "offline-protected-refresh-must-not-send"
WORKSPACE = "quota-offline-saved-workspace"
CLI = {"x-9r-cli-token": "68977b6fd9422fbb", "x-internal-token": "discard", "Cookie": "discard"}
RELAY = "https://quota-fixture.offline-lane.workers.dev"
RELAY_KEY = "Q" * 43
shared.ZED_HOST, shared.CLINE_HOST = CODEX, CLAUDE
shared.SECRETS = (*TOKENS.values(), REFRESH, RELAY_KEY, WORKSPACE)
SEEN, ERRORS, DIRECT_HELLOS = [], [], []
MODE, USED, SWITCH = "ok", 12, None


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, location=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        ERRORS.append(("unexpected write", self.path))
        self.reply(405, {})

    def do_GET(self):
        try:
            host = self.headers["Host"]
            is_codex = host == CODEX
            assert host in (CODEX, CLAUDE)
            assert self.path in ((USAGE, CREDITS) if is_codex else (OAUTH, SETTINGS, ORG))
            saved = TOKENS["codex" if is_codex else "claude"]
            assert self.headers.get_all("Authorization") == ["Bearer " + saved]
            assert not any(self.headers[name] for name in (
                "Content-Length", "Cookie", "Proxy-Authorization", "Originator", "x-internal-token",
                "x-relay-auth", "x-relay-path", "x-relay-target", "x-9r-cli-token",
            ))
            assert "claude-cli" not in self.headers.get("User-Agent", "")
            if is_codex:
                assert self.headers["Accept"] == "application/json"
                if self.path == CREDITS:
                    assert self.headers["ChatGPT-Account-ID"] == WORKSPACE
                    assert self.headers["OpenAI-Beta"] == "codex-1"
                else:
                    assert not self.headers["ChatGPT-Account-ID"] and not self.headers["OpenAI-Beta"]
            else:
                assert self.headers["anthropic-version"] == CLAUDE_VERSION
                assert self.headers["anthropic-beta"] == ("oauth-2025-04-20" if self.path == OAUTH else None)
            SEEN.append(self.path)
            if MODE == "redirect":
                return self.reply(307, {}, "https://untrusted.invalid/credential-sink")
            if MODE == "auth":
                return self.reply(401, {"error": {"message": f"Unauthorized: {saved}"}})
            if self.path == USAGE:
                return self.reply(200, {"plan_type": "pro", "rate_limit": {"primary_window": {"used_percent": 20}}})
            if self.path == CREDITS:
                return self.reply(200, {"available_count": 2, "credits": [{"status": "available", "granted_at": 1700000000}]})
            if self.path == SETTINGS:
                return self.reply(200, {"plan": "fixture org", "organization_name": "Fixture", "organization_id": "org-offline"})
            if self.path == ORG:
                return self.reply(200, {"legacy": {"remaining": 9}})
            if MODE == "legacy":
                SWITCH()
                return self.reply(429, {"error": "retry later"})
            self.reply(200, {"five_hour": {"utilization": USED}})
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((self.path.partition("?")[0], repr(error)))
            self.reply(422, {})


class DirectProbe(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        initial = self.request.recv(1024)
        if initial:
            DIRECT_HELLOS.append(initial)


def save(db, provider, details=None, auth="oauth", expiry="2099-01-01T00:00:00Z", token=True):
    data = {"refreshToken": REFRESH,
            "providerSpecificData": {"baseUrl": "https://untrusted.invalid", "workspaceId": WORKSPACE, **(details or {})}}
    if token:
        data["accessToken"] = TOKENS[provider]
    if expiry is not None:
        data["expiresAt"] = expiry
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,?,'Offline Quota',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider, provider, auth, json.dumps(data)))


def global_proxy(db, address=None, bypass=None):
    data = {}
    if address:
        data = {"outboundProxyEnabled": True, "outboundProxyUrl": address, "outboundNoProxy": bypass or ""}
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,?)", (json.dumps(data),))


def get(port, account, suffix="", code=200, headers=CLI):
    status, raw = shared.get(port, "/api/usage/" + account + suffix, headers)
    assert all(secret.encode() not in raw for secret in shared.SECRETS)
    assert status == code, (account, suffix, status, raw)
    return json.loads(raw)


def snapshot(db):
    with sqlite3.connect(db) as sql:
        return sql.execute("SELECT id,data,updatedAt FROM providerConnections ORDER BY id").fetchall()


def verify(port, db, connect, tunnel, socks, direct_probe):
    global MODE, USED, SWITCH
    credits = "/codex-reset-credits"
    expect_credits = {"availableCount": 2, "credits": [
        {"status": "available", "grantedAt": "2023-11-14T22:13:20.000Z", "expiresAt": None},
    ]}
    save(db, "codex", tunnel)
    save(db, "claude", {"proxyPoolId": "socks"})
    get(port, "codex", code=401, headers={})
    get(port, "codex", credits, code=401, headers={})
    assert not SEEN and not shared.TUNNELS
    before = snapshot(db)
    assert get(port, "codex")["quotas"]["session"]["remaining"] == 80
    assert get(port, "codex", credits) == expect_credits
    claude = get(port, "claude")
    assert claude["quotas"]["session (5h)"]["used"] == 12 and claude["resetCredits"] is None
    assert get(port, "claude") == claude and SEEN == [USAGE, CREDITS, OAUTH]
    assert shared.TUNNELS == [("connect", CODEX, 443), ("connect", CODEX, 443), ("socks", CLAUDE, 443)]
    assert shared.HELLOS == [CODEX, CODEX, CLAUDE]
    assert snapshot(db) == before

    save(db, "codex", {"proxyPoolId": "socks"}, auth="access_token", expiry=None)
    assert get(port, "codex", credits) == expect_credits and shared.TUNNELS[-1] == ("socks", CODEX, 443)
    assert "Usage not available" in get(port, "codex")["message"]
    save(db, "codex", tunnel)
    for provider, path in (("codex", ""), ("codex", credits), ("claude", "?force=1")):
        for expiry, token in (("2020-01-01T00:00:00Z", True), (0, True), ("invalid", True), (None, False)):
            save(db, provider, tunnel, expiry=expiry, token=token)
            count, stored = (len(SEEN), len(shared.TUNNELS)), snapshot(db)
            assert "A current access token is required" in get(port, provider, path, 401)["error"]
            assert (len(SEEN), len(shared.TUNNELS)) == count and snapshot(db) == stored
        save(db, provider, tunnel)

    MODE = "auth"
    before = snapshot(db)
    assert "unavailable (401)" in get(port, "codex")["message"]
    assert get(port, "codex", credits, 500)["error"] == "Unauthorized: [redacted]"
    assert SEEN[-2:] == [USAGE, CREDITS] and snapshot(db) == before
    MODE = "redirect"
    count, connections = len(SEEN), len(shared.TUNNELS)
    assert "redirect refused" in get(port, "claude", "?force=1")["message"]
    assert SEEN[count:] == [OAUTH] and len(shared.TUNNELS) == connections + 1
    assert "redirect refused" in get(port, "codex", credits, 500)["error"]
    MODE = "legacy"

    save(db, "claude")
    http = tunnel["connectionProxyUrl"]
    global_proxy(db, http)
    SWITCH = lambda: global_proxy(db, socks)
    count = len(shared.TUNNELS)
    assert get(port, "claude", "?force=1") == {"plan": "fixture org", "organization": "Fixture",
                                              "quotas": {"legacy": {"remaining": 9}}}
    assert SEEN[-3:] == [OAUTH, SETTINGS, ORG]
    assert shared.TUNNELS[count:] == [("connect", CLAUDE, 443)] * 3
    MODE, USED = "ok", 34
    assert get(port, "claude") == claude
    assert get(port, "claude", "?force=1")["quotas"]["session (5h)"]["used"] == 34
    global_proxy(db, http)
    assert get(port, "claude")["plan"] == "fixture org"

    global_proxy(db)
    count = (len(SEEN), len(shared.TUNNELS))
    for details in ({"proxyPoolId": "relay"}, {"proxyPoolId": "missing"}, {"strictProxy": True},
                    tunnel | {"strictProxy": True, "connectionNoProxy": ".chatgpt.com"},
                    {"vercelRelayUrl": RELAY}):
        save(db, "codex", details)
        assert get(port, "codex", code=503)["error"] == "Account quota proxy is unavailable"
    assert (len(SEEN), len(shared.TUNNELS)) == count
    save(db, "codex", tunnel)
    connect.reject = True
    assert "fetch failed" in get(port, "codex", code=500)["error"]
    assert len(SEEN) == count[0] and len(shared.TUNNELS) == count[1] + 1
    assert not DIRECT_HELLOS
    if direct_probe:
        save(db, "codex", tunnel | {"connectionNoProxy": ".chatgpt.com"})
        assert "fetch failed" in get(port, "codex", code=500)["error"]
        assert len(SEEN) == count[0] and len(shared.TUNNELS) == count[1] + 1
        assert len(DIRECT_HELLOS) == 1 and DIRECT_HELLOS[0].startswith(b"\x16\x03")
        assert b"chatgpt.com" in DIRECT_HELLOS[0]
        assert not any(secret.encode() in DIRECT_HELLOS[0] for secret in shared.SECRETS)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)


with tempfile.TemporaryDirectory(prefix="9router-quota-protected-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, tls, _ = shared.tls_services(stack, scratch, Vendor)
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, tls
        stack.enter_context(shared.serving(proxy))
    direct_probe = "--bypass-probe" in sys.argv[2:]
    if direct_probe:
        stack.enter_context(shared.serving(shared.Proxy(("127.0.0.1", 443), DirectProbe)))
    tunnel = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    with shared.gateway(scratch, "quota", ca, [("socks", "socks5", socks_url), ("relay", "cloudflare", RELAY)]) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                        (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
        verify(port, db, connect, tunnel, socks_url, direct_probe)

print("Codex and Claude quota reads preserved frozen CONNECT/SOCKS5h routing, expiry checks and refresh isolation.")
