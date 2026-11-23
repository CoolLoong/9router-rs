from contextlib import ExitStack, closing, contextmanager
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import socketserver
import sqlite3
import ssl
import subprocess
import sys
import tempfile
from threading import Thread
import time


ROOT = Path(__file__).resolve().parents[2]
REFERENCE = sys.argv[1] == "reference"
BINARY = None if REFERENCE else str(Path(sys.argv[2]).resolve())
STRICT = sys.argv[-1] == "strict"
PROXY = sys.argv[-1] == "proxy"
VALIDATE = "/api/providers/validate"
SAVED = "/api/providers/fal-fixture-account/test"
HOST = "api.fal.ai"
CATALOG = "/v1/models?limit=1"
UPSTREAM = "/validate/" + HOST + CATALOG
RESPONSES = {"ok": 200, "no-content": 204, "missing": 404, "unauthorized": 401, "forbidden": 403,
             "rate-limited": 429, "unavailable": 503, "redirect": 307}
LEDGER, CAPTURES, ERRORS = [], [], []
DESTINATIONS, TUNNELS, HELLOS = [], [], []


class Catalog(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, headers=None):
        body = b"" if status == 204 else json.dumps({"models": [], "message": "fixture data"}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        ERRORS.append("Unexpected catalog method POST")
        self.reply(404)

    def do_GET(self):
        if self.path == "/redirected":
            CAPTURES.append(self.headers.get("Authorization"))
            return self.reply(200)
        try:
            proxied = getattr(self.server, "proxied", False)
            assert self.path == (CATALOG if proxied else UPSTREAM), self.path
            if proxied:
                assert self.headers["Host"] == HOST
            auth, = self.headers.get_all("Authorization") or []
            assert auth.startswith("Key offline-fal-")
            assert not any(self.headers[name] for name in ("Cookie", "Proxy-Authorization", "X-Relay-Auth"))
            DESTINATIONS.append(proxied)
            label = auth.removeprefix("Key offline-fal-")
            LEDGER.append(label)
            if label == "disconnect":
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            status = RESPONSES[label]
            redirect = {"Location": "http://127.0.0.1:52979/redirected"} if status == 307 else None
            self.reply(status, redirect)
        except (AssertionError, KeyError, ValueError) as error:
            ERRORS.append(str(error) or "Unexpected Fal catalog request")
            self.reply(422)


def call(port, path, body=None, *, method="POST", cookie=None):
    headers = {"Content-Type": "application/json"}
    if cookie:
        headers["Cookie"] = cookie
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=10)) as client:
        client.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read()), response.getheaders()


