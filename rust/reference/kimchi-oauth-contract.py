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
secret = "kimchi-fixture-session"
token = "eyJoffline_kimchi_fixture.e30.synthetic-signature"
first = "/kimchi-oauth/supported-providers"
second = "/kimchi-oauth/me"
ledger = []
failures = []
entered = Event()
release = Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        ledger.append(self.path)
        try:
            assert self.headers.get_all("Authorization") == ["Bearer " + token]
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("Content-Length") in {None, "0"}
            if not reference:
                assert self.headers.get("User-Agent") == "9Router"
            if self.path == first:
                if scenario == "concurrent":
                    entered.set()
                    assert release.wait(5)
                if scenario == "rejected":
                    return self.reply(403, {"error": token})
                if scenario == "redirect":
                    return self.reply(302, {}, {"Location": "http://127.0.0.1:52989/forbidden"})
                if scenario == "disconnect":
                    self.close_connection = True
                    return
                return self.reply(204, {})
            assert self.path == second
            if scenario == "missing-profile":
                return self.reply(503, {"error": token})
            if scenario == "reflect-profile":
                return self.reply(200, {"id": token, "email": token, "username": "echo " + token, "name": token})
            self.reply(200, {"id": 773, "username": "fixture-user", "name": "Fixture Name", "email": "fixture@example.invalid"})
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def reply(self, status, payload, extra=None):
        body = b"" if status == 204 else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)


def reset(name):
    global scenario
    scenario = name
    ledger.clear()
    failures.clear()


def call(path, body=None, headers=None):
    connection = HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(auth if headers is None else headers)
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    try:
        connection.request("GET" if body is None else "POST", path,
                           None if body is None else json.dumps(body), request_headers)
        response = connection.getresponse()
        raw = response.read()
        assert token.encode() not in raw
        if path.startswith(("/api/oauth/kimchi/", "/callback")):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, response.headers, raw
    finally:
        connection.close()


def get_code(redirect=None, headers=None):
    path = "/api/oauth/kimchi/authorize"
    if redirect is not None:
        path += "?" + urlencode({"redirect_uri": redirect})
    status, _, data = call(path, headers=headers)
    return status, json.loads(data)


def authorize(redirect=None, headers=None):
    status, result = get_code(redirect, headers)
    assert status == 200, (status, result)
    assert result["redirectUri"] == (redirect or callback)
    assert result["callbackPath"] == "/callback" and result["flowType"] == "browser_token"
    assert result["state"].startswith("kimchi_") and len(result["codeVerifier"]) == 43
    expected = urlsafe_b64encode(sha256(result["codeVerifier"].encode()).digest()).rstrip(b"=").decode()
    assert result["codeChallenge"] == expected
    assert_target(result["authUrl"], result["redirectUri"], result["state"])
    return result


def assert_target(url, redirect, state):
    url = urlsplit(url)
    assert (url.scheme, url.netloc, url.path) == ("https", "app.kimchi.dev", "/cli-auth")
    assert parse_qs(url.query) == {"callback": [redirect], "state": [state]}


def attempt(result):
    return {key: result[key] for key in ("state", "codeVerifier", "redirectUri")} | {"code": "  " + token + "  "}


def exchange(payload, headers=None):
    status, _, body = call("/api/oauth/kimchi/exchange", payload, headers)
    return status, json.loads(body)


def inspect(id):
    with sqlite3.connect(database) as db:
        provider, kind, active, email, raw = db.execute("SELECT provider,authType,isActive,email,data FROM providerConnections WHERE id=?", (id,)).fetchone()
    fields = json.loads(raw)
    assert (provider, kind, active, fields["testStatus"]) == ("kimchi", "oauth", 1, "active")
    return {key: fields.get(key) for key in ("accessToken", "refreshToken", "displayName", "providerSpecificData")} | {"email": email}


def run_reference():
    given = {"token": "  " + token + "  ", "redirect": "http://127.0.0.1:8080/callback", "state": "offline-proof"}
    output = subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/kimchi-oauth-reference.mjs"), json.dumps(given),
    ], cwd=root, env={"PATH": os.environ["PATH"]})
    result = json.loads(output)
    assert result["flow"] == "browser_token"
    assert_target(result["authorize"], given["redirect"], given["state"])
    return result["result"]


def run_native(onboarded):
    status, result = exchange(attempt(authorize(callback)))
    if not onboarded:
        assert status == 500 and set(result) == {"error"}
        return {"success": False, "error": result["error"]}
    assert status == 200 and set(result) == {"success", "connection"}
    connection = result["connection"]
    assert connection["provider"] == "kimchi" and result["success"]
    fields = inspect(connection["id"])
    assert connection.get("email") == fields["email"] and connection.get("displayName") == fields["displayName"]
    return {"success": True, "fields": fields}


