import base64
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
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
from urllib.parse import urlencode


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS.pop(0)).resolve())
CHECK = ARGS[0]
BASE = "/api/oauth/kiro"
DEFAULT_START = "https://view.awsapps.com/start"
ORGANIZATION_START = "https://fixture-org.awsapps.com/start"
ISSUER = "https://identitycenter.amazonaws.com/ssoins-722374e8c3c8e6c6"
GRANT = "urn:ietf:params:oauth:grant-type:device_code"
PROFILE = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE-DEVICE"
EMAIL = "fixture-device@example.invalid"
REFRESH = "synthetic-fixture-device-refresh-token"
ACCESS = "eyJhbGciOiJub25lIn0." + base64.urlsafe_b64encode(
    json.dumps({"preferred_username": EMAIL}).encode()
).decode().rstrip("=") + ".fixture"
AUTH = {"x-9r-cli-token": "68977b6fd9422fbb"}
calls = []
issues = []
clients = {}
scenarios = []
held = threading.Event()
release = threading.Event()
success_held = threading.Event()
release_success = threading.Event()


class AWS(ThreadingHTTPServer):
    allow_reuse_address = True


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, payload, headers=None):
        raw = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        try:
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            payload = json.loads(raw)
            assert self.headers["Content-Type"] == "application/json"
            assert self.headers["Accept"] == "application/json"
            self.handle_provider(payload)
        except AssertionError as problem:
            issues.append(str(problem) or self.path)
            self.send(400, {"error": "invalid_request"})

    def handle_provider(self, payload):
        suffix = self.path.removeprefix("/kiro-device/")
        assert self.path.startswith("/kiro-device/")
        if suffix == "profile":
            calls.append("profile")
            assert self.headers["Authorization"] == "Bearer " + ACCESS
            assert payload == {"maxResults": 10}
            self.send(200, {"profiles": [{"arn": " "}, {"arn": "  " + PROFILE + " "}]})
            return
        region, path = suffix.split("/", 1)
        calls.append(region + "/" + path)
        assert self.headers["Authorization"] is None
        if path == "client/register":
            assert payload == {
                "clientName": "kiro-oauth-client", "clientType": "public",
                "scopes": ["codewhisperer:completions", "codewhisperer:analysis", "codewhisperer:conversations"],
                "grantTypes": [GRANT, "refresh_token"], "issuerUrl": ISSUER,
            }
            assert scenarios, "Unexpected client registration"
            stage = scenarios.pop(0)
            ix = len(clients) + 1
            client_id = f"fixture-id-{ix}"
            secret = f"fixture-client-secret-{ix}"
            if stage.get("failed") == "registration":
                self.send(403, {"error": "echo: " + secret})
                return
            clients[client_id] = stage | {
                "client_id": client_id, "secret": secret, "device": f"aws-device-fixture-{ix}",
                "polls": 0, "region": region,
            }
            self.send(200, {"clientId": client_id, "clientSecret": secret})
            return
        data = clients[payload["clientId"]]
        assert data["region"] == region
        assert payload["clientSecret"] == data["secret"]
        if path == "device_authorization":
            assert payload == {"clientId": data["client_id"], "clientSecret": data["secret"], "startUrl": data["start"]}
            if data.get("failed") == "authorization":
                self.send(400, {"error": "echo: " + data["secret"]})
                return
            self.send(200, {
                "deviceCode": data["device"], "userCode": "ABCD-FIXT",
                "verificationUri": f"https://device.sso.{region}.amazonaws.com",
                "verificationUriComplete": f"https://device.sso.{region}.amazonaws.com/?user_code=ABCD-FIXT",
                "expiresIn": data.get("expires", 60), "interval": data.get("interval", 1),
            })
            return
        assert path == "token", "Unexpected request"
        assert payload == {
            "clientId": data["client_id"], "clientSecret": data["secret"],
            "deviceCode": data["device"], "grantType": GRANT,
        }
        position = data["polls"]
        data["polls"] += 1
        script = data["script"]
        assert position < len(script), "Repeated AWS token request"
        step = script[position]
        if step == "wait":
            held.set()
            assert release.wait(timeout=5)
            step = "authorization_pending"
        if step == "hold_success":
            success_held.set()
            assert release_success.wait(timeout=5)
            step = "direct"
        if step == "disconnect":
            self.close_connection = True
            return
        if step == "redirect":
            self.send(307, {}, {"Location": "http://127.0.0.1:52989/kiro-device/never"})
            return
        if step in {"direct", "profile"}:
            value = {"accessToken": ACCESS, "refreshToken": REFRESH, "expiresIn": 3600}
            if step == "direct":
                value["profileArn"] = PROFILE
            self.send(200, value)
            return
        detail = {} if step in {"authorization_pending", "slow_down"} else {
            "error_description": "Fixture must not echo " + data["secret"] + " or " + data["device"],
        }
        self.send(400, {"error": step} | detail)