def save(db, label, details=None):
    data = {"apiKey": "offline-fal-" + label, "testStatus": "unknown"}
    if details is not None:
        data["providerSpecificData"] = details
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('fal-fixture-account','fal-ai','apikey','Offline Fal fixture',1,1,?,
                    '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""", (json.dumps(data),))


def observed(db):
    with sqlite3.connect(db) as sql:
        data, = sql.execute("SELECT data FROM providerConnections WHERE id='fal-fixture-account'").fetchone()
        return json.loads(data)


def expectation(label, saved):
    if label == "disconnect":
        return {"valid": False, "error": "fetch failed"}
    status = RESPONSES[label]
    if status in (401, 403):
        return {"valid": False, "error": "Invalid API key"}
    if (saved and status != 307) or 200 <= status < 300:
        return {"valid": True, "error": None}
    return {"valid": False, "error": f"Unable to verify API key (Fal returned {status})"}


def check(port, db, cookie, label, saved, details=None):
    key = "offline-fal-" + label
    if saved:
        save(db, label, details)
        route, body = SAVED, {}
    else:
        route, body = VALIDATE, {"provider": "fal-ai", "apiKey": key,
                                 "providerSpecificData": {
                                     "baseUrl": "http://127.0.0.1:52979/redirected", **(details or {}),
                                 }}
    before = len(LEDGER)
    status, result, _ = call(port, route, body, cookie=cookie)
    expected = expectation(label, saved)
    if saved:
        expected["refreshed"] = False
        data = observed(db)
        assert data["apiKey"] == key
        assert data["testStatus"] == ("active" if expected["valid"] else "error"), data
        assert data.get("lastError") == expected["error"], data
        assert bool(data.get("lastErrorAt")) != expected["valid"], data
    assert (status, result) == (200, expected), (route, label, status, result, expected, ERRORS)
    observed_calls = LEDGER[before:]
    assert observed_calls and all(call == label for call in observed_calls)
    if label != "disconnect" or not REFERENCE:
        assert len(observed_calls) == 1
    assert key not in json.dumps(result)
    return result


class Tunnel(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            if self.server.mode == "connect":
                method, authority, version = self.rfile.readline(4096).decode().strip().split()
                assert (method, authority, version) == ("CONNECT", HOST + ":443", "HTTP/1.1")
                while line := self.rfile.readline(4096).strip():
                    assert not line.lower().startswith(b"authorization:") and b"offline-fal-" not in line
            else:
                client_version, count = self.rfile.read(2)
                assert client_version == 5 and 0 in self.rfile.read(count)
                self.request.sendall(b"\x05\x00")
                assert self.rfile.read(4) == b"\x05\x01\x00\x03"
                size, = self.rfile.read(1)
                assert self.rfile.read(size).decode() == HOST and int.from_bytes(self.rfile.read(2), "big") == 443
            TUNNELS.append(self.server.mode)
            if self.server.mode == "connect":
                result = (b"HTTP/1.1 200 Connection Established\r\n\r\n" if not self.server.reject else
                          b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            else:
                result = b"\x05" + (b"\x01" if self.server.reject else b"\x00") + b"\x00\x01\x7f\x00\x00\x01\x00\x00"
            self.request.sendall(result)
            if self.server.reject:
                return
            with socket.create_connection(self.server.target, timeout=3) as vendor:
                while ready := select.select((self.request, vendor), [], [], 5)[0]:
                    for peer in ready:
                        payload = peer.recv(65536)
                        if not payload:
                            return
                        (vendor if peer is self.request else self.request).sendall(payload)
        except (AssertionError, ValueError) as error:
            ERRORS.append(f"{self.server.mode}: {error}")
        except OSError:
            pass


class LocalProxy(socketserver.ThreadingTCPServer):
    daemon_threads = True


@contextmanager
def serve(server):
    worker = Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True)
    worker.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


def proxy_services(stack, scratch):
    def openssl(*args):
        subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    ca, ca_key, key, csr, cert, ext = (scratch / name for name in ("ca.crt", "ca.key", "key", "csr", "cert", "ext"))
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Fal Fixture CA",
            "-keyout", ca_key, "-out", ca, "-addext", "basicConstraints=critical,CA:TRUE")
    openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=Fal Fixture", "-keyout", key, "-out", csr)
    ext.write_text(f"basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:{HOST}\nextendedKeyUsage=serverAuth\n")
    openssl("x509", "-req", "-days", "1", "-in", csr, "-CA", ca, "-CAkey", ca_key, "-set_serial", "1",
            "-out", cert, "-extfile", ext)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    tls.set_servername_callback(lambda _socket, name, _context: HELLOS.append(name))
    vendor = ThreadingHTTPServer(("127.0.0.1", 0), Catalog)
    vendor.proxied = True
    vendor.socket = tls.wrap_socket(vendor.socket, server_side=True)
    stack.enter_context(serve(vendor))
    proxies = []
    for mode in ("connect", "socks"):
        proxy = LocalProxy(("127.0.0.1", 0), Tunnel)
        proxy.mode, proxy.target, proxy.reject = mode, vendor.server_address, False
        proxies.append(stack.enter_context(serve(proxy)))
    return ca, *proxies


def global_proxy(db, enabled, address="", bypass=""):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?)
            ON CONFLICT(id) DO UPDATE SET data=json_patch(settings.data,excluded.data)""",
                    (json.dumps({"outboundProxyEnabled": enabled, "outboundProxyUrl": address,
                                 "outboundNoProxy": bypass}),))


def proxy_contract(port, db, cookie, connect, socks):
    http = f"http://127.0.0.1:{connect.server_address[1]}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    details = {"connectionProxyEnabled": True, "connectionProxyUrl": http}
    with sqlite3.connect(db) as sql:
        sql.executemany("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        [(name, json.dumps({"type": kind, "proxyUrl": address, "strictProxy": True}))
                         for name, kind, address in (("fal-socks-test", "socks5", socks_url), ("fal-relay-test", "cloudflare", http))])

    def expect(label, saved, route=None, data=None):
        before = len(TUNNELS)
        check(port, db, cookie, label, saved, data)
        assert DESTINATIONS[-1] == (route is not None)
        assert TUNNELS[before:] == ([route] if route else [])

    def rejected(saved, data=None, tunnel=None):
        if saved:
            save(db, "ok", data)
        before = len(LEDGER), len(TUNNELS)
        path = SAVED if saved else VALIDATE
        body = {} if saved else {"provider": "fal-ai", "apiKey": "offline-fal-ok", "providerSpecificData": data}
        expected = {"valid": False, "error": "fetch failed", **({"refreshed": False} if saved else {})}
        assert call(port, path, body, cookie=cookie)[:2] == (200, expected)
        assert len(LEDGER) == before[0] and TUNNELS[before[1]:] == ([tunnel] if tunnel else [])
        if saved:
            stored = observed(db)
            assert stored["testStatus"] == "error" and stored["lastError"] == "fetch failed"

    expect("ok", False, data=details)
    expect("ok", True, "connect", details)
    expect("ok", True, "socks", {"proxyPoolId": "fal-socks-test"})
    expect("ok", True, data={**details, "connectionNoProxy": ".fal.ai"})
    expect("ok", True, "connect", {**details, "connectionNoProxy": "other-fal.ai"})
    for unusable in ({"proxyPoolId": "fal-relay-test"}, {"proxyPoolId": "missing-fal-pool"},
                     {"vercelRelayUrl": http}, {"connectionProxyEnabled": True}, {"strictProxy": True},
                     {**details, "connectionProxyUrl": http + "/invalid"},
                     {**details, "connectionProxyUrl": http + "/invalid", "connectionNoProxy": HOST}):
        rejected(True, unusable)

    for address, route in ((http, "connect"), (socks_url, "socks")):
        global_proxy(db, True, address)
        for label in ("ok", "no-content", "unauthorized", "forbidden", "missing", "rate-limited", "unavailable", "redirect"):
            expect(label, False, route, {**details, "connectionNoProxy": "*"})
            expect(label, True, route)
    expect("ok", True, "connect", details)
    global_proxy(db, True, http, "unrelated.invalid, " + HOST)
    expect("ok", False, data=details)
    expect("ok", True)
    global_proxy(db, True, http + "/invalid", HOST)
    rejected(False, details)
    rejected(True)
    global_proxy(db, True, "http://")
    rejected(False, details)
    rejected(True)
    global_proxy(db, True, http)
    connect.reject = True
    rejected(False, details, "connect")
    rejected(True, details, "connect")
    global_proxy(db, True, socks_url)
    socks.reject = True
    rejected(False, details, "socks")
    rejected(True, None, "socks")
    assert HELLOS and all(name == HOST for name in HELLOS) and not CAPTURES
    print("Fal Test and Validate preserved status semantics over HTTP CONNECT and SOCKS5h with fixed vendor TLS, NO_PROXY and no fallback.")


with tempfile.TemporaryDirectory(prefix="9router-fal-probe-") as folder, ExitStack() as services:
    scratch = Path(folder)
    data, home, executable = (scratch / name for name in ("data", "home", "empty-path"))
    home.mkdir()
    executable.mkdir()
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], cwd=home,
                   check=True, env={"HOME": str(home), "PATH": os.environ["PATH"], "NODE_NO_WARNINGS": "1"})
    db = data / "db/data.sqlite"
    mock = ThreadingHTTPServer(("127.0.0.1", 52979), Catalog)
    Thread(target=mock.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback = scratch / "fallback.log"
    env = {"HOME": str(home), "DATA_DIR": str(data), "PORT": str(port), "HOSTNAME": "127.0.0.1",
           "PATH": os.environ["PATH"] if REFERENCE else str(executable),
           "NEXT_TELEMETRY_DISABLED": "1", "MODEL_CATALOG_SYNC": "off",
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback)}
    if PROXY:
        ca, connect, socks = proxy_services(services, scratch)
        env["NINEROUTER_MODEL_LOCAL_PROXY_CA"] = str(ca)
    args = ["node", "--import", "./rust/fal-catalog-probe-replay/original.mjs", ".next/standalone/custom-server.js"] if REFERENCE else [
        BINARY, "--no-browser",
    ]
    with (scratch / "server.log").open("w+") as logs:
        app = subprocess.Popen(args, cwd=ROOT, env=env, stdout=logs, stderr=logs)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if call(port, "/api/health", method="GET")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("Fal fixture gateway did not start")
            assert call(port, VALIDATE, {"provider": "fal-ai", "apiKey": "offline-fal-ok"})[:2] == (401, {"error": "Unauthorized"})
            status, _, headers = call(port, "/api/auth/login", {"password": "123456"})
            assert status == 200
            cookie = next(value.split(";", 1)[0] for name, value in headers if name.lower() == "set-cookie")
            assert call(port, VALIDATE, {"provider": "fal-ai"}, cookie=cookie)[:2] == (
                400, {"error": "Provider and API key required"})
            assert not LEDGER
            if PROXY:
                proxy_contract(port, db, cookie, connect, socks)
            elif STRICT:
                check(port, db, cookie, "redirect", False)
                check(port, db, cookie, "redirect", True)
                assert not CAPTURES
                print("Fal Validate and saved Test stop same-origin redirects without forwarding the key.")
            else:
                labels = [name for name in RESPONSES if name != "redirect"] + ["disconnect"]
                values = {label: [check(port, db, cookie, label, saved) for saved in (False, True)] for label in labels}
                print(json.dumps(values, sort_keys=True))
            assert not ERRORS and not CAPTURES
            assert not fallback.exists() or not fallback.read_bytes()
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr)
            print(LEDGER, CAPTURES, ERRORS, file=sys.stderr)
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)
            mock.shutdown()
            mock.server_close()
