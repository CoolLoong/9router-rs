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
    "ollama_cloud_quota_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "ollama.com"
USAGE, PROFILE, BALANCE = "/api/usage", "/api/me", "/api/balance"
KEY = "offline-ollama-cloud-quota-key"
RELAY = "https://ollama-usage.offline-lane.workers.dev"
RELAY_KEY = "L" * 43
CLI = {"x-9r-cli-token": "68977b6fd9422fbb", "x-internal-token": "not forwarded", "Cookie": "discard"}
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.ollama.invalid"
shared.SECRETS = (KEY, RELAY_KEY)
CALLS, ERRORS, PROBES = [], [], []
MODE, SWITCH = "ratios", None


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, payload, location=None):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def check(self):
        try:
            assert self.headers["Host"] == HOST and self.path in (USAGE, PROFILE, BALANCE)
            assert self.command == ("POST" if self.path == PROFILE else "GET")
            assert self.headers.get_all("Authorization") == ["Bearer " + KEY]
            assert self.headers.get_all("Accept") == ["application/json"]
            assert self.headers.get_all("Content-Length") == (["0"] if self.path == PROFILE else None)
            assert not any(self.headers[field] for field in ("Proxy-Authorization", "x-relay-auth", "Cookie",
                                                             "x-internal-token", "x-relay-path", "x-relay-target", "Content-Type"))
            CALLS.append((self.command, self.path))
            if self.path == USAGE:
                if SWITCH:
                    SWITCH()
                if MODE == "auth":
                    return self.reply(401, {"error": "key " + KEY})
                if MODE == "error":
                    return self.reply(503, {"error": "vendor echoed " + KEY})
                if MODE == "malformed":
                    return self.reply(200, b"not JSON")
                if MODE == "usage redirect":
                    return self.reply(307, {}, "https://untrusted.invalid/capture")
                if MODE == "ratios":
                    return self.reply(200, {"limits": {"session": {"usage": "0.193"}, "weekly": {"usage": 1.2},
                                                        "monthly": {"usage": "unusable"}}})
                return self.reply(200, {"limits": {}})
            if self.path == PROFILE:
                if MODE == "profile redirect":
                    return self.reply(307, {}, f"https://{HOST}/must-not-follow")
                if MODE == "denied profile":
                    return self.reply(503, {"error": "optional field unavailable"})
                return self.reply(200, {"Plan": "TEAM", "CreatedAt": "2023-01-31T10:20:30.999Z"})
            if MODE == "balance redirect":
                return self.reply(307, {}, f"https://{HOST}/must-not-follow")
            self.reply(200, {"included": {"session": {"remaining_percent": "91.268"},
                                        "allowance_usd": "100.40", "balance_usd": "85.21"},
                             "purchased": {"balance_usd": "12.75"}})
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((self.command, self.path, repr(error)))
            self.reply(422, {})

    do_GET = do_POST = check


class DirectProbe(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(3)
        if hello := self.request.recv(1024):
            PROBES.append(hello)


def settings(db, proxy=None):
    data = {"requireLogin": True}
    if proxy:
        data.update(outboundProxyEnabled=True, outboundProxyUrl=proxy)
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,?)", (json.dumps(data),))


def save(db, flags=None, key=KEY):
    data = {"apiKey": key, "providerSpecificData": {"baseUrl": "https://untrusted.invalid", **(flags or {})}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('ollama-cloud','ollama','api_key','Offline Ollama Cloud',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps(data),))


def snapshot(db):
    with sqlite3.connect(db) as sql:
        return sql.execute("SELECT data,updatedAt FROM providerConnections WHERE id='ollama-cloud'").fetchone()


def read(port, status=200, headers=CLI):
    code, raw = shared.get(port, "/api/usage/ollama-cloud", headers)
    assert not any(secret.encode() in raw for secret in shared.SECRETS)
    assert code == status, (code, raw)
    return json.loads(raw)