def request(port, method, path, payload=None, headers=AUTH):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=7)
    try:
        headers = dict(headers)
        if payload is not None:
            headers["Content-Type"] = "application/json"
            payload = json.dumps(payload)
        connection.request(method, path, payload, headers)
        response = connection.getresponse()
        if not REFERENCE and path.startswith(BASE):
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def wait(child, port):
    deadline = time.monotonic() + 8
    while child.poll() is None and time.monotonic() < deadline:
        if REFERENCE and select.select([child.stdout], [], [], .25)[0]:
            line = child.stdout.readline().decode()
            if line.startswith("ready "):
                return int(line.split()[1])
        if not REFERENCE:
            try:
                if request(port, "GET", "/api/health")[0] == 200:
                    return port
            except OSError:
                time.sleep(.025)
    raise RuntimeError("Kiro device-code fixture did not start")


def auth_start(port, region="us-east-1", start=DEFAULT_START, method="builder-id", **scenario):
    scenarios.append({"start": start, "script": [], **scenario})
    query = urlencode({"start_url": " " + start + " ", "region": " " + region + " ", "auth_method": method})
    status, body = request(port, "GET", BASE + "/device-code?" + query)
    assert status == 200, body
    assert body["user_code"] == "ABCD-FIXT" and body["verification_uri"].startswith("https://device.sso.")
    assert body["_region"] == region and body["_startUrl"] == start and body["_authMethod"] == method
    active = list(clients.values())[-1]
    if REFERENCE:
        assert body["device_code"] == active["device"]
    else:
        assert re.fullmatch(r"[A-Za-z0-9_-]{43}", body["device_code"])
        assert active["device"] not in json.dumps(body) and active["secret"] not in json.dumps(body)
        assert "_clientSecret" not in body and "_clientId" not in body
    return body


def poll(port, data, extra=None):
    fields = {name: data.get(name) for name in ["_clientId", "_clientSecret", "_region", "_authMethod", "_startUrl"]}
    return request(port, "POST", BASE + "/poll", {
        "deviceCode": data["device_code"], "codeVerifier": data["codeVerifier"],
        "extraData": fields if extra is None else extra,
    })


def record(db, connection, method, region, start):
    assert set(connection) == {"id", "provider"} and connection["provider"] == "kiro"
    with sqlite3.connect(db) as sql:
        auth, email, data = sql.execute(
            "SELECT authType,email,data FROM providerConnections WHERE id=?", (connection["id"],)
        ).fetchone()
    value = json.loads(data)
    details = value["providerSpecificData"]
    assert auth == "oauth" and email == EMAIL
    assert value["accessToken"] == ACCESS and value["refreshToken"] == REFRESH and value["testStatus"] == "active"
    assert value["expiresIn"] == 3600 and int(datetime.fromisoformat(value["expiresAt"].replace("Z", "+00:00")).timestamp()) > time.time()
    assert details["profileArn"] == PROFILE and details["region"] == region and details["authMethod"] == method and details["startUrl"] == start
    active = list(clients.values())[-1]
    assert details["clientId"] == active["client_id"] and details["clientSecret"] == active["secret"]
    return {"expiresIn": value["expiresIn"], "region": region, "method": method, "resolvedProfile": True}


def compare(port, database):
    assert request(port, "GET", BASE + "/device-code", headers={}) == (401, {"error":"Unauthorized"})
    assert request(port, "POST", BASE + "/poll", {}) == (400, {"error":"Missing device code"})
    bad = request(port, "GET", BASE + "/device-code?region=us-east-1.invalid")
    assert bad == (500, {"error":"Invalid region"}), bad
    assert calls == []
    observed = []
    for method, region, start, success in [
        ("builder-id", "us-east-1", DEFAULT_START, "direct"),
        ("idc", "eu-central-1", ORGANIZATION_START, "profile"),
    ]:
        result = auth_start(port, region, start, method, script=["authorization_pending", success])
        assert result["expires_in"] == 60 and result["interval"] == 1
        assert poll(port, result) == (200, {"success":False,"error":"authorization_pending","pending":True})
        if not REFERENCE:
            time.sleep(1.05)
        status, success = poll(port, result)
        assert status == 200 and success["success"], success
        row = record(database, success["connection"], method, region, start)
        observed.append(row)
    print(json.dumps({
        "authorized": observed, "network": calls,
        "missing_code": 400, "invalid_region": 500, "unauthenticated": 401,
    }, separators=(",", ":")))


