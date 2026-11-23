from contextlib import ExitStack, contextmanager
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
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(sys.argv[1]).resolve()
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
ZED_ACCESS = "zed-proxy-fixture-access"
ZED_SYSTEM = "zed-proxy-fixture-system"
CLINE_ACCESS = "eyJfixture.cline-probe.oauth"
CLINE_REFRESH = "fixture-cline-refresh-do-not-use"
PASS_KEY = "clp_fixture_pass_key"
ZED_HOST, CLINE_HOST = "cloud.zed.dev", "api.cline.bot"
SECRETS = (ZED_ACCESS, ZED_SYSTEM, CLINE_ACCESS, CLINE_REFRESH, PASS_KEY)
REQUESTS, TUNNELS, HELLOS, ERRORS, LEASES = [], [], [], [], []
MODE = {}


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, value, location=None):
        payload = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(payload)

    def handle_vendor(self):
        host = self.headers["Host"]
        path = self.path
        direct = self.server.server_port == 52989
        if direct:
            _, vendor, path = path.split("/", 2)
            path = "/" + path
            host = ZED_HOST if vendor == "zed-fixture" else CLINE_HOST
            assert vendor in {"zed-fixture", "cline"}
        assert host in {ZED_HOST, CLINE_HOST}
        assert self.headers["Accept"] == "application/json"
        for header in ("Proxy-Authorization", "X-Relay-Target", "X-Relay-Path"):
            assert self.headers[header] is None
        auth = self.headers["Authorization"]
        REQUESTS.append((direct, host, path, auth))
        if host == ZED_HOST:
            if path == "/client/users/me":
                assert self.command == "GET" and auth == "17 " + ZED_ACCESS
                assert self.headers["x-zed-system-id"] == ZED_SYSTEM
                self.reply(200, {"organizations": [{"id": "fixture-org", "is_personal": True}]})
            elif path == "/client/llm_tokens":
                assert self.command == "POST" and auth == "17 " + ZED_ACCESS
                assert self.headers["x-zed-system-id"] == ZED_SYSTEM
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert body == {"organization_id": "fixture-org"}
                lease = f"fixture-zed-issued-lease-{len(LEASES)}"
                LEASES.append(lease)
                self.reply(200, {"token": lease})
            else:
                assert path == "/models" and self.command == "GET"
                assert self.headers["x-zed-system-id"] is None
                assert self.headers["x-zed-client-supports-x-ai"] == "true"
                assert auth.removeprefix("Bearer ") in LEASES
                if MODE.get("zed") == "expired":
                    MODE.pop("zed")
                    self.reply(401, {"message": "expired fixture"})
                elif MODE.get("zed") == "denied":
                    self.reply(403, {"message": f"fixture refused {auth} and {ZED_ACCESS}"})
                else:
                    self.reply(200, {"models": [
                        {"id": "fixture-live-zed", "display_name": "Zed catalog", "provider": "x_ai",
                         "max_token_count": 123456, "supports_tools": True},
                        {"id": "disabled-zed", "is_disabled": True},
                    ]})
        elif path == "/api/v1/ai/cline/recommended-models":
            assert self.command == "GET" and auth is None and self.headers["X-Title"] is None
            self.reply(200, {"free": [{"id": "cline-free/fixture", "name": "Free"}]})
        else:
            assert self.command == "GET" and path == "/api/v1/models"
            if auth == "Bearer " + PASS_KEY:
                assert self.headers["HTTP-Referer"] is None
            else:
                assert auth == "Bearer workos:" + CLINE_ACCESS
                assert self.headers["X-Title"] == "Cline" and self.headers["X-CLIENT-TYPE"] == "9router"
            models = {"data": [{"id": "fixture-live-cline", "name": "Cline catalog"},
                               {"id": "cline-pass/fixture", "name": "Pass catalog"}]}
            if MODE.get("cline") == "redirect":
                self.reply(307, models, f"https://{ZED_HOST}/should-not-follow")
            elif MODE.get("cline") == "denied":
                self.reply(401, {"error": f"fixture refused {auth}"})
            else:
                if MODE.get("cline") == "echo":
                    models["data"].append({"id": "cline-pass/" + auth.removeprefix("Bearer ")})
                    models["data"][0]["name"] = auth
                self.reply(200, models)

    def checked_request(self):
        try:
            self.handle_vendor()
        except (AssertionError, KeyError, ValueError) as error:
            ERRORS.append((self.path, repr(error)))
            self.send_error(422)

    do_GET = checked_request
    do_POST = checked_request