def verify(port, db, connect, http, socks, wrong_tls, bypass):
    global MODE, SWITCH
    save(db)
    settings(db, http["connectionProxyUrl"])
    read(port, 401, {})
    assert not CALLS and not shared.TUNNELS
    MODE = "denied profile"
    SWITCH = lambda: settings(db, socks)
    before = snapshot(db)
    value = read(port)
    assert value["plan"] == "Ollama Cloud"
    assert value["quotas"]["Session (5h)"]["used"] == 8.73
    assert value["quotas"]["Allowance ($)"]["used"] == 15.19
    assert value["quotas"]["Purchased Credits"]["total"] == 12.75
    assert CALLS == [("GET", USAGE), ("POST", PROFILE), ("GET", BALANCE)]
    assert shared.TUNNELS == [("connect", HOST, 443)] * 3
    assert snapshot(db) == before
    SWITCH, MODE = None, "ratios"
    value = read(port)
    assert value["plan"] == "Team" and value["quotas"]["Session (5h)"]["used"] == 19
    assert value["quotas"]["Weekly (7d)"]["remainingPercentage"] == 0 and "Monthly" not in value["quotas"]
    assert CALLS[-2:] == [("GET", USAGE), ("POST", PROFILE)]
    assert shared.TUNNELS[-2:] == [("socks", HOST, 443)] * 2
    assert all(host == HOST for host in shared.HELLOS)

    settings(db)
    save(db, {"proxyPoolId": "socks"})
    for mode, result in (("auth", "Ollama Cloud API key invalid or expired."),
                         ("error", "Ollama Cloud usage API error (503)."),
                         ("malformed", "Ollama Cloud usage response was not JSON."),
                         ("usage redirect", "Ollama Cloud error: Quota upstream redirect refused")):
        MODE = mode
        count = (len(CALLS), len(shared.TUNNELS))
        assert read(port)["message"] == result
        assert CALLS[count[0]:] == [("GET", USAGE)] and shared.TUNNELS[count[1]:] == [("socks", HOST, 443)]
    for mode, plan, quotas in (("profile redirect", "Ollama Cloud", True), ("balance redirect", "Team", False)):
        MODE = mode
        count = (len(CALLS), len(shared.TUNNELS))
        value = read(port)
        assert value["plan"] == plan and bool(value["quotas"]) == quotas
        assert CALLS[count[0]:] == [("GET", USAGE), ("POST", PROFILE), ("GET", BALANCE)]
        assert shared.TUNNELS[count[1]:] == [("socks", HOST, 443)] * 3

    MODE = "ratios"
    count = (len(CALLS), len(shared.TUNNELS))
    for flags in ({"proxyPoolId": "missing"}, {"proxyPoolId": "relay"}, {"strictProxy": True},
                  {"vercelRelayUrl": RELAY}, http | {"strictProxy": True, "connectionNoProxy": "*"}):
        save(db, flags)
        assert read(port, 503)["error"] == "Account quota proxy is unavailable"
    save(db, http, key="")
    assert read(port)["message"] == "Ollama Cloud API key not available."
    save(db, http, key="malformed\nkey")
    assert read(port, 401)["error"] == "The saved quota API key is invalid"
    assert (len(CALLS), len(shared.TUNNELS)) == count

    save(db, http)
    connect.target = wrong_tls
    assert "fetch failed" in read(port)["message"]
    connect.reject = True
    assert "fetch failed" in read(port)["message"]
    assert len(CALLS) == count[0] and shared.TUNNELS[count[1]:] == [("connect", HOST, 443)] * 2
    assert not PROBES
    if bypass:
        save(db, http | {"connectionNoProxy": ".ollama.com"})
        assert "fetch failed" in read(port)["message"]
        assert len(CALLS) == count[0] and len(shared.TUNNELS) == count[1] + 2
        assert len(PROBES) == 1 and PROBES[0].startswith(b"\x16\x03") and HOST.encode() in PROBES[0]
        assert not any(secret.encode() in PROBES[0] for secret in shared.SECRETS)


with tempfile.TemporaryDirectory(prefix="9router-ollama-cloud-quota-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, upstream, wrong_tls = shared.tls_services(stack, scratch, Vendor)
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, upstream
        stack.enter_context(shared.serving(proxy))
    bypass = "--bypass-probe" in sys.argv[2:]
    if bypass:
        stack.enter_context(shared.serving(shared.Proxy(("127.0.0.1", 443), DirectProbe)))
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_address = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    with shared.gateway(scratch, "ollama-cloud", ca,
                        [("socks", "socks5", socks_address), ("relay", "cloudflare", RELAY)]) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                        (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
        verify(port, db, connect, http, socks_address, wrong_tls, bypass)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)

print("Ollama Cloud quota used one protected route for usage, profile and balance; explicit bypass and failure boundaries verified.")
