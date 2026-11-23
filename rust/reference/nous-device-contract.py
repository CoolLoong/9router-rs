from base64 import urlsafe_b64encode
from concurrent.futures import ThreadPoolExecutor
from hashlib import sha256
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event, Thread
import time
from urllib.parse import parse_qs

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
secret = "nous-fixture-session-key"
scope = "inference:invoke inference:mint_agent_key"
grant = "urn:ietf:params:oauth:grant-type:device_code"
access = "offline-nous-test-access"
refresh = "offline-nous-test-renewal"
agent = "offline-nous-test-agent-key"
code = "offline-nous-test-device-code"
data = {"access_token": access, "refresh_token": refresh, "expires_in": 3600, "scope": scope}
mint = {"api_key": agent, "key_id": "fixture-agent-id", "expires_at": "2035-01-01T00:00:00Z", "expires_in": 7200}
device = {"user_code": "TESX-YVTA", "verification_uri": "https://portal.nousresearch.com/device",
          "verification_uri_complete": "https://portal.nousresearch.com/device?user_code=TESX-YVTA",
          "expires_in": 60, "interval": 1}
events = []
failures = []
token_started = Event()
token_release = Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        events.append(self.path)
        try:
            assert self.headers.get("Accept") == "application/json"
            if self.path == "/nous-device/device/code":
                assert self.headers.get("Authorization") is None
                assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
                assert parse_qs(raw.decode()) == {"client_id": ["hermes-cli"], "scope": [scope]}
                if scenario == "bad-device":
                    return self.reply(400, {"error": code})
                if scenario == "bad-verification":
                    return self.reply(200, dict(device, device_code=code, verification_uri="https://example.invalid/device"))
                return self.reply(200, dict(device, device_code=code))
            if self.path == "/nous-device/token":
                assert self.headers.get("Authorization") is None
                assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
                assert parse_qs(raw.decode()) == {"grant_type": [grant], "client_id": ["hermes-cli"], "device_code": [code]}
                number = events.count(self.path)
                if scenario in {"pending-success", "slow-success"} and number == 1:
                    return self.reply(400, {"error": "authorization_pending" if scenario == "pending-success" else "slow_down"})
                if scenario == "denied":
                    return self.reply(403, {"error": "access_denied", "error_description": code})
                if scenario == "concurrent":
                    token_started.set()
                    assert token_release.wait(4)
                if scenario == "disconnect":
                    self.close_connection = True
                    return
                if scenario == "redirect-token":
                    return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/must-not-receive"})
                return self.reply(200, data)
            if self.path == "/nous-device/agent-key":
                assert self.headers.get("Authorization") == "Bearer " + access
                assert self.headers.get("Content-Type") == "application/json"
                assert json.loads(raw) == {"min_ttl_seconds": 7200}
                if scenario == "mint-fail":
                    return self.reply(403, {"error": access})
                if scenario == "redirect-mint":
                    return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/must-not-receive"})
                return self.reply(200, mint)
            raise AssertionError("Unexpected request path")
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def reply(self, status, payload, headers=None):
        raw = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)


def reset(name):
    global scenario
    scenario = name
    events.clear()
    failures.clear()


def verify_ledger(token_count, agent_count):
    assert not failures, failures
    assert events == ["/nous-device/device/code"] + ["/nous-device/token"] * token_count + ["/nous-device/agent-key"] * agent_count, events


def request(path, payload=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    forwarded = dict(auth if headers is None else headers)
    if payload is not None:
        forwarded["Content-Type"] = "application/json"
    try:
        connection.request("GET" if payload is None else "POST", path,
                           None if payload is None else json.dumps(payload), headers=forwarded)
        response = connection.getresponse()
        raw = response.read().decode()
        assert all(sensitive not in raw for sensitive in (code, refresh, access, agent)), raw
        if path.startswith("/api/oauth/nous/"):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        connection.close()


def attempt(headers=None):
    status, granted = request("/api/oauth/nous/device-code", headers=headers)
    assert status == 200, granted
    handle = granted.pop("device_code")
    verifier = granted.pop("codeVerifier")
    assert len(handle) == len(verifier) == 43 and handle != verifier
    return granted, {"deviceCode": handle}


def inspect(id):
    with sqlite3.connect(database) as db:
        row = db.execute("SELECT provider, authType, isActive, data FROM providerConnections WHERE id=?", (id,)).fetchone()
    assert row[:3] == ("nous", "oauth", 1)
    stored = json.loads(row[-1])
    assert stored["testStatus"] == "active" and stored["expiresAt"].endswith("Z")
    return {key: stored[key] for key in ("accessToken", "refreshToken", "apiKey", "scope", "expiresIn", "providerSpecificData")}


def poll(payload, headers=None):
    status, result = request("/api/oauth/nous/poll", payload, headers)
    assert status == 200, result
    return result


def boot(binary, directory):
    global port, database, fallback_log
    install = Path(directory)
    home = install / "home"
    home.mkdir()
    runtime = install / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(runtime)], check=True,
                   stderr=subprocess.DEVNULL)
    database = runtime / "db/data.sqlite"
    fallback_log = install / "fallback.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    child = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
        "PATH": "", "HOME": str(home), "DATA_DIR": str(runtime), "NINEROUTER_MIGRATION_DATA_DIR": str(runtime),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "JWT_SECRET": secret, "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
        "PORT": str(port), "HOSTNAME": "127.0.0.1",
    }, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and child.poll() is None:
        try:
            if request("/api/health")[0] == 200:
                return child
        except OSError:
            pass
        time.sleep(.03)
    child.terminate()
    raise RuntimeError("Native gateway did not start")


