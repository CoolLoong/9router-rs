import base64
from collections import deque
from contextlib import closing
from datetime import date, datetime, timedelta, timezone
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import re
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
from urllib.parse import parse_qs


ROOT = Path(__file__).resolve().parents[2]
BINARY = str(Path(sys.argv[1]).resolve())
CHAT = sys.argv[2:] == ["chat"]
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
CHAT_AUTH = {"Authorization": "Bearer offline-chat-key"}
CLAUDE = "br/us.anthropic.claude-sonnet-4-5-20250929-v1:0"
CLAUDE_ARN = "br/arn:aws:bedrock:us-west-2:123456789000:inference-profile/" + CLAUDE.removeprefix("br/")
GROK = "brx/us.xai.grok-4.6"
CHAT_KEYS = {
    "bedrock": ("us-west-2", "ASIAFIXTUREBEDROCKKEY", "offline-bedrock-secret", "offline-bedrock-session"),
    "bedrock-xai": ("us-east-1", "AKIAFIXTUREGROKKEY", "offline-bedrock-grok-secret", None),
}
ROLE_SOURCE = ("AKIA_FIXTURE_PROXY_SOURCE", "offline-role-proxy-source", "offline-role-proxy-session")
AWS_HOSTS = {
    "us-west-2": ("ASIA_BEDROCK_PROXY", "fixture-bedrock-proxy-secret", "fixture-bedrock-proxy-session"),
    "eu-central-1": ("AKIA_BEDROCK_PROXY_XAI", "fixture-bedrock-xai-secret", None),
}
HTTP = "http://bedrock-proxy:fixture-proxy-password@127.0.0.1:52984"
SOCKS = "socks5h://127.0.0.1:52985"
RELAY = "http://127.0.0.1:52986/trap"
STAGES = deque()
CALLS = []
DIRECT = []
TUNNELS = []
HELLOS = []
FAILURES = []
REJECT_CONNECT = False


def account(region, **flags):
    key, secret, session = AWS_HOSTS[region]
    return {"apiKey": secret, "providerSpecificData": {"region": region, "accessKeyId": key,
            "sessionToken": session, **flags}}


def signature(request):
    assert request.command == "GET" and request.path == "/foundation-models"
    assert request.headers.get("Content-Length", "0") == "0"
    assert request.headers["Accept"] == "application/json"
    assert request.headers.get("Proxy-Authorization") is None
    assert request.headers.get("X-Relay-Target") is None and request.headers.get("X-Relay-Path") is None
    host = request.headers["Host"]
    region, = [region for region in AWS_HOSTS if host == f"bedrock.{region}.amazonaws.com"]
    access, secret, token = AWS_HOSTS[region]
    assert request.headers["X-Amz-Security-Token"] == token
    authorization = request.headers["Authorization"]
    parsed = re.fullmatch(r"AWS4-HMAC-SHA256 Credential=([^/]+)/([^,]+), SignedHeaders=([^,]+), Signature=([0-9a-f]{64})", authorization)
    assert parsed, "SigV4 authorization missing"
    key, scope, signed, digest = parsed.groups()
    stamp = request.headers["X-Amz-Date"]
    fields = signed.split(";")
    assert fields == ["accept", "host", "x-amz-date"] + (["x-amz-security-token"] if token else [])
    assert key == access and scope == f"{stamp[:8]}/{region}/bedrock/aws4_request"
    headers = "".join(f"{field}:{request.headers[field]}\n" for field in fields)
    canonical = f"GET\n/foundation-models\n\n{headers}\n{signed}\n{hashlib.sha256(b'').hexdigest()}"
    payload = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
    derived = ("AWS4" + secret).encode()
    for component in scope.split("/"):
        derived = hmac.digest(derived, component.encode(), "sha256")
    assert hmac.compare_digest(hmac.digest(derived, payload.encode(), "sha256").hex(), digest)
    return region, authorization


