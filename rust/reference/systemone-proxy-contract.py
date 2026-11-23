from contextlib import ExitStack, closing
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import sys


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "systemone_proxy_helpers", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "v1m.ir"
OPENCODE = "opencode.ai"
KEY = "offline-systemone-v1m"
BACKUP = "offline-systemone-v1m-backup"
RELAY = "https://systemone-fixture.vercel.app/api/relay"
RELAY_KEY = "s" * 43
MODELS = ["fixture-live/fixture-combo-a", "fixture-live/fixture-combo-b"]
INPUT = {"model": "jev/rev-latest", "state": {"text": "fixture", "revision": 3},
         "questions": {"route": {"type": "choice"}}, "provider_extension": {"active": True}}
OUTPUT = {"answers": {"route": {"choice": "b", "confidence": 0.96}},
          "usage": {"input_tokens": 9, "output_tokens": 3}, "provider_extension": {"active": True}}
shared.ZED_HOST, shared.CLINE_HOST = HOST, OPENCODE
shared.SECRETS = (KEY, BACKUP, RELAY_KEY)
deliveries = []
errors = []
mode = "success"


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, data, location=None):
        payload = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/v1/chat/completions":
                assert self.server.server_port == 52989
                assert body["model"] == "fixture-combo-b", body
                return self.reply(200, {"id": "offline-chat", "object": "chat.completion",
                    "model": body["model"], "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "fixture selected second model"}}]})
            if self.path == "/zen/v1/systemone":
                assert self.headers["Host"] == OPENCODE
                assert self.headers["Authorization"] == "Bearer public"
                assert body == {**INPUT, "model": "jev-1.13-free"}, body
                return self.reply(200, OUTPUT)
            relay = self.path == "/vercel-relay/systemone-fixture.vercel.app"
            direct = self.server.server_port == 52989 and not relay
            if relay:
                assert self.headers["X-Relay-Auth"] == RELAY_KEY
                assert self.headers["X-Relay-Target"] == "https://v1m.ir"
                assert self.headers["X-Relay-Path"] == "/v1/systemone"
            else:
                assert self.path == ("/v1m/v1/systemone" if direct else "/v1/systemone")
                assert self.headers["X-Relay-Auth"] is None
            assert direct or relay or self.headers["Host"] == HOST
            assert self.headers["Authorization"] == "Bearer " + KEY
            assert self.headers.get("Proxy-Authorization") is None
            assert self.headers.get("Cookie") is None
            deliveries.append((direct, body["state"]))
            if body["state"] == "choose second fixture model":
                assert body["model"] == "rev-latest"
                assert list(body["questions"]["route"]["criteria"]) == MODELS
                return self.reply(200, {"answers": {"route": {"choice": MODELS[1], "confidence": 0.99}}})
            assert body == {**INPUT, "model": "rev-latest"}, body
            if mode == "redirect":
                return self.reply(307, {}, "/must-not-follow")
            if mode == "deny":
                detail = "Rejected " + KEY + (" and " + RELAY_KEY if relay else "")
                return self.reply(403, {"error": {"message": detail}})
            return self.reply(202, OUTPUT)
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            errors.append((self.path, repr(error)))
            self.reply(422, {"error": "unexpected fixture request"})


def invoke(port, path="/v1/systemone", content=INPUT):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=6)) as connection:
        connection.request("POST", path, json.dumps(content), {"Content-Type": "application/json",
            "Authorization": "Bearer offline-chat-key", "Cookie": "unrelated=never-forward"})
        response = connection.getresponse()
        body = response.read()
        assert all(key.encode() not in body for key in (KEY, BACKUP, RELAY_KEY))
        return response.status, json.loads(body)


