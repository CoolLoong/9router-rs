from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler
import importlib.util
import json
from pathlib import Path
import socketserver
import sqlite3
import sys
import tempfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "quota_plan_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

CODING = "/v1/api/openplatform/coding_plan/remains"
TARGETS = {
    "minimax": (("www.minimax.io", "/v1/token_plan/remains"), ("api.minimax.io", CODING)),
    "minimax-cn": (("www.minimaxi.com", CODING), ("api.minimaxi.com", CODING)),
    "kimi": (("api.kimi.com", "/coding/v1/usages"),),
}
KEYS = {name: "offline-bound-plan-key-" + name for name in TARGETS}
EXTRA_TOKEN = "offline-unused-kimi-bearer"
DEVICE = "offline-unused-kimi-device"
RELAY = "https://quota-plans.offline-lane.workers.dev"
RELAY_KEY = "P" * 43
CLI = {"x-9r-cli-token": "68977b6fd9422fbb", "x-internal-token": "do-not-forward", "Cookie": "discard"}
shared.SECRETS = (*KEYS.values(), EXTRA_TOKEN, DEVICE, RELAY_KEY)
REQUESTS, ERRORS, DIRECT = [], [], []
MODE, MODE_STATUS, SWITCH = "success", 0, None


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, location=None):
        raw = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        ERRORS.append("unexpected upstream POST")
        self.reply(405, {})

    def do_GET(self):
        try:
            host = self.headers["Host"]
            provider, targets = next((provider, targets) for provider, targets in TARGETS.items()
                                     if any(host == target[0] for target in targets))
            rank = next(index for index, target in enumerate(targets) if target[0] == host)
            assert self.path == targets[rank][1]
            assert self.headers["Accept"] == self.headers["Content-Type"] == "application/json"
            assert not any(self.headers[field] for field in ("Cookie", "x-internal-token", "Proxy-Authorization",
                                                             "x-relay-path", "x-relay-target", "x-relay-auth", "Content-Length"))
            key = KEYS[provider]
            if provider == "kimi":
                assert self.headers.get_all("x-api-key") == [key] and self.headers["Authorization"] is None
                assert not any(field.lower().startswith("x-msh-") for field in self.headers)
            else:
                assert self.headers.get_all("Authorization") == ["Bearer " + key] and self.headers["x-api-key"] is None
            REQUESTS.append((provider, host))
            if MODE == "fallback" and rank == 0:
                if SWITCH:
                    SWITCH()
                return self.reply(MODE_STATUS, {"message": "temporarily unavailable"})
            if MODE == "auth":
                return self.reply(401, {"message": f"synthetic rejected {key}"})
            if MODE == "echo":
                if provider == "kimi":
                    return self.reply(403, {"message": f"quota permission denied for {key}"})
                return self.reply(200, {"baseResp": {"statusCode": 59, "statusMsg": f"synthetic error {key}"}})
            if MODE == "redirect":
                return self.reply(307, {}, f"https://{host}/should-not-follow")
            if provider == "kimi":
                return self.reply(200, {"usage": {"limit": 100, "used": 27, "remaining": 73,
                                                 "resetTime": "2026-11-27T00:00:00.000Z"},
                                        "user": {"membership": {"level": "LEVEL_STANDARD"}}})
            self.reply(200, {"modelRemains": [
                {"modelName": "general", "currentIntervalTotalCount": 100, "currentIntervalUsageCount": 8,
                 "endTime": 1795046400, "currentWeeklyRemainingPercent": 60},
            ]})
        except (AssertionError, KeyError, TypeError, ValueError, StopIteration) as error:
            ERRORS.append((self.path, repr(error)))
            self.reply(422, {})


class DirectProbe(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        if initial := self.request.recv(1024):
            DIRECT.append(initial)


def save(db, provider, details=None, *, missing=False, auth="apikey"):
    data = {"providerSpecificData": {"baseUrl": "https://untrusted.invalid", "deviceId": DEVICE, **(details or {})}}
    if not missing:
        data["apiKey"] = KEYS[provider]
    if provider == "kimi":
        data["accessToken"] = EXTRA_TOKEN
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?, ?, ?, 'Offline M Plan',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider, provider, auth, json.dumps(data)))


def read(port, provider, status=200, headers=CLI):
    code, raw = shared.get(port, "/api/usage/" + provider, headers)
    assert not any(secret.encode() in raw for secret in shared.SECRETS)
    assert code == status, (provider, code, raw)
    return json.loads(raw)


def settings(db, proxy=None, bypass=None):
    data = {"requireLogin": True}
    if proxy:
        data.update(outboundProxyEnabled=True, outboundProxyUrl=proxy, outboundNoProxy=bypass or "")
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,?)", (json.dumps(data),))


def snapshot(db):
    with sqlite3.connect(db) as sql:
        return sql.execute("SELECT id,data,updatedAt FROM providerConnections ORDER BY id").fetchall()


def assert_minimax(value, coding):
    expected = 92 if coding else 8
    quotas = value["quotas"]
    assert quotas["M-series (5h)"] == {
        "used": expected, "total": 100, "remaining": 100 - expected,
        "remainingPercentage": 100 - expected, "resetAt": "2026-11-19T00:00:00.000Z", "unlimited": False,
    }
    assert quotas["M-series (7d)"]["remainingPercentage"] == 60


