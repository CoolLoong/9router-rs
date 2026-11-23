import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import select
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGUMENTS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGUMENTS.pop(0)).resolve())
CASE, = ARGUMENTS
CLIENT = "b1a00492-073a-47ea-816f-4c329264a828"
REDIRECT = "http://127.0.0.1:56121/callback"
SCOPE = "openid profile email offline_access grok-cli:access api:access"
EMAIL = "oauth-xai@example.invalid"
MODEL = "grok-4.7"
KEY = "fixture-xai-key"
ACCESS = "fixture-xai-access"
ROTATED = "fixture-xai-next-access"
REFRESH = "fixture-xai-refresh"
NEXT_REFRESH = "fixture-xai-next-refresh"
CHAT_AUTH = {"authorization": "Bearer offline-chat-key"}
ADMIN_AUTH = {"x-9r-cli-token": "68977b6fd9422fbb"}
FAILURES = []
WIRE = []
EXPECTED = []


def base64url(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def identity(payload):
    return f"fixture.{base64url(json.dumps(payload).encode())}.signature"


FIRST_ID = identity({"preferred_username": EMAIL, "sub": "unused-id"})
NEXT_ID = identity({"email": EMAIL, "sub": "next-id"})


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, fields):
        raw = json.dumps(fields).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        try:
            assert self.path == "/xai-oauth/.well-known/openid-configuration"
            assert self.headers.get("Accept") == "application/json"
            assert not self.headers.get("Authorization")
            WIRE.append({"path": self.path, "method": "GET"})
            self.reply(200, {
                "authorization_endpoint": "https://fixture.x.ai/alternate/authorize",
                "token_endpoint": "https://x.ai.example.invalid/alternate/token" if CASE == "fallback" else
                                  "https://fixture.x.ai/alternate/token",
            })
        except AssertionError as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            assert EXPECTED, f"unexpected request on {self.path}"
            stage, expected = EXPECTED.pop(0)
            assert not self.headers.get("x-api-key")
            assert not self.headers.get("user-agent", "").startswith("grok-cli/")
            if stage == "exchange" or stage == "refresh" or stage == "token-reject":
                assert self.path == ("/xai-oauth/oauth2/token" if CASE == "fallback" else "/xai-oauth/alternate/token")
                assert self.headers.get("Accept") == "application/json"
                assert self.headers.get_content_type() == "application/x-www-form-urlencoded"
                assert not self.headers.get("Authorization")
                body = parse_qs(raw.decode(), strict_parsing=True)
                assert body == {k: [v] for k, v in expected.items()}, (stage, list(body))
                WIRE.append({"path": self.path, "method": "POST", "form": list(body)})
                if stage == "token-reject":
                    return self.reply(403, {"error": raw.decode()})
                self.reply(200, {
                    "access_token": ROTATED if stage == "refresh" else ACCESS,
                    "refresh_token": NEXT_REFRESH if stage == "refresh" else REFRESH,
                    "id_token": NEXT_ID if stage == "refresh" else FIRST_ID,
                    "expires_in": 3600, "scope": SCOPE,
                })
                return
            assert self.path == "/static-chat/xai"
            assert self.headers.get("Authorization") == "Bearer " + expected
            body = json.loads(raw)
            assert body["model"] == MODEL and body["stream"] is False, body
            assert isinstance(body["messages"], list) and "input" not in body, body
            WIRE.append({"path": self.path, "method": "POST", "bearer": expected, "body": body})
            if stage == "denied":
                self.reply(401, {"error": {"message": "Fixture access denied"}})
                return
            self.reply(200, {
                "id": "chatcmpl-xai-fixture", "object": "chat.completion", "created": 1795000000, "model": MODEL,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "Fixture xAI reply"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
            })
        except (AssertionError, KeyError, ValueError) as error:
            FAILURES.append(str(error) or self.path)
            self.reply(422, {"error": {"message": "Mock xAI wire mismatch"}})


def request(port, path, body=None, headers=ADMIN_AUTH):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request("GET" if body is None else "POST", path,
                           None if body is None else json.dumps(body),
                           headers if body is None else {**headers, "content-type": "application/json"})
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def callback(payload, path="/callback", extra=None, headers=None):
    query = {"state": payload["state"], "code": payload["code"]} if extra is None else extra
    connection = http.client.HTTPConnection("127.0.0.1", 56121, timeout=12)
    try:
        connection.request("GET", path + "?" + urlencode(query), headers=headers or {})
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type"), response.read().decode()
    finally:
        connection.close()