def patch(db, **changes):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?)
            ON CONFLICT(id) DO UPDATE SET data=json_patch(settings.data,excluded.data)""",
                    (json.dumps(changes),))


def route(db, address, bypass=""):
    patch(db, outboundProxyEnabled=True, outboundProxyUrl=address, outboundNoProxy=bypass)


def account_route(db, **details):
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData',json(?)) "
                    "WHERE id='systemone-1'", (json.dumps(details),))


def pool(db, pool_id, **details):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (pool_id, json.dumps(details)))


with tempfile.TemporaryDirectory(prefix="9router-systemone-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca, vendor, wrong_name = shared.tls_services(stack, scratch, Provider)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Provider)))
    proxies = {}
    for transport, scheme in [("connect", "http"), ("socks", "socks5h")]:
        proxy = shared.Proxy(("127.0.0.1", 0), shared.Tunnel)
        proxy.mode, proxy.reject, proxy.target = transport, False, vendor
        stack.enter_context(shared.serving(proxy))
        proxies[transport] = (proxy, f"{scheme}://127.0.0.1:{proxy.server_address[1]}")
    with shared.gateway(scratch, "native", ca, []) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.executemany("""INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'v1m','apikey',?, ?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                [(f"systemone-{rank}", f"System One {rank}", rank, json.dumps({"apiKey": key}))
                 for rank, key in enumerate((KEY, BACKUP), 1)])
            sql.execute("UPDATE providerConnections SET isActive=1 WHERE id='fixture-live-connection'")
            sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                VALUES ('fixture-routed-smart','fixture-routed-smart','llm',?,
                '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (json.dumps(MODELS),))
        patch(db, comboStrategies={"fixture-routed-smart": {"fallbackStrategy": "smart",
                    "routerModel": "jev/rev-latest", "smartPolicy": "per-turn"}})

        assert invoke(port) == (200, OUTPUT) and deliveries[-1][0] and not shared.TUNNELS
        for transport, (_, address) in proxies.items():
            route(db, address)
            assert invoke(port) == (200, OUTPUT) and not deliveries[-1][0]
            assert shared.TUNNELS[-1] == (transport, HOST, 443)
            assert shared.HELLOS[-1] == HOST
        connect, address = proxies["connect"]
        route(db, address, HOST)
        before = len(shared.TUNNELS)
        assert invoke(port) == (200, OUTPUT) and deliveries[-1][0] and len(shared.TUNNELS) == before
        route(db, address, "127.0.0.1")
        code, value = invoke(port, "/v1/chat/completions", {"model": "fixture-routed-smart", "stream": False,
            "messages": [{"role": "user", "content": "choose second fixture model"}]})
        assert code == 200 and value["choices"][0]["message"]["content"] == "fixture selected second model", value
        assert deliveries[-1] == (False, "choose second fixture model") and len(shared.TUNNELS) == before + 1

        for rejection, status in [("redirect", 502), ("deny", 403)]:
            mode = rejection
            before = len(deliveries), len(shared.TUNNELS)
            code, value = invoke(port)
            assert code == status, (code, value)
            assert (len(deliveries), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 1)
        connect.reject = True
        before = len(deliveries), len(shared.TUNNELS)
        assert invoke(port)[0] == 502
        assert (len(deliveries), len(shared.TUNNELS)) == (before[0], before[1] + 1)
        connect.reject, connect.target = False, wrong_name
        assert invoke(port)[0] == 502
        assert (len(deliveries), len(shared.TUNNELS)) == (before[0], before[1] + 2)
        route(db, "http://127.0.0.1:9/invalid-path", HOST)
        assert invoke(port)[0] == 503 and len(deliveries) == before[0]

        mode, connect.target = "success", vendor
        socks_address = proxies["socks"][1]
        route(db, socks_address)
        account_route(db, connectionProxyEnabled=True, connectionProxyUrl=address)
        assert invoke(port) == (200, OUTPUT)
        assert shared.TUNNELS[-1] == ("connect", HOST, 443)

        patch(db, outboundProxyEnabled=False)
        account_route(db, connectionProxyEnabled=True, connectionProxyUrl=address, connectionNoProxy=HOST)
        before = len(shared.TUNNELS)
        assert invoke(port) == (200, OUTPUT) and deliveries[-1][0] and len(shared.TUNNELS) == before

        pool(db, "systemone-socks", type="http", proxyUrl=socks_address, strictProxy=True)
        account_route(db, proxyPoolId="systemone-socks")
        assert invoke(port) == (200, OUTPUT) and shared.TUNNELS[-1] == ("socks", HOST, 443)
        patch(db, providerStrategies={"opencode": {"proxyPoolId": "systemone-socks"}})
        assert invoke(port, content={**INPUT, "model": "oc/jev-1.13-free"}) == (200, OUTPUT)
        assert shared.TUNNELS[-1] == ("socks", OPENCODE, 443) and shared.HELLOS[-1] == OPENCODE
        pool(db, "systemone-socks", type="http", proxyUrl=socks_address, strictProxy=True, noProxy=HOST)
        before = len(deliveries), len(shared.TUNNELS)
        assert invoke(port)[0] == 503 and (len(deliveries), len(shared.TUNNELS)) == before

        pool(db, "systemone-relay", type="vercel", proxyUrl=RELAY, _managedVercel={"url": RELAY, "token": RELAY_KEY})
        account_route(db, proxyPoolId="systemone-relay")
        assert invoke(port) == (200, OUTPUT) and len(deliveries) == before[0] + 1
        mode = "deny"
        status, problem = invoke(port)
        assert status == 403 and problem["error"]["message"] == "[403]: Rejected [redacted] and [redacted]", problem
        pool(db, "systemone-relay", type="vercel", proxyUrl=RELAY)
        before = len(deliveries), len(shared.TUNNELS)
        assert invoke(port)[0] == 503 and (len(deliveries), len(shared.TUNNELS)) == before
        assert not errors and not shared.ERRORS, (errors, shared.ERRORS)
        print("System One global and saved egress, OpenCode static pool and protected Smart classifier passed.")