def run_reference(polls):
    output = subprocess.check_output(["node", str(root / "rust/reference/nous-device-reference.mjs"), json.dumps({"polls": polls})],
                                     cwd=root, stderr=subprocess.DEVNULL)
    return json.loads(output)


def run_native(polls):
    granted, payload = attempt()
    results = []
    for _ in range(polls):
        time.sleep(1.05)
        result = poll(payload)
        if result["success"]:
            assert set(result) == {"success", "connection"}
            assert set(result["connection"]) == {"provider", "id"}
            assert result["connection"]["provider"] == "nous"
            result = {"success": True, "tokens": inspect(result["connection"]["id"])}
        results.append(result)
    return {"device": granted, "results": results}


def compare():
    observations = {}
    for name, polls, minted in [("pending-success", 2, True), ("mint-fail", 1, True), ("denied", 1, False)]:
        reset(name)
        observation = (run_reference if mode == "reference" else run_native)(polls)
        verify_ledger(polls, int(minted))
        assert observation["device"] == device
        for result in observation["results"]:
            if result["success"]:
                expected = {
                    "accessToken": access, "refreshToken": refresh, "apiKey": agent if name == "pending-success" else access,
                    "expiresIn": 3600, "scope": scope,
                    "providerSpecificData": {"inferenceBaseUrl": "https://inference-api.nousresearch.com/v1"},
                }
                if name == "pending-success":
                    expected["providerSpecificData"].update(agentKeyId=mint["key_id"], agentKeyExpiresAt=mint["expires_at"])
                assert result.pop("tokens") == expected
                result["credentialPersistence"] = "agent-key" if name == "pending-success" else "access-token"
        observations[name] = observation
    print(json.dumps(observations, sort_keys=True))


def session():
    encode = lambda value: urlsafe_b64encode(value).rstrip(b"=")
    claims = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signing = encode(b'{"alg":"HS256"}') + b"." + encode(json.dumps(claims).encode())
    signature = encode(hmac.digest(secret.encode(), signing, sha256))
    return {"Cookie": "auth_token=" + (signing + b"." + signature).decode()}


def strict():
    reset("concurrent")
    assert request("/api/oauth/nous/device-code", headers={}) == (401, {"error": "Unauthorized"})
    assert request("/api/oauth/nous/poll", {}, headers={}) == (401, {"error": "Unauthorized"})
    assert request("/api/oauth/nous/poll", {}) == (400, {"error": "Missing device code"})
    assert not events
    _, payload = attempt()
    assert poll(payload, session())["error"] == "expired_token"
    assert poll({"deviceCode": code})["error"] == "expired_token"
    assert poll(payload) == {"success": False, "pending": True, "error": "slow_down"}
    verify_ledger(0, 0)
    time.sleep(1.05)
    with ThreadPoolExecutor(2) as threads:
        winner = threads.submit(poll, payload)
        try:
            assert token_started.wait(4)
            assert poll(payload) == {"success": False, "pending": True, "error": "authorization_pending"}
        finally:
            token_release.set()
        result = winner.result(timeout=5)
    assert inspect(result["connection"]["id"])["apiKey"] == agent
    assert poll(payload)["error"] == "expired_token"
    verify_ledger(1, 1)

    reset("slow-success")
    browser_session = session()
    _, payload = attempt(browser_session)
    time.sleep(1.05)
    assert poll(payload, browser_session)["error"] == "slow_down"
    time.sleep(1.05)
    assert poll(payload, browser_session)["error"] == "slow_down"
    verify_ledger(1, 0)
    time.sleep(5.05)
    assert poll(payload, browser_session)["success"]
    verify_ledger(2, 1)

    for name in ("redirect-token", "disconnect", "redirect-mint"):
        reset(name)
        _, payload = attempt()
        time.sleep(1.05)
        result = poll(payload)
        if name == "redirect-mint":
            assert inspect(result["connection"]["id"])["apiKey"] == access
        else:
            assert result["fatal"] and result["error"] == "exchange_failed"
        assert poll(payload)["error"] == "expired_token"
        verify_ledger(1, int(name == "redirect-mint"))
    for name in ("bad-device", "bad-verification"):
        reset(name)
        assert request("/api/oauth/nous/device-code") == (500, {"error": "Nous device code request failed"})
        verify_ledger(0, 0)
    with sqlite3.connect(database) as db:
        assert db.execute("""SELECT count(*) FROM providerConnections
            WHERE provider='nous' AND authType='oauth' AND json_extract(data,'$.accessToken')=?""",
            (access,)).fetchone() == (3,)
    assert fallback_log.read_bytes() == b""
    print("Nous OAuth route verified: auth, owner binding, pacing, single-use, persistence, redaction, no fallback")


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as provider, tempfile.TemporaryDirectory(prefix="9router-nous-") as directory:
    thread = Thread(target=provider.serve_forever, daemon=True)
    thread.start()
    child = None if mode == "reference" else boot(str(Path(sys.argv[2]).resolve()), directory)
    try:
        if mode == "strict":
            strict()
        else:
            compare()
    finally:
        if child is not None:
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=10)
        provider.shutdown()
        thread.join()
