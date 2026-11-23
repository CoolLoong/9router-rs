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

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
session_secret = "muse-fixture-session-secret"
issuer = "offline-private-meta-access"
issuer_refresh = "offline-private-meta-refresh"
key = "LLM|offline-private-muse-key"
device_code = "offline-private-muse-issuer-code"
client_id = "1031625952748946"
grant = "urn:ietf:params:oauth:grant-type:device_code"
verification = "https://www.meta.com/device"
device = {"user_code": "MUSE-RTRT", "verification_uri": verification,
          "verification_uri_complete": verification + "?user_code=MUSE-RTRT", "expires_in": 60, "interval": 1}
register = "/muse-device/authorization"
poll_path = "/muse-device/token"
mint = "/muse-device/key"
ledger = []
failures = []
entered_mint = Event()
release_mint = Event()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        ledger.append(self.path)
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        try:
            if self.path in {"/muse-chat/v1/responses", "/muse-chat/v1/chat/completions"}:
                return self.inference(json.loads(raw))
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("x-api-version") == "1.0.0"
            if not reference:
                assert self.headers.get("User-Agent") == "9Router"
            if self.path == register:
                return self.register(raw)
            if self.path == poll_path:
                return self.poll(raw)
            if self.path == mint:
                return self.mint(raw)
            raise AssertionError("Unexpected outbound destination")
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def do_GET(self):
        ledger.append(self.path)
        try:
            assert self.path == "/muse-chat/v1/models"
            assert self.headers.get_all("Authorization") == ["Bearer " + key]
            assert self.headers.get("x-api-version") == "1.0.0"
            self.reply(200, {"data":[{"id":"muse-spark-1.3","object":"model"}]})
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def inference(self, body):
        assert self.headers.get_all("Authorization") == ["Bearer " + key]
        assert self.headers.get("x-api-version") == "1.0.0"
        if self.path.endswith("/responses"):
            assert body["model"] == "muse-spark-1.3" and body["reasoning"]["effort"] == "low"
            assert body["include"] == ["reasoning.encrypted_content"]
            assert body["input"][0]["encrypted_content"] == "fixture-prior-encrypted-reasoning"
            return self.reply(200, {"id":"resp-muse-fixture","object":"response","created_at":1795046400,
                "status":"completed","model":body["model"],"output":[{"id":"rs-muse-fixture","type":"reasoning",
                    "summary":[],"encrypted_content":"fixture-new-encrypted-reasoning"}],
                "usage":{"input_tokens":12,"output_tokens":3,"total_tokens":15}})
        assert body["model"] == "fixture-muse-chat" and body["messages"][0]["content"] == "fixture"
        return self.reply(200,{"id":"chatcmpl-muse-fixture","object":"chat.completion","created":1795046400,
            "model":body["model"],"choices":[{"index":0,"message":{"role":"assistant","content":"Muse fixture"},
                                             "finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}})

    def register(self, raw):
        assert self.headers.get("Authorization") is None
        assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
        assert parse_qs(raw.decode()) == {"client_id": [client_id]}
        response = dict(device, device_code=device_code)
        if scenario == "bad-verification":
            response["verification_uri"] = "https://www.meta.com.example.invalid/device"
        if scenario == "secret-in-link":
            response["verification_uri_complete"] = f"{verification}?ticket={device_code}"
        self.reply(200, response)

    def poll(self, raw):
        assert self.headers.get("Authorization") is None
        assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
        assert parse_qs(raw.decode()) == {"grant_type": [grant], "device_code": [device_code], "client_id": [client_id]}
        if ledger.count(poll_path) == 1 and scenario in {"pending-success", "slow-success"}:
            return self.reply(400, {"error": "authorization_pending" if scenario == "pending-success" else "slow_down"})
        if scenario == "denied":
            return self.reply(403, {"error": "access_denied", "error_description": issuer})
        self.reply(200, {"access_token": issuer, "refresh_token": issuer_refresh, "expires_in": 1200})

    def mint(self, raw):
        assert self.headers.get_all("Authorization") == ["Bearer " + issuer]
        assert self.headers.get("Content-Type") == "application/json"
        assert json.loads(raw) == {"onboard": True}
        if scenario == "inactive":
            return self.reply(200, {"is_subs_active": False})
        if scenario == "concurrent-mint":
            entered_mint.set()
            assert release_mint.wait(5)
        if scenario == "payment":
            return self.reply(200, {"require_payment": True, "require_payment_action_url": f"https://muse.ai/subscription?token={issuer}#private"})
        if scenario == "bad-action":
            return self.reply(403, {"title": issuer, "detail": issuer_refresh, "action_url": "https://muse.ai.example.invalid/payment"})
        if scenario == "path-secret":
            return self.reply(403, {"action_url": f"https://muse.ai/action/{issuer}"})
        if scenario == "mint-timeout":
            self.close_connection = True
            return
        if scenario == "mint-redirect":
            return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
        if scenario == "mint-429":
            return self.reply(429, {"error": issuer})
        if scenario == "mint-500":
            return self.reply(500, {"error": issuer})
        if scenario == "mint-invalid-json":
            return self.reply(200, b"malformed fixture data")
        self.reply(200, {"api_key": key, "user_email": " Fixture@Example.Invalid ", "subs_tier_name": "fixture-tier", "is_subs_active": True})

    def reply(self, status, payload, extra=None):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)


def reset(name):
    global scenario
    scenario = name
    ledger.clear()
    failures.clear()


def call(path, payload=None, headers=None):
    client = HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(auth if headers is None else headers)
    if payload is not None:
        request_headers["Content-Type"] = "application/json"
    try:
        client.request("POST" if payload is not None else "GET", path,
                       None if payload is None else json.dumps(payload), request_headers)
        response = client.getresponse()
        raw = response.read().decode()
        assert all(secret not in raw for secret in [issuer, issuer_refresh, key, device_code])
        if path.startswith("/api/oauth/muse"):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        client.close()


def begin(headers=None):
    status, code = call("/api/oauth/muse/device-code", headers=headers)
    assert status == 200, (status, code)
    opaque, proof = code.pop("device_code"), code.pop("codeVerifier")
    assert len(opaque) == len(proof) == 43 and opaque != proof
    return code, {"deviceCode": opaque, "codeVerifier": proof}


def poll(payload, headers=None):
    status, result = call("/api/oauth/muse/poll", payload, headers)
    assert status == 200, (status, result)
    return result


def stored(connection):
    with sqlite3.connect(database) as db:
        provider, auth_type, email, active, raw = db.execute("SELECT provider,authType,email,isActive,data FROM providerConnections WHERE id=?", (connection,)).fetchone()
    saved = json.loads(raw)
    assert (provider, auth_type, active, saved["testStatus"]) == ("muse", "oauth", 1, "active")
    return {field: saved.get(field) for field in ("accessToken", "refreshToken", "expiresIn", "providerSpecificData")} | {"email": email}


def run_reference(polls):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/muse-device-reference.mjs"), json.dumps({"polls": polls}),
    ], cwd=root, env={"PATH": os.environ["PATH"]}))


