from base64 import urlsafe_b64encode
from concurrent.futures import ThreadPoolExecutor
from hashlib import sha256
import hmac
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event, Thread
import time
from urllib.parse import parse_qs, urlencode, urlsplit

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
cookie_secret = "offline-codebuddy-session-key"
raw_state = "offline-codebuddy-state+/=fixture"
access = "offline-codebuddy-fixture-token"
refresh = "offline-codebuddy-fixture-refresh"
auth_url = "https://copilot.tencent.com/cli/auth?" + urlencode({"state": raw_state})
state_path = "/codebuddy-device/cn/v2/plugin/auth/state"
token_path = "/codebuddy-device/cn/v2/plugin/auth/token"
start = "/api/oauth/codebuddy-cn/device-code"
redeem = "/api/oauth/codebuddy-cn/poll"
ledger = []
failures = []
started = Event()
release = Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.serve("POST")

    def do_GET(self):
        self.serve("GET")

    def serve(self, method):
        url = urlsplit(self.path)
        ledger.append((method, url.path))
        try:
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("Authorization") is None
            assert self.headers.get("User-Agent") == ("CLI/2.63.2 CodeBuddy/2.63.2" if reference else "9Router")
            if not reference:
                for name in ("X-Requested-With", "X-Domain", "X-Product", "X-No-Authorization", "X-No-User-Id",
                             "X-No-Enterprise-Id", "X-No-Department-Info", "X-IDE-Name", "X-CodeBuddy-Request"):
                    assert self.headers.get(name) is None
            if url.path == state_path:
                assert method == "POST" and self.headers.get("Content-Type") == "application/json"
                assert parse_qs(url.query) == {"platform": ["CLI"]}
                assert self.rfile.read(int(self.headers["Content-Length"])) == b"{}"
                if outcome == "register-redirect":
                    return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
                link = "https://copilot.tencent.com.example.invalid/cli/auth" if outcome == "bad-url" else auth_url
                return self.reply(200, {"code": 0, "data": {"state": raw_state, "authUrl": link}})
            assert url.path == token_path and method == "GET"
            assert parse_qs(url.query) == {"state": [raw_state]}
            assert self.headers.get("Content-Length") in {None, "0"}
            if outcome == "pending-success" and ledger.count(("GET", token_path)) == 1:
                return self.reply(200, {"code": 11217, "msg": "RetryFetchToken"})
            if outcome == "concurrent":
                started.set()
                assert release.wait(5)
            if outcome == "denied":
                return self.reply(200, {"code": 11345, "msg": "access_denied"})
            if outcome == "fail-status":
                return self.reply(403, {"error": access})
            if outcome == "reflect-error":
                return self.reply(200, {"code": 11345, "msg": access + refresh})
            if outcome == "disconnect":
                self.close_connection = True
                return
            if outcome == "poll-redirect":
                return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
            data = {"accessToken": access, "refreshToken": refresh, "tokenType": "Bearer"}
            if outcome != "concurrent":
                data["expiresIn"] = 3600
            return self.reply(200, {"code": 0, "data": data})
        except AssertionError as error:
            failures.append(str(error) or url.path)
            self.reply(500, {})

    def reply(self, status, body, extra=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)


def reset(scenario):
    global outcome
    outcome = scenario
    ledger.clear()
    failures.clear()


def call(path, body=None, headers=None):
    client = HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(auth if headers is None else headers)
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    try:
        client.request("POST" if body is not None else "GET", path, None if body is None else json.dumps(body), request_headers)
        response = client.getresponse()
        raw = response.read().decode()
        assert access not in raw and refresh not in raw
        if path.startswith("/api/oauth/codebuddy-cn"):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        client.close()


def begin(headers=None):
    status, code = call(start, headers=headers)
    assert status == 200, (status, code)
    proof, handle = code.pop("codeVerifier"), code.pop("device_code")
    assert len(proof) == len(handle) == 43 and proof != handle and handle != raw_state
    assert code.pop("expires_in") == 120
    return code, {"deviceCode": handle, "codeVerifier": proof}


def poll(payload, headers=None):
    status, data = call(redeem, payload, headers)
    assert status == 200
    return data


def stored(id):
    with sqlite3.connect(database) as db:
        provider, kind, active, raw = db.execute("SELECT provider,authType,isActive,data FROM providerConnections WHERE id=?", (id,)).fetchone()
    fields = json.loads(raw)
    assert (provider, kind, active, fields["testStatus"]) == ("codebuddy-cn", "oauth", 1, "active")
    assert fields["expiresAt"].endswith("Z")
    return {key: fields[key] for key in ("accessToken", "refreshToken", "expiresIn", "providerSpecificData")}


def run_reference(polls):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/codebuddy-device-reference.mjs"), json.dumps({"polls": polls}),
    ], cwd=root, env={"PATH": os.environ["PATH"]}))


def run_native(polls):
    code, payload = begin()
    results = []
    for _ in range(polls):
        time.sleep(5.05)
        data = poll(payload)
        if data["success"]:
            assert set(data) == {"success", "connection"} and data["connection"]["provider"] == "codebuddy-cn"
            data = {"success": True, "fields": stored(data["connection"]["id"])}
        results.append(data)
    return {"device": code, "results": results}