def authorize(port):
    status, body = request(port, "/api/oauth/xai/authorize?" + urlencode({"redirect_uri": REDIRECT}))
    assert status == 200, body
    assert set(body) == {"authUrl", "state", "codeVerifier", "codeChallenge", "redirectUri", "flowType", "fixedPort", "callbackPath"}
    assert body["flowType"] == "authorization_code_pkce" and body["redirectUri"] == REDIRECT
    assert body["fixedPort"] == 56121 and body["callbackPath"] == "/callback"
    assert re.fullmatch(r"[-_a-zA-Z0-9]{128}", body["codeVerifier"])
    assert re.fullmatch(r"[-_a-zA-Z0-9]{43}", body["state"])
    assert body["codeChallenge"] == base64url(hashlib.sha256(body["codeVerifier"].encode()).digest())
    url = urlsplit(body["authUrl"])
    target = ("auth.x.ai", "/oauth2/authorize") if CASE == "fallback" else ("fixture.x.ai", "/alternate/authorize")
    assert (url.scheme, url.hostname, url.path) == ("https", *target), url
    query = parse_qs(url.query, strict_parsing=True)
    assert "%20" in url.query and "+" not in url.query
    assert re.fullmatch(r"[0-9a-f]{32}", query["nonce"][0])
    expected = {
        "response_type": "code", "client_id": CLIENT, "redirect_uri": REDIRECT,
        "scope": SCOPE, "code_challenge": body["codeChallenge"], "code_challenge_method": "S256",
        "state": body["state"], "nonce": query["nonce"][0], "plan": "generic", "referrer": "cli-proxy-api",
    }
    assert query == {k: [v] for k, v in expected.items()}
    payload = {name: body[name] for name in ("redirectUri", "codeVerifier", "state")}
    payload["code"] = "fixture-xai authorization + code"
    return payload, {"origin": url.netloc, "path": url.path, **{k: v for k, v in expected.items()
                     if k not in {"code_challenge", "state", "nonce"}}}


def expected_exchange(payload, stage="exchange"):
    EXPECTED.append((stage, {
        "grant_type": "authorization_code", "client_id": CLIENT, "code": payload["code"],
        "redirect_uri": payload["redirectUri"], "code_verifier": payload["codeVerifier"],
    }))


def exchange(port, payload, stage="exchange"):
    expected_exchange(payload, stage)
    return request(port, "/api/oauth/xai/exchange", payload)


def proxy(port, action, **query):
    return request(port, f"/api/oauth/xai/{action}?" + urlencode(query))


def register_callback(port, payload):
    return proxy(port, "start-proxy", app_port=port, state=payload["state"], code_verifier=payload["codeVerifier"],
                 redirect_uri=payload["redirectUri"])


def completed_callback(port, database, payload, manually=False):
    assert register_callback(port, payload) == (200, {"success": True, "serverSide": True})
    assert proxy(port, "poll-status", state=payload["state"]) == (200, {"status": "pending"})
    expected_exchange(payload)
    if manually:
        status, result = request(port, "/api/oauth/xai/manual-code", {"state": payload["state"], "code": payload["code"]})
        assert status == 200 and result["success"] is True, (status, result, FAILURES)
        connection = result["connection"]
        assert connection["provider"] == "xai" and connection["email"] == EMAIL
        account_id = connection["id"]
    else:
        status, content_type, response = callback(payload)
        assert status == 200 and content_type.startswith("text/html") and "You can close this window." in response, (status, response, FAILURES)
        status, result = proxy(port, "poll-status", state=payload["state"])
        assert status == 200 and result["status"] == "done" and result["email"] == EMAIL, result
        if not REFERENCE:
            assert set(result) == {"status", "connectionId", "email"}, result
        account_id = result["connectionId"]
    assert proxy(port, "poll-status", state=payload["state"]) == (200, {"status": "unknown"})
    assert credentials(database, account_id)["accessToken"] == ACCESS


def credentials(database, id_):
    with sqlite3.connect(database) as sql:
        (row,), = sql.execute("SELECT data FROM providerConnections WHERE id=? AND provider='xai' AND authType='oauth'", (id_,))
    return json.loads(row)


def simulate_expiry(database, id_):
    fields = credentials(database, id_)
    fields["expiresAt"] = "2000-01-01T00:00:00.000Z"
    with sqlite3.connect(database) as sql:
        sql.execute("UPDATE providerConnections SET data=? WHERE id=?", (json.dumps(fields), id_))


