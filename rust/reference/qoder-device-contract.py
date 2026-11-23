from base64 import urlsafe_b64encode
from concurrent.futures import ThreadPoolExecutor
from hashlib import sha256
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event, Thread
import time
from urllib.parse import parse_qs, urlsplit
from uuid import UUID

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
secret = "qoder-fixture-session"
token = "dt-offline-qoder-fixture-access"
refresh = "offline-qoder-fixture-refresh"
token_path = "/api/v1/deviceToken/poll"
profile_path = "/api/v1/userinfo"
hosts = {"qoder": "qoder.com", "qoder-cn": "qoder.com.cn"}
ledger = []
failures = []
registrations = {}
observed = {}
issuer_verifiers = {}
start_poll = Event()
release_poll = Event()


def encoded(value):
    return urlsafe_b64encode(value).rstrip(b"=").decode()


def challenge(verifier):
    return encoded(sha256(verifier.encode()).digest())


def decode_registration(region, code):
    url = urlsplit(code["verification_uri_complete"])
    nonce, machine = code["device_code"], code["_qoderMachineId"]
    assert url.scheme == "https" and url.netloc == hosts[region] and url.path == "/device/selectAccounts"
    assert code["verification_uri"] == f"https://{hosts[region]}/device/selectAccounts"
    assert code["user_code"] == nonce[:8].upper() and code["_qoderNonce"] == nonce
    assert code["expires_in"] == 300 and code["interval"] == 2
    assert UUID(nonce).version == 4 and UUID(machine).version == 4 and nonce != machine
    query = parse_qs(url.query)
    assert set(query) == {"challenge", "challenge_method", "machine_id", "nonce"}
    assert query["nonce"] == [nonce] and query["machine_id"] == [machine]
    assert query["challenge_method"] == ["S256"]
    proof = code["codeVerifier"]
    assert len(query["challenge"][0]) == len(proof) == 43
    assert (challenge(proof) == query["challenge"][0]) == reference
    registrations[(region, nonce)] = (machine, query["challenge"][0], proof)


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            assert reference
            parts = self.path.split("/")
            assert parts == ["", "qoder-device", parts[2], "fixture-registration"] and parts[2] in hosts
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            decode_registration(parts[2], data)
            self.reply(200, {})
        except AssertionError as error:
            failures.append(str(error) or "registration")
            self.reply(500, {})

    def do_GET(self):
        try:
            path = urlsplit(self.path)
            _, prefix, region, endpoint = path.path.split("/", 3)
            assert prefix == "qoder-device" and region in hosts
            assert self.headers.get("Accept") == "application/json"
            assert self.headers.get("User-Agent") == ("Go-http-client/2.0" if reference else "9Router")
            if "/" + endpoint == token_path:
                self.token(region, parse_qs(path.query))
            elif "/" + endpoint == profile_path:
                self.profile(region, path.query)
            else:
                raise AssertionError("Unexpected upstream route")
        except (AssertionError, ValueError) as error:
            failures.append(str(error) or self.path.split("?")[0])
            self.reply(500, {})

    def token(self, region, query):
        ledger.append((region, "poll"))
        assert self.headers.get("Authorization") is None
        assert set(query) == {"nonce", "verifier", "challenge_method"} and query["challenge_method"] == ["S256"]
        nonce, verifier = query["nonce"][0], query["verifier"][0]
        machine, expected, proof = registrations[(region, nonce)]
        assert len(verifier) == 43 and challenge(verifier) == expected
        assert (verifier == proof) == reference
        observed[region] = machine
        issuer_verifiers[region] = verifier
        if outcome in {"standard", "missing-profile"} and ledger.count((region, "poll")) == 1:
            return self.reply(202 if region == "qoder" else 404, {})
        if outcome == "concurrent":
            start_poll.set()
            assert release_poll.wait(5)
        if outcome == "redirect":
            return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/forbidden"})
        if outcome == "disconnect":
            self.close_connection = True
            return
        if outcome == "denied":
            return self.reply(403, {"message": verifier + " " + token})
        if outcome == "long-duration":
            return self.reply(200, {"token": token, "user_id": 272, "expires_in": 172800})
        self.reply(200, {"token": token, "refresh_token": refresh, "user_id": "offline-qoder-user", "expires_at": "2000-01-01T00:00:00Z"})

    def profile(self, region, query):
        ledger.append((region, "profile"))
        assert not query and self.headers.get_all("Authorization") == ["Bearer " + token]
        if outcome == "missing-profile":
            return self.reply(503, {"message": token})
        self.reply(200, {"username": "  Offline-Name  ", "email": " test@example.invalid ", "organization_id": " org-1 "})

    def reply(self, status, body, extra=None):
        encoded_body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded_body)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(encoded_body)


