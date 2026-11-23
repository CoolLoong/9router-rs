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
from urllib.parse import parse_qs
from uuid import UUID

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
default_client = "17e5f671-d194-4dfb-9706-5516cb48c098"
registered_client = "offline-user-registered-oauth-client"
live_client = registered_client if mode == "strict" else default_client
version = json.loads((root / "package.json").read_text())["version"]
grant = "urn:ietf:params:oauth:grant-type:device_code"
secret = "kimi-fixture-session-secret"
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
device_code = "offline-kimi-issuer-device-code"
token = "offline-kimi-bearer"
refresh = "offline-kimi-refresh-grant"
new_token = "offline-kimi-rotated-bearer"
new_refresh = "offline-kimi-rotated-grant"
ledger = []
failures = []
registrations = set()
started = Event()
proceed = Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            form = parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
            device = self.headers["X-Msh-Device-Id"]
            assert UUID(device).version == 4
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
            assert self.headers.get("Authorization") is None
            assert self.headers.get("X-Msh-Platform") == "9router"
            assert self.headers.get("X-Msh-Version") == version
            assert self.headers.get("X-Msh-Device-Model")
            if not reference:
                assert self.headers.get("User-Agent") == "9Router/" + version
                assert self.headers.get("X-Msh-Device-Name") == "9Router"
            if self.path == "/kimi-device/device_authorization":
                ledger.append(("register", device))
                assert form == {"client_id": [live_client]}
                registrations.add(device)
                data = {"device_code": device_code, "user_code": "KIMI-ABCD", "expires_in": 60, "interval": 1}
                if scenario == "untrusted-verification":
                    data.update(verification_uri="https://example.invalid/consent", verification_uri_complete="https://www.kimi.com.example.invalid/login")
                return self.reply(200, data)
            assert self.path == "/kimi-device/token"
            if form.get("grant_type") == ["refresh_token"]:
                return self.rotate(form, device)
            self.poll(form, device)
        except (AssertionError, TypeError, ValueError) as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def poll(self, form, device):
        ledger.append(("poll", device))
        assert device in registrations
        assert form == {"client_id": [live_client], "grant_type": [grant], "device_code": [device_code]}
        count = sum(operation == "poll" for operation, _ in ledger)
        if count == 1 and scenario in {"pending-success", "slow-success"}:
            return self.reply(200, {"error": "authorization_pending" if scenario == "pending-success" else "slow_down"})
        if scenario == "concurrent-poll":
            started.set()
            assert proceed.wait(5)
        if scenario == "poll-redirect":
            return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
        if scenario == "poll-disconnect":
            self.close_connection = True
            return
        if scenario == "denied":
            return self.reply(400, {"error": "access_denied", "error_description": device_code + token})
        self.reply(200, {"access_token": token, "refresh_token": refresh, "expires_in": 3600, "token_type": "Bearer"})

    def rotate(self, form, device):
        ledger.append(("refresh", device))
        assert device == expected_device
        assert form == {"grant_type": ["refresh_token"], "client_id": [live_client], "refresh_token": [refresh]}
        if not reference:
            assert stored()["kimiRefreshAttempt"] and stored()["refreshToken"] == refresh
        if scenario == "refresh-redirect":
            return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
        if scenario == "refresh-disconnect":
            self.close_connection = True
            return
        if scenario == "refresh-throttle" and len(ledger) == 1:
            return self.reply(429, {})
        if scenario == "refresh-concurrent":
            time.sleep(.15)
        self.reply(200, {"access_token": new_token, "refresh_token": new_refresh, "expires_in": 7200})

    def reply(self, status, body, extra=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)


def reset(name):
    global scenario
    scenario = name
    ledger.clear()
    failures.clear()
    registrations.clear()


def call(path, payload=None, headers=None):
    client = HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(cli if headers is None else headers)
    if payload is not None:
        request_headers["Content-Type"] = "application/json"
    try:
        client.request("POST" if payload is not None else "GET", path, None if payload is None else json.dumps(payload), request_headers)
        response = client.getresponse()
        raw = response.read().decode()
        assert all(secret not in raw for secret in (device_code, token, refresh, new_token, new_refresh))
        if path.startswith("/api/oauth/kimi"):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        client.close()


def begin(alias="kimi", headers=None):
    status, code = call(f"/api/oauth/{alias}/device-code", headers=headers)
    assert status == 200, (status, code)
    assert UUID(code["_kimiDeviceId"]).version == 4
    assert len(code["device_code"]) == len(code["codeVerifier"]) == 43
    assert set(registrations) == {code["_kimiDeviceId"]}
    return code, {"deviceCode": code["device_code"], "extraData": {"_kimiDeviceId": "bad-caller-device"}}


def poll(alias, payload, headers=None):
    status, body = call(f"/api/oauth/{alias}/poll", payload, headers)
    assert status == 200
    return body