class Tunnel(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            if self.server.mode == "connect":
                method, authority, version = self.rfile.readline(4096).decode().strip().split()
                assert (method, version) == ("CONNECT", "HTTP/1.1")
                host, port = authority.rsplit(":", 1)
                port = int(port)
                while line := self.rfile.readline(4096).strip():
                    assert line.partition(b":")[0].lower() not in {b"authorization", b"x-zed-system-id"}
                    assert all(secret.encode() not in line for secret in (*SECRETS, *LEASES, *getattr(self.server, "secrets", ())))
            else:
                version, size = self.rfile.read(2)
                assert version == 5 and 0 in self.rfile.read(size)
                self.request.sendall(b"\x05\x00")
                assert self.rfile.read(4) == b"\x05\x01\x00\x03"
                length, = self.rfile.read(1)
                host = self.rfile.read(length).decode()
                port = int.from_bytes(self.rfile.read(2), "big")
            TUNNELS.append((self.server.mode, host, port))
            assert host in getattr(self.server, "hosts", (ZED_HOST, CLINE_HOST)) and port == 443
            if self.server.reject:
                self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n" if self.server.mode == "connect" else
                                     b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00")
                return
            if self.server.mode == "connect":
                self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            else:
                self.request.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            with socket.create_connection(self.server.target, timeout=3) as upstream:
                sockets = (self.request, upstream)
                while ready := select.select(sockets, [], [], 6)[0]:
                    for reader in ready:
                        data = reader.recv(65536)
                        if not data:
                            return
                        (upstream if reader is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            ERRORS.append(("tunnel", repr(error)))
        except OSError:
            pass


class Proxy(socketserver.ThreadingTCPServer):
    daemon_threads = True


@contextmanager
def serving(server):
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()


def run_openssl(*arguments):
    subprocess.run(["openssl", *arguments], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def tls_services(stack, scratch, handler=Vendor, hosts=(ZED_HOST, CLINE_HOST)):
    ca, key = scratch / "ca.crt", scratch / "ca.key"
    run_openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Fixture CA",
                "-keyout", key, "-out", ca, "-addext", "basicConstraints=critical,CA:TRUE")
    services = []
    for number, names in enumerate((hosts, ("localhost",)), 1):
        leaf, csr, cert, extensions = (scratch / f"{number}-{name}" for name in ("key", "csr", "crt", "extensions"))
        san = ",".join("DNS:" + name for name in names)
        extensions.write_text(f"basicConstraints=critical,CA:FALSE\nsubjectAltName={san}\nextendedKeyUsage=serverAuth\n")
        run_openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=TLS fixture",
                    "-keyout", leaf, "-out", csr)
        run_openssl("x509", "-req", "-days", "1", "-in", csr, "-CA", ca, "-CAkey", key, "-set_serial",
                    str(number), "-out", cert, "-extfile", extensions)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, leaf)
        context.set_servername_callback(lambda _socket, name, _context: HELLOS.append(name))
        service = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        service.socket = context.wrap_socket(service.socket, server_side=True)
        services.append(stack.enter_context(serving(service)).server_address)
    return ca, *services


def save(db, provider, details=None, **overrides):
    saved = {
        "zed": ("oauth", {"accessToken": ZED_ACCESS, "providerSpecificData": {"userId": 17, "systemId": ZED_SYSTEM}}),
        "cline": ("oauth", {"accessToken": CLINE_ACCESS, "refreshToken": CLINE_REFRESH,
                            "expiresAt": "2099-12-31T00:00:00Z"}),
        "clinepass": ("apikey", {"apiKey": PASS_KEY}),
    }
    kind, data = saved[provider]
    kind = overrides.pop("auth_type", kind)
    data.update(overrides)
    data.setdefault("providerSpecificData", {}).update(details or {})
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,?, ?,1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            (provider, provider, kind, "Fixture " + provider, json.dumps(data)))


