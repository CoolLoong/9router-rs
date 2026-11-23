from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import base64
import http.client
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
from urllib.parse import parse_qs, urlsplit


fixture_path = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("image_route_fixture", fixture_path)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
OPENAI, GEMINI = "api.openai.com", "generativelanguage.googleapis.com"
OPENAI_PATH, GEMINI_PATH = "/v1/images/generations", "/v1beta/models/gemini-2.5-flash-image:generateContent"
KEYS = {OPENAI: "offline-openai-speech", GEMINI: "offline-provider-placeholder"}
PRIMARY = {"openai": OPENAI, "gemini": GEMINI}
MODELS = {"openai": "openai/gpt-image-2.5", "gemini": "gemini/gemini-2.5-flash-image"}
PROMPTS = {"openai": "OpenAI fixture image with usage", "gemini": "Gemini image with data URI"}
RELAY = "https://image-fixture.offline-lane.workers.dev"
RELAY_TOKEN = "R" * 43
RELAY_PATH = "/cloudflare-relay/image-fixture.offline-lane.workers.dev"
AUTH = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}
ASSET = "http://93.184.216.34:52991/generated.png?signature=offline-fixture"
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII=")
DELIVERIES, DOWNLOADS, FAILURES = [], [], []
BEHAVIOR = "normal"
shared.ZED_HOST, shared.CLINE_HOST = OPENAI, GEMINI
shared.SECRETS = (*KEYS.values(), RELAY_TOKEN)


class ImageVendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw, mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        try:
            assert self.server.server_port == 52991 and self.path == urlsplit(ASSET).path + "?" + urlsplit(ASSET).query
            assert not any(self.headers[x] for x in ("Authorization", "x-goog-api-key", "x-relay-auth", "Cookie"))
            DOWNLOADS.append(self.path)
            self.reply(200, PNG, "image/png")
        except AssertionError:
            FAILURES.append(("unexpected GET", self.path))
            self.reply(422, b"{}")

    def do_POST(self):
        try:
            original = urlsplit(self.path)
            host = self.headers["Host"]
            relay = original.path == RELAY_PATH
            if relay:
                host = urlsplit(self.headers["x-relay-target"]).hostname
                assert self.headers["x-relay-auth"] == RELAY_TOKEN and not original.query
                target = urlsplit(self.headers["x-relay-path"])
                assert not target.query
            elif self.server.server_port == 52989:
                host = OPENAI if original.path.startswith("/openai/") else GEMINI
                target = urlsplit(self.path.removeprefix("/openai").removeprefix("/gemini"))
            else:
                target = original
            lane = "relay" if relay else "direct" if self.server.server_port == 52989 else "tunnel"
            assert target.path == (OPENAI_PATH if host == OPENAI else GEMINI_PATH)
            assert self.headers["Content-Type"] == "application/json"
            assert not any(self.headers[x] for x in ("Proxy-Authorization", "Cookie", "x-internal-token"))
            assert relay or not any(self.headers[x] for x in ("x-relay-auth", "x-relay-target", "x-relay-path"))
            if host == OPENAI:
                assert self.headers.get_all("Authorization") == ["Bearer " + KEYS[host]]
                assert not target.query and self.headers["x-goog-api-key"] is None
            else:
                assert host == GEMINI and self.headers["Authorization"] is None
                if relay:
                    assert self.headers.get_all("x-goog-api-key") == [KEYS[host]]
                else:
                    assert parse_qs(target.query) == {"key": [KEYS[host]]} and self.headers["x-goog-api-key"] is None
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            DELIVERIES.append((lane, host, target.path, json.loads(raw)))
            if BEHAVIOR == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            if BEHAVIOR == "unavailable":
                return self.reply(503, json.dumps({"error":{"message":f"fixture rejected {KEYS[host]} {RELAY_TOKEN}"}}).encode())
            if BEHAVIOR == "asset":
                return self.reply(200, json.dumps({"created": 1784419200, "data": [{"url": ASSET}]}).encode())
            mapped = "/openai" + OPENAI_PATH if host == OPENAI else "/gemini" + GEMINI_PATH + "?key=" + KEYS[GEMINI]
            headers = {"Content-Type": "application/json"}
            if host == OPENAI:
                headers["Authorization"] = self.headers["Authorization"]
            with closing(http.client.HTTPConnection("127.0.0.1", 52988, timeout=3)) as upstream:
                upstream.request("POST", mapped, raw, headers)
                response = upstream.getresponse()
                data = response.read()
                assert response.status == 200, data
                self.reply(200, data)
        except (AssertionError, KeyError, ValueError, TypeError) as error:
            FAILURES.append((self.path, repr(error)))
            self.reply(422, b"{}")