def strict(port, database):
    assert not REFERENCE
    assert request(port, "GET", BASE + "/device-code?start_url=http%3A%2F%2Finvalid") == (400, {"error":"Invalid start URL"})
    assert request(port, "POST", BASE + "/poll", {"deviceCode":"Z"*43}) == (200, {"success":False,"error":"expired_token","pending":False})
    assert calls == []

    for step, expected in [("registration", "Client registration failed"), ("authorization", "Device authorization failed")]:
        scenarios.append({"start": DEFAULT_START, "script": [], "failed": step})
        assert request(port, "GET", BASE + "/device-code") == (500, {"error":expected})

    result = auth_start(port, script=["wait", "slow_down", "direct"])
    with ThreadPoolExecutor(max_workers=2) as threads:
        first = threads.submit(poll, port, result, {"_region":"other.invalid","_clientSecret":"incorrect"})
        assert held.wait(timeout=3)
        size = len(calls)
        assert poll(port, result) == (200, {"success":False,"error":"authorization_pending","pending":True})
        assert len(calls) == size
        release.set()
        assert first.result() == (200, {"success":False,"error":"authorization_pending","pending":True})
    assert poll(port, result)[1]["error"] == "slow_down"
    time.sleep(1.05)
    assert poll(port, result) == (200, {"success":False,"error":"slow_down","pending":True})
    count = len(calls)
    assert poll(port, result)[1]["error"] == "slow_down" and len(calls) == count

    for step, wanted in [("expired_token", "expired_token"), ("access_denied", "access_denied"),
                         ("redirect", "exchange_failed"), ("disconnect", "exchange_failed")]:
        data = auth_start(port, script=[step])
        status, failed = poll(port, data)
        assert status == 200 and failed["error"] == wanted and failed["pending"] is False
        active = list(clients.values())[-1]
        assert active["secret"] not in json.dumps(failed) and active["device"] not in json.dumps(failed)
        count = len(calls)
        assert poll(port, data)[1]["error"] == "expired_token" and len(calls) == count

    expired = auth_start(port, expires=1, script=[])
    time.sleep(1.05)
    count = len(calls)
    assert poll(port, expired)[1]["error"] == "expired_token" and len(calls) == count
    with sqlite3.connect(database) as sql:
        assert sql.execute("SELECT COUNT(*) FROM providerConnections WHERE provider='kiro'").fetchone()[0] == 0

    data = auth_start(port, "eu-west-2", ORGANIZATION_START, "idc", script=["profile"])
    status, good = poll(port, data, {"_region": "untrusted.invalid", "_clientSecret": "malicious-extra"})
    assert status == 200 and good["success"]
    record(database, good["connection"], "idc", "eu-west-2", ORGANIZATION_START)
    count = len(calls)
    assert poll(port, data)[1]["error"] == "expired_token" and len(calls) == count

    disconnect = auth_start(port, script=["hold_success"])
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request("POST", BASE + "/poll", json.dumps({"deviceCode": disconnect["device_code"]}),
                       AUTH | {"Content-Type": "application/json"})
    assert success_held.wait(timeout=3)
    connection.close()
    release_success.set()
    expected_client = list(clients)[-1]
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        with sqlite3.connect(database) as sql:
            data, = sql.execute("SELECT data FROM providerConnections WHERE provider='kiro'").fetchone()
        if json.loads(data)["providerSpecificData"]["clientId"] == expected_client:
            break
        time.sleep(.025)
    else:
        raise AssertionError("Kiro grant was lost after caller disconnect")
    counted = len(calls)
    assert poll(port, disconnect)[1]["error"] == "expired_token" and len(calls) == counted
    print("Kiro device OAuth enforced one in-flight poll, cooldown, expiry, single grant, and secret redaction without Node")


with tempfile.TemporaryDirectory(prefix="9router-kiro-device-") as tmp:
    scratch = Path(tmp)
    config = scratch / "data"
    home = scratch / "home"
    home.mkdir()
    commands = scratch / "empty-path"
    commands.mkdir()
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(config)],
                   cwd=ROOT, check=True, stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME": str(home), "PATH": os.environ["PATH"] if REFERENCE else str(commands),
        "PORT": str(port), "DATA_DIR": str(config), "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallbacks"),
    }
    server = AWS(("127.0.0.1", 52989), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    command = ["node", "--no-warnings", str(Path(__file__).with_name("original.mjs")), str(config)] if REFERENCE else [BINARY, "--no-browser"]
    app = subprocess.Popen(command, env=env, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        port = wait(app, port)
        database = config / "db/data.sqlite"
        (compare if CHECK == "compare" else strict)(port, database)
        assert not issues, issues
        if not REFERENCE:
            assert (scratch / "fallbacks").read_bytes() == b""
    finally:
        release.set()
        release_success.set()
        app.terminate()
        try:
            app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            app.kill()
            app.wait(timeout=3)
        server.shutdown()
        server.server_close()
