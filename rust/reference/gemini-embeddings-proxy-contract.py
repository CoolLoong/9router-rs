from contextlib import ExitStack, closing
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import parse_qs, urlsplit


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "gemini_embedding_proxy_helpers", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "generativelanguage.googleapis.com"
MODEL = "gemini-embedding-001"
KEY = "offline-gemini-embedding-primary"
BACKUP = "offline-gemini-embedding-backup"
RELAY = "https://gemini-embedding-fixture.vercel.app/api/relay"
RELAY_KEY = "e" * 43
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unrelated-embedding.invalid"
shared.SECRETS = (KEY, BACKUP, RELAY_KEY)
delivered, errors = [], []
mode = "success"


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, location=None):
        output = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(output)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(output)

    def do_POST(self):
        try:
            path = self.path
            header_key = self.headers.get("x-goog-api-key")
            relay = path == "/vercel-relay/gemini-embedding-fixture.vercel.app"
            if relay:
                assert self.headers["x-relay-auth"] == RELAY_KEY
                assert self.headers["x-relay-target"] == "https://" + HOST
                path = self.headers["x-relay-path"]
                lane = "relay"
            elif path.startswith("/gemini/"):
                path = path.removeprefix("/gemini")
                lane = "bypass" if header_key else "direct"
                assert self.server.server_port == 52989
            else:
                lane = "tunnel"
                assert self.headers["host"] == HOST
            target = urlsplit(path)
            if lane == "direct":
                assert parse_qs(target.query) == {"key": [KEY]}
            else:
                assert header_key == KEY and not target.query
            if lane != "relay":
                assert self.headers.get("x-relay-auth") is None
            assert self.headers.get("cookie") is None and self.headers.get("authorization") is None
            assert self.headers.get("proxy-authorization") is None
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            batch = target.path.endswith(":batchEmbedContents")
            operation = "batchEmbedContents" if batch else "embedContent"
            assert target.path == f"/v1beta/models/{MODEL}:{operation}"
            requests = [{"model": f"models/{MODEL}", "content": {"parts": [{"text": text}]},
                         "outputDimensionality": 3} for text in ["fixture α", "fixture β"]]
            assert body == ({"requests": requests} if batch else requests[0]), body
            delivered.append((lane, operation))
            if mode == "redirect":
                return self.reply(307, {}, "https://unrelated-embedding.invalid/should-not-follow")
            if mode == "deny":
                return self.reply(403, {"error": {"message": f"Rejected {KEY} and {RELAY_KEY}"}})
            embeddings = [{"values": [0.125, 0.25, 0.5]}, {"values": [-1, 0, 1]}]
            return self.reply(200, {"embeddings": embeddings} if batch else {"embedding": embeddings[0]})
        except (AssertionError, KeyError, ValueError) as error:
            errors.append(str(error) or self.path)
            self.reply(422, {"error": "unexpected embedding fixture request"})


def embed(port, batch=False, model=MODEL):
    body = {"model": "gemini/" + model, "dimensions": 3,
            "input": ["fixture α", "fixture β"] if batch else "fixture α"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as connection:
        connection.request("POST", "/v1/embeddings", json.dumps(body), {
            "Content-Type": "application/json", "Authorization": "Bearer offline-chat-key",
            "Cookie": "not-for-upstream=1",
        })
        result = connection.getresponse()
        raw = result.read()
        assert all(secret.encode() not in raw for secret in shared.SECRETS)
        return result.status, json.loads(raw)


def use_connection(db, **details):
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData',json(?)) "
                    "WHERE id='gemini-embedding-1'", (json.dumps(details),))


def add_pool(db, name, **details):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (name, json.dumps(details)))


with tempfile.TemporaryDirectory(prefix="9router-gemini-embedding-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca, endpoint, wrong_name = shared.tls_services(stack, scratch, Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    proxies = {}
    for kind, scheme in [("connect", "http"), ("socks", "socks5h")]:
        proxy = shared.Proxy(("127.0.0.1", 0), shared.Tunnel)
        proxy.mode, proxy.reject, proxy.target = kind, False, endpoint
        stack.enter_context(shared.serving(proxy))
        proxies[kind] = (proxy, f"{scheme}://127.0.0.1:{proxy.server_address[1]}")
    with shared.gateway(scratch, "native", ca, []) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.executemany("""INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'gemini','apikey',?, ?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                [(f"gemini-embedding-{rank}", f"Gemini embedding {rank}", rank, json.dumps({"apiKey": key}))
                 for rank, key in enumerate((KEY, BACKUP), 1)])
        direct = [embed(port, batch) for batch in (False, True)]
        assert all(code == 200 for code, _ in direct) and not shared.TUNNELS, direct

        for kind, (_, proxy_url) in proxies.items():
            use_connection(db, connectionProxyEnabled=True, connectionProxyUrl=proxy_url)
            assert [embed(port, batch) for batch in (False, True)] == direct
            assert shared.TUNNELS[-2:] == [(kind, HOST, 443)] * 2 and shared.HELLOS[-2:] == [HOST] * 2

        connect, proxy_url = proxies["connect"]
        use_connection(db, connectionProxyEnabled=True, connectionProxyUrl=proxy_url)
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE providerConnections SET authType='oauth' WHERE id='gemini-embedding-2'")
        assert embed(port) == direct[0] and delivered[-1][0] == "tunnel"
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE providerConnections SET authType='oauth' WHERE id='gemini-embedding-1'")
        before = len(delivered), len(shared.TUNNELS)
        assert embed(port)[0] == 503 and (len(delivered), len(shared.TUNNELS)) == before
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE providerConnections SET authType='apikey' WHERE provider='gemini'")

        use_connection(db, connectionProxyEnabled=True, connectionProxyUrl=proxy_url, connectionNoProxy=HOST)
        before = len(shared.TUNNELS)
        assert embed(port) == direct[0] and delivered[-1][0] == "bypass" and len(shared.TUNNELS) == before

        add_pool(db, "managed", type="vercel", proxyUrl=RELAY, _managedVercel={"url": RELAY, "token": RELAY_KEY})
        use_connection(db, proxyPoolId="managed")
        assert [embed(port, batch) for batch in (False, True)] == direct and delivered[-1][0] == "relay"
        mode = "deny"
        status, problem = embed(port)
        assert status == 403 and problem["error"]["message"] == "[403]: Rejected [redacted] and [redacted]", problem
        before = len(delivered), len(shared.TUNNELS)
        assert embed(port, model=MODEL + "?test=invalid")[0] == 400
        add_pool(db, "managed", type="vercel", proxyUrl=RELAY)
        assert embed(port)[0] == 503
        add_pool(db, "strict", type="http", strictProxy=True, proxyUrl=proxy_url, noProxy=HOST)
        use_connection(db, proxyPoolId="strict")
        assert embed(port)[0] == 503 and (len(delivered), len(shared.TUNNELS)) == before

        use_connection(db, connectionProxyEnabled=True, connectionProxyUrl=proxy_url)
        mode = "redirect"
        assert embed(port)[0] == 502 and (len(delivered), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 1)
        connect.target = wrong_name
        assert embed(port)[0] == 502 and (len(delivered), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 2)
        connect.reject = True
        assert embed(port)[0] == 502 and (len(delivered), len(shared.TUNNELS)) == (before[0] + 1, before[1] + 3)
        assert not errors and not shared.ERRORS, (errors, shared.ERRORS)
        print("Gemini single and batch embeddings passed CONNECT, SOCKS5h, explicit bypass, and managed Relay without key-bearing proxy metadata.")
