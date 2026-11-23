import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import hmac
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
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS.pop(0)).resolve())
CASE, = ARGS
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
JWT_SECRET = "offline-glm-fixture-session-secret"
AUTH_URI = "https://zcode.z.ai/oauth/authorize?flow=fixture"
ACCESS = "fixture-owner-zai-access"
BUSINESS = "fixture-owner-business-token"
JWT = "fixture-owner-zcode-jwt"
REFRESH = "fixture-owner-zai-refresh"
KEY_ID = "fixture/key%?"
SECRET = "fixture-api-key-secret"
KEY = f"{KEY_ID}.{SECRET}"
EMAIL = "glm-owner@example.invalid"
NAME = "Fixture Owner"
DEVICE = "/api/oauth/glm/device-code"
POLL = "/api/oauth/glm/poll"
ZCODE = "/glm-consent/zcode.z.ai/api/v1/oauth/cli"
ZAI = "/glm-consent/api.z.ai/api"
KEYS = ZAI + "/biz/v1/organization/444/projects/personal-default/api_keys"
OWNER = {"email": EMAIL, "name": NAME, "user_id": 42}
NETWORK = []
ERRORS = []
FLOWS = []
SCENARIO = CASE
CREATED = False
CREATE_ARRIVED = threading.Event()
CREATE_RELEASE = threading.Event()


def auth(token):
    return "Bearer " + token


def session(subject):
    b64 = lambda data: base64.urlsafe_b64encode(data).decode().rstrip("=")
    header = b64(b'{"alg":"HS256"}')
    payload = b64(json.dumps({"authenticated": True, "sub": subject, "iat": int(time.time()),
                             "exp": int(time.time()) + 600}).encode())
    signed = f"{header}.{payload}"
    signature = b64(hmac.new(JWT_SECRET.encode(), signed.encode(), hashlib.sha256).digest())
    return {"Cookie": f"auth_token={signed}.{signature}"}


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, body, status=200, **headers):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in headers.items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def dispatch(self):
        global CREATED
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        assert "zcode" not in self.headers.get("user-agent", "").lower()
        if self.command == "GET":
            assert not body
        else:
            assert self.headers.get_content_type() == "application/json"
        if self.path == ZCODE + "/init":
            assert self.command == "POST" and json.loads(body) == {"provider": "zai"}
            header = self.headers.get("Authorization", "")
            assert re.fullmatch(r"Bearer [a-f0-9]{64}", header)
            assert all(flow["bearer"] != header for flow in FLOWS)
            id_ = f"fixture-flow-{len(FLOWS)+1} /%?"
            FLOWS.append({"id": id_, "bearer": header, "polls": 0, "scenario": SCENARIO})
            NETWORK.append("init")
            verification = "https://z.ai.example.invalid/oauth/authorize" if SCENARIO == "bad-issuer" else AUTH_URI
            data = {"flow_id": id_, "authorize_url": verification, "poll_interval_sec": 1}
            if SCENARIO == "expired":
                data["expires_at"] = int(time.time()) - 60
            if CASE == "create":
                data["expires_at"] = str(int(time.time() * 1000) + 120_000)
            return self.reply({"code": "200", "data": data})
        if self.path.startswith(ZCODE + "/poll/"):
            flow, = [flow for flow in FLOWS if self.path == ZCODE + "/poll/" + quote(flow["id"], safe="")]
            assert self.command == "GET" and self.headers.get("Authorization") == flow["bearer"]
            flow["polls"] += 1
            NETWORK.append("poll")
            circumstance = flow["scenario"]
            if circumstance == "denied":
                return self.reply({"code": 0, "data": {"status": "failed"}})
            if circumstance == "existing" and flow["polls"] == 1:
                return self.reply({"code": 0, "data": {"status": "pending"}})
            if circumstance == "wrong-provider":
                return self.reply({"code": 0, "data": {"status": "ready", "providerId": "bigmodel",
                    "accessToken": ACCESS, "user": OWNER}})
            data = {"status": "ready", "token": JWT, "user": OWNER, "providerId": "zai"}
            if circumstance == "create":
                data.update(accessToken=ACCESS, refresh_token=REFRESH)
            else:
                data["zai"] = {"access_token": ACCESS, "refresh_token": REFRESH}
            return self.reply({"code": 0, "data": data})
        if self.path == ZAI + "/auth/z/login":
            assert self.command == "POST" and json.loads(body) == {"token": ACCESS}
            assert self.headers.get("Authorization") is None
            NETWORK.append("business login")
            return self.reply({"data": {"accessToken": BUSINESS + " "}})
        assert self.headers.get("Authorization") == auth(BUSINESS)
        assert self.headers.get_content_type() == "application/json"
        if self.path == ZAI + "/biz/customer/getCustomerInfo":
            assert self.command == "GET"
            NETWORK.append("organization and project")
            return self.reply({"code": 0, "data": {"organizations": [
                {"organizationName": "Other", "organizationId": "other", "projects": [
                    {"projectName": "default", "projectId": "unselected-other"}]},
                {"organizationName": "default", "organizationId": "teams", "projects": [
                    {"projectType": 2, "projectName": "default", "projectId": "unselected-team"}]},
                {"organizationName": "我的默认机构", "organizationId": 444, "projects": [
                    {"projectType": " 2 ", "projectName": "default", "projectId": "unselected-other-team"},
                    {"projectName": "Personal", "projectId": "personal-other"},
                    {"projectName": "我的默认项目", "projectId": "personal-default"}]},
            ]}})
        if self.path == KEYS and self.command == "GET":
            NETWORK.append("list keys")
            if SCENARIO == "bad-list":
                return self.reply({"code": 0, "data": {"unrecognized": []}})
            keys = [{"name": "other", "apiKey": "unselected-other"}]
            if SCENARIO in {"existing", "copy-redirect"} or CREATED:
                keys.append({"name": "zcode-api-key", "apiKey": KEY_ID})
            return self.reply({"code": "0", "data": keys})
        if self.path == KEYS and self.command == "POST":
            assert json.loads(body) == {"name": "zcode-api-key"}
            NETWORK.append("create key")
            assert not CREATED
            CREATED = True
            if SCENARIO == "ambiguous-create":
                CREATE_ARRIVED.set()
                assert CREATE_RELEASE.wait(5)
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            return self.reply({"code": 200, "data": {"name": "zcode-api-key", "apiKey": KEY_ID + " "}})
        if self.path == KEYS + "/copy/" + quote(KEY_ID, safe=""):
            assert self.command == "GET"
            NETWORK.append("copy key")
            if SCENARIO == "copy-redirect":
                return self.reply({}, 302, Location="http://127.0.0.1:52989/untrusted")
            return self.reply({"success": True, "data": {"secretKey": " " + SECRET + " "}})
        raise AssertionError("Unexpected path " + self.path)

    def handle_fixture(self):
        try:
            self.dispatch()
        except (AssertionError, KeyError, ValueError) as error:
            ERRORS.append(str(error) or self.path)
            self.reply({"error": "Fixture request mismatch"}, 422)

    do_GET = do_POST = handle_fixture