def compare():
    observations = {}
    for name in ("live-profile", "missing-profile", "rejected"):
        reset(name)
        result = run_reference() if reference else run_native(name != "rejected")
        if name == "rejected":
            assert result == {"success": False, "error": "Kimchi token validation failed: 403"}
            assert ledger == [first] and not failures
        else:
            assert result["success"] and ledger == [first, second] and not failures
            fields = result["fields"]
            assert fields.pop("accessToken") == token and fields["refreshToken"] is None
            if name == "live-profile":
                assert fields["displayName"] == "Fixture Name" and fields["email"] == "fixture@example.invalid"
                assert fields["providerSpecificData"] == {"authMethod": "browser_token", "userId": "773", "username": "fixture-user"}
            else:
                assert fields["displayName"] is None and fields["email"] is None
                assert fields["providerSpecificData"] == {"authMethod": "browser_token", "userId": "", "username": ""}
        observations[name] = result
    if not reference:
        assert fallback_log.read_bytes() == b""
    print(json.dumps(observations, sort_keys=True))


def session():
    def encode(value):
        return urlsafe_b64encode(value).rstrip(b"=")
    payload = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signed = encode(b'{"alg":"HS256"}') + b"." + encode(json.dumps(payload).encode())
    signature = encode(hmac.digest(secret.encode(), signed, sha256))
    return {"Cookie": "auth_token=" + (signed + b"." + signature).decode()}


def strict():
    reset("concurrent")
    invalid = (400, {"error": "Invalid or expired OAuth state"})
    assert get_code(headers={}) == (401, {"error": "Unauthorized"})
    assert exchange({}, headers={}) == (401, {"error": "Unauthorized"})
    assert exchange({"code": token, "redirectUri": callback}) == (400, {"error": "Missing required fields"})
    for redirect in ("https://example.invalid/callback", callback + "?extra=1", callback + "#fragment",
                     callback + "/wrong", callback.replace("http:", "https:")):
        assert get_code(redirect) == (400, {"error": "Invalid redirect URI"})
    assert get_code(callback, dict(auth, Origin="https://example.invalid")) == (400, {"error": "Invalid redirect URI"})
    assert authorize() and authorize(callback.replace("http:", "https:"), dict(auth, **{"x-forwarded-proto": "https"}))
    owner = session()
    issued = authorize(callback, owner)
    payload = attempt(issued)
    assert exchange(payload) == invalid
    assert exchange(dict(payload, redirectUri=callback + "/wrong"), owner) == invalid
    assert exchange(dict(payload, state="wrong-state"), owner) == invalid
    assert exchange(dict(payload, codeVerifier="wrong-proof"), owner) == invalid
    assert exchange(dict(payload, code=token + "\nmalformed"), owner) == (400, {"error": "Invalid Kimchi token"})
    assert not ledger
    cb = "/callback?" + urlencode({"token": token, "state": issued["state"]})
    status, headers, html = call(cb, headers=owner)
    assert status == 200 and headers["Referrer-Policy"] == "no-referrer" and "script-src 'self'" in headers["Content-Security-Policy"]
    assert b"/__9router-ui/oauth-callback.js" in html
    status, _, script = call("/__9router-ui/oauth-callback.js")
    assert status == 200
    subprocess.run(["node", str(root / "rust/reference/kimchi-callback-vm.mjs")], input=script,
                   check=True, cwd=root, env={"PATH": os.environ["PATH"]})

    with ThreadPoolExecutor(2) as threads:
        winner = threads.submit(exchange, payload, owner)
        try:
            assert entered.wait(5)
            assert exchange(payload, owner) == invalid
        finally:
            release.set()
        status, result = winner.result(timeout=8)
    assert status == 200 and result["success"]
    assert inspect(result["connection"]["id"])["accessToken"] == token
    assert exchange(payload, owner) == invalid and ledger == [first, second] and not failures

    for name, message in [("redirect", "Kimchi token validation failed: 302"), ("disconnect", "Could not validate the Kimchi token")]:
        reset(name)
        payload = attempt(authorize(callback))
        assert exchange(payload) == (500, {"error": message})
        assert exchange(payload) == invalid and ledger == [first] and not failures

    reset("reflect-profile")
    payload = attempt(authorize(callback))
    payload["token"] = payload.pop("code")
    status, result = exchange(payload)
    assert status == 200 and result["success"]
    seen = inspect(result["connection"]["id"])
    assert seen["providerSpecificData"] == {"authMethod": "browser_token", "userId": "", "username": ""}
    assert seen["email"] is None and seen["displayName"] is None and ledger == [first, second] and not failures
    assert fallback_log.read_bytes() == b"" and token.encode() not in log_path.read_bytes()
    print("Kimchi browser-token flows verified: same-origin state, session/proof binding, single redemption, no redirect or fallback")


def boot(directory, binary):
    global port, callback, database, fallback_log, log_path
    folder = Path(directory)
    home = folder / "home"
    home.mkdir()
    data = folder / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
    fallback_log = folder / "fallback.log"
    log_path = folder / "process.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    callback = f"http://127.0.0.1:{port}/callback"
    with log_path.open("wb") as logs:
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
            "PATH": "", "HOME": str(home), "JWT_SECRET": secret, "PORT": str(port), "HOSTNAME": "127.0.0.1",
            "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data), "NINEROUTER_MIGRATION_FIXTURE": "1",
            "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
        }, stdout=logs, stderr=subprocess.STDOUT)
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


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as provider, tempfile.TemporaryDirectory(prefix="9router-kimchi-oauth-") as directory:
    thread = Thread(target=provider.serve_forever, daemon=True)
    thread.start()
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
        provider.shutdown()
        thread.join()