def configure(db, provider, fields, other=False, key=None):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES (?,?,'apikey','Offline image provider',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if other else "-primary"), provider, 2 if other else 1,
                     json.dumps({"apiKey": key or KEYS[PRIMARY[provider]], "providerSpecificData": fields})))


def settings(db, **fields):
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT INTO settings(id,data) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                    (json.dumps(fields),))


def application(port, provider, binary=False, model=None):
    path = "/v1/images/generations" + ("?response_format=binary" if binary else "")
    payload = {"model": model or MODELS[provider], "prompt": PROMPTS[provider]}
    if provider == "openai":
        payload |= {"n": 2, "size": "512x512", "quality": "high", "style": "vivid", "output_format": "png",
                    "response_format": "b64_json"}
    else:
        payload |= {"image": "data:image/jpeg;base64," + base64.b64encode(b"fake uploaded jpeg").decode(),
                    "output_format": "webp"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as conn:
        conn.request("POST", path, json.dumps(payload), AUTH)
        res = conn.getresponse()
        return res.status, res.getheader("Content-Type"), res.read()


def cases(port, db, connect, http_proxy, socks):
    global BEHAVIOR
    for provider in PRIMARY:
        configure(db, provider, {})
        configure(db, provider, {}, other=True, key="offline-backup-never-used")
    baseline = {provider: application(port, provider, binary=provider == "gemini") for provider in PRIMARY}
    direct = tuple(DELIVERIES)
    assert len(direct) == 2 and all(row[0] == "direct" for row in direct)
    assert all(reply[0] == 200 for reply in baseline.values())
    assert baseline["gemini"][1:] == ("image/webp", b"fixture Gemini webp bytes")

    configure(db, "openai", http_proxy)
    configure(db, "gemini", {"proxyPoolId": "fixture-socks"})
    assert application(port, "openai") == baseline["openai"]
    assert application(port, "gemini", binary=True) == baseline["gemini"]
    assert [row[0] for row in DELIVERIES[-2:]] == ["tunnel", "tunnel"]
    assert [row[1:] for row in DELIVERIES[-2:]] == [row[1:] for row in direct]
    assert [call[0] for call in shared.TUNNELS] == ["connect", "socks"]
    assert shared.HELLOS == [OPENAI, GEMINI]

    configure(db, "openai", http_proxy | {"connectionNoProxy": ".openai.com"})
    assert application(port, "openai") == baseline["openai"]
    assert DELIVERIES[-1][0] == "direct" and len(shared.TUNNELS) == 2
    settings(db, outboundProxyEnabled=True, outboundProxyUrl=socks)
    assert application(port, "openai") == baseline["openai"]
    assert DELIVERIES[-1][0] == "tunnel" and shared.TUNNELS[-1][0] == "socks", (DELIVERIES[-1][0], shared.TUNNELS)
    settings(db)

    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.googleapis.com') WHERE id='fixture-socks'")
    before = (len(DELIVERIES), len(shared.TUNNELS))
    assert application(port, "gemini", binary=True)[0] == 503
    for flags in ({"proxyPoolId": "missing"}, {"strictProxy": True}, {"proxyPoolId": "fixture-unmanaged"},
                  {"proxyPoolId": "fixture-mismatched"}, {"connectionProxyEnabled": True, "connectionNoProxy": "*"}):
        configure(db, "openai", flags)
        assert application(port, "openai")[0] == 503
    assert (len(DELIVERIES), len(shared.TUNNELS)) == before

    for provider in PRIMARY:
        configure(db, provider, {"proxyPoolId":"fixture-managed"})
        assert application(port, provider, binary=provider == "gemini") == baseline[provider]
    assert [row[0] for row in DELIVERIES[-2:]] == ["relay", "relay"]

    BEHAVIOR = "redirect"
    before = len(DELIVERIES)
    status, _, raw = application(port, "gemini", binary=True)
    assert status == 502 and b"redirect refused" in raw and len(DELIVERIES) == before + 1
    BEHAVIOR = "unavailable"
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES ('proxy-combo','proxy-combo','image',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps([MODELS["openai"], MODELS["openai"]]),))
    before = len(DELIVERIES)
    status, _, raw = application(port, "openai", model="proxy-combo")
    assert status == 503 and b"[redacted]" in raw and not any(secret.encode() in raw for secret in shared.SECRETS)
    assert len(DELIVERIES) == before + 1
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES ('mixed-proxy-combo','mixed-proxy-combo','image',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps(["nanobanana/nanobanana-pro", MODELS["openai"]]),))
    status, _, raw = application(port, "openai", model="mixed-proxy-combo")
    assert status == 503 and b"cannot use the configured proxy" in raw and len(DELIVERIES) == before + 1
    BEHAVIOR = "normal"
    connect.reject = True
    configure(db, "openai", http_proxy)
    before = (len(DELIVERIES), len(shared.TUNNELS))
    assert application(port, "openai", model="proxy-combo")[0] == 502
    assert len(DELIVERIES) == before[0] and len(shared.TUNNELS) == before[1] + 1
    connect.reject = False

    BEHAVIOR = "asset"
    assert application(port, "openai", binary=True) == (200, "image/png", PNG)
    assert DOWNLOADS == [urlsplit(ASSET).path + "?" + urlsplit(ASSET).query]
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("Synchronous OpenAI/Gemini image wire, binary output, CONNECT/SOCKS, no-proxy and validated Relay passed without resubmit.")