def stored():
    with sqlite3.connect(database) as db:
        provider, auth_type, payload = db.execute("SELECT provider,authType,data FROM providerConnections WHERE id=?", (connection,)).fetchone()
        assert (provider, auth_type) == ("kimi", "oauth")
    return json.loads(payload)


def expire(grant=refresh):
    with sqlite3.connect(database) as db:
        db.execute("""UPDATE providerConnections SET data=json_set(data,'$.expiresAt','2000-01-01T00:00:00Z',
            '$.refreshToken',?,'$.kimiRefreshAttempt',NULL) WHERE id=?""", (grant, connection))


def test():
    status, body = call(f"/api/providers/{connection}/test", {})
    assert status == 200, (status, body)
    return body


def run_reference(fields):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/kimi-device-reference.mjs"), json.dumps(fields),
    ], cwd=root, env={"PATH": os.environ["PATH"]}))


def compare():
    global connection, expected_device
    reset("pending-success")
    if reference:
        recorded = run_reference({"polls": 2})
        code, polls = recorded["code"], recorded["polls"]
        fields = polls[1]["fields"]
    else:
        code, payload = begin("kimi-coding")
        time.sleep(1.05)
        pending = poll("kimi", payload)
        time.sleep(1.05)
        saved = poll("kimi-coding", payload)
        assert set(saved) == {"connection", "success"} and saved["connection"]["provider"] == "kimi"
        connection = saved["connection"]["id"]
        fields = stored()
        polls = [pending, saved]
    expected_device = code["_kimiDeviceId"]
    assert set(registrations) == {expected_device}
    assert code["verification_uri"] == "https://www.kimi.com/code/authorize_device"
    assert code["verification_uri_complete"] == code["verification_uri"] + "?user_code=KIMI-ABCD"
    assert code["interval"] == 1 and code["expires_in"] == 60
    assert polls[0] == {"success": False, "error": "authorization_pending", "pending": True} and polls[1]["success"]
    assert fields["providerSpecificData"] == {"authMethod": "device_code", "deviceId": expected_device}
    assert (fields["accessToken"], fields["refreshToken"], fields["expiresIn"]) == (token, refresh, 3600)
    assert ledger == [(item, expected_device) for item in ("register", "poll", "poll")] and not failures

    reset("refresh-success")
    if reference:
        rotated = run_reference({"refresh": refresh, "account": fields})
    else:
        expire()
        assert test() == {"valid": True, "error": None, "refreshed": True}
        rotated = stored()
        assert rotated["providerSpecificData"]["deviceId"] == expected_device and not rotated["kimiRefreshAttempt"]
        assert fallback_log.read_bytes() == b""
    assert (rotated["accessToken"], rotated["refreshToken"], rotated["expiresIn"]) == (new_token, new_refresh, 7200)
    assert ledger == [("refresh", expected_device)] and not failures
    print(json.dumps({"deviceAccepted": True, "pending": True, "savedDevicePreserved": True, "expiry": 3600,
                      "refreshAccepted": True, "renewsFor": 7200}, sort_keys=True))


def owner_session():
    def encode(raw):
        return urlsafe_b64encode(raw).rstrip(b"=")
    claims = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signed = encode(b'{"alg":"HS256"}') + b"." + encode(json.dumps(claims).encode())
    sig = encode(hmac.digest(secret.encode(), signed, sha256))
    return {"Cookie": "auth_token=" + (signed + b"." + sig).decode()}