class Bedrock(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, **extra):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in extra.items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        try:
            region, signed = signature(self)
            expected_region, result = STAGES.popleft()
            assert region == expected_region
            CALLS.append((region, result))
            if result == "success":
                self.reply(200, {"modelSummaries": []})
            elif result == "role-denied":
                self.reply(403, {}, **{"X-Amzn-ErrorType": "AccessDeniedException:aws"})
            elif result == "signature":
                access, secret, token = AWS_HOSTS[region]
                echoed = f"fixture error {access} {secret} {token} {signed}"
                self.reply(403, {"message": echoed}, **{"X-Amzn-ErrorType": "InvalidSignatureException:aws"})
            elif result == "redirect":
                self.reply(307, {}, Location="https://localhost:52987/foundation-models")
            else:
                raise AssertionError("Unknown fixture state")
        except (AssertionError, KeyError, IndexError, ValueError) as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {})

    def do_POST(self):
        port = self.server.server_port
        if not CHAT or port not in (52987, 52989):
            FAILURES.append("Unexpected outbound POST " + self.path)
            return self.reply(422, {})
        try:
            if self.headers["Host"] == "sts.us-west-2.amazonaws.com":
                return self.assume_role(port == 52989)
            hosts = {f"bedrock-runtime.{region}.amazonaws.com" for region, *_ in CHAT_KEYS.values()}
            direct = port == 52989
            assert self.headers["Host"] in hosts
            assert self.path.startswith("/bedrock/model/" if direct else "/model/")
            assert not any(self.headers[name] for name in ("Proxy-Authorization", "X-Relay-Target", "X-Relay-Path"))
            payload = self.rfile.read(int(self.headers["Content-Length"]))
            calls = DIRECT if direct else CALLS
            calls.append((self.headers["Host"], self.path))
            headers = dict(self.headers)
            with closing(http.client.HTTPConnection("127.0.0.1", 52988, timeout=4)) as upstream:
                upstream.request("POST", self.path if direct else "/bedrock" + self.path, payload, headers)
                reply = upstream.getresponse()
                data = reply.read()
                if reply.status == 422 and b"echo " not in data:
                    raise AssertionError(data.decode())
                self.send_response(reply.status)
                self.send_header("Content-Length", str(len(data)))
                for name in ("Content-Type", "Location"):
                    if reply.getheader(name):
                        self.send_header(name, reply.getheader(name))
                self.end_headers()
                self.wfile.write(data[:29])
                self.wfile.write(data[29:])
        except (AssertionError, KeyError, ValueError) as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {})

    def assume_role(self, direct):
        assert self.path == ("/bedrock-profile/sts/us-west-2" if direct else "/")
        assert not self.headers["Proxy-Authorization"] and not self.headers["X-Relay-Target"]
        payload = self.rfile.read(int(self.headers["Content-Length"]))
        fields = parse_qs(payload.decode(), strict_parsing=True)
        assert fields["Action"] == ["AssumeRole"] and fields["Version"] == ["2011-06-15"]
        assert fields["RoleArn"] == ["arn:aws:iam::123456789012:role/Fixture"]
        assert fields["RoleSessionName"][0].startswith("aws-sdk-js-")
        assert fields["DurationSeconds"] == ["3600"]
        access, secret, session = ROLE_SOURCE
        assert self.headers["X-Amz-Security-Token"] == session
        auth = re.fullmatch(r"AWS4-HMAC-SHA256 Credential=([^/]+)/([^,]+), SignedHeaders=([^,]+), Signature=([0-9a-f]{64})",
                            self.headers["Authorization"])
        assert auth, "STS signature was missing"
        key, scope, signed, digest = auth.groups()
        stamp = self.headers["X-Amz-Date"]
        assert key == access and scope == f"{stamp[:8]}/us-west-2/sts/aws4_request"
        assert signed == "content-type;host;x-amz-date;x-amz-security-token"
        headers = "".join(f"{field}:{self.headers[field]}\n" for field in signed.split(";"))
        canonical = f"POST\n/\n\n{headers}\n{signed}\n{hashlib.sha256(payload).hexdigest()}"
        signable = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
        derived = ("AWS4" + secret).encode()
        for component in scope.split("/"):
            derived = hmac.digest(derived, component.encode(), "sha256")
        assert hmac.compare_digest(hmac.digest(derived, signable.encode(), "sha256").hex(), digest)
        (DIRECT if direct else CALLS).append((self.headers["Host"], self.path))
        _, selected, selected_secret, selected_session = CHAT_KEYS["bedrock"]
        expiry = (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()
        xml = (f"<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{selected}</AccessKeyId>"
               f"<SecretAccessKey>{selected_secret}</SecretAccessKey><SessionToken>{selected_session}</SessionToken>"
               f"<Expiration>{expiry}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/xml")
        self.send_header("Content-Length", str(len(xml)))
        self.end_headers()
        self.wfile.write(xml)


class Proxy(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            if self.server.server_address[1] == 52984:
                target, port = self.connect()
            else:
                target, port = self.socks()
            allowed = ({f"bedrock-runtime.{region}.amazonaws.com" for region, *_ in CHAT_KEYS.values()} if CHAT
                       else {f"bedrock.{region}.amazonaws.com" for region in AWS_HOSTS})
            assert (target == "localhost" and port == 52987) or (port == 443 and target in allowed), (target, port)
            TUNNELS.append(("http" if self.server.server_address[1] == 52984 else "socks5h", target, port))
            if REJECT_CONNECT:
                return
            with socket.create_connection(("127.0.0.1", 52987), timeout=3) as upstream:
                sockets = [self.request, upstream]
                while True:
                    readable, _, _ = select.select(sockets, [], [], 10)
                    if not readable:
                        return
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            FAILURES.append(str(error) or "proxy invariant")
        except OSError:
            pass

    def connect(self):
        method, address, protocol = self.rfile.readline(4096).decode().strip().split(" ")
        assert method == "CONNECT" and protocol == "HTTP/1.1"
        assert ":" in address
        host, port = address.rsplit(":", 1)
        headers = {}
        for _ in range(30):
            line = self.rfile.readline(4096).strip()
            if not line:
                break
            name, value = line.decode().split(":", 1)
            headers[name.lower()] = value.strip()
        assert not {"authorization", "x-amz-security-token", "x-amz-date"}.intersection(headers)
        assert headers.get("proxy-authorization") == "Basic " + base64.b64encode(b"bedrock-proxy:fixture-proxy-password").decode()
        self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n" if REJECT_CONNECT else
                             b"HTTP/1.1 200 Connection Established\r\n\r\n")
        return host, int(port)

    def socks(self):
        version, length = self.rfile.read(2)
        assert version == 5 and 0 in self.rfile.read(length)
        self.request.sendall(b"\x05\x00")
        assert self.rfile.read(4) == b"\x05\x01\x00\x03"
        length, = self.rfile.read(1)
        hostname = self.rfile.read(length).decode()
        port = int.from_bytes(self.rfile.read(2), "big")
        self.request.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
        return hostname, port


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def tls_context(scratch):
    ca, ca_key, cert, key, req = (scratch / filename for filename in ["ca.pem", "ca.key", "leaf.pem", "leaf.key", "leaf.csr"])
    extensions = scratch / "extensions.conf"
    extensions.write_text("basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n")
    for command in [
        ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Bedrock local fixture root",
         "-keyout", ca_key, "-out", ca, "-addext", "basicConstraints=critical,CA:TRUE"],
        ["req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", key, "-out", req],
        ["x509", "-req", "-days", "1", "-in", req, "-CA", ca, "-CAkey", ca_key, "-set_serial", "1", "-out", cert,
         "-extfile", extensions],
    ]:
        subprocess.run(["openssl", *command], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    context.set_servername_callback(lambda _connection, name, _context: HELLOS.append(name))
    return context, ca


def seed(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True,
                   cwd=ROOT, env={"PATH": os.environ["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.executemany("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                         VALUES (?,?, 'active',?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        [(name, enabled, json.dumps({"type": kind, "proxyUrl": url, "strictProxy": True}))
                         for name, enabled, kind, url in [("bedrock-socks", 1, "socks5", SOCKS),
                             ("bedrock-relay", 1, "cloudflare", RELAY), ("bedrock-missing", 0, "http", HTTP)]])
        sql.executemany("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                         VALUES (?,?,'apikey','Fixture AWS',?,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        [(name, provider, priority, json.dumps(fields))
                         for priority, (name, provider, fields) in enumerate([
                             ("bedrock-http", "bedrock", account("us-west-2", connectionProxyEnabled=True, connectionProxyUrl=HTTP)),
                             ("bedrock-xai-socks", "bedrock-xai", account("eu-central-1", proxyPoolId="bedrock-socks")),
                             ("bedrock-relay-account", "bedrock-xai", account("eu-central-1", proxyPoolId="bedrock-relay")),
                             ("bedrock-missing-account", "bedrock-xai", account("eu-central-1", proxyPoolId="bedrock-missing")),
                             ("bedrock-profile-proxy", "bedrock-xai", {"providerSpecificData": {"profile": "unloaded", "proxyPoolId": "bedrock-socks"}}),
                         ], 1)])
        if CHAT:
            for pool, strict in (("bedrock-bypass", False), ("bedrock-strict-bypass", True)):
                sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                    VALUES (?,1,'active',?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                            (pool, json.dumps({"type": "http", "proxyUrl": HTTP, "strictProxy": strict,
                                               "noProxy": "BEDROCK-RUNTIME.US-WEST-2.AMAZONAWS.COM"})))
            aws = home / ".aws"
            aws.mkdir()
            region, key, secret, session = CHAT_KEYS["bedrock"]
            source, source_secret, source_session = ROLE_SOURCE
            (aws / "credentials").write_text(f"[local-static]\naws_access_key_id={key}\n"
                                             f"aws_secret_access_key={secret}\naws_session_token={session}\n"
                                             f"[role-source]\naws_access_key_id={source}\naws_secret_access_key={source_secret}\n"
                                             f"aws_session_token={source_session}\n")
            (aws / "config").write_text(f"[profile network-profile]\nregion={region}\nsource_profile=role-source\n"
                                         "role_arn=arn:aws:iam::123456789012:role/Fixture\n"
                                         "[profile sso-profile]\nsso_session=fixture\nsso_account_id=123456789012\n"
                                         "sso_role_name=Read\n[sso-session fixture]\nsso_region=us-east-1\n"
                                         "sso_start_url=https://fixture.awsapps.com/start\n")
            sql.execute("UPDATE providerConnections SET isActive=0 WHERE provider IN ('bedrock','bedrock-xai')")
            for provider, (region, key, secret, session) in CHAT_KEYS.items():
                details = {"apiKey": secret, "providerSpecificData": {
                    "region": region, "accessKeyId": key, "sessionToken": session,
                }}
                for priority, lane in ((1, "proxy"), (2, "direct")):
                    fields = json.loads(json.dumps(details))
                    if lane == "proxy":
                        fields["providerSpecificData"].update(
                            {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP} if provider == "bedrock"
                            else {"proxyPoolId": "bedrock-socks"}
                        )
                    sql.execute("""INSERT INTO providerConnections
                        (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                        VALUES (?,?,'apikey','Fixture AWS chat',?,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                                (f"{provider}-chat-{lane}", provider, priority, json.dumps(fields)))
            sql.execute("""INSERT INTO combos(id,name,kind,models,createdAt,updatedAt)
                VALUES ('bedrock-proxy-combo','bedrock-proxy-combo','chat',?,
                        '2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""", (json.dumps([CLAUDE, CLAUDE]),))
    return db


def settings(database, proxy, no_proxy=None):
    with sqlite3.connect(database) as sql:
        sql.execute("""INSERT INTO settings (id,data) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (json.dumps({"outboundProxyEnabled": bool(proxy), "outboundProxyUrl": proxy or "",
                                 "outboundNoProxy": no_proxy}),))


def stored(database, key):
    with sqlite3.connect(database) as sql:
        (raw,), = sql.execute("SELECT data FROM providerConnections WHERE id=?", (key,))
    fields = json.loads(raw)
    return fields["testStatus"], fields.get("lastError")


def request(port, path, body=None, headers=ADMIN):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request("POST" if body is not None else "GET", path, None if body is None else json.dumps(body),
                           headers if body is None else {**headers, "Content-Type": "application/json"})
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait(app, port):
    deadline = time.monotonic() + 10
    while app.poll() is None and time.monotonic() < deadline:
        try:
            if request(port, "/api/health")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.03)
    raise AssertionError("Native Bedrock gateway did not start")


def validate(port, provider, region):
    return request(port, "/api/providers/validate", {"provider": provider, **account(region, connectionProxyEnabled=True,
                                                                           connectionProxyUrl=RELAY)})


def positive(port, database):
    settings(database, HTTP)
    STAGES.extend([("us-west-2", "success"), ("eu-central-1", "role-denied")])
    for provider, region in [("bedrock", "us-west-2"), ("bedrock-xai", "eu-central-1")]:
        assert validate(port, provider, region) == (200, {"valid": True, "error": None})
    settings(database, SOCKS)
    STAGES.extend([("us-west-2", "success"), ("eu-central-1", "role-denied"),
                   ("us-west-2", "signature"), ("eu-central-1", "redirect")])
    assert request(port, "/api/providers/bedrock-http/test", {}) == (200, {"valid": True, "error": None, "refreshed": False})
    assert stored(database, "bedrock-http") == ("active", None)
    assert request(port, "/api/providers/bedrock-xai-socks/test", {}) == (200, {"valid": True, "error": None, "refreshed": False})
    status, batch = request(port, "/api/providers/test-batch", {"mode": "provider", "providerId": "bedrock"})
    assert status == 200 and batch["summary"] == {"total": 1, "passed": 0, "failed": 1}, batch
    (verdict,) = batch["results"]
    failure = verdict["error"]
    assert verdict["connectionId"] == "bedrock-http" and "InvalidSignatureException" in failure and "[redacted]" in failure
    assert all(value not in failure for value in AWS_HOSTS["us-west-2"]) and not re.search(r"Signature=[a-f0-9]{64}", failure)
    assert stored(database, "bedrock-http") == ("error", failure)
    assert request(port, "/api/providers/bedrock-xai-socks/test", {}) == (200, {"valid": False, "error": "fetch failed", "refreshed": False})
    settings(database, None)
    assert request(port, "/api/providers/bedrock-relay-account/test", {}) == (200, {"valid": False,
        "error": "AWS credential probes require an HTTP or SOCKS proxy, or a direct connection", "refreshed": False})
    assert request(port, "/api/providers/bedrock-missing-account/test", {}) == (200, {"valid": False,
        "error": "AWS credential probe proxy is unavailable", "refreshed": False})
    assert request(port, "/api/providers/bedrock-profile-proxy/test", {}) == (200, {"valid": False,
        "error": "Could not resolve the named AWS profile", "refreshed": False})
    assert TUNNELS == [(kind, "localhost", 52987) for kind in ["http", "http", "http", "socks5h", "http", "socks5h"]]
    assert HELLOS == ["localhost"] * 6 and len(CALLS) == 6
    print("Locally trusted TLS carried exact Bedrock SigV4 through HTTP CONNECT and SOCKS5h. Validate, Test and batch passed.")


def actual_target(port, database):
    settings(database, HTTP)
    assert validate(port, "bedrock", "us-west-2") == (200, {"valid": False, "error": "fetch failed"})
    settings(database, SOCKS)
    assert validate(port, "bedrock-xai", "eu-central-1") == (200, {"valid": False, "error": "fetch failed"})
    hosts = [f"bedrock.{region}.amazonaws.com" for region in ("us-west-2", "eu-central-1")]
    assert TUNNELS == [(kind, host, 443) for kind, host in zip(["http", "socks5h"], hosts)]
    assert HELLOS == hosts and CALLS == []
    print("AWS CONNECT authorities and TLS names stayed intact. The localhost certificate was rejected before any signed HTTP request.")


def invoke(port, model, prompt, *, stream=False, messages=False):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=8)) as connection:
        endpoint = "/v1/messages" if messages else "/v1/chat/completions"
        payload = {"model": model, "messages": [{"role": "user", "content": prompt}],
                   "stream": stream, "max_tokens": 90}
        connection.request("POST", endpoint, json.dumps(payload), CHAT_AUTH | {"Content-Type": "application/json"})
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type", "").split(";", 1)[0], response.read().decode()


def chat_config(db, provider, **fields):
    region, key, secret, session = CHAT_KEYS[provider]
    data = {"apiKey": secret, "providerSpecificData": {
        "region": region, "accessKeyId": key, "sessionToken": session, **fields,
    }}
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET data=? WHERE id=?",
                    (json.dumps(data), f"{provider}-chat-proxy"))


def chat_positive(port, database):
    global REJECT_CONNECT
    for model, prompt, expected, messages in (
        (CLAUDE_ARN, "Bedrock Messages JSON fixture", "Bedrock Messages JSON", True),
        (CLAUDE, "Bedrock Chat pivot fixture", "Bedrock Messages JSON", False),
        (GROK, "Bedrock xAI JSON fixture", "Bedrock xAI JSON", False),
    ):
        status, mime, raw = invoke(port, model, prompt, messages=messages)
        assert (status, mime) == (200, "application/json") and expected in raw, raw
    for model, prompt, expected, messages in (
        (CLAUDE, "Bedrock Messages stream fixture", "Bedrock Messages SSE", True),
        (GROK, "Bedrock xAI stream fixture", "Bedrock xAI SSE", False),
    ):
        status, mime, raw = invoke(port, model, prompt, messages=messages, stream=True)
        assert (status, mime) == (200, "text/event-stream") and expected in raw, raw
        assert ("event: message_stop" if messages else "data: [DONE]") in raw
    status, _, raw = invoke(port, CLAUDE, "Bedrock redaction fixture")
    assert status == 422 and "[redacted]" in raw and "Signature=[redacted]" in raw
    assert all(secret not in raw for secret in CHAT_KEYS["bedrock"][1:])
    assert len(CALLS) == 6 and HELLOS == ["localhost"] * 6
    assert [kind for kind, *_ in TUNNELS] == ["http", "http", "socks5h", "http", "socks5h", "http"]

    status, _, raw = invoke(port, "bedrock-proxy-combo", "Bedrock redirect fixture")
    assert status == 502 and "unexpected redirect" in raw and len(CALLS) == 7, (status, raw, CALLS)
    REJECT_CONNECT = True
    status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
    REJECT_CONNECT = False
    assert status == 502 and "Bedrock fetch failed" in raw and len(CALLS) == 7 and len(TUNNELS) == 8

    for fields, expected in (
        ({"proxyPoolId": "bedrock-relay"}, "HTTP or SOCKS tunnel"),
        ({"proxyPoolId": "bedrock-missing"}, "proxy is unavailable"),
        ({"strictProxy": True}, "proxy is unavailable"),
        ({"connectionProxyEnabled": True, "connectionProxyUrl": "ftp://127.0.0.1:52984",
          "connectionNoProxy": ".amazonaws.com"}, "proxy is unavailable"),
        ({"connectionProxyEnabled": True, "connectionProxyUrl": HTTP + "/invalid",
          "connectionNoProxy": ".amazonaws.com"}, "proxy is unavailable"),
        ({"connectionProxyEnabled": True, "connectionNoProxy": ".amazonaws.com"}, "proxy is unavailable"),
        ({"connectionProxyEnabled": True, "connectionProxyUrl": HTTP, "strictProxy": True,
          "connectionNoProxy": ".amazonaws.com"}, "proxy is unavailable"),
        ({"proxyPoolId": "bedrock-strict-bypass"}, "proxy is unavailable"),
    ):
        chat_config(database, "bedrock", **fields)
        status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
        assert status == 503 and expected in raw, (fields, status, raw)
    assert len(CALLS) == 7 and len(TUNNELS) == 8

    chat_config(database, "bedrock", profile="network-profile", proxyPoolId="bedrock-socks")
    status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
    assert status == 200 and "Bedrock Messages JSON" in raw, (status, raw)
    assert len(CALLS) == 9 and CALLS[-2] == ("sts.us-west-2.amazonaws.com", "/")
    assert len(TUNNELS) == 10 and TUNNELS[-2:] == [("socks5h", "localhost", 52987)] * 2

    chat_config(database, "bedrock", profile="sso-profile", proxyPoolId="bedrock-socks")
    status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
    assert status == 502 and "session is missing or invalid" in raw, (status, raw)
    assert len(CALLS) == 9 and len(TUNNELS) == 10

    chat_config(database, "bedrock", profile="local-static", proxyPoolId="bedrock-socks")
    with sqlite3.connect(database) as sql:
        sql.execute("UPDATE providerConnections SET data=json_set(data,'$.apiKey','unselected-synthetic-key') "
                    "WHERE id='bedrock-chat-proxy'")
    status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
    assert status == 200 and "Bedrock Messages JSON" in raw
    assert len(CALLS) == 10 and len(TUNNELS) == 11 and TUNNELS[-1] == ("socks5h", "localhost", 52987)

    chat_config(database, "bedrock")
    settings(database, HTTP)
    status, _, raw = invoke(port, CLAUDE, "Bedrock Chat pivot fixture")
    assert status == 200 and "Bedrock Messages JSON" in raw
    assert len(CALLS) == 11 and len(TUNNELS) == 12 and TUNNELS[-1] == ("http", "localhost", 52987)
    assert not DIRECT
    print("Bedrock Messages and xAI Chat kept SigV4, JSON and EventStream over CONNECT/SOCKS5h; protected failures stayed closed.")
    chat_bypass(port, database)


def chat_bypass(port, database):
    prompt = "Bedrock Chat pivot fixture"
    local = {"connectionProxyEnabled": True, "connectionProxyUrl": HTTP, "connectionNoProxy": ".amazonaws.com"}
    tunnel_count = len(TUNNELS)
    for fields, global_proxy, global_no_proxy in (
        (local, None, None),
        ({"connectionProxyEnabled": True, "connectionProxyUrl": HTTP, "proxyPoolId": "bedrock-bypass"}, None, None),
        ({}, HTTP, "bedrock-runtime.us-west-2.amazonaws.com"),
    ):
        before = len(DIRECT)
        chat_config(database, "bedrock", **fields)
        settings(database, global_proxy, global_no_proxy)
        status, _, raw = invoke(port, CLAUDE, prompt)
        assert status == 200 and "Bedrock Messages JSON" in raw, (status, raw)
        assert len(DIRECT) == before + 1 and len(TUNNELS) == tunnel_count

    chat_config(database, "bedrock", **local)
    settings(database, SOCKS)
    status, _, raw = invoke(port, CLAUDE, prompt)
    assert status == 200 and "Bedrock Messages JSON" in raw, (status, raw)
    assert len(DIRECT) == 3 and len(TUNNELS) == tunnel_count + 1
    assert TUNNELS[-1] == ("socks5h", "localhost", 52987)

    settings(database, "ftp://127.0.0.1:52984", ".amazonaws.com")
    chat_config(database, "bedrock")
    status, _, raw = invoke(port, CLAUDE, prompt)
    assert status == 503 and "proxy is unavailable" in raw, (status, raw)

    settings(database, None)
    chat_config(database, "bedrock", profile="network-profile", **local)
    status, _, raw = invoke(port, CLAUDE, prompt)
    assert status == 200 and "Bedrock Messages JSON" in raw, (status, raw)
    assert len(DIRECT) == 5 and DIRECT[-2] == ("sts.us-west-2.amazonaws.com", "/bedrock-profile/sts/us-west-2")
    assert len(TUNNELS) == tunnel_count + 1
    print(f"{date.today().isoformat()} Bedrock direct NoProxy for valid account/pool/global routes proved by SigV4; SOCKS precedence and closed invalid cases verified.")


def chat_actual_target(port, _database):
    assert invoke(port, CLAUDE, "Bedrock Chat pivot fixture")[0] == 502
    assert invoke(port, GROK, "Bedrock xAI JSON fixture")[0] == 502
    hosts = [f"bedrock-runtime.{region}.amazonaws.com" for region, *_ in CHAT_KEYS.values()]
    assert TUNNELS == [(kind, host, 443) for kind, host in zip(["http", "socks5h"], hosts)]
    assert HELLOS == hosts and not CALLS and not DIRECT
    print("Bedrock Runtime AWS tunnel destinations and SNI matched the signatures; the local-only certificate received no signed payload.")


def exercise(scratch, ca, compare):
    label = "local" if ca else "actual"
    home, data, empty = (scratch / f"{label}-{name}" for name in ("home", "data", "path"))
    home.mkdir()
    empty.mkdir()
    database = seed(data, home)
    port = free_port()
    env = {"PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data), "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(scratch / label)}
    if ca:
        env["NINEROUTER_BEDROCK_LOCAL_PROBE_CA"] = str(ca)
    with (scratch / (label + ".log")).open("w+") as logs:
        app = subprocess.Popen([BINARY, "--no-browser"], cwd=home, env=env, stdout=logs, stderr=logs)
        try:
            wait(app, port)
            compare(port, database)
            assert not FAILURES and not STAGES, (FAILURES, STAGES)
            assert (scratch / label).read_text() == ""
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            print(TUNNELS, HELLOS, CALLS, FAILURES, file=sys.stderr)
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)


with tempfile.TemporaryDirectory(prefix="9router-bedrock-proxy-") as directory:
    scratch = Path(directory)
    context, root = tls_context(scratch)
    aws = ThreadingHTTPServer(("127.0.0.1", 52987), Bedrock)
    aws.socket = context.wrap_socket(aws.socket, server_side=True)
    servers = [aws, Server(("127.0.0.1", 52984), Proxy), Server(("127.0.0.1", 52985), Proxy),
               ThreadingHTTPServer(("127.0.0.1", 52986), Bedrock),
               ThreadingHTTPServer(("127.0.0.1", 52989), Bedrock)]
    for server in servers:
        threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    mock = subprocess.Popen(["node", str(Path(__file__).with_name("chat-upstream.mjs"))], cwd=scratch,
                            env={"PATH": os.environ["PATH"], "HOME": str(scratch)}, stdout=subprocess.PIPE,
                            text=True) if CHAT else None
    try:
        if mock:
            assert select.select([mock.stdout], [], [], 5)[0] and mock.stdout.readline().strip() == "ready"
        exercise(scratch, root, chat_positive if CHAT else positive)
        TUNNELS.clear()
        HELLOS.clear()
        CALLS.clear()
        DIRECT.clear()
        exercise(scratch, None, chat_actual_target if CHAT else actual_target)
    finally:
        if mock:
            mock.terminate()
            mock.wait(timeout=4)
        for server in servers:
            server.shutdown()
            server.server_close()
