from contextlib import ExitStack, contextmanager
from concurrent.futures import ThreadPoolExecutor
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
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[2]
BINARY = str(Path(sys.argv[1]).resolve())
PATH = "/api/providers/fixture-kiro-proxy-models/models"
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
API = {"Authorization": "Bearer offline-chat-key"}
LIVE_ID = "kr/fixture-tunnel-model"
USER_AGENT = "9Router/" + json.loads((ROOT / "package.json").read_text())["version"]
REGION = "eu-west-2"
FRANKFURT = "eu-central-1"
PROFILE = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE"
EU_PROFILE = PROFILE.replace(":us-east-1:", f":{FRANKFURT}:")
OPERATOR_PROFILE = PROFILE.replace("/FIXTURE", "/OPERATOR")
KEY = "ksk_fixture_router_only"
TOKEN = "fixture-kiro-profile-token"
FRESH = "fixture-kiro-renewed-access"
GRANT = "fixture-kiro-rotation-grant"
ROTATED = "fixture-kiro-rotated-refresh"
CLIENT_SECRET = "fixture-kiro-oidc-secret"
OIDC = "oidc.us-east-1.amazonaws.com"
EU_OIDC = f"oidc.{FRANKFURT}.amazonaws.com"
CW = "codewhisperer.us-east-1.amazonaws.com"
SOCIAL = "prod.us-east-1.auth.desktop.kiro.dev"
MICROSOFT = "login.microsoftonline.com"
MICROSOFT_PATH = "/fixture/oauth2/v2.0/token"
API_HOST = f"q.{REGION}.amazonaws.com"
US_Q = "q.us-east-1.amazonaws.com"
EU_Q = f"q.{FRANKFURT}.amazonaws.com"
ISSUERS = {OIDC: "/token", EU_OIDC: "/token", SOCIAL: "/refreshToken", MICROSOFT: MICROSOFT_PATH}
HOSTS = (API_HOST, US_Q, EU_Q, CW, *ISSUERS)
SECRETS = (KEY, TOKEN, FRESH, GRANT, ROTATED, CLIENT_SECRET)
UPSTREAM = {"models": [{"modelId": "fixture-tunnel-model", "modelName": "Tunnel model",
                        "rateMultiplier": 1.25, "tokenLimits": {"maxInputTokens": 300000}}]}
WARNING = "Kiro returned no models; falling back to static catalog."
TUNNELS, HELLOS, CALLS, ERRORS = [], [], [], []
OAUTH, BEARERS = [], []
PROFILE_CALLS = []
REJECT = False
STATUS = 200
GRANT_MODE = "rotate"
PROFILE_MODE = "ok"
PROFILE_DB = None