def strict(original_process):
    global connection, expected_device
    reset("concurrent-poll")
    assert call("/api/oauth/kimi/device-code", headers={}) == (401, {"error": "Unauthorized"})
    assert call("/api/oauth/kimi/poll", {}, headers={}) == (401, {"error": "Unauthorized"})
    assert call("/api/oauth/kimi/poll", {}) == (400, {"error": "Missing device code"})
    assert not ledger
    owner = owner_session()
    code, payload = begin("kimi-coding", owner)
    expected_device = code["_kimiDeviceId"]
    with sqlite3.connect(database) as db:
        assert db.execute("""SELECT count(*) FROM providerConnections
            WHERE provider='kimi' AND json_extract(data,'$.providerSpecificData.deviceId')=?""",
            (expected_device,)).fetchone() == (0,)
    assert poll("kimi", payload)["error"] == "expired_token"
    assert poll("kimi", {"deviceCode": device_code}, owner)["error"] == "expired_token"
    assert poll("kimi", payload, owner)["error"] == "slow_down"
    assert ledger == [("register", expected_device)]
    time.sleep(1.05)
    with ThreadPoolExecutor(2) as pool:
        winner = pool.submit(poll, "kimi", payload, owner)
        try:
            assert started.wait(5)
            assert poll("kimi-coding", payload, owner)["error"] == "authorization_pending"
        finally:
            proceed.set()
        saved = winner.result(timeout=8)
    connection = saved["connection"]["id"]
    assert saved["connection"]["provider"] == "kimi" and poll("kimi", payload, owner)["error"] == "expired_token"
    assert stored()["providerSpecificData"] == {"authMethod": "device_code", "deviceId": expected_device,
                                               "deviceClientId": registered_client}
    assert ledger == [("register", expected_device), ("poll", expected_device)] and not failures

    reset("slow-success")
    code, payload = begin()
    assert code["_kimiDeviceId"] != expected_device
    time.sleep(1.05)
    assert poll("kimi", payload)["error"] == "slow_down"
    time.sleep(1.05)
    assert poll("kimi", payload)["error"] == "slow_down" and len(ledger) == 2
    time.sleep(5.05)
    assert poll("kimi", payload)["success"] and ledger == [(op, code["_kimiDeviceId"]) for op in ("register", "poll", "poll")]

    for name in ("denied", "poll-redirect", "poll-disconnect"):
        reset(name)
        code, payload = begin()
        time.sleep(1.05)
        assert poll("kimi", payload)["error"] == ("access_denied" if name == "denied" else "exchange_failed")
        assert poll("kimi", payload)["error"] == "expired_token"
        assert ledger == [(op, code["_kimiDeviceId"]) for op in ("register", "poll")] and not failures
    reset("untrusted-verification")
    code, _ = begin()
    assert code["verification_uri"] == "https://www.kimi.com/code/authorize_device"
    assert code["verification_uri_complete"].startswith(code["verification_uri"] + "?")
    assert fallback_log.read_bytes() == b""
    stop(original_process)
    replacement = boot(None)
    failed = {"valid": False, "error": "Token expired and refresh failed", "refreshed": False}
    try:
        reset("refresh-concurrent")
        expire()
        with ThreadPoolExecutor(2) as pool:
            replies = list(pool.map(lambda _: test(), range(2)))
        assert all(reply["valid"] for reply in replies) and any(reply["refreshed"] for reply in replies)
        assert ledger == [("refresh", expected_device)] and not failures
        row = stored()
        assert row["accessToken"] == new_token and row["refreshToken"] == new_refresh and row["testStatus"] == "active"
        assert row["providerSpecificData"]["deviceId"] == expected_device and row["providerSpecificData"]["deviceClientId"] == registered_client

        reset("refresh-throttle")
        expire()
        assert test() == failed and not stored()["kimiRefreshAttempt"]
        assert test() == {"valid": True, "error": None, "refreshed": True}
        assert ledger == [("refresh", expected_device)] * 2 and not failures

        for name in ("refresh-redirect", "refresh-disconnect"):
            reset(name)
            expire()
            assert test() == failed and stored()["kimiRefreshAttempt"]
            assert test() == failed and ledger == [("refresh", expected_device)] and not failures
        assert fallback_log.read_bytes() == b""
    finally:
        stop(replacement)
    restarted = boot(None)
    try:
        assert test() == failed and ledger == [("refresh", expected_device)] and not failures
        status, accounts = call("/api/providers")
        assert status == 200 and "kimiRefreshAttempt" not in json.dumps(accounts)
        assert fallback_log.read_bytes() == b""
    finally:
        stop(restarted)
    print("Kimi device and refresh verified: owner/alias binding, stable device and client ID, pacing, one-use rotation, zero fallbacks")


def boot(client_id):
    settings = dict(environment)
    if client_id is not None:
        settings["KIMI_CODING_OAUTH_CLIENT_ID"] = client_id
    process = subprocess.Popen([binary, "--no-browser"], cwd=root, env=settings,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if call("/api/health")[0] == 200:
                return process
        except OSError:
            pass
        time.sleep(.03)
    stop(process)
    raise RuntimeError("Native gateway did not start")


def stop(process):
    if process.poll() is None:
        process.terminate()
        process.wait(timeout=8)


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as server, tempfile.TemporaryDirectory(prefix="9router-kimi-device-") as directory:
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    child = None
    try:
        if not reference:
            binary = str(Path(sys.argv[2]).resolve())
            home = Path(directory) / "home"
            home.mkdir()
            data = Path(directory) / "data"
            subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
            database = data / "db/data.sqlite"
            fallback_log = Path(directory) / "fallback.log"
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            environment = {"PATH": "", "HOME": str(home), "JWT_SECRET": secret, "DATA_DIR": str(data),
                           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
                           "NINEROUTER_MIGRATION_DATA_DIR": str(data), "PORT": str(port), "HOSTNAME": "127.0.0.1"}
            child = boot(registered_client if mode == "strict" else None)
        if mode == "strict":
            strict(child)
        else:
            compare()
    finally:
        if child is not None:
            stop(child)
        server.shutdown()
        thread.join()
