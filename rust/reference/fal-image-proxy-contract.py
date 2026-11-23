from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit

sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("fal_route_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "queue.fal.run"
ORIGIN = "https://" + HOST
MODEL = "fal-ai/fal-ai/flux/schnell"
BACKUP_MODEL = "fal-ai/fal-ai/flux/dev"
PROVIDER_KEY = "offline-fal-image"
BACKUP_KEY = "offline-fal-backup"
RELAY = "https://fal-fixture.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/fal-fixture.offline-lane.workers.dev"
RELAY_KEY = "F" * 43
AUTH = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.fal-fixture.dev"
shared.SECRETS = (PROVIDER_KEY, BACKUP_KEY, RELAY_KEY)
DELIVERIES, FAILURES = [], []
MODE = "normal"


class QueueVendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw, location=None):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def exchange(self):
        try:
            route = "tunnel"
            path = self.path
            if path == RELAY_PATH:
                route = "relay"
                assert self.headers["x-relay-auth"] == RELAY_KEY
                assert self.headers["x-relay-target"] == ORIGIN
                path = self.headers["x-relay-path"]
            elif self.server.server_port == 52989:
                route = "direct"
                assert path.startswith("/fal-image/")
                path = path.removeprefix("/fal-image")
            else:
                assert self.headers["Host"] == HOST
            assert route == "relay" or not any(self.headers[name] for name in ("x-relay-auth", "x-relay-target", "x-relay-path"))
            assert path.startswith("/fal-ai/flux/") and not urlsplit(path).query
            assert all(secret not in path for secret in shared.SECRETS)
            assert self.headers.get_all("Authorization") == ["Key " + PROVIDER_KEY]
            assert not any(self.headers[name] for name in ("Cookie", "Proxy-Authorization", "x-gateway-secret"))
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            DELIVERIES.append((route, self.command, path, json.loads(raw) if raw else None))

            if MODE == "secret-job" and self.command == "POST":
                base = f"{ORIGIN}/fal-ai/flux/requests/{RELAY_KEY}"
                result = {"request_id": RELAY_KEY, "response_url": base, "status_url": base + "/status"}
                return self.reply(200, json.dumps(result).encode())
            if MODE == "relay-reject" and self.command == "POST":
                return self.reply(401, json.dumps({"message": f"refused {PROVIDER_KEY} {RELAY_KEY}"}).encode())
            if MODE == "relay-redirect" and path.endswith("/status"):
                return self.reply(307, b"{}", "http://127.0.0.1:52989/unsafe")
            headers = {"Authorization": self.headers["Authorization"]}
            if self.command == "POST":
                headers["Content-Type"] = "application/json"
            with closing(HTTPConnection("127.0.0.1", 52988, timeout=4)) as upstream:
                upstream.request(self.command, "/fal-image" + path, raw or None, headers)
                response = upstream.getresponse()
                data = response.read()
                if MODE == "change-route" and self.command == "POST":
                    configure(DATABASE, {"proxyPoolId": "managed-relay"})
                    with sqlite3.connect(DATABASE) as sql:
                        sql.execute("UPDATE proxyPools SET isActive=0 WHERE id='fixture-socks'")
                if MODE == "fail-poll-tunnel" and self.command == "POST":
                    CONNECT.reject = True
                if MODE == "fail-result-tunnel" and path.endswith("/status") and json.loads(data)["status"] == "COMPLETED":
                    CONNECT.reject = True
                self.reply(response.status, data, location=response.getheader("Location"))
        except (AssertionError, ValueError, KeyError, TypeError) as error:
            FAILURES.append((self.path, repr(error)))
            self.reply(422, b"{}")

    do_POST = do_GET = exchange


def configure(db, fields, backup=False):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'fal-ai','apikey','Offline Fal',?,1,?,
                '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    ("fal-backup" if backup else "fal-main", 2 if backup else 1,
                     json.dumps({"apiKey": BACKUP_KEY if backup else PROVIDER_KEY, "providerSpecificData": fields})))


def generate(port, model=MODEL, prompt="Offline protected Fal image"):
    payload = {"model": model, "prompt": prompt, "n": 2, "size": "1024x1536"}
    with closing(HTTPConnection("127.0.0.1", port, timeout=7)) as connection:
        connection.request("POST", "/v1/images/generations", json.dumps(payload), AUTH)
        response = connection.getresponse()
        raw = response.read()
        assert not any(secret.encode() in raw for secret in shared.SECRETS)
        return response.status, json.loads(raw)


def success(port, route):
    before = len(DELIVERIES)
    status, result = generate(port)
    assert status == 200 and result["data"] == [{"url": "https://image.example.invalid/fal-finished.png"}], result
    rows = DELIVERIES[before:]
    assert [call[0:2] for call in rows] == [(route, "POST"), (route, "GET"), (route, "GET"), (route, "GET")]
    submit_path = "/fal-ai/flux/schnell"
    assert rows[0][2:] == (submit_path, {"prompt": "Offline protected Fal image", "num_images": 2,
                                      "image_size": {"width": 1024, "height": 1536}})
    assert rows[1][2] == rows[2][2] == rows[3][2] + "/status"
    return rows


