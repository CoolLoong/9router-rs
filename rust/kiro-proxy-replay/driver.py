import datetime
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
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
BINARY = str(Path(sys.argv[1]).resolve())
AUTH = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}
SESSION = {**AUTH, "X-Session-Id": "isolated-proxy-session"}
HTTP = "http://127.0.0.1:52984"
SOCKS = "socks5h://127.0.0.1:52985"
RELAY = "http://127.0.0.1:52986/relay"
ENDPOINT = "/generateAssistantResponse"
ISSUER = "oidc.eu-central-1.amazonaws.com"
RENEWED = "offline-fixture-kiro-idc-new"
REFRESH = "fixture-kiro-oauth-refresh"
ROTATED = "fixture-kiro-rotated-refresh"
BEARERS = {"idc": "offline-fixture-kiro-idc", "github": "offline-fixture-kiro-social",
           "external_idp": "offline-fixture-kiro-external"}
REGIONS = {BEARERS["idc"]: "eu-central-1", BEARERS["github"]: "us-east-1", BEARERS["external_idp"]: "us-east-1"}
REGIONS[RENEWED] = REGIONS[BEARERS["idc"]]
USER_TOOLS = [{"type": "function", "function": {"name": "lookup_ticket", "parameters":
               {"type": "object", "properties": {"ticket": {"type": "string"}}}}}]
CALLS = []
TUNNELS = []
HELLOS = []
FAILURES = []
GRANTS, CHAT_BEARERS, CHAT_PAYLOADS = [], [], []
REJECT = False