class Catalog(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def respond(self):
        try:
            direct = self.server.server_port == 52989
            if self.path in ("/ListAvailableProfiles", f"/kiro-refresh/{CW}/ListAvailableProfiles"):
                return self.lookup_profile(direct)
            if self.path.startswith("/kiro-refresh/"):
                _, _, issuer, path = self.path.split("/", 3)
                return self.renew(issuer, "/" + path, direct)
            if self.headers["Host"] in ISSUERS:
                return self.renew(self.headers["Host"], self.path, direct)
            status = STATUS
            output = UPSTREAM
            authorization = self.headers.get_all("Authorization")
            if self.command == "GET":
                parsed = urlsplit(self.path)
                params = parse_qs(parsed.query, strict_parsing=True)
                api_key = authorization == ["Bearer " + KEY]
                if direct:
                    _, kiro, service, region, action = parsed.path.split("/")
                    assert (kiro, service, action) == ("kiro", "q", "ListAvailableModels")
                else:
                    assert parsed.path == "/ListAvailableModels"
                    family, region, suffix = self.headers["Host"].split(".", 2)
                    assert family == "q" and suffix == "amazonaws.com"
                assert self.headers["X-Amz-Target"] is None
                if api_key:
                    assert region == REGION and self.headers["TokenType"] == "API_KEY"
                    assert params == {"origin": ["AI_EDITOR"]}
                else:
                    assert region in {"us-east-1", FRANKFURT} and self.headers["TokenType"] is None
                    expected = (EU_PROFILE,) if region == FRANKFURT else (PROFILE, OPERATOR_PROFILE, None)
                    profiles = params.pop("profileArn", [None])
                    assert len(profiles) == 1 and profiles[0] in expected
                    assert params == {"origin": ["AI_EDITOR"]}
            else:
                assert self.path == ("/kiro/codewhisperer/us-east-1" if direct else "/")
                assert direct or self.headers["Host"] == CW
                assert self.headers["TokenType"] is None
                assert self.headers["Content-Type"] == "application/x-amz-json-1.0"
                assert self.headers["X-Amz-Target"] == "AmazonCodeWhispererService.ListAvailableModels"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert body.pop("profileArn") in (PROFILE, OPERATOR_PROFILE) and body == {"origin": "AI_EDITOR"}
            if authorization != ["Bearer " + KEY]:
                assert authorization in (["Bearer " + TOKEN], ["Bearer " + FRESH])
                BEARERS.append(authorization[0])
                if authorization == ["Bearer " + FRESH]:
                    status = 200
                    output = {"models": [*UPSTREAM["models"], {"modelId": FRESH}]}
            assert self.headers["Accept"] == "application/json" and self.headers["User-Agent"] == USER_AGENT
            assert not any(self.headers[header] for header in (
                "X-Relay-Path", "X-Relay-Target", "X-Amz-User-Agent", "X-Amz-Sso-Bearer", "Amz-Sdk-Invocation-Id"
            ))
            CALLS.append((direct, self.command))
            self.reply(status, output)
        except (AssertionError, KeyError, ValueError) as error:
            ERRORS.append(repr(error) + " from " + self.path)
            self.send_error(422)

    def reply(self, status, data):
        payload = json.dumps(data).encode()
        self.send_response(status)
        if status == 307:
            self.send_header("Location", f"https://{self.headers['Host']}/never-follow")
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def lookup_profile(self, direct):
        assert self.command == "POST" and (direct or self.headers["Host"] == CW)
        assert self.headers.get_all("Authorization") == ["Bearer " + FRESH]
        assert self.headers["Content-Type"] == "application/json" and self.headers["Accept"] == "application/json"
        assert self.headers["X-Relay-Target"] is None and self.headers["Proxy-Authorization"] is None
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body == {"maxResults": 10}
        current = stored(PROFILE_DB)
        assert current["accessToken"] == FRESH and current["refreshToken"] == ROTATED
        assert current["kiroRefreshAttempt"] is None
        PROFILE_CALLS.append((direct, self.path))
        if PROFILE_MODE == "operator":
            with sqlite3.connect(PROFILE_DB) as sql:
                sql.execute("""UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData.profileArn',?)
                    WHERE id='fixture-kiro-proxy-models'""", (OPERATOR_PROFILE,))
        self.reply(503 if PROFILE_MODE == "unavailable" else 200, {
            "profiles": [{"arn": "invalid-profile"}, {"arn": EU_PROFILE}, {"arn": f" {PROFILE} "}],
        })

    def renew(self, issuer, path, direct):
        assert self.command == "POST"
        assert path == ISSUERS[issuer]
        assert self.headers["Accept"] == "application/json"
        assert all(self.headers[name] is None for name in (
            "Authorization", "X-Amz-Target", "X-9r-Cli-Token", "Proxy-Authorization", "X-Amz-Sso-Bearer",
        ))
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if issuer == MICROSOFT:
            assert self.headers["Content-Type"] == "application/x-www-form-urlencoded"
            assert parse_qs(body.decode()) == {"client_id": ["fixture-client"], "refresh_token": [GRANT],
                                              "grant_type": ["refresh_token"], "scope": ["one offline_access"]}
            tokens = {"access_token": FRESH, "refresh_token": ROTATED, "expires_in": 3600}
        else:
            assert self.headers["Content-Type"] == "application/json"
            if issuer == SOCIAL:
                assert json.loads(body) == {"refreshToken": GRANT} and self.headers["User-Agent"] == "9Router"
            else:
                assert json.loads(body) == {"clientId": "fixture-client", "clientSecret": CLIENT_SECRET,
                                            "refreshToken": GRANT, "grantType": "refresh_token"}
            tokens = {"accessToken": FRESH, "refreshToken": ROTATED, "expiresIn": 3600}
        OAUTH.append((direct, self.path))
        if GRANT_MODE == "disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.close_connection = True
            return
        if GRANT_MODE == "profile":
            tokens["profileArn"] = f" {PROFILE} "
        self.reply(200, tokens)

    do_GET = respond
    do_POST = respond


class Tunnel(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            if self.server.http:
                method, authority, version = self.rfile.readline(4096).decode().strip().split(" ")
                assert (method, version) == ("CONNECT", "HTTP/1.1")
                host, port = authority.rsplit(":", 1)
                port = int(port)
                while line := self.rfile.readline(4096).strip():
                    name = line.partition(b":")[0].lower()
                    assert name not in {b"authorization", b"x-relay-target", b"x-amz-sso-bearer"}
                    assert all(secret.encode() not in line for secret in (*SECRETS, PROFILE))
            else:
                version, length = self.rfile.read(2)
                assert version == 5 and 0 in self.rfile.read(length)
                self.request.sendall(b"\x05\x00")
                assert self.rfile.read(4) == b"\x05\x01\x00\x03"
                length, = self.rfile.read(1)
                host = self.rfile.read(length).decode()
                port = int.from_bytes(self.rfile.read(2), "big")
            TUNNELS.append(("http" if self.server.http else "socks5h", host, port))
            assert host in HOSTS and port == 443
            if REJECT:
                assert self.server.http
                self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                return
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n" if self.server.http else
                                 b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            with socket.create_connection(("127.0.0.1", 52987), timeout=3) as upstream:
                sockets = (self.request, upstream)
                while ready := select.select(sockets, [], [], 6)[0]:
                    for source in ready:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            ERRORS.append(repr(error))
        except OSError:
            pass


class ProxyServer(socketserver.ThreadingTCPServer):
    daemon_threads = True


@contextmanager
def serving(server):
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()


def tls(directory):
    ca, key, cert, leaf, csr = (directory / name for name in ("ca.pem", "ca.key", "cert.pem", "cert.key", "csr"))
    extensions = directory / "extensions"
    names = ",".join(f"DNS:{name}" for name in HOSTS)
    extensions.write_text(f"basicConstraints=critical,CA:FALSE\nsubjectAltName={names}\nextendedKeyUsage=serverAuth\n")
    for args in [
        ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Fixture CA",
         "-keyout", key, "-out", ca, "-addext", "basicConstraints=critical,CA:TRUE"],
        ["req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", leaf, "-out", csr],
        ["x509", "-req", "-days", "1", "-in", csr, "-CA", ca, "-CAkey", key, "-set_serial", "1", "-out", cert,
         "-extfile", extensions],
    ]:
        subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, leaf)
    context.set_servername_callback(lambda _conn, name, _ctx: HELLOS.append(name))
    return ca, context