def verify(port, db, provider, connect, wrong, tunnel, socks, direct_probe):
    global MODE, MODE_STATUS, SWITCH
    host = TARGETS[provider][0][0]
    alternate = TARGETS[provider][-1][0]
    kimi = provider == "kimi"
    auth = "api_key" if provider == "minimax-cn" else "apikey"
    start_requests = len(REQUESTS)
    start_direct = len(DIRECT)
    settings(db)
    save(db, provider, tunnel, auth=auth)
    read(port, provider, 401, {})
    assert len(REQUESTS) == start_requests and not shared.TUNNELS
    before = snapshot(db)
    result = read(port, provider)
    if kimi:
        assert result["plan"] == "Vivace" and result["quotas"]["Weekly"]["remainingPercentage"] == 73
    else:
        assert_minimax(result, provider != "minimax")
    assert shared.TUNNELS == [("connect", host, 443)] and shared.HELLOS == [host]
    assert snapshot(db) == before
    save(db, provider, {"proxyPoolId": "socks"}, auth=auth)
    assert read(port, provider) == result and shared.TUNNELS[-1] == ("socks", host, 443)

    if not kimi:
        save(db, provider, auth=auth)
        http = tunnel["connectionProxyUrl"]
        settings(db, http)
        for code in (404, 405, 503):
            MODE, MODE_STATUS = "fallback", code
            SWITCH = lambda: settings(db, socks)
            before = len(shared.TUNNELS)
            assert_minimax(read(port, provider), True)
            assert shared.TUNNELS[before:] == [("connect", host, 443), ("connect", alternate, 443)]
            assert REQUESTS[-2:] == [(provider, host), (provider, alternate)]
            MODE, SWITCH = "success", None
            assert read(port, provider) == result and shared.TUNNELS[-1] == ("socks", host, 443)
            settings(db, http)
        middle = (len(REQUESTS), len(shared.TUNNELS))
        settings(db, http, host)
        assert read(port, provider, 503)["error"] == "Account quota proxy is unavailable"
        assert (len(REQUESTS), len(shared.TUNNELS)) == middle
        settings(db)

    save(db, provider, tunnel, auth=auth)
    for mode, marker in (("redirect", "redirect refused"), ("auth", "authentication expired" if kimi else "invalid or inactive"),
                         ("echo", "[redacted]")):
        MODE = mode
        before = (len(REQUESTS), len(shared.TUNNELS))
        assert marker in read(port, provider)["message"]
        assert (len(REQUESTS), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 1)
    MODE = "success"
    before = (len(REQUESTS), len(shared.TUNNELS))
    for invalid in ({"proxyPoolId": "missing"}, {"proxyPoolId": "relay"}, {"strictProxy": True},
                    {"vercelRelayUrl": RELAY}, tunnel | {"strictProxy": True, "connectionNoProxy": "*"}):
        save(db, provider, invalid, auth=auth)
        assert read(port, provider, 503)["error"] == "Account quota proxy is unavailable"
    save(db, provider, tunnel, missing=True, auth=auth)
    assert "not available" in read(port, provider)["message"]
    assert (len(REQUESTS), len(shared.TUNNELS)) == before
    save(db, provider, tunnel, auth=auth)
    upstream = connect.target
    connect.target = wrong
    assert "fetch failed" in read(port, provider)["message"]
    connect.target, connect.reject = upstream, True
    assert "fetch failed" in read(port, provider)["message"]
    assert len(REQUESTS) == before[0] and len(shared.TUNNELS) == before[1] + 2
    assert shared.TUNNELS[-2:] == [("connect", host, 443)] * 2 and len(DIRECT) == start_direct
    if direct_probe:
        save(db, provider, tunnel | {"connectionNoProxy": ",".join({host, alternate})}, auth=auth)
        assert "fetch failed" in read(port, provider)["message"]
        assert len(REQUESTS) == before[0] and len(shared.TUNNELS) == before[1] + 2
        assert len(DIRECT) == start_direct + 1 and DIRECT[-1].startswith(b"\x16\x03") and host.encode() in DIRECT[-1]
        assert not any(secret.encode() in DIRECT[-1] for secret in shared.SECRETS)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)


with tempfile.TemporaryDirectory(prefix="9router-quota-plans-proxy-") as directory, ExitStack() as outer:
    direct_probe = "--bypass-probe" in sys.argv[2:]
    if direct_probe:
        outer.enter_context(shared.serving(shared.Proxy(("127.0.0.1", 443), DirectProbe)))
    for provider, targets in TARGETS.items():
        shared.ZED_HOST, shared.CLINE_HOST = targets[0][0], targets[-1][0]
        shared.TUNNELS.clear()
        shared.HELLOS.clear()
        scratch = Path(directory) / provider
        scratch.mkdir()
        with ExitStack() as stack:
            ca, upstream, wrong = shared.tls_services(stack, scratch, Vendor)
            connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
            for proxy, mode in ((connect, "connect"), (socks, "socks")):
                proxy.mode, proxy.reject, proxy.target = mode, False, upstream
                stack.enter_context(shared.serving(proxy))
            tunnel = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
            socks_address = f"socks5h://127.0.0.1:{socks.server_address[1]}"
            with shared.gateway(scratch, "plan", ca, [("socks", "socks5", socks_address), ("relay", "cloudflare", RELAY)]) as (port, db):
                with sqlite3.connect(db) as sql:
                    sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                                (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
                verify(port, db, provider, connect, wrong, tunnel, socks_address, direct_probe)

print("MiniMax, MiniMax-CN and Kimi API-key quota retained fixed protected reads without credential or route downgrade.")