def run_native(polls):
    code, payload = begin()
    results = []
    for _ in range(polls):
        time.sleep(1.05)
        result = poll(payload)
        if result["success"]:
            assert set(result) == {"success", "connection"} and result["connection"]["provider"] == "muse"
            result = {"success": True, "fields": stored(result["connection"]["id"])}
        results.append(result)
    return {"device": code, "results": results}


def compare():
    reports = {}
    for name, polls, minted in [("pending-success", 2, True), ("inactive", 1, True), ("denied", 1, False)]:
        reset(name)
        result = (run_reference if reference else run_native)(polls)
        assert result["device"] == device and not failures
        assert ledger == [register] + [poll_path] * polls + [mint] * minted
        if name == "pending-success":
            assert result["results"][0] == {"success": False, "error": "authorization_pending", "pending": True}
            saved = result["results"][1]["fields"]
            assert saved.pop("accessToken") == key
            assert saved["refreshToken"] is None and saved["expiresIn"] is None
            assert saved["email"] == "fixture@example.invalid"
            assert saved["providerSpecificData"].pop("oauthAccessToken") == issuer
            assert saved["providerSpecificData"] == {"authMethod": "device_code", "subscriptionTier": "fixture-tier"}
        if name == "inactive":
            assert result["results"][0]["fatal"] and "subscription is inactive" in result["results"][0]["errorDescription"]
        reports[name] = result
    if not reference:
        assert fallback_log.read_bytes() == b""
    print(json.dumps(reports, sort_keys=True))


def session():
    def encode(raw):
        return urlsafe_b64encode(raw).rstrip(b"=")
    payload = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signed = encode(b'{"alg":"HS256"}') + b"." + encode(json.dumps(payload).encode())
    signature = encode(hmac.digest(session_secret.encode(), signed, sha256))
    return {"Cookie": "auth_token=" + (signed + b"." + signature).decode()}