def save(db, method, detail=None, expiry=None, region=None, current=None, renewable=False, profile=PROFILE):
    oauth = method != "api_key"
    data = {"accessToken": TOKEN if oauth else KEY, "providerSpecificData": {
        "authMethod": method, "region": region or ("us-east-1" if oauth else REGION), **(detail or {}),
    }}
    if oauth:
        data["providerSpecificData"]["profileArn"] = profile
        if current:
            data["refreshToken"] = "fixture-do-not-refresh"
            data["expiresAt"] = time.time() + (expiry if expiry is not None else 3600)
        if renewable:
            data["refreshToken"] = GRANT
            if method in {"idc", "builder-id"}:
                data["providerSpecificData"].update(clientId="fixture-client", clientSecret=CLIENT_SECRET)
            elif method == "external_idp":
                data["providerSpecificData"].update(clientId="fixture-client", scope=["one", "offline_access"],
                                                    tokenEndpoint=f"https://{MICROSOFT}{MICROSOFT_PATH}")
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES ('fixture-kiro-proxy-models','kiro',?,'Synthetic Kiro',1,1,?,
                  '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""",
                    ("oauth" if oauth else "api_key", json.dumps(data)))


def enable_global_proxy(db, url):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?)
            ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (json.dumps({"outboundProxyEnabled": True, "outboundProxyUrl": url}),))


