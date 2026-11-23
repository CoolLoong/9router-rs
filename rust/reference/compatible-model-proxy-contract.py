from contextlib import ExitStack
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import select
import socket
import socketserver
import sqlite3
import tempfile
from urllib.parse import urlsplit


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
host = "catalog-fixture.invalid.org"
relay_host = "catalog-fixture.owner.workers.dev"
relay_token = "L" * 43
key = "synthetic-compatible-catalog-key"
api = {"Authorization": "Bearer offline-chat-key", "X-Unrelated": "never-forwarded"}
providers = ("openai-compatible-catalog-proxy", "anthropic-compatible-catalog-proxy")
paths = ("/openai/v1", "/anthropic/v1/messages")
prefixes = ("catalog-openai", "catalog-anthropic")
live_ids = {prefix + "/live-model" for prefix in prefixes}
ledger, dials, failures = [], [], []
probes = []


class Vendor(BaseHTTPRequestHandler):
    reject = False
    probe_status = None
    edit = None

    def log_message(self, *_args):
        pass

    def do_CONNECT(self):
        try:
            assert self.path == f"{host}:443"
            assert self.headers["Authorization"] is None and self.headers["X-Api-Key"] is None
            dials.append(("connect", self.path))
            self.send_error(502)
        except AssertionError as error:
            failures.append((self.path, repr(error)))
            self.send_error(422)

    def answer(self):
        try:
            target = urlsplit(self.path)
            path = target.path
            if target.scheme:
                assert target.scheme == "http" and target.netloc == host
                route = "http-proxy"
            elif self.server.server_port != 52989:
                route = "socks5h"
            elif path.startswith("/cloudflare-relay/"):
                assert path == "/cloudflare-relay/" + relay_host
                assert self.headers["X-Relay-Auth"] == relay_token
                assert self.headers["X-Relay-Target"] == "https://" + host
                path = self.headers["X-Relay-Path"]
                route = "relay"
            else:
                route = "direct"
            if self.probe_status is not None and path == "/api/tags":
                assert self.command == "GET" and self.headers["Authorization"] is None
                assert self.headers["X-Unrelated"] is None
                probes.append((route, self.command, path))
                return self.respond(200, {"models":[]})
            if self.probe_status is not None and path in ("/embedding/models", "/embedding/embeddings"):
                assert self.headers.get_all("Authorization") == ["Bearer " + key]
                probes.append((route, self.command, path))
                if self.command == "GET":
                    assert path == "/embedding/models"
                    return self.respond(self.probe_status.get("embedding", 404), {})
                assert self.command == "POST" and path == "/embedding/embeddings"
                assert json.loads(self.rfile.read(int(self.headers["Content-Length"]))) == {
                    "model":"test", "input":"ping",
                }
                return self.respond(200, {})
            if self.probe_status is None:
                assert path in {"/openai/v1/models", "/anthropic/v1/models", "/anthropic/v1/messages"}
            else:
                assert (self.command, path) in {
                    ("GET", "/openai/v1/models"), ("POST", "/anthropic/v1/v1/messages"),
                }
            headers = self.headers
            if self.probe_status is None or self.command == "POST":
                assert headers["Content-Type"] == "application/json"
            assert headers.get_all("Authorization") == ["Bearer " + key]
            assert headers["Proxy-Authorization"] is None and headers["X-9r-Cli-Token"] is None
            assert headers["X-Unrelated"] is None
            if path.startswith("/anthropic"):
                assert headers["X-Api-Key"] == key and headers["Anthropic-Version"] == "2023-06-01"
            else:
                assert headers["X-Api-Key"] is None
            internal = headers["X-9r-Internal-Models-Fetch"]
            if self.probe_status is not None:
                assert internal is None
                if self.command == "POST":
                    payload = json.loads(self.rfile.read(int(headers["Content-Length"])))
                    assert payload == {"model":"fixture-claude", "max_tokens":1,
                                       "messages":[{"role":"user", "content":"test"}]}
                probes.append((route, self.command, path))
                if self.edit and self.command == "GET":
                    with sqlite3.connect(self.edit) as sql:
                        sql.execute("""UPDATE providerConnections
                            SET data=json_set(data,'$.apiKey','operator-edited-key','$.testStatus','paused')
                            WHERE id=?""", (providers[0],))
                return self.respond(self.probe_status[self.command], {"error": key})
            assert internal in (None, "1")
            ledger.append((route, path, internal))
            reflected = "reflected-" + key if len(key) >= 6 else key
            status, body = 200, {"data": [{"id":"live-model"}, {"id": reflected}]}
            if self.reject:
                status, body = 502, {"error": key}
            self.respond(status, body)
        except (AssertionError, KeyError) as error:
            failures.append((self.path, repr(error)))
            self.send_error(422)

    def respond(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_GET = answer
    do_POST = answer


class Socks(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            version, size = self.rfile.read(2)
            assert version == 5 and 0 in self.rfile.read(size)
            self.request.sendall(b"\x05\x00")
            assert self.rfile.read(4) == b"\x05\x01\x00\x03"
            length, = self.rfile.read(1)
            domain = self.rfile.read(length).decode()
            port = int.from_bytes(self.rfile.read(2), "big")
            dials.append(("socks5h", domain, port))
            assert (domain, port) == (host, 80)
            with socket.create_connection(self.server.destination, timeout=3) as upstream:
                self.request.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
                sockets = (self.request, upstream)
                while available := select.select(sockets, [], [], 5)[0]:
                    for source in available:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            failures.append(("SOCKS", repr(error)))
        except OSError:
            pass


def save(db, origin, details=None):
    with sqlite3.connect(db) as sql:
        for provider, path, prefix in zip(providers, paths, prefixes):
            data = {"apiKey": key, "providerSpecificData": {
                "baseUrl": origin + path, "prefix": prefix, **(details or {}),
            }}
            sql.execute("""INSERT OR REPLACE INTO providerConnections
              (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
              VALUES (?,?,'apikey','Synthetic compatible node',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        (provider, provider, json.dumps(data)))


def settings(db, **fields):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?)
            ON CONFLICT(id) DO UPDATE SET data=excluded.data""", (json.dumps(fields),))


def get(port, path, headers=api):
    status, raw = shared["get"](port, path, headers)
    assert all(secret.encode() not in raw for secret in (key, relay_token) if len(secret) >= 6), raw
    return status, json.loads(raw)


def public(port, expected=live_ids, headers=api):
    status, body = get(port, "/v1/models", headers)
    assert status == 200, (status, body)
    seen = {model["id"] for model in body["data"] if model["owned_by"] in prefixes}
    assert seen == expected, seen
    return {model["id"]: model for model in body["data"] if model["id"] in seen}


def admin(port, expected=200):
    for provider in providers:
        status, body = get(port, f"/api/providers/{provider}/models", shared["ADMIN"])
        assert status == expected, (status, body)
        if expected == 200:
            assert {"id":"live-model"} in body["models"] and body["provider"] == provider


def post(port, path, body=None, headers=shared["ADMIN"]):
    conn = HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        conn.request("POST", path, body=json.dumps(body) if body is not None else None,
                     headers={**headers, "Content-Type":"application/json"})
        response = conn.getresponse()
        raw = response.read()
        assert all(secret.encode() not in raw for secret in (key, relay_token)), raw
        return response.status, json.loads(raw)
    finally:
        conn.close()


def probe_tests(port, db, local, remote, secure, http_url):
    missing = "/api/providers/absent/test"
    routes = tuple(f"/api/providers/{provider}/test" for provider in providers)
    batch = "/api/providers/test-batch"
    expected_valid = (200, {"valid":True,"error":None,"refreshed":False})
    denied = "Invalid API key or base URL"

    def configure(origin, details=None):
        save(db, origin, details)
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE providerConnections SET data=json_set(data,'$.defaultModel','fixture-claude') WHERE id=?",
                        (providers[1],))

    def saved():
        with sqlite3.connect(db) as sql:
            return {provider: json.loads(row) for provider, row in sql.execute(
                "SELECT id,data FROM providerConnections WHERE id IN (?,?)", providers)}

    settings(db, outboundProxyEnabled=False)
    Vendor.probe_status = {"GET":200,"POST":400}
    configure(remote, {"connectionProxyEnabled":True,"connectionProxyUrl":http_url})
    assert post(port, routes[0], headers={})[0] == 401 and not probes
    assert post(port, missing) == (404, {"error":"Connection not found"})
    assert tuple(post(port, route) for route in routes) == (expected_valid,) * 2
    assert probes == [("http-proxy", "GET", "/openai/v1/models"),
                      ("http-proxy", "POST", "/anthropic/v1/v1/messages")]
    assert all(row["testStatus"] == "active" and row["lastError"] is None for row in saved().values())

    Vendor.probe_status = {"GET":401,"POST":403}
    configure(remote, {"proxyPoolId": "catalog-socks"})
    status, result = post(port, batch, {"mode":"compatible"})
    assert status == 200 and result["summary"] == {"total":2,"passed":0,"failed":2}
    assert {row["connectionId"] for row in result["results"]} == set(providers)
    assert all(row["error"] == denied and row["diagnosis"] is None and row["latencyMs"] >= 0
               for row in result["results"])
    assert {entry[0] for entry in probes[-2:]} == {"socks5h"}
    assert all(row["testStatus"] == "error" and row["lastError"] == denied and row["lastErrorAt"]
               for row in saved().values())

    Vendor.probe_status = {"GET":200,"POST":529}
    configure(secure, {"proxyPoolId": "catalog-managed"})
    assert post(port, routes[0]) == expected_valid and post(port, routes[1]) == expected_valid
    assert [entry[0] for entry in probes[-2:]] == ["relay", "relay"]

    settings(db, outboundProxyEnabled=True, outboundProxyUrl=http_url, outboundNoProxy="127.0.0.1")
    configure(local)
    Vendor.edit = db
    assert post(port, routes[0]) == expected_valid and probes[-1][0] == "direct"
    Vendor.edit = None
    assert saved()[providers[0]]["apiKey"] == "operator-edited-key"
    assert saved()[providers[0]]["testStatus"] == "paused"

    settings(db, outboundProxyEnabled=True, outboundProxyUrl=http_url)
    configure(remote)
    Vendor.probe_status = {"GET":407,"POST":407}
    status, auth_failure = post(port, routes[1])
    assert status == 200 and not auth_failure["valid"] and auth_failure["error"] == "Proxy authentication required"

    before = len(probes)
    configure(secure)
    assert post(port, routes[0]) == (200, {"valid":False,"error":"fetch failed","refreshed":False})
    assert not post(port, routes[1])[1]["valid"] and len(probes) == before
    configure(local, {"connectionProxyEnabled":True,"connectionProxyUrl":"ftp://127.0.0.1:52989"})
    assert not post(port, routes[0])[1]["valid"] and not post(port, routes[1])[1]["valid"]
    assert len(probes) == before

    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES ('ollama-probe','ollama-local','apikey','Offline Ollama',1,1,?,
                  '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps({"providerSpecificData":{"baseUrl":remote}}),))
    assert post(port, "/api/providers/ollama-probe/test") == expected_valid
    assert probes[-1] == ("http-proxy", "GET", "/api/tags")

    with sqlite3.connect(db) as sql:
        for provider, path in zip(providers, paths):
            sql.execute("""INSERT INTO providerNodes (id,type,name,data,createdAt,updatedAt)
                VALUES (?,?,?,?,?,?)""", (provider, "compatible", "Saved node fixture", json.dumps({
                    "baseUrl":remote + path, "defaultModel":"fixture-claude",
                }), "2026-11-20", "2026-11-20"))
        sql.execute("""INSERT INTO providerNodes (id,type,name,data,createdAt,updatedAt)
            VALUES ('custom-embedding-proxy','custom-embedding','Saved Embedding',?, '2026-11-20', '2026-11-20')""",
                    (json.dumps({"baseUrl":remote + "/embedding"}),))
    Vendor.probe_status = {"GET":200,"POST":400}
    before_saved = saved()
    for provider in providers:
        status, result = post(port, "/api/providers/validate", {"provider":provider,"apiKey":key,
            "providerSpecificData":{"baseUrl":"http://127.0.0.1:52989/trap",
                                    "connectionProxyEnabled":True,"connectionProxyUrl":"ftp://127.0.0.1:52989"}})
        assert status == 200 and result == {"valid":True,"error":None}
    assert probes[-2:] == [("http-proxy", "GET", "/openai/v1/models"),
                           ("http-proxy", "POST", "/anthropic/v1/v1/messages")]
    embedding = {"provider":"custom-embedding-proxy", "apiKey":key}
    assert post(port, "/api/providers/validate", embedding) == (200, {"valid":True,"error":None})
    assert probes[-2:] == [("http-proxy", "GET", "/embedding/models"),
                           ("http-proxy", "POST", "/embedding/embeddings")]
    Vendor.probe_status["embedding"] = 407
    before = len(probes)
    assert post(port, "/api/providers/validate", embedding) == (200, {
        "valid":False, "error":"Proxy authentication required",
    })
    assert probes[before:] == [("http-proxy", "GET", "/embedding/models")]
    assert saved() == before_saved
    Vendor.probe_status = None


with tempfile.TemporaryDirectory(prefix="9router-compatible-catalog-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    serve = shared["serving"]
    stack.enter_context(serve(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    http = stack.enter_context(serve(ThreadingHTTPServer(("127.0.0.1", 0), Vendor)))
    target = stack.enter_context(serve(ThreadingHTTPServer(("127.0.0.1", 0), Vendor)))
    socks = shared["Proxy"](("127.0.0.1", 0), Socks)
    socks.destination = target.server_address
    stack.enter_context(serve(socks))
    http_url = f"http://127.0.0.1:{http.server_port}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("catalog-socks", "socks5", socks_url)]
    with shared["gateway"](scratch, "protected-catalog", None, pools) as (port, db):
        local = "http://127.0.0.1:52989"
        remote = "http://" + host
        secure = "https://" + host
        save(db, local)
        baseline = public(port)
        admin(port)
        assert ledger == [("direct", "/openai/v1/models", "1"), ("direct", "/anthropic/v1/messages", "1"),
                          ("direct", "/openai/v1/models", None), ("direct", "/anthropic/v1/models", None)]
        before = len(ledger)
        public(port, set(), {**api, "X-9r-Internal-Models-Fetch": "1"})
        assert len(ledger) == before

        settings(db, outboundProxyEnabled=True, outboundProxyUrl=http_url)
        save(db, remote)
        assert public(port) == baseline
        admin(port)
        assert [entry[0] for entry in ledger[-4:]] == ["http-proxy"] * 4
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO providerConnections
              (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
              VALUES ('qoder-legacy','qoder','apikey','Offline Qoder',1,1,'{}',
                      '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""")
        save(db, remote, {"proxyPoolId": "catalog-socks"})
        assert public(port) == baseline
        admin(port)
        assert [entry[0] for entry in ledger[-4:]] == ["socks5h"] * 4
        assert dials == [("socks5h", host, 80)] * 4

        settings(db, outboundProxyEnabled=True, outboundProxyUrl=http_url, outboundNoProxy="127.0.0.1")
        save(db, local)
        assert public(port) == baseline
        admin(port)
        assert [entry[0] for entry in ledger[-4:]] == ["direct"] * 4

        settings(db, outboundProxyEnabled=True, outboundProxyUrl=http_url)
        relay_address = "https://" + relay_host
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                VALUES ('catalog-managed',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        (json.dumps({"type":"cloudflare", "proxyUrl":relay_address, "strictProxy":True,
                                     "_managedCloudflare":{"url":relay_address,"token":relay_token}}),))
        save(db, secure, {"proxyPoolId": "catalog-managed"})
        assert public(port) == baseline
        admin(port)
        assert [entry[0] for entry in ledger[-4:]] == ["relay"] * 4
        before = len(ledger)
        save(db, local, {"proxyPoolId": "catalog-managed"})
        public(port, set())
        admin(port, 500)
        assert len(ledger) == before

        save(db, secure)
        public(port, set())
        admin(port, 500)
        assert dials[-4:] == [("connect", f"{host}:443")] * 4 and len(ledger) == before
        save(db, remote)
        Vendor.reject = True
        public(port, set())
        admin(port, 502)
        Vendor.reject = False
        assert [entry[0] for entry in ledger[before:]] == ["http-proxy"] * 4

        settings(db, outboundProxyEnabled=False)
        before = len(ledger)
        save(db, local, {"proxyPoolId": "missing"})
        public(port, set())
        admin(port, 500)
        assert len(ledger) == before
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE providerConnections SET isActive=0 WHERE id='qoder-legacy'")
        key = "model"
        save(db, local)
        assert public(port) == baseline
        key = "synthetic-compatible-catalog-key"
        probe_tests(port, db, local, remote, secure, http_url)
        assert not failures, failures
print("Compatible model discovery and connection probes passed direct, HTTP proxy, SOCKS5h, Relay, and failure checks.")