def get(port, path, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=7)
    try:
        connection.request("GET", path, headers=headers or {})
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def models(port, provider, headers=ADMIN):
    status, raw = get(port, f"/api/providers/{provider}/models", headers)
    assert status == (200 if headers else 401), (status, raw)
    assert all(secret.encode() not in raw for secret in (*SECRETS, *LEASES)), raw
    return json.loads(raw)


@contextmanager
def gateway(scratch, name, ca, pools):
    home = scratch / name
    home.mkdir()
    data = home / "data"
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, cwd=home,
                   env={"PATH": os.environ["PATH"], "HOME": str(home)}, stderr=subprocess.DEVNULL)
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
        sql.executemany("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            [(name, json.dumps({"type": kind, "proxyUrl": url, "strictProxy": True})) for name, kind, url in pools])
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback = home / "fallback.log"
    env = {"PATH": "", "HOME": str(home), "DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1", "MODEL_CATALOG_SYNC": "off"}
    if ca:
        env["NINEROUTER_MODEL_LOCAL_PROXY_CA"] = str(ca)
    with subprocess.Popen([BINARY, "--no-browser", "--skip-update"], env=env, cwd=home,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if get(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise RuntimeError("Native model proxy fixture did not become healthy")
            yield port, db
            assert fallback.read_text() == "", fallback.read_text()
        finally:
            process.terminate()
            try:
                process.wait(timeout=7)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)


def requires_warning(port, provider):
    value = models(port, provider)
    if provider == "zed":
        assert value["models"] == [] and "Failed to fetch Zed models" in value["warning"], value
    else:
        name = "ClinePass" if provider == "clinepass" else "Cline"
        assert value["models"] and value["warning"] == f"{name} returned no live models; falling back to static catalog."
    return value


def enable_global_proxy(db, address):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?)
            ON CONFLICT(id) DO UPDATE SET data=json_patch(settings.data,excluded.data)""",
                    (json.dumps({"outboundProxyEnabled": True, "outboundProxyUrl": address}),))


def cases(port, db, http, connect):
    direct = {}
    for provider in ("zed", "cline", "clinepass"):
        save(db, provider)
        assert models(port, provider, {}) == {"error": "Unauthorized"}
        direct[provider] = models(port, provider)
        assert "warning" not in direct[provider]
    assert [row["id"] for row in direct["cline"]["models"]] == [
        "fixture-live-cline", "cline-pass/fixture", "cline-free/fixture",
    ]
    assert [row["id"] for row in direct["clinepass"]["models"]] == ["cline-pass/fixture"]
    assert direct["zed"]["models"][0]["contextLength"] == 123456
    assert direct["zed"]["models"][0]["supportsTools"] is True and len(direct["zed"]["models"]) == 1
    assert len(REQUESTS) == 6 and not TUNNELS
    assert all(row[0] for row in REQUESTS)

    for details in (http, {"proxyPoolId": "fixture-socks"}):
        for provider in direct:
            save(db, provider, details)
            assert models(port, provider) == direct[provider]
    assert len(set(LEASES)) == 3
    assert [(row[0], row[1], row[2]) for row in REQUESTS if row[2] == "/models"] == [
        (True, ZED_HOST, "/models"), (False, ZED_HOST, "/models"), (False, ZED_HOST, "/models"),
    ]
    assert {route for route, _, _ in TUNNELS} == {"connect", "socks"}
    assert HELLOS == [host for _, host, _ in TUNNELS]

    MODE["zed"] = "expired"
    previous = len(LEASES)
    assert models(port, "zed") == direct["zed"] and len(LEASES) == previous + 1
    MODE["zed"] = "denied"
    assert "[redacted]" in requires_warning(port, "zed")["warning"]
    MODE.clear()

    for provider in direct:
        before = (len(TUNNELS), len(REQUESTS))
        for details in ({"proxyPoolId": "fixture-relay"}, {"proxyPoolId": "missing-proxy"},
                        {**http, "connectionNoProxy": ".zed.dev,.cline.bot"},
                        {**http, "vercelRelayUrl": http["connectionProxyUrl"]},
                        {**http, "connectionProxyUrl": http["connectionProxyUrl"] + "/not-a-tunnel"}):
            save(db, provider, details)
            requires_warning(port, provider)
        assert (len(TUNNELS), len(REQUESTS)) == before

    before = (len(TUNNELS), len(REQUESTS))
    save(db, "cline", http, expiresAt="2000-12-31T00:00:00Z")
    requires_warning(port, "cline")
    save(db, "cline", http, apiKey=PASS_KEY)
    requires_warning(port, "cline")
    save(db, "zed", http, auth_type="apikey")
    requires_warning(port, "zed")
    assert (len(TUNNELS), len(REQUESTS)) == before

    save(db, "cline", http)
    MODE["cline"] = "redirect"
    requires_warning(port, "cline")
    assert len(REQUESTS) == before[1] + 1 and REQUESTS[-1][1:3] == (CLINE_HOST, "/api/v1/models")
    MODE["cline"] = "denied"
    requires_warning(port, "cline")
    MODE["cline"] = "echo"
    save(db, "cline", http, accessToken=" " + CLINE_ACCESS + " ")
    echoed = models(port, "cline")["models"]
    assert [row["id"] for row in echoed] == [row["id"] for row in direct["cline"]["models"]]
    assert "[redacted]" in echoed[0]["name"]
    MODE.clear()
    with sqlite3.connect(db) as sql:
        saved, = sql.execute("SELECT data FROM providerConnections WHERE id='cline'").fetchone()
        account = json.loads(saved)
        assert account["accessToken"].strip() == CLINE_ACCESS and account["refreshToken"] == CLINE_REFRESH

    connect.reject = True
    before = (len(TUNNELS), len(REQUESTS))
    for provider in direct:
        save(db, provider, http)
        requires_warning(port, provider)
    assert len(TUNNELS) == before[0] + 3 and len(REQUESTS) == before[1]
    connect.reject = False

    enable_global_proxy(db, http["connectionProxyUrl"])
    for provider in direct:
        save(db, provider)
        assert models(port, provider) == direct[provider]
    assert not any(row[0] for row in REQUESTS[6:]), REQUESTS[6:]


def main():
    with tempfile.TemporaryDirectory(prefix="9router-models-proxy-") as directory, ExitStack() as stack:
        scratch = Path(directory)
        ca, target, wrong_host = tls_services(stack, scratch)
        stack.enter_context(serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
        connect, socks, relay = [Proxy(("127.0.0.1", 0), Tunnel) for _ in range(3)]
        for proxy, mode in ((connect, "connect"), (socks, "socks"), (relay, "connect")):
            proxy.mode, proxy.reject, proxy.target = mode, False, target
            stack.enter_context(serving(proxy))
        http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
        pools = [("fixture-socks", "socks5", f"socks5h://127.0.0.1:{socks.server_address[1]}"),
                 ("fixture-relay", "cloudflare", f"http://127.0.0.1:{relay.server_address[1]}")]
        with gateway(scratch, "with-local-ca", ca, pools) as (port, db):
            cases(port, db, http, connect)
            connect.target = wrong_host
            before = len(REQUESTS)
            for provider, expected in (("zed", ZED_HOST), ("cline", CLINE_HOST)):
                save(db, provider, http)
                requires_warning(port, provider)
                assert HELLOS[-1] == expected and len(REQUESTS) == before
            connect.target = target
        with gateway(scratch, "without-local-ca", None, pools) as (port, db):
            before = len(REQUESTS)
            for provider, expected in (("zed", ZED_HOST), ("cline", CLINE_HOST)):
                save(db, provider, http)
                requires_warning(port, provider)
                assert HELLOS[-1] == expected and len(REQUESTS) == before
        assert not ERRORS, ERRORS
    print("Zed and Cline model catalogs match direct over CONNECT and SOCKS5h, with fixed TLS names and no fallback.")


if __name__ == "__main__":
    main()