with tempfile.TemporaryDirectory(prefix="9router-image-proxy-") as temp, ExitStack() as stack:
    scratch = Path(temp)
    ca, target, _ = shared.tls_services(stack, scratch, ImageVendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), ImageVendor)))
    stack.enter_context(shared.serving(ThreadingHTTPServer(("93.184.216.34", 52991), ImageVendor)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http_proxy = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks", "socks5", socks_url), ("fixture-unmanaged", "cloudflare", RELAY),
             ("fixture-managed", "cloudflare", RELAY), ("fixture-mismatched", "cloudflare", RELAY)]
    node = subprocess.Popen(["node", "--input-type=module", "-e", """
      import {createServer} from 'node:http';
      import {handleImageGenerationMock} from './rust/reference/mock-image-generation.mjs';
      createServer(handleImageGenerationMock).listen(52988, '127.0.0.1', () => console.log('ready'));
    """], cwd=shared.ROOT, env={"HOME": temp, "PATH": os.environ["PATH"], "NODE_NO_WARNINGS": "1"},
                            stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([node.stdout], [], [], 5)[0] and node.stdout.readline().strip() == "ready"
        with shared.gateway(scratch, "image", ca, pools) as (port, db):
            with sqlite3.connect(db) as sql:
                for id_, url in (("fixture-managed", RELAY), ("fixture-mismatched", RELAY + "/wrong")):
                    sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id=?",
                                (json.dumps({"url": url, "token": RELAY_TOKEN}), id_))
            cases(port, db, connect, http_proxy, socks_url)
    finally:
        node.terminate()
        node.wait(timeout=4)