class Upstream(BaseHTTPRequestHandler):
    hold = None

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert self.server.server_port == 52987
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            body = json.loads(raw)
            if self.path == "/token":
                assert self.headers["Host"] == ISSUER and self.headers["Authorization"] is None
                assert body == {"clientId":"fixture-kiro-idc-client","clientSecret":"fixture-kiro-idc-secret",
                                "refreshToken":REFRESH,"grantType":"refresh_token"}
                GRANTS.append(ISSUER)
                value = json.dumps({"accessToken":RENEWED,"refreshToken":ROTATED,"expiresIn":3600}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(value)))
                self.end_headers()
                self.wfile.write(value)
                return
            assert self.path == ENDPOINT
            bearer, = self.headers.get_all("Authorization") or []
            assert bearer.startswith("Bearer ")
            token = bearer.removeprefix("Bearer ")
            region = REGIONS[token]
            profile = f"arn:aws:codewhisperer:{region}:123456789012:profile/chat-fixture"
            assert self.headers["X-Amz-Sso-Bearer"] == token
            assert self.headers["X-Amzn-Codewhisperer-Profile-Arn"] == body["profileArn"] == profile
            assert all(self.headers.get(name) is None for name in ("X-Relay-Target", "X-Relay-Path", "X-Amz-Target"))
            conversation = body["conversationState"]
            current = conversation["currentMessage"]["userInputMessage"]
            message = current["content"]
            CALLS.append((region, message, conversation["conversationId"]))
            CHAT_BEARERS.append(token)
            CHAT_PAYLOADS.append(body)
            if (message.endswith("fixture session renew after 401") and token != RENEWED
                or message.endswith("fixture route changed")
                or message.endswith("fixture deny after expiry")):
                if message.endswith("fixture route changed"):
                    arrived, release = self.hold
                    arrived.set()
                    assert release.wait(5)
                self.send_response(401)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")
                return
            if message.endswith("fixture proxy redirect"):
                self.send_response(307)
                self.send_header("Location", "https://localhost:52987" + ENDPOINT)
                self.end_headers()
                return
            headers = {name.lower(): value for name, value in self.headers.items()}
            if token == RENEWED:
                headers["authorization"] = "Bearer " + BEARERS["idc"]
                headers["x-amz-sso-bearer"] = BEARERS["idc"]
            if "fixture image" in message:
                assert message.endswith("fixture image") and current["modelId"] == "claude-sonnet-4.5"
                assert current["images"] == [{"format": "png", "source": {"bytes": "ZmFrZS1pbWFnZQ=="}}]
                current["modelId"] = "auto"
                raw = json.dumps(body).encode()
                headers["content-length"] = str(len(raw))
            upstream = http.client.HTTPConnection("127.0.0.1", 52988, timeout=5)
            try:
                upstream.request("POST", f"/kiro/q/{region}{ENDPOINT}", body=raw, headers=headers)
                reply = upstream.getresponse()
                payload = reply.read()
                assert reply.status != 422, payload.decode()
                self.send_response(reply.status)
                self.send_header("Content-Type", reply.getheader("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
            finally:
                upstream.close()
        except (AssertionError, KeyError, ValueError) as error:
            FAILURES.append(str(error) or "Unexpected Kiro request")
            self.send_error(422)


class Proxy(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            http_connect = self.server.server_address[1] == 52984
            if http_connect:
                method, authority, version = self.rfile.readline(4096).decode().strip().split(" ")
                assert (method, version) == ("CONNECT", "HTTP/1.1")
                target, port = authority.rsplit(":", 1)
                port = int(port)
                headers = {}
                for _ in range(30):
                    line = self.rfile.readline(4096).strip()
                    if not line:
                        break
                    name, value = line.decode().split(":", 1)
                    headers[name.lower()] = value.strip()
                assert not {"authorization", "x-amz-sso-bearer", "x-amzn-codewhisperer-profile-arn"}.intersection(headers)
            else:
                version, length = self.rfile.read(2)
                assert version == 5 and 0 in self.rfile.read(length)
                self.request.sendall(b"\x05\x00")
                assert self.rfile.read(4) == b"\x05\x01\x00\x03"
                length, = self.rfile.read(1)
                target = self.rfile.read(length).decode()
                port = int.from_bytes(self.rfile.read(2), "big")
            actual = [(f"q.{region}.amazonaws.com", 443) for region in REGIONS.values()]
            assert (target, port) in [("localhost", 52987), (ISSUER, 443), *actual], (target, port)
            TUNNELS.append(("http" if http_connect else "socks5h", target, port))
            if REJECT:
                assert http_connect
                self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                return
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n" if http_connect else
                                 b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            with socket.create_connection(("127.0.0.1", 52987), timeout=3) as upstream:
                sockets = [self.request, upstream]
                while True:
                    ready, _, _ = select.select(sockets, [], [], 8)
                    if not ready:
                        return
                    for source in ready:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            FAILURES.append(str(error) or "Unexpected proxy request")
        except OSError:
            pass


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def tls(scratch):
    ca, ca_key, cert, key, req = (scratch / name for name in ("ca.pem", "ca.key", "leaf.pem", "leaf.key", "leaf.csr"))
    extensions = scratch / "leaf.conf"
    extensions.write_text(f"basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost,DNS:{ISSUER}\nextendedKeyUsage=serverAuth\n")
    for command in [
        ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Kiro local fixture CA",
         "-keyout", ca_key, "-out", ca, "-addext", "basicConstraints=critical,CA:TRUE"],
        ["req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", key, "-out", req],
        ["x509", "-req", "-days", "1", "-in", req, "-CA", ca, "-CAkey", ca_key, "-set_serial", "1", "-out", cert,
         "-extfile", extensions],
    ]:
        subprocess.run(["openssl", *command], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    context.set_servername_callback(lambda _socket, name, _context: HELLOS.append(name))
    return ca, context


def seed(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, cwd=ROOT,
                   env={"PATH": os.environ["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
    database = data / "db/data.sqlite"
    with sqlite3.connect(database) as sql:
        sql.executemany("""INSERT INTO proxyPools(id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'active',?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        [("kiro-tunnel-" + style, json.dumps({"type": kind, "proxyUrl": url, "strictProxy": True}))
                         for style, kind, url in [("socks", "socks5", SOCKS), ("relay", "cloudflare", RELAY)]])
    return database


def credentials(database, method, flags=None, seconds=3600, profile_region=None, backup=False, renew=False):
    token = BEARERS[method]
    region = REGIONS[token]
    profile = f"arn:aws:codewhisperer:{profile_region or region}:123456789012:profile/chat-fixture"
    now = datetime.datetime.now(datetime.timezone.utc)
    fields = {"accessToken": token, "expiresAt":
              (now + datetime.timedelta(seconds=seconds)).isoformat(), "providerSpecificData": {
                  "authMethod": method, "region": region, "profileArn": profile, **(flags or {})}}
    if renew:
        assert method == "idc"
        fields["refreshToken"] = REFRESH
        fields["providerSpecificData"].update(clientId="fixture-kiro-idc-client", clientSecret="fixture-kiro-idc-secret")
    with sqlite3.connect(database) as sql:
        sql.execute("DELETE FROM providerConnections WHERE provider='kiro'")
        sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES ('kiro-proxy-main','kiro','oauth','Fixture Kiro',1,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                    (json.dumps(fields),))
        if backup:
            fields["providerSpecificData"] = {"authMethod": method, "region": region, "profileArn": profile}
            sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                           VALUES ('kiro-direct-backup','kiro','oauth','Fixture Kiro direct',2,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        (json.dumps(fields),))
    return profile


def settings(database, url):
    overrides = {"kiro": {"headers": {"tokentype": "wrong-type", "x-amz-sso-bearer": "wrong-token",
                                     "x-amzn-codewhisperer-profile-arn": "wrong-profile"}}}
    with sqlite3.connect(database) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (json.dumps({"outboundProxyEnabled": bool(url), "outboundProxyUrl": url or "", "providerOverrides": overrides}),))


def request(port, body=None, headers=AUTH):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=9)
    try:
        connection.request("POST" if body is not None else "GET", "/v1/chat/completions" if body is not None else "/api/health",
                           headers=headers, body=None if body is None else json.dumps(body))
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type", "").split(";", 1)[0], response.read().decode()
    finally:
        connection.close()


def body(text, stream=False):
    return {"model": "kr/auto", "messages": [{"role": "user", "content": "fixture " + text}], "stream": stream}


def check_chat(port, body, headers=AUTH):
    status, mime, raw = request(port, body, headers)
    assert (status, mime) == (200, "application/json"), (status, mime, raw)
    response = json.loads(raw)
    assert response["choices"][0]["message"]["content"] == "Kiro fixture 世界"
    assert response["usage"]["completion_tokens"] == 2


def positive(port, database):
    global REJECT
    settings(database, HTTP)
    credentials(database, "github", seconds=150)
    check_chat(port, body("oauth social"))
    settings(database, HTTP)
    credentials(database, "idc", {"proxyPoolId": "kiro-tunnel-socks"})
    tool = {**body("tool calls", True), "tools": USER_TOOLS}
    status, mime, raw = request(port, tool)
    assert (status, mime) == (200, "text/event-stream"), (status, mime, raw)
    assert "fixture_ticket_call" in raw and "tool_calls" in raw and "data: [DONE]" in raw
    image = {**body("image"), "model": "kr/claude-sonnet-4.5"}
    image["messages"][0]["content"] = [{"type": "text", "text": "fixture image"},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,ZmFrZS1pbWFnZQ=="}}]
    check_chat(port, image)
    check_chat(port, body("session first"), SESSION)
    check_chat(port, body("session no history"), SESSION)
    assert CALLS[-2][2] == CALLS[-1][2]
    credentials(database, "external_idp", {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP})
    check_chat(port, body("oauth external"))
    profile = credentials(database, "github")
    status, _, raw = request(port, body("unauthorized"))
    assert status in {401, 503} and "[redacted]" in raw and profile not in raw and BEARERS["github"] not in raw
    with sqlite3.connect(database) as sql:
        (saved,), = sql.execute("SELECT data FROM providerConnections WHERE id='kiro-proxy-main'")
        assert "kiroRefreshAttempt" not in json.loads(saved)
    credentials(database, "github", {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP})
    status, _, raw = request(port, body("proxy redirect"))
    assert status in {502, 503} and "redirect refused" in raw, (status, raw)
    assert [item[0] for item in TUNNELS] == ["http", "socks5h", "socks5h", "socks5h", "socks5h", "http", "http", "http"]
    assert HELLOS == ["localhost"] * 8 and len(CALLS) == 8

    settings(database, None)
    credentials(database, "github", {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP}, backup=True)
    REJECT = True
    status, _, raw = request(port, body("proxy unreachable"))
    REJECT = False
    assert status in {502, 503} and len(TUNNELS) == 9 and len(CALLS) == 8, (status, raw)

    for options, payload in [
        ({"flags": {"proxyPoolId": "kiro-tunnel-relay"}}, body("relay")),
        ({"flags": {"proxyPoolId": "does-not-exist"}}, body("unavailable pool")),
        ({"flags": {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP, "connectionNoProxy": ".amazonaws.com"}}, body("bypass")),
        ({"flags": {"proxyPoolId": "kiro-tunnel-relay"}}, {**body("unsupported"), "unsupported_option": True}),
        ({"flags": {"proxyPoolId": "kiro-tunnel-socks"}, "seconds": -10}, body("expired")),
        ({"flags": {"proxyPoolId": "kiro-tunnel-socks"}, "profile_region": "eu-central-1"}, body("wrong region")),
    ]:
        credentials(database, "github", **options)
        status, _, raw = request(port, payload)
        assert status in {502, 503} and any(word in raw.lower() for word in ("proxy", "tunnel")), (status, raw)
    assert len(TUNNELS) == 9 and len(CALLS) == 8
    renewal(port, database)
    print("Kiro OAuth CONNECT and SOCKS5h verified EventStream, session continuation, protected expiry and 401 renewal.")


def renewal(port, database):
    def saved():
        with sqlite3.connect(database) as sql:
            value, = sql.execute("SELECT data FROM providerConnections WHERE id='kiro-proxy-main'").fetchone()
        return json.loads(value)

    settings(database, None)
    credentials(database, "idc", {"proxyPoolId":"kiro-tunnel-socks"}, seconds=-30, renew=True)
    start = len(TUNNELS)
    check_chat(port, body("proactive renewal"))
    assert TUNNELS[start:] == [("socks5h",ISSUER,443),("socks5h","localhost",52987)]
    assert CHAT_BEARERS[-1] == RENEWED and GRANTS == [ISSUER]
    assert saved()["refreshToken"] == ROTATED and saved()["kiroRefreshAttempt"] is None

    settings(database, HTTP)
    credentials(database, "idc", renew=True)
    scope = {**AUTH, "X-Session-Id":"isolated-renewal-session"}
    check_chat(port, body("session first"), scope)
    session = CALLS[-1][2]
    start = len(TUNNELS)
    status, mime, raw = request(port, body("session renew after 401", True), scope)
    assert (status,mime) == (200,"text/event-stream"), (status,mime,raw)
    assert "Kiro fixture " in raw and "世界" in raw and "data: [DONE]" in raw
    assert TUNNELS[start:] == [("http","localhost",52987),("http",ISSUER,443),("http","localhost",52987)]
    assert CHAT_BEARERS[-2:] == [BEARERS["idc"],RENEWED] and GRANTS == [ISSUER]*2
    assert CALLS[-2][2] == CALLS[-1][2] == session and CHAT_PAYLOADS[-2] == CHAT_PAYLOADS[-1]
    assert saved()["accessToken"] == RENEWED

    credentials(database, "idc", seconds=-30, renew=True)
    start = len(TUNNELS)
    assert request(port, body("deny after expiry"))[0] in {401,503}
    assert TUNNELS[start:] == [("http",ISSUER,443),("http","localhost",52987)]
    assert GRANTS == [ISSUER]*3 and saved()["refreshToken"] == ROTATED

    credentials(database, "idc", renew=True)
    arrived, release = Upstream.hold = threading.Event(), threading.Event()
    start = len(TUNNELS)
    with ThreadPoolExecutor() as workers:
        pending = workers.submit(request,port,body("route changed"))
        try:
            assert arrived.wait(5)
            settings(database,None)
        finally:
            release.set()
        assert pending.result()[0] in {401,503}
    Upstream.hold = None
    assert TUNNELS[start:] == [("http","localhost",52987)] and GRANTS == [ISSUER]*3
    assert saved()["refreshToken"] == REFRESH


def production_address(port, database):
    settings(database, HTTP)
    credentials(database, "github")
    assert request(port, body("official address"))[0] in {502, 503}
    settings(database, SOCKS)
    credentials(database, "idc")
    assert request(port, body("official address"))[0] in {502, 503}
    hosts = ["q.us-east-1.amazonaws.com", "q.eu-central-1.amazonaws.com"]
    assert TUNNELS == [("http", hosts[0], 443), ("socks5h", hosts[1], 443)]
    assert HELLOS == hosts and not CALLS
    print("The official Kiro CONNECT authorities and SNI stayed intact. The local-only certificate was rejected before bearer delivery.")


def exercise(scratch, ca, check):
    label = "localhost" if ca else "production"
    home, data, empty = (scratch / (label + "-" + item) for item in ("home", "data", "path"))
    home.mkdir()
    empty.mkdir()
    database = seed(data, home)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data), "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1",
           "RUST_ANYTHING_FALLBACK_LOG": str(scratch / (label + ".fallbacks"))}
    if ca:
        env["NINEROUTER_KIRO_LOCAL_PROXY_CA"] = str(ca)
        env["NINEROUTER_MODEL_LOCAL_PROXY_CA"] = str(ca)
    with (scratch / (label + ".log")).open("w+") as logs:
        app = subprocess.Popen([BINARY, "--no-browser"], cwd=home, env=env, stdout=logs, stderr=logs)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if request(port)[0] == 200:
                        break
                except OSError:
                    time.sleep(.03)
            else:
                raise AssertionError("Kiro gateway fixture did not start")
            check(port, database)
            assert not FAILURES and not (scratch / (label + ".fallbacks")).read_bytes()
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            print(TUNNELS, HELLOS, CALLS, FAILURES, file=sys.stderr)
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)


with tempfile.TemporaryDirectory(prefix="9router-kiro-proxy-") as directory:
    scratch = Path(directory)
    ca, context = tls(scratch)
    upstream = ThreadingHTTPServer(("127.0.0.1", 52987), Upstream)
    upstream.socket = context.wrap_socket(upstream.socket, server_side=True)
    servers = [upstream, Server(("127.0.0.1", 52984), Proxy), Server(("127.0.0.1", 52985), Proxy),
               ThreadingHTTPServer(("127.0.0.1", 52986), Upstream), ThreadingHTTPServer(("127.0.0.1", 52989), Upstream)]
    for server in servers:
        threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    simulator = subprocess.Popen(["node", "--no-warnings", str(Path(__file__).with_name("upstream.mjs"))],
                                 env={"PATH": os.environ["PATH"], "HOME": str(scratch)}, stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([simulator.stdout], [], [], 8)[0] and simulator.stdout.readline().strip() == "ready"
        exercise(scratch, ca, positive)
        TUNNELS.clear()
        HELLOS.clear()
        CALLS.clear()
        exercise(scratch, None, production_address)
    finally:
        simulator.terminate()
        simulator.wait(timeout=4)
        for server in servers:
            server.shutdown()
            server.server_close()