def reset(scenario):
    global outcome
    outcome = scenario
    ledger.clear()
    failures.clear()
    registrations.clear()
    observed.clear()
    issuer_verifiers.clear()


def path(region, action):
    return f"/api/oauth/{region}/{action}"


def call(path, body=None, headers=None):
    transport = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(auth if headers is None else headers)
    try:
        if body is not None:
            request_headers["Content-Type"] = "application/json"
        transport.request("POST" if body is not None else "GET", path,
                          None if body is None else json.dumps(body), request_headers)
        response = transport.getresponse()
        raw = response.read()
        assert token.encode() not in raw and refresh.encode() not in raw
        if path.startswith("/api/oauth/qoder"):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        transport.close()


def begin(region, headers=None):
    status, code = call(path(region, "device-code"), headers=headers)
    assert status == 200
    decode_registration(region, code)
    return code, {"deviceCode": code["device_code"], "codeVerifier": code["codeVerifier"], "extraData": {"_qoderMachineId": "must-not-persist"}}


def poll(region, body, headers=None):
    status, result = call(path(region, "poll"), body, headers=headers)
    assert status == 200, result
    return result


def stored(region, connection_id):
    with sqlite3.connect(database) as db:
        auth_type, name, email, raw = db.execute("SELECT authType,name,email,data FROM providerConnections WHERE id=? AND provider=?", (connection_id, region)).fetchone()
    fields = json.loads(raw)
    assert auth_type == "oauth" and fields["testStatus"] == "active"
    assert name == email and fields["expiresAt"].endswith("Z")
    fields["email"] = email
    return {key: fields.get(key) for key in ("accessToken", "refreshToken", "email", "displayName", "expiresIn", "providerSpecificData")}


def reference_run(region):
    raw = subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/qoder-device-reference.mjs"), region,
    ], cwd=root)
    result = json.loads(raw)
    return result["code"], result["results"]


def native_run(region):
    code, payload = begin(region)
    results = []
    for _ in range(2):
        time.sleep(2.05)
        result = poll(region, payload)
        if result["success"]:
            assert set(result) == {"success", "connection"} and set(result["connection"]) == {"id", "provider"}
            assert result["connection"]["provider"] == region
            result = {"success": True, "fields": stored(region, result["connection"]["id"])}
        results.append(result)
    return code, results


def compare():
    summary = {}
    for region, behavior in [("qoder", "standard"), ("qoder-cn", "missing-profile")]:
        reset(behavior)
        code, results = (reference_run if reference else native_run)(region)
        assert not failures and ledger == [(region, "poll"), (region, "poll"), (region, "profile")], (failures, ledger)
        assert results[0] == {"success": False, "error": "authorization_pending", "pending": True}
        assert results[1]["success"]
        expected_email = "test@example.invalid" if behavior == "standard" else "qoder-user-offline-qoder-user"
        fields = results[1]["fields"]
        assert fields.pop("accessToken") == token and fields.pop("refreshToken") == refresh
        assert fields["expiresIn"] == 86400 and fields["email"] == expected_email
        assert fields["displayName"] == ("Offline-Name" if behavior == "standard" else None)
        assert fields["providerSpecificData"].pop("machineId") == code["_qoderMachineId"] == observed[region]
        assert fields["providerSpecificData"] == {
            "authMethod": "device", "userId": "offline-qoder-user", "organizationId": "org-1" if behavior == "standard" else "",
        }
        summary[region] = {"login": code["verification_uri"], "pending": results[0]["pending"], "saved": fields}
    print(json.dumps(summary, sort_keys=True))