def generate(port, client, token, stage="chat"):
    EXPECTED.append((stage, token))
    path = "/v1/chat/completions" if client == "chat" else "/v1/responses"
    fields = {"model": "xai/" + MODEL, "stream": False}
    if client == "chat":
        fields["messages"] = [{"role": "user", "content": "Fixture Chat"}]
    else:
        fields.update(input="Fixture Responses", instructions="Synthetic instructions", max_output_tokens=84)
    status, result = request(port, path, fields, CHAT_AUTH)
    if stage == "denied":
        assert status == 503 and "[401]" in str(result) and "Fixture access denied" in str(result), (status, result)
        return
    assert status == 200, (status, result, WIRE, FAILURES)
    if client == "chat":
        assert result["object"] == "chat.completion" and result["choices"][0]["message"]["content"] == "Fixture xAI reply", result
        return {"object": result["object"], "usage": result["usage"]}
    assert result["object"] == "response" and result["output"][0]["content"][0]["text"] == "Fixture xAI reply", result
    return {"object": result["object"], "status": result["status"], "usage": result["usage"]}


def compare(port, database):
    if CASE == "key":
        return {"chat": generate(port, "chat", KEY), "responses": generate(port, "responses", KEY), "wire": WIRE}
    payload, authorization = authorize(port)
    if CASE in {"callback", "manual"}:
        completed_callback(port, database, payload, manually=CASE == "manual")
        return {"authorization": authorization, "completed": True, "wire": WIRE}
    status, result = exchange(port, payload)
    assert status == 200 and result["success"] is True, (status, result, FAILURES)
    account = result["connection"]
    assert account["provider"] == "xai" and account["email"] == EMAIL
    account_id = account["id"]
    fields = credentials(database, account_id)
    first = {name: fields[name] for name in ("accessToken", "refreshToken", "scope", "testStatus", "providerSpecificData")}
    assert first == {"accessToken": ACCESS, "refreshToken": REFRESH, "scope": SCOPE,
                     "testStatus": "active", "providerSpecificData": {"idToken": FIRST_ID}}
    if CASE == "fallback":
        return {"authorization": authorization, "saved": first, "wire": WIRE}
    if CASE == "reject":
        generate(port, "chat", ACCESS, stage="denied")
        return {"wire": WIRE}
    simulate_expiry(database, account_id)
    EXPECTED.append(("refresh", {"grant_type": "refresh_token", "client_id": CLIENT, "refresh_token": REFRESH}))
    response = generate(port, "responses", ROTATED)
    fields = credentials(database, account_id)
    updated = {name: fields[name] for name in ("accessToken", "refreshToken", "idToken", "scope", "providerSpecificData")}
    assert updated == {"accessToken": ROTATED, "refreshToken": NEXT_REFRESH, "idToken": NEXT_ID,
                       "scope": SCOPE, "providerSpecificData": {"idToken": FIRST_ID}}
    return {"authorization": authorization, "saved": first, "rotated": updated, "responses": response, "wire": WIRE}