def cases(port, db, http, socks):
    global MODE
    configure(db, {})
    configure(db, {}, backup=True)
    success(port, "direct")
    assert not shared.TUNNELS
    configure(db, http)
    success(port, "tunnel")
    assert shared.TUNNELS == [("connect", HOST, 443)] * 4 and shared.HELLOS == [HOST] * 4

    configure(db, {"proxyPoolId": "fixture-socks"})
    MODE = "change-route"
    success(port, "tunnel")
    assert shared.TUNNELS[-4:] == [("socks", HOST, 443)] * 4
    MODE = "normal"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET isActive=1 WHERE id='fixture-socks'")
    configure(db, {"proxyPoolId": "managed-relay"})
    success(port, "relay")

    before = len(shared.TUNNELS)
    configure(db, http | {"connectionNoProxy": ".fal.run"})
    success(port, "direct")
    assert len(shared.TUNNELS) == before
    shared.enable_global_proxy(db, socks)
    success(port, "tunnel")
    assert shared.TUNNELS[-4:] == [("socks", HOST, 443)] * 4
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.fal.run') WHERE id='fixture-socks'")
        sql.execute("""INSERT INTO combos(id,name,kind,models,createdAt,updatedAt)
            VALUES ('protected-fal-combo','protected-fal-combo','image',?,
                    '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps([MODEL, BACKUP_MODEL]),))
    before = (len(DELIVERIES), len(shared.TUNNELS))
    for fields in ({"proxyPoolId":"fixture-socks"}, {"proxyPoolId":"missing"},
                   {"proxyPoolId":"unmanaged-relay"}, {"strictProxy":True}):
        configure(db, fields)
        assert generate(port, "protected-fal-combo")[0] == 502
        assert (len(DELIVERIES), len(shared.TUNNELS)) == before

    for scenario, deliveries, stages, reason in (
        ("fail-poll-tunnel", 1, ["POST"], "Fal status request failed"),
        ("fail-result-tunnel", 3, ["POST", "GET", "GET"], "Fal result request failed"),
    ):
        MODE = scenario
        configure(db, http)
        before = len(DELIVERIES)
        status, result = generate(port, "protected-fal-combo")
        CONNECT.reject = False
        assert status == 502 and reason in result["error"]["message"], result
        assert len(DELIVERIES) == before + deliveries
        assert [row[:2] for row in DELIVERIES[before:]] == [("tunnel", method) for method in stages]

    for scenario, status, stages, reason in (
        ("secret-job", 502, ["POST"], "valid request ID"),
        ("relay-reject", 401, ["POST"], "[redacted]"),
        ("relay-redirect", 502, ["POST", "GET"], "Fal status 307"),
    ):
        MODE = scenario
        configure(db, {"proxyPoolId":"managed-relay"})
        before = len(DELIVERIES)
        code, response = generate(port, "protected-fal-combo")
        assert code == status and reason in response["error"]["message"], response
        assert [row[:2] for row in DELIVERIES[before:]] == [("relay", method) for method in stages]
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("Fal shared one selected CONNECT/SOCKS5h or validated Relay across all phases; explicit bypass and no resubmit proved.")


with tempfile.TemporaryDirectory(prefix="9router-fal-proxy-") as folder, ExitStack() as stack:
    scratch = Path(folder)
    ca, target, _ = shared.tls_services(stack, scratch, QueueVendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), QueueVendor)))
    CONNECT, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((CONNECT, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{CONNECT.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks", "socks5", socks_url), ("managed-relay", "cloudflare", RELAY),
             ("unmanaged-relay", "cloudflare", RELAY)]
    node = subprocess.Popen(["node", "--input-type=module", "-e", """
      import {createServer} from 'node:http';
      import {handleImageGenerationMock} from './rust/reference/mock-image-generation.mjs';
      createServer(handleImageGenerationMock).listen(52988, '127.0.0.1', () => console.log('ready'));
    """], cwd=shared.ROOT, env={"HOME": folder, "PATH": os.environ["PATH"], "NODE_NO_WARNINGS": "1"},
                            stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([node.stdout], [], [], 5)[0] and node.stdout.readline().strip() == "ready"
        with shared.gateway(scratch, "image", ca, pools) as (port, DATABASE):
            with sqlite3.connect(DATABASE) as sql:
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed-relay'",
                            (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
                sql.execute("INSERT OR REPLACE INTO settings (id,data) VALUES(1,'{}')")
            cases(port, DATABASE, http, socks_url)
    finally:
        node.terminate()
        node.wait(timeout=4)
