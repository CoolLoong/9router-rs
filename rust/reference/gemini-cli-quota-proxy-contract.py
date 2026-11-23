from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import platform
import sqlite3
import sys
import tempfile


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "gemini_quota_proxy_helpers", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "cloudcode-pa.googleapis.com"
ASSIST = "/v1internal:loadCodeAssist"
QUOTA = "/v1internal:retrieveUserQuota"
TOKEN = "offline-gemini-cli-quota-access"
REFRESH = "offline-gemini-cli-quota-refresh"
MODEL = "gemini-fixture"
RESET = "2031-04-05T06:07:08.000Z"
RELAY = "https://gemini-quota-fixture.example.workers.dev"
shared.ZED_HOST, shared.CLINE_HOST = HOST, "oauth2.googleapis.com"
shared.SECRETS = (TOKEN, REFRESH)
records = []
errors = []
mode = "success"
switch = None


class Google(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, location=None):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        try:
            assert self.path in (ASSIST, QUOTA) and self.headers["Host"] == HOST
            assert self.headers.get_all("Authorization") == ["Bearer " + TOKEN]
            for field in ("Cookie", "X-9r-cli-token", "Proxy-Authorization", "X-Relay-Auth"):
                assert self.headers.get(field) is None
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            records.append((self.path, body))
            if self.path == ASSIST:
                platform_id = 4 if platform.machine() in ("aarch64", "arm64") else 3
                assert body == {"metadata": {"ideType": 9, "platform": platform_id, "pluginType": 2}}
                if mode == "assist-redirect":
                    return self.reply(307, {}, "https://oauth2.googleapis.com/forbidden")
                if switch:
                    switch()
                return self.reply(200, {"cloudaicompanionProject": {"id": "fixture-discovered"},
                                       "currentTier": {"name": "Fixture Tier " + TOKEN}})
            assert body in ({"project": "projects/fixture-stored"}, {"project": "fixture-discovered"}), body
            if mode == "quota-redirect":
                return self.reply(307, {}, "https://oauth2.googleapis.com/forbidden")
            if mode == "quota-unauthorized":
                return self.reply(401, {"error": {"message": "Rejected " + TOKEN}})
            return self.reply(200, {"buckets": [
                {"modelId": MODEL, "remainingFraction": 0.75, "resetTime": RESET},
                {"modelId": TOKEN, "remainingFraction": 0.5, "resetTime": RESET},
            ]})
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            errors.append(str(error) or self.path)
            self.reply(422, {"error": "unexpected quota fixture request"})


def account(db, details=None, project=None, expires="2099-12-31T00:00:00Z", token=TOKEN, refresh=REFRESH):
    data = {"accessToken": token, "refreshToken": refresh, "expiresAt": expires,
            "projectId": project, "providerSpecificData": details or {}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('gemini-quota','gemini-cli','oauth','Fixture Gemini',1,1,?,
                    '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (json.dumps(data),))


def configure_global(db, address=None):
    data = {"outboundProxyEnabled": address is not None, "outboundProxyUrl": address or ""}
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES (1,?)", (json.dumps(data),))


def raw_reply(port, id="gemini-quota", headers=shared.ADMIN):
    status, raw = shared.get(port, "/api/usage/" + id, headers)
    assert all(secret.encode() not in raw for secret in shared.SECRETS)
    return status, json.loads(raw)


def expected(plan):
    return 200, {"plan": plan, "quotas": {MODEL: {"used": 250, "total": 1000, "resetAt": RESET,
                                                 "remainingPercentage": 75, "unlimited": False}}}


with tempfile.TemporaryDirectory(prefix="9router-gemini-quota-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca, target, wrong_name = shared.tls_services(stack, scratch, Google)
    proxies = {}
    for method, scheme in [("connect", "http"), ("socks", "socks5h")]:
        proxy = shared.Proxy(("127.0.0.1", 0), shared.Tunnel)
        proxy.mode, proxy.reject, proxy.target = method, False, target
        stack.enter_context(shared.serving(proxy))
        proxies[method] = (proxy, f"{scheme}://127.0.0.1:{proxy.server_address[1]}")
    connect, http = proxies["connect"]
    socks = proxies["socks"][1]
    tunnel = {"connectionProxyEnabled": True, "connectionProxyUrl": http}
    with shared.gateway(scratch, "native", ca, [("socks", "socks5", socks), ("relay", "cloudflare", RELAY)]) as (port, db):
        account(db, tunnel, project="projects/fixture-stored")
        assert raw_reply(port, headers={})[0] == 401 and not records and not shared.TUNNELS
        assert raw_reply(port) == expected("Free")
        assert records == [(QUOTA, {"project": "projects/fixture-stored"})]
        assert shared.TUNNELS == [("connect", HOST, 443)]

        account(db, {"proxyPoolId": "socks"})
        assert raw_reply(port) == expected("Fixture Tier [redacted]")
        assert [path for path, _ in records[-2:]] == [ASSIST, QUOTA]
        assert shared.TUNNELS[-2:] == [("socks", HOST, 443)] * 2

        account(db)
        configure_global(db, http)
        switch = lambda: configure_global(db, socks)
        assert raw_reply(port) == expected("Fixture Tier [redacted]")
        assert shared.TUNNELS[-2:] == [("connect", HOST, 443)] * 2
        switch = None
        configure_global(db)

        account(db, tunnel, project="projects/fixture-stored", refresh={"malformed": True})
        assert raw_reply(port) == expected("Free")

        before = len(records), len(shared.TUNNELS)
        for expiry, token in (("2020-01-01T00:00:00Z", TOKEN), ("malformed", TOKEN), (None, None)):
            account(db, tunnel, expires=expiry, token=token)
            assert raw_reply(port)[0] == 401
        for details in ({"proxyPoolId": "relay"}, {"proxyPoolId": "missing"}, {"strictProxy": True}):
            account(db, details, project="projects/fixture-stored")
            assert raw_reply(port)[0] == 503
        assert (len(records), len(shared.TUNNELS)) == before

        account(db, tunnel)
        mode = "assist-redirect"
        assert "project ID not available" in raw_reply(port)[1]["message"]
        assert [path for path, _ in records[before[0]:]] == [ASSIST]
        account(db, tunnel, project="projects/fixture-stored")
        for mode, status in [("quota-redirect", 307), ("quota-unauthorized", 401)]:
            before = len(records), len(shared.TUNNELS)
            assert raw_reply(port) == (200, {"plan": "Free", "message": f"Gemini CLI quota error ({status})."})
            assert (len(records), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 1)
        before = len(records), len(shared.TUNNELS)
        connect.target = wrong_name
        assert raw_reply(port) == (200, {"message": "Gemini CLI error: fetch failed"})
        connect.reject = True
        assert raw_reply(port) == (200, {"message": "Gemini CLI error: fetch failed"})
        assert (len(records), len(shared.TUNNELS)) == (before[0], before[1] + 2)
        assert all(name == HOST for name in shared.HELLOS)
        assert not errors and not shared.ERRORS, (errors, shared.ERRORS)
        print("Gemini CLI quota discovery and reads passed CONNECT, SOCKS5h, frozen egress and credential isolation.")