def strict(port, database):
    assert request(port, "/api/oauth/xai/authorize", headers={})[0] == 401
    assert request(port, "/api/oauth/xai/authorize?redirect_uri=file%3A%2F%2F%2Ftmp")[0] == 400
    assert WIRE == []
    payload, _ = authorize(port)
    for key, value in [("state", "unknown"), ("codeVerifier", "altered"), ("redirectUri", "http://127.0.0.1:1234/callback")]:
        assert request(port, "/api/oauth/xai/exchange", {**payload, key: value}) == (
            400, {"error": "Invalid or expired OAuth state"})
    status, rejected = exchange(port, payload, "token-reject")
    assert status == 500 and rejected == {"error": "xAI token exchange failed"}, rejected
    assert request(port, "/api/oauth/xai/exchange", payload)[0] == 400
    payload, _ = authorize(port)
    status, completed = exchange(port, payload)
    assert status == 200 and completed["success"] and request(port, "/api/oauth/xai/exchange", payload)[0] == 400
    assert [entry["method"] for entry in WIRE] == ["GET", "POST", "POST"], WIRE
    account_id = completed["connection"]["id"]
    simulate_expiry(database, account_id)
    EXPECTED.append(("refresh", {"grant_type": "refresh_token", "client_id": CLIENT, "refresh_token": REFRESH}))
    with ThreadPoolExecutor(2) as clients:
        turns = [clients.submit(generate, port, "responses", ROTATED) for _ in range(2)]
        assert all(turn.result()["status"] == "completed" for turn in turns)
    assert [entry["method"] for entry in WIRE] == ["GET", "POST", "POST", "GET", "POST", "POST", "POST"], WIRE
    assert credentials(database, account_id)["refreshToken"] == NEXT_REFRESH
    payload, _ = authorize(port)
    assert register_callback(port, {**payload, "codeVerifier": "wrong"})[0] == 400
    with socket.socket() as busy:
        busy.bind(("127.0.0.1", 56121))
        busy.listen()
        assert register_callback(port, payload) == (200, {"success": False, "serverSide": False, "reason": "port_busy"})
    assert register_callback(port, payload) == (200, {"success": True, "serverSide": True})
    assert callback(payload, headers={"Host": "example.invalid:56121"})[0] == 403
    assert callback(payload, headers={"Origin": "https://example.invalid"})[0] == 403
    assert callback(payload, path="/auth/callback")[0] == 400
    assert callback(payload, extra={"state": "unknown", "code": "fixture-xai authorization + code"})[0] == 400
    assert proxy(port, "poll-status", state=payload["state"]) == (200, {"status": "pending"})
    assert request(port, "/api/oauth/xai/manual-code", {"state": payload["state"], "code": payload["code"]}, {})[0] == 401
    assert proxy(port, "stop-proxy", state=payload["state"]) == (200, {"success": True})
    assert proxy(port, "poll-status", state=payload["state"]) == (200, {"status": "unknown"})
    assert request(port, "/api/oauth/xai/manual-code", {"state": payload["state"], "code": payload["code"]})[0] == 400
    deadline = time.monotonic() + 2
    while True:
        try:
            callback(payload)
        except ConnectionRefusedError:
            break
        assert time.monotonic() < deadline, "xAI listener remained open after stopping the last session"
        time.sleep(.025)
    print("xAI rejected invalid and replayed PKCE data, redacted token errors, and sent one refresh for concurrent turns")
    print("xAI loopback rejected untrusted origins, wrong paths and inactive state, and closed after cancellation")


def wait(app, port):
    deadline = time.monotonic() + 12
    while app.poll() is None and time.monotonic() < deadline:
        if REFERENCE and select.select([app.stdout], [], [], .25)[0]:
            line = app.stdout.readline().decode().strip()
            if line.startswith("ready "):
                return int(line.split()[1])
        if not REFERENCE:
            try:
                if request(port, "/api/health")[0] == 200:
                    return port
            except OSError:
                time.sleep(.025)
    raise RuntimeError("xAI fixture process did not start")


with tempfile.TemporaryDirectory(prefix="9router-xai-") as directory:
    scratch = Path(directory)
    home, data, empty = (scratch / name for name in ("home", "data", "empty-path"))
    home.mkdir()
    empty.mkdir()
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, cwd=ROOT,
                   env={**os.environ, "NODE_NO_WARNINGS": "1"})
    database = data / "db/data.sqlite"
    if CASE == "key":
        with sqlite3.connect(database) as sql:
            sql.execute("""INSERT INTO providerConnections (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
                        VALUES ('fixture-xai-key','xai','apikey','Fixture xAI key',1,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
                        (json.dumps({"apiKey": KEY, "testStatus": "active"}),))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": os.environ["PATH"] if REFERENCE else str(empty), "HOME": str(home), "DATA_DIR": str(data),
           "PORT": str(port), "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallbacks")}
    mock = ThreadingHTTPServer(("127.0.0.1", 52989), Upstream)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    command = ["node", "--no-warnings", str(Path(__file__).with_name("original.mjs")), str(data)] if REFERENCE else [BINARY, "--no-browser"]
    with (scratch / "app.log").open("w+") as logs:
        app = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=logs)
        try:
            port = wait(app, port)
            if CASE == "strict":
                strict(port, database)
            else:
                print(json.dumps(compare(port, database), sort_keys=True))
            assert not FAILURES and not EXPECTED, (FAILURES, EXPECTED)
            if not REFERENCE:
                assert (scratch / "fallbacks").read_bytes() == b""
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            raise
        finally:
            app.terminate()
            try:
                app.wait(timeout=5)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait(timeout=3)
            mock.shutdown()
            mock.server_close()