def request(port, path, body=None, headers=ADMIN):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request("GET" if body is None else "POST", path,
                           None if body is None else json.dumps(body),
                           headers if body is None else {**headers, "content-type": "application/json"})
        response = connection.getresponse()
        data = json.loads(response.read())
        assert response.getheader("Cache-Control") == "no-store" or REFERENCE or path == "/api/health"
        return response.status, data
    finally:
        connection.close()


def begin(port, headers=ADMIN):
    status, data = request(port, DEVICE, headers=headers)
    assert status == 200, (status, data, NETWORK, ERRORS)
    assert data["verification_uri"] == AUTH_URI and data["interval"] == 1
    assert re.fullmatch(r"[-_a-zA-Z0-9]{43}", data["codeVerifier"])
    if CASE == "create":
        assert 115 <= data["expires_in"] <= 120
    else:
        assert data["expires_in"] == 300
    code = data["device_code"]
    if REFERENCE:
        assert code == FLOWS[-1]["id"] and auth(data["_zcodePollToken"]) == FLOWS[-1]["bearer"]
    else:
        assert re.fullmatch(r"[-_a-zA-Z0-9]{43}", code) and code != data["codeVerifier"]
        assert set(data) == {"device_code", "verification_uri", "interval", "expires_in", "codeVerifier"}
    return {"deviceCode": code, "codeVerifier": data["codeVerifier"],
            "extraData": {"_zcodePollToken": data.get("_zcodePollToken", "fake-client-secret")}}


def saved(database, id_=None):
    with sqlite3.connect(database) as sql:
        rows = list(sql.execute("""SELECT id,authType,email,data FROM providerConnections
            WHERE provider='glm' AND id!='fixture-glm-plan-auth'"""))
    if id_ is None:
        assert not rows, rows
        return
    (identifier, kind, email, raw), = rows
    assert identifier == id_ and kind == "oauth" and email == EMAIL
    record = json.loads(raw)
    assert record["accessToken"] == KEY and record.get("refreshToken") is None and record.get("expiresAt") is None
    assert record["displayName"] == NAME and record["testStatus"] == "active"
    details = record["providerSpecificData"]
    identity = {"authMethod": "cli_poll", "username": NAME, "userId": 42}
    if REFERENCE:
        assert details == {**identity, "zcodeJwtToken": JWT, "zaiBusinessToken": BUSINESS, "zaiRefreshToken": REFRESH}
    else:
        assert details == identity and all(secret not in raw for secret in [JWT, ACCESS, BUSINESS, REFRESH])
    return {"email": email, "name": NAME, "mode": kind, "identity": identity, "hasCodingPlanKey": True}