def stored(db):
    with sqlite3.connect(db) as sql:
        raw, = sql.execute("SELECT data FROM providerConnections WHERE id='fixture-kiro-proxy-models'").fetchone()
    return json.loads(raw)


def request(port, path=PATH, headers=ADMIN, method="GET"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        connection.request(method, path, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


@contextmanager
def gateway(scratch, name, ca, pools):
    home = scratch / name
    home.mkdir()
    data = home / "data"
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
                   check=True, cwd=home, env={"PATH": os.environ["PATH"], "HOME": str(home)},
                   stderr=subprocess.DEVNULL)
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.executemany("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
            VALUES (?,1,'unknown',?,'2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""", [
            (name, json.dumps({"type": kind, "proxyUrl": url, "strictProxy": True}))
            for name, kind, url in pools
        ])
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback = home / "fallback.log"
    env = {"PATH": "", "HOME": str(home), "DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
    if ca:
        env["NINEROUTER_MODEL_LOCAL_PROXY_CA"] = str(ca)
    with (home / "server.log").open("w+") as log:
        server = subprocess.Popen([BINARY, "--no-browser"], cwd=home, env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while server.poll() is None and time.monotonic() < deadline:
                try:
                    if request(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.05)
            else:
                raise RuntimeError("Native gateway did not start")
            yield port, db
            assert not ERRORS and not fallback.read_bytes(), (ERRORS, fallback.read_text())
        except Exception:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            print(TUNNELS, HELLOS, CALLS, OAUTH, ERRORS, file=sys.stderr)
            raise
        finally:
            server.terminate()
            server.wait(timeout=5)


def rejected(port):
    status, body = request(port)
    assert status == 200 and body["models"] == [] and body["warning"] == WARNING, (status, body)


def routed(port, path="/v1/models"):
    status, body = request(port, path, API)
    assert status == 200, (status, body)
    assert all(secret not in json.dumps(body) for secret in SECRETS)
    return {row["id"]: row for row in body["data"] if row["owned_by"] == "kr"}


def cases(port, db, http_url, socks_url):
    global REJECT, STATUS
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": http_url}
    socks = {"proxyPoolId": "fixture-socks"}
    save(db, "api_key")
    direct_key = request(port)
    direct_public = routed(port)
    assert direct_public[LIVE_ID + "-thinking"]["capabilities"] == {"thinking": True, "agentic": False}
    save(db, "idc")
    direct_profile = request(port)
    assert routed(port) == direct_public
    assert direct_key == direct_profile and direct_key[0] == 200 and len(direct_key[1]["models"]) == 4
    assert CALLS == [(True, "GET"), (True, "POST")]
    model = direct_key[1]["models"][-1]
    assert (model["id"], model["contextLength"], model["rateMultiplier"]) == (
        "fixture-tunnel-model-thinking-agentic", 300000, 1.25)
    save(db, "api_key", http)
    assert request(port, headers={})[0] == 401 and not TUNNELS
    assert request(port) == request(port) == direct_key and len(TUNNELS) == 1
    assert routed(port, "/api/v1/models") == direct_public
    save(db, "api_key", socks)
    assert request(port) == direct_key and len(TUNNELS) == 2
    assert routed(port) == direct_public
    save(db, "idc", socks, current=True)
    assert request(port) == direct_profile and len(TUNNELS) == 3
    assert routed(port) == direct_public
    save(db, "builder-id", http, current=True)
    assert request(port) == direct_profile and len(TUNNELS) == 4
    assert [tunnel[0] for tunnel in TUNNELS] == ["http", "socks5h", "socks5h", "http"]
    assert CALLS[2:] == [(False, "GET"), (False, "GET"), (False, "POST"), (False, "POST")]

    for method, detail, expiry, region, current in [
        ("idc", {"proxyPoolId": "fixture-relay"}, None, None, True),
        ("api_key", {"proxyPoolId": "missing-pool"}, None, None, False),
        ("api_key", {**http, "connectionNoProxy": ".amazonaws.com"}, None, None, False),
        ("api_key", {**http, "vercelRelayUrl": http_url}, None, None, False),
        ("api_key", {**http, "connectionProxyUrl": socks_url.replace("socks5h:", "socks5:")}, None, None, False),
        ("idc", socks, -30, None, True),
        ("idc", socks, None, None, False),
        ("builder-id", socks, None, REGION, True),
        ("unknown", socks, None, None, True),
    ]:
        save(db, method, detail, expiry, region, current)
        rejected(port)
        assert LIVE_ID not in routed(port)
    assert len(TUNNELS) == 4 and len(CALLS) == 6

    save(db, "api_key", socks)
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES ('fixture-unmigrated','qoder','apikey','Offline Qoder',1,1,'{}',
                  '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""")
    assert routed(port) == direct_public and len(TUNNELS) == 4
    with sqlite3.connect(db) as sql:
        sql.execute("DELETE FROM providerConnections WHERE id='fixture-unmigrated'")

    for suffix, failure, status in (("redirect", False, 307), ("reject", True, 200)):
        proxy = http_url.replace("127.0.0.1", f"fixture-{suffix}@127.0.0.1")
        save(db, "api_key", {**http, "connectionProxyUrl": proxy})
        REJECT, STATUS = failure, status
        rejected(port)
    save(db, "idc", current=True)
    enable_global_proxy(db, http_url.replace("127.0.0.1", "fixture-global@127.0.0.1"))
    REJECT, STATUS = False, 401
    rejected(port)
    with sqlite3.connect(db) as sql:
        saved, = sql.execute("SELECT data FROM providerConnections WHERE id='fixture-kiro-proxy-models'").fetchone()
        account = json.loads(saved)
        assert account["accessToken"] == TOKEN and account["refreshToken"] == "fixture-do-not-refresh"
    REJECT, STATUS = False, 200
    enable_global_proxy(db, http_url.replace("127.0.0.1", "fixture-empty@127.0.0.1"))
    models = UPSTREAM["models"]
    UPSTREAM["models"] = []
    try:
        rejected(port)
    finally:
        UPSTREAM["models"] = models
    assert len(TUNNELS) == 8 and CALLS[6:] == [(False, "GET"), (False, "POST"), (False, "POST")]
    assert HELLOS == [API_HOST, API_HOST, CW, CW, API_HOST, CW, CW]


def refresh_cases(port, db, http_url):
    global STATUS, GRANT_MODE
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": http_url}
    direct_oidc = f"/kiro-refresh/{OIDC}/token"
    STATUS = 401
    save(db, "idc", current=True, renewable=True)
    success = request(port)
    assert success[0] == 200 and len(success[1]["models"]) == 4
    assert all(secret not in json.dumps(success) for secret in SECRETS)
    live = routed(port)
    assert LIVE_ID in live and BEARERS == ["Bearer " + TOKEN, "Bearer " + FRESH]
    assert OAUTH == [(True, direct_oidc)] and not TUNNELS

    save(db, "builder-id", http, -30, current=True, renewable=True)
    assert request(port) == success and routed(port) == live
    assert OAUTH == [(True, direct_oidc), (False, "/token")]
    assert TUNNELS == [("http", OIDC, 443), ("http", CW, 443)]
    assert BEARERS[-1] == "Bearer " + FRESH and len(BEARERS) == 3

    before = len(TUNNELS)
    save(db, "idc", {"proxyPoolId": "fixture-socks"}, current=True, renewable=True)
    with ThreadPoolExecutor(max_workers=4) as workers:
        assert all(row == live for row in workers.map(lambda _: routed(port), range(4)))
    assert OAUTH[-1] == (False, "/token") and len(OAUTH) == 3
    assert TUNNELS[before:].count(("socks5h", OIDC, 443)) == 1
    assert all(row[0] == "socks5h" for row in TUNNELS[before:])
    current = stored(db)
    assert current["accessToken"] == FRESH and current["refreshToken"] == ROTATED
    assert current["kiroRefreshAttempt"] is None and current["providerSpecificData"]["profileArn"] == PROFILE

    before = len(TUNNELS), len(OAUTH), len(CALLS)
    for details in ({"proxyPoolId": "fixture-relay"}, {"proxyPoolId": "missing"},
                    http | {"connectionNoProxy": OIDC}):
        save(db, "idc", details, -30, current=True, renewable=True)
        rejected(port)
        assert not stored(db).get("kiroRefreshAttempt")
    assert (len(TUNNELS), len(OAUTH), len(CALLS)) == before

    GRANT_MODE = "disconnect"
    save(db, "idc", http, -30, current=True, renewable=True)
    rejected(port)
    rejected(port)
    assert len(OAUTH) == before[1] + 1 and len(CALLS) == before[2]
    assert stored(db)["refreshToken"] == GRANT and stored(db)["kiroRefreshAttempt"]
    GRANT_MODE, STATUS = "rotate", 200
    return success[1]["models"]


def regional_cases(port, db, http_url, snapshot):
    global STATUS
    STATUS = 401
    save(db, "idc", region=FRANKFURT, current=True, renewable=True, profile=EU_PROFILE)
    assert request(port)[1]["models"] == snapshot
    assert OAUTH == [(True, f"/kiro-refresh/{EU_OIDC}/token")]
    assert CALLS == [(True, "GET"), (True, "GET")] and not TUNNELS
    live = routed(port)
    assert LIVE_ID in live
    assert stored(db)["providerSpecificData"]["profileArn"] == EU_PROFILE

    for method, region, profile, issuer, target, tunnel in (
        ("idc", FRANKFURT, EU_PROFILE, EU_OIDC, EU_Q, "socks5h"),
        ("google", "us-east-1", PROFILE, SOCIAL, US_Q, "http"),
        ("github", FRANKFURT, EU_PROFILE, SOCIAL, EU_Q, "http"),
        ("external_idp", "us-east-1", PROFILE, MICROSOFT, US_Q, "http"),
    ):
        detail = {"proxyPoolId": "fixture-socks"} if tunnel == "socks5h" else {
            "connectionProxyEnabled": True,
            "connectionProxyUrl": http_url.replace("127.0.0.1", f"fixture-{method}@127.0.0.1"),
        }
        before = len(TUNNELS), len(OAUTH), len(CALLS)
        save(db, method, detail, -30, region=region, current=True, renewable=True, profile=profile)
        assert routed(port) == live and request(port)[1]["models"] == snapshot
        assert TUNNELS[before[0]:] == [(tunnel, issuer, 443), (tunnel, target, 443)]
        assert HELLOS[-2:] == [issuer, target]
        assert OAUTH[before[1]:] == [(False, ISSUERS[issuer])] and CALLS[before[2]:] == [(False, "GET")]
        account = stored(db)
        assert account["refreshToken"] == ROTATED and account["providerSpecificData"]["profileArn"] == profile

    before = len(TUNNELS), len(OAUTH), len(CALLS)
    malformed = EU_PROFILE.replace(FRANKFURT, "eu-central-1.invalid")
    for region, profile in ((FRANKFURT, malformed), ("us-east-1", EU_PROFILE)):
        save(db, "idc", {"proxyPoolId": "fixture-socks"}, -30, region=region,
             profile=profile, current=True, renewable=True)
        rejected(port)
    assert (len(TUNNELS), len(OAUTH), len(CALLS)) == before
    STATUS = 200


def profileless_cases(port, db, http_url, snapshot):
    global GRANT_MODE, PROFILE_DB, PROFILE_MODE, STATUS
    PROFILE_DB = db
    save(db, "google", profile=None, current=True, renewable=True)
    assert request(port)[1]["models"] == snapshot
    assert CALLS == [(True, "GET")] and not OAUTH and not PROFILE_CALLS
    assert not stored(db)["providerSpecificData"]["profileArn"]

    GRANT_MODE = "profile"
    save(db, "builder-id", expiry=-30, current=True, renewable=True, profile=None)
    assert request(port)[1]["models"] == snapshot
    assert OAUTH == [(True, f"/kiro-refresh/{OIDC}/token")]
    assert CALLS[-1] == (True, "POST") and not PROFILE_CALLS
    assert stored(db)["providerSpecificData"]["profileArn"] == PROFILE

    GRANT_MODE = "rotate"
    STATUS = 401
    detail = {"proxyPoolId": "fixture-socks"}
    save(db, "idc", detail, current=True, renewable=True, profile=None)
    assert LIVE_ID in routed(port) and request(port)[1]["models"] == snapshot
    assert OAUTH[-1] == (False, "/token") and PROFILE_CALLS == [(False, "/ListAvailableProfiles")]
    assert TUNNELS == [("socks5h", US_Q, 443), ("socks5h", OIDC, 443),
                       ("socks5h", CW, 443), ("socks5h", CW, 443)]
    assert stored(db)["providerSpecificData"]["profileArn"] == PROFILE

    for mode, expected_profile, target in (
        ("unavailable", None, US_Q),
        ("operator", OPERATOR_PROFILE, CW),
    ):
        PROFILE_MODE = mode
        route = http_url.replace("127.0.0.1", f"fixture-{mode}@127.0.0.1")
        detail = {"connectionProxyEnabled": True, "connectionProxyUrl": route}
        before = len(TUNNELS), len(OAUTH), len(PROFILE_CALLS)
        save(db, "idc", detail, -30, current=True, renewable=True, profile=None)
        assert request(port)[1]["models"] == snapshot
        assert TUNNELS[before[0]:] == [("http", OIDC, 443), ("http", CW, 443), ("http", target, 443)]
        assert OAUTH[before[1]:] == [(False, "/token")]
        assert PROFILE_CALLS[before[2]:] == [(False, "/ListAvailableProfiles")]
        current = stored(db)
        assert current["providerSpecificData"]["profileArn"] == expected_profile
        assert current["accessToken"] == FRESH and current["refreshToken"] == ROTATED

    before = len(PROFILE_CALLS)
    save(db, "external_idp", expiry=-30, current=True, renewable=True, profile=None)
    assert request(port)[1]["models"] == snapshot
    assert OAUTH[-1] == (True, f"/kiro-refresh/{MICROSOFT}{MICROSOFT_PATH}")
    assert len(PROFILE_CALLS) == before and stored(db)["providerSpecificData"]["profileArn"] is None
    PROFILE_MODE, PROFILE_DB, STATUS = "ok", None, 200


def connection_cases(port, db):
    path = "/api/providers/fixture-kiro-proxy-models/test"
    save(db, "idc", {"proxyPoolId":"fixture-socks"}, -30, current=True, renewable=True)
    assert request(port, path, method="POST") == (200,{"valid":True,"error":None,"refreshed":True})
    assert TUNNELS == [("socks5h", OIDC, 443)] and OAUTH == [(False, "/token")] and not CALLS
    assert stored(db)["accessToken"] == FRESH and stored(db)["testStatus"] == "active"
    assert request(port, path, method="POST") == (200,{"valid":True,"error":None,"refreshed":False})
    assert len(TUNNELS) == 1
    save(db, "idc", {"proxyPoolId":"missing"}, -30, current=True, renewable=True)
    assert request(port, path, method="POST") == (200,{
        "valid":False,"error":"Token expired and refresh failed","refreshed":False,
    })
    assert len(TUNNELS) == 1 and stored(db)["testStatus"] == "error"


with tempfile.TemporaryDirectory(prefix="9router-kiro-models-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, context = tls(scratch)
    catalog = ThreadingHTTPServer(("127.0.0.1", 52987), Catalog)
    catalog.socket = context.wrap_socket(catalog.socket, server_side=True)
    stack.enter_context(serving(catalog))
    stack.enter_context(serving(ThreadingHTTPServer(("127.0.0.1", 52989), Catalog)))
    connect, socks, relay = [ProxyServer(("127.0.0.1", 0), Tunnel) for _ in range(3)]
    connect.http, socks.http, relay.http = True, False, True
    for proxy in (connect, socks, relay):
        stack.enter_context(serving(proxy))
    http_url = f"http://127.0.0.1:{connect.server_address[1]}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    relay_url = f"http://127.0.0.1:{relay.server_address[1]}"
    pools = [("fixture-socks", "socks5", socks_url), ("fixture-relay", "cloudflare", relay_url)]
    with gateway(scratch, "local-tls", ca, pools) as (port, db):
        cases(port, db, http_url, socks_url)
    for ledger in (CALLS, TUNNELS, HELLOS, BEARERS, OAUTH):
        ledger.clear()
    with gateway(scratch, "oauth-refresh", ca, pools) as (port, db):
        snapshot = refresh_cases(port, db, http_url)
    for ledger in (CALLS, TUNNELS, HELLOS, BEARERS, OAUTH):
        ledger.clear()
    with gateway(scratch, "regional-oauth", ca, pools) as (port, db):
        regional_cases(port, db, http_url, snapshot)
    for ledger in (CALLS, TUNNELS, HELLOS, BEARERS, OAUTH):
        ledger.clear()
    with gateway(scratch, "profileless-oauth", ca, pools) as (port, db):
        profileless_cases(port, db, http_url, snapshot)
    for ledger in (CALLS, TUNNELS, HELLOS, BEARERS, OAUTH, PROFILE_CALLS):
        ledger.clear()
    with gateway(scratch, "oauth-connection-test", ca, pools) as (port, db):
        connection_cases(port, db)
    for ledger in (CALLS, TUNNELS, HELLOS, BEARERS, OAUTH, PROFILE_CALLS):
        ledger.clear()
    with gateway(scratch, "official-origin", None, pools) as (port, db):
        save(db, "api_key", {"connectionProxyEnabled": True, "connectionProxyUrl": http_url})
        rejected(port)
        save(db, "idc", {"proxyPoolId": "fixture-socks"}, current=True)
        rejected(port)
        assert TUNNELS == [("http", f"q.{REGION}.amazonaws.com", 443),
                           ("socks5h", "codewhisperer.us-east-1.amazonaws.com", 443)]
        assert HELLOS == [host for _, host, _ in TUNNELS] and not CALLS
if sys.argv[2:] == ["--snapshot"]:
    print(json.dumps(snapshot, sort_keys=True))
else:
    print("Kiro catalog and AWS OAuth renewal passed direct, CONNECT and SOCKS5h contracts, including concurrent rotation and blocked routes.")