def session():
    body = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180}
    signed = encoded(b'{"alg":"HS256"}') + "." + encoded(json.dumps(body).encode())
    signature = encoded(hmac.digest(secret.encode(), signed.encode(), sha256))
    return {"Cookie": f"auth_token={signed}.{signature}"}


def strict():
    region = "qoder"
    reset("concurrent")
    assert call(path(region, "device-code"), headers={}) == (401, {"error": "Unauthorized"})
    assert call(path(region, "poll"), {}, headers={}) == (401, {"error": "Unauthorized"})
    assert call(path(region, "poll"), {}) == (400, {"error": "Missing device code"})
    assert call(path(region, "poll"), {"deviceCode": "fixture"}) == (400, {"error": "Missing code verifier"})
    owner = session()
    code, payload = begin(region, owner)
    assert not ledger
    assert poll(region, payload)["error"] == "expired_token"
    assert poll("qoder-cn", payload, owner)["error"] == "expired_token"
    assert poll(region, dict(payload, codeVerifier="bad-proof"), owner)["error"] == "expired_token"
    assert poll(region, payload, owner)["error"] == "slow_down" and not ledger
    time.sleep(2.05)
    with ThreadPoolExecutor(2) as threads:
        winner = threads.submit(poll, region, payload, owner)
        try:
            assert start_poll.wait(5)
            assert poll(region, payload, owner)["error"] == "authorization_pending"
        finally:
            release_poll.set()
        first = winner.result(timeout=8)
    assert first["success"] and poll(region, payload, owner)["error"] == "expired_token"
    row = stored(region, first["connection"]["id"])
    assert row["providerSpecificData"]["machineId"] == code["_qoderMachineId"]
    assert ledger == [(region, "poll"), (region, "profile")] and not failures

    for scenario in ("redirect", "disconnect", "denied"):
        reset(scenario)
        _, payload = begin("qoder-cn")
        time.sleep(2.05)
        error = poll("qoder-cn", payload)
        assert error["error"] == "poll_failed" and not error["pending"]
        assert payload["codeVerifier"] not in error["errorDescription"]
        assert issuer_verifiers["qoder-cn"] not in error["errorDescription"]
        assert poll("qoder-cn", payload)["error"] == "expired_token"
        assert ledger == [("qoder-cn", "poll")] and not failures, (ledger, failures)
    with sqlite3.connect(database) as db:
        assert db.execute("""SELECT count(*) FROM providerConnections
            WHERE provider='qoder-cn' AND authType='oauth' AND json_extract(data,'$.accessToken')=?""",
            (token,)).fetchone() == (0,)

    reset("long-duration")
    _, payload = begin("qoder-cn")
    time.sleep(2.05)
    registered = poll("qoder-cn", payload)
    assert registered["connection"]["id"] != first["connection"]["id"]
    row = stored("qoder-cn", registered["connection"]["id"])
    assert row["providerSpecificData"]["userId"] == 272 and row["expiresIn"] in {172799, 172800}
    assert row["refreshToken"] is None
    assert fallback_log.read_bytes() == b""
    print("Qoder device flows verified: regional/session/proof isolation, single redemption, PKCE binding, no redirect or fallback")


def boot(directory, binary):
    global port, database, fallback_log
    location = Path(directory)
    home = location / "home"
    home.mkdir()
    data = location / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], check=True, stderr=subprocess.DEVNULL)
    database = data / "db/data.sqlite"
    fallback_log = location / "fallback.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
        "PATH": "", "HOME": str(home), "NINEROUTER_MIGRATION_FIXTURE": "1", "DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_DATA_DIR": str(data), "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
        "PORT": str(port), "HOSTNAME": "127.0.0.1", "JWT_SECRET": secret,
    }, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as server, tempfile.TemporaryDirectory(prefix="9router-qoder-device-") as directory:
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    process = None if reference else boot(directory, str(Path(sys.argv[2]).resolve()))
    try:
        if mode == "strict":
            strict()
        else:
            compare()
    finally:
        if process is not None:
            process.terminate()
            process.wait(timeout=8)
        server.shutdown()
        thread.join()