def compare():
    reports = {}
    for scenario, polls in [("pending-success", 2), ("denied", 1), ("fail-status", 1)]:
        reset(scenario)
        result = (run_reference if reference else run_native)(polls)
        assert result["device"] == {"verification_uri": auth_url, "interval": 5, "user_code": "", "_isCodeBuddy": True}
        assert not failures and ledger == [("POST", state_path)] + [("GET", token_path)] * polls
        if polls == 2:
            assert result["results"][0] == {"success": False, "error": "authorization_pending", "pending": True}
            fields = result["results"][1]["fields"]
            assert fields.pop("accessToken") == access and fields.pop("refreshToken") == refresh
            assert fields["expiresIn"] == 3600 and fields["providerSpecificData"] == {}
        reports[scenario] = result
    if not reference:
        assert fallback_log.read_bytes() == b""
    print(json.dumps(reports, sort_keys=True))


def session():
    def encode(raw):
        return urlsafe_b64encode(raw).rstrip(b"=")
    payload = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signed = encode(b'{"alg":"HS256"}') + b"." + encode(json.dumps(payload).encode())
    signature = encode(hmac.digest(cookie_secret.encode(), signed, sha256))
    return {"Cookie": "auth_token=" + (signed + b"." + signature).decode()}


def strict():
    reset("concurrent")
    assert call(start, headers={}) == (401, {"error": "Unauthorized"})
    assert call(redeem, {}, headers={}) == (401, {"error": "Unauthorized"})
    assert call(redeem, {}) == (400, {"error": "Missing device code"})
    assert call(redeem, {"deviceCode": "fixture"}) == (400, {"error": "Missing code verifier"})
    assert not ledger
    owner = session()
    _, payload = begin(owner)
    assert poll(payload)["error"] == "expired_token"
    assert poll(dict(payload, codeVerifier="wrong-proof"), owner)["error"] == "expired_token"
    assert poll(dict(payload, deviceCode=raw_state), owner)["error"] == "expired_token"
    assert poll(payload, owner)["error"] == "slow_down" and ledger == [("POST", state_path)]
    time.sleep(5.05)
    with ThreadPoolExecutor(2) as threads:
        future = threads.submit(poll, payload, owner)
        try:
            assert started.wait(5)
            assert poll(payload, owner) == {"success": False, "error": "authorization_pending", "pending": True}
        finally:
            release.set()
        succeeded = future.result(timeout=8)
    assert succeeded["connection"]["provider"] == "codebuddy-cn" and succeeded["success"]
    row = stored(succeeded["connection"]["id"])
    assert row["accessToken"] == access and row["refreshToken"] == refresh and row["expiresIn"] == 86400
    assert poll(payload, owner)["error"] == "expired_token"
    assert ledger == [("POST", state_path), ("GET", token_path)] and not failures

    for name in ("reflect-error", "disconnect", "poll-redirect"):
        reset(name)
        _, payload = begin()
        time.sleep(5.05)
        result = poll(payload)
        assert result == {"success": False, "error": "unknown_error" if name == "reflect-error" else "request_failed", "pending": False}
        assert poll(payload)["error"] == "expired_token"
        assert ledger == [("POST", state_path), ("GET", token_path)] and not failures
    for name in ("bad-url", "register-redirect"):
        reset(name)
        assert call(start) == (500, {"error": "CodeBuddy state request failed"})
        assert ledger == [("POST", state_path)] and not failures
    assert fallback_log.read_bytes() == b""
    assert all(secret.encode() not in output_log.read_bytes() for secret in (raw_state, access, refresh))
    assert call("/api/oauth/codebuddy-intl/device-code")[0] == 503
    assert "/api/oauth/codebuddy-intl/device-code" in fallback_log.read_text()
    print("CodeBuddy China verified: honest identity, single owner/state grant, sanitized host and errors; Intl remains on legacy")


def boot(directory, binary):
    global port, database, fallback_log, output_log
    location = Path(directory)
    home = location / "home"
    home.mkdir()
    data = location / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
    fallback_log = location / "fallback.log"
    output_log = location / "output.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with output_log.open("wb") as out:
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
            "PATH": "", "HOME": str(home), "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
            "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
            "PORT": str(port), "HOSTNAME": "127.0.0.1", "JWT_SECRET": cookie_secret,
        }, stdout=out, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if call("/api/health")[0] == 200:
                return process
        except OSError:
            pass
        time.sleep(.03)
    process.terminate()
    process.wait(timeout=8)
    raise RuntimeError("Native gateway did not start")


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as server, tempfile.TemporaryDirectory(prefix="9router-codebuddy-device-") as directory:
    runner = Thread(target=server.serve_forever, daemon=True)
    runner.start()
    child = None
    try:
        if not reference:
            child = boot(directory, str(Path(sys.argv[2]).resolve()))
        if mode == "strict":
            strict()
        else:
            compare()
    finally:
        if child is not None:
            child.terminate()
            child.wait(timeout=8)
        server.shutdown()
        runner.join()