def strict():
    reset("concurrent-mint")
    assert call("/api/oauth/muse/device-code", headers={}) == (401, {"error": "Unauthorized"})
    assert call("/api/oauth/muse/poll", {}, headers={}) == (401, {"error": "Unauthorized"})
    assert call("/api/oauth/muse/poll", {}) == (400, {"error": "Missing device code"})
    assert call("/api/oauth/muse/poll", {"deviceCode": "fixture"}) == (400, {"error": "Missing code verifier"})
    with sqlite3.connect(database) as db:
        db.execute("""INSERT INTO settings(id,data) VALUES(1,'{"requireLogin":false}')
            ON CONFLICT(id) DO UPDATE SET data=json_set(settings.data,'$.requireLogin',json('false'))""")
    assert call("/api/oauth/muse/device-code", headers={})[0] == 401 and not ledger

    owner = session()
    _, payload = begin(owner)
    assert poll(payload)["error"] == "expired_token"
    assert poll(dict(payload, codeVerifier="not-the-proof"), owner)["error"] == "expired_token"
    assert poll(dict(payload, deviceCode=device_code), owner)["error"] == "expired_token"
    assert poll(payload, owner)["error"] == "slow_down" and ledger == [register]
    time.sleep(1.05)
    with ThreadPoolExecutor(2) as threads:
        winner = threads.submit(poll, payload, owner)
        try:
            assert entered_mint.wait(5)
            result = poll(payload, owner)
            assert result == {"success": False, "error": "authorization_pending", "pending": True}
        finally:
            release_mint.set()
        saved = winner.result(timeout=8)
    id = saved["connection"]["id"]
    row = stored(id)
    assert row["accessToken"] == key and row["providerSpecificData"]["oauthAccessToken"] == issuer
    assert row["refreshToken"] is None and poll(payload, owner)["error"] == "expired_token"
    assert ledger == [register, poll_path, mint] and not failures
    status, data = call(f"/api/providers/{id}", headers=owner)
    assert status == 200 and "oauthAccessToken" not in json.dumps(data)
    status, data = call("/api/providers", headers=owner)
    assert status == 200 and "oauthAccessToken" not in json.dumps(data)
    with sqlite3.connect(database) as db:
        db.execute("UPDATE settings SET data=json_set(data,'$.requireApiKey',json('false')) WHERE id=1")
    status, data = call(f"/api/providers/{id}/models", headers=owner)
    assert status == 200 and data["models"] == [{"id":"muse-spark-1.3","object":"model"}]
    assert call(f"/api/providers/{id}/test", {}, owner) == (200, {"valid":True,"error":None,"refreshed":False})
    status, data = call("/v1/chat/completions", {"model":"muse/fixture-muse-chat",
        "messages":[{"role":"user","content":"fixture"}],"stream":False}, owner)
    assert status == 200 and data["choices"][0]["message"]["content"] == "Muse fixture"
    status, data = call("/v1/responses", {"model":"muse/muse-spark-1.3","stream":False,
        "input":[{"type":"reasoning","id":"rs-prior","summary":[],"encrypted_content":"fixture-prior-encrypted-reasoning"},
                 {"role":"user","content":[{"type":"input_text","text":"fixture"}]}],
        "reasoning":{"effort":"low"},"include":["reasoning.encrypted_content"]}, owner)
    assert status == 200 and data["output"][0]["encrypted_content"] == "fixture-new-encrypted-reasoning", data
    assert ledger[3:] == ["/muse-chat/v1/models","/muse-chat/v1/models","/muse-chat/v1/chat/completions","/muse-chat/v1/responses"]
    assert not failures

    reset("slow-success")
    _, payload = begin()
    time.sleep(1.05)
    assert poll(payload)["error"] == "slow_down"
    time.sleep(1.05)
    assert poll(payload)["error"] == "slow_down" and ledger == [register, poll_path]
    time.sleep(5.05)
    assert poll(payload)["success"] and ledger == [register, poll_path, poll_path, mint] and not failures

    for name, expected_url in [("payment", "https://muse.ai/subscription"), ("bad-action", None), ("path-secret", None),
                               ("mint-timeout", None), ("mint-redirect", None), ("mint-429", None), ("mint-500", None), ("mint-invalid-json", None)]:
        reset(name)
        _, payload = begin()
        time.sleep(1.05)
        result = poll(payload)
        assert result["error"] == "exchange_failed" and result["fatal"] and not result["pending"]
        assert result.get("actionUrl") == expected_url
        assert poll(payload)["error"] == "expired_token"
        assert ledger == [register, poll_path, mint] and not failures, (name, ledger, failures)

    reset("bad-verification")
    assert call("/api/oauth/muse/device-code") == (500, {"error": "Muse Code device code request failed"})
    assert ledger == [register] and not failures
    reset("secret-in-link")
    exposed, _ = begin()
    assert "verification_uri_complete" not in exposed and exposed["verification_uri"] == verification
    with sqlite3.connect(database) as db:
        assert db.execute("""SELECT count(*) FROM providerConnections
            WHERE provider='muse' AND authType='oauth' AND json_extract(data,'$.accessToken')=?""",
            (key,)).fetchone() == (1,)
    assert fallback_log.read_bytes() == b""
    assert all(secret.encode() not in log.read_bytes() for secret in [issuer, issuer_refresh, key, device_code])
    print("Muse native OAuth, control plane and minted-key Chat/Responses verified without fallbacks")


def boot(directory, binary):
    global port, database, fallback_log, log
    folder = Path(directory)
    data = folder / "data"
    home = folder / "home"
    home.mkdir()
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
    fallback_log = folder / "fallback.log"
    log = folder / "gateway.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with log.open("wb") as output:
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
            "PATH": "", "HOME": str(home), "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
            "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
            "PORT": str(port), "HOSTNAME": "127.0.0.1", "JWT_SECRET": session_secret,
        }, stdout=output, stderr=subprocess.STDOUT)
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


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as provider, tempfile.TemporaryDirectory(prefix="9router-muse-device-") as directory:
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