def compare(port, database):
    attempt = begin(port)
    time.sleep(1.04)
    if CASE == "existing":
        assert request(port, POLL, attempt) == (200, {"success": False, "error": "authorization_pending", "pending": True})
        assert NETWORK == ["init", "poll"]
        saved(database)
        time.sleep(1.04)
    status, response = request(port, POLL, attempt)
    assert status == 200, (status, response, NETWORK, ERRORS)
    if CASE == "denied":
        assert response == {"success": False, "error": "access_denied", "pending": False,
                            "errorDescription": "ZCode authorization failed or was cancelled"}
        assert NETWORK == ["init", "poll"]
        saved(database)
        return {"network": NETWORK, "poll": response}
    assert response["success"] is True and response["connection"]["provider"] == "glm", response
    expected = ["init", "poll"] + (["poll"] if CASE == "existing" else []) + ["business login", "organization and project", "list keys"]
    assert NETWORK == expected + (["create key"] if CASE == "create" else []) + ["copy key"], NETWORK
    return {"network": NETWORK, "stored": saved(database, response["connection"]["id"])}


def strict(port, database):
    global SCENARIO
    alice, bob = session("alice"), session("bob")
    assert request(port, DEVICE, headers={}) == (401, {"error": "Unauthorized"})
    assert request(port, POLL, {"deviceCode": "forged"}, bob)[1]["error"] == "expired_token"
    assert NETWORK == []
    for malformed in ["bad-issuer", "expired"]:
        SCENARIO = malformed
        assert request(port, DEVICE, headers=alice) == (500, {"error": "Could not start Z.ai device authorization"})
    SCENARIO = "ambiguous-create"
    attempt = begin(port, alice)
    assert request(port, POLL, attempt, alice)[1]["error"] == "slow_down"
    assert request(port, POLL, attempt, bob)[1]["error"] == "expired_token"
    assert NETWORK == ["init"] * 3
    time.sleep(1.04)
    with ThreadPoolExecutor(max_workers=2) as clients:
        first = clients.submit(request, port, POLL, attempt, alice)
        assert CREATE_ARRIVED.wait(6), (NETWORK, ERRORS)
        second = clients.submit(request, port, POLL, attempt, alice)
        assert second.result() == (200, {"success": False, "error": "authorization_pending", "pending": True})
        CREATE_RELEASE.set()
        status, failed = first.result()
    assert status == 200 and failed["fatal"] and "confirm Z.ai API key creation" in failed["errorDescription"], failed
    assert request(port, POLL, attempt, alice)[1]["error"] == "expired_token"
    assert NETWORK == ["init"] * 3 + ["poll", "business login", "organization and project", "list keys", "create key"]
    saved(database)
    for circumstance in ["wrong-provider", "bad-list", "copy-redirect", "recover"]:
        SCENARIO = circumstance
        attempt = begin(port, alice)
        time.sleep(1.04)
        status, result = request(port, POLL, attempt, alice)
        assert status == 200
        if circumstance == "recover":
            assert result["success"] and result["connection"]["provider"] == "glm", result
            saved(database, result["connection"]["id"])
        elif circumstance == "wrong-provider":
            assert result["error"] == "access_denied" and NETWORK[-2:] == ["init", "poll"]
        else:
            assert result["fatal"] and result["error"] == "exchange_failed"
            saved(database)
    assert NETWORK.count("create key") == 1 and NETWORK.count("copy key") == 2
    return "Owner, issuer, interval, authorization, disconnect during create, single-use continuation and credential redaction passed."


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
    raise RuntimeError("GLM consent fixture process did not start")


with tempfile.TemporaryDirectory(prefix="9router-glm-device-") as directory:
    scratch = Path(directory)
    home, data, empty = (scratch / name for name in ("home", "data", "empty-path"))
    home.mkdir()
    empty.mkdir()
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], cwd=ROOT, check=True,
                   env={"PATH": os.environ["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
    database = data / "db/data.sqlite"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": os.environ["PATH"] if REFERENCE else str(empty), "HOME": str(home), "DATA_DIR": str(data),
           "JWT_SECRET": JWT_SECRET, "PORT": str(port), "NINEROUTER_MIGRATION_FIXTURE": "1",
           "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallbacks")}
    mock = ThreadingHTTPServer(("127.0.0.1", 52989), Provider)
    threading.Thread(target=mock.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    command = ["node", "--no-warnings", str(Path(__file__).with_name("original.mjs")), str(data)] if REFERENCE else [BINARY, "--no-browser"]
    with (scratch / "app.log").open("w+") as logs:
        app = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=logs)
        try:
            port = wait(app, port)
            if CASE == "strict":
                print(strict(port, database))
            else:
                print(json.dumps(compare(port, database), sort_keys=True))
            assert not ERRORS, ERRORS
            if not REFERENCE:
                assert (scratch / "fallbacks").read_bytes() == b""
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            raise
        finally:
            CREATE_RELEASE.set()
            app.terminate()
            try:
                app.wait(timeout=4)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait(timeout=2)
            mock.shutdown()
            mock.server_close()
