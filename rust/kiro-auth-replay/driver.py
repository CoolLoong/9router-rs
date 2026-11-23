import base64
import concurrent.futures
import datetime as dt
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid

MODE, *ARGS, CASE = sys.argv[1:]
ROOT = Path(__file__).resolve().parent
VERSION = json.loads((ROOT.parent.parent / "package.json").read_text())["version"]
KEY = "ksk_fixture_only"
UNKNOWN = "ksk_fixture_invalid"
REFRESH = "fixture-oidc-refresh"
ROTATED = "fixture-rotated-oidc-refresh"
CLIENT = "fixture-oidc-client"
SECRET = "fixture-oidc-client-secret"
EMAIL = "aws-fixture@example.invalid"
PROFILE = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE"
OTHER_PROFILE = "arn:aws:codewhisperer:us-east-1:123456789012:profile/OTHER"
REQUESTS = []


def jwt(**claims):
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    return f"eyJmaXh0dXJlIjp0cnVlfQ.{payload}.synthetic"


AWS_TOKEN = jwt(email=EMAIL)
AWS_TOKEN_2 = jwt(email=EMAIL, issue=2)
KEY_JWT = jwt(preferred_username="key-fixture@example.invalid")


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, value, headers=None):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass

    def native_client(self):
        return MODE == "reference" or (
            self.headers.get("User-Agent") == f"9Router/{VERSION}"
            and not self.headers.get("X-Amz-User-Agent")
            and not self.headers.get("X-Amzn-Kiro-Agent-Mode")
        )

    def do_GET(self):
        region = "eu-west-2" if CASE == "key-eu" else "us-east-1"
        key = {"key-jwt-email": KEY_JWT, "key-rejected": UNKNOWN}.get(CASE, KEY)
        valid = (
            self.native_client() and self.path == f"/q/{region}/ListAvailableModels?origin=AI_EDITOR"
            and self.headers.get("Authorization") == "Bearer " + key
            and self.headers.get("TokenType") == "API_KEY"
            and self.headers.get("Accept") == "application/json"
        )
        REQUESTS.append({"method": "GET", "path": self.path, "valid": valid})
        if CASE == "key-rejected":
            return self.send(401, {"error": "Unauthorized", "reflectedKey": key})
        if CASE == "key-redirect":
            return self.send(302, {"models": [{"modelId": "fixture"}]}, {"Location": "http://127.0.0.1:1"})
        return self.send(200, {"models": [] if CASE == "key-empty" else [{"modelId": "fixture"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        region = "eu-west-2" if CASE == "idc-regional" else "us-east-1"
        valid = (
            self.native_client() and self.path == f"/oidc/{region}/token"
            and self.headers.get("Content-Type") == "application/json"
            and not self.headers.get("Authorization")
            and body == {"clientId": CLIENT, "clientSecret": SECRET, "refreshToken": REFRESH,
                         "grantType": "refresh_token"}
        )
        REQUESTS.append({"method": "POST", "path": self.path, "valid": valid})
        if CASE.startswith("idc-overlap") or CASE == "idc-client-disconnect":
            time.sleep(0.25)
        if CASE == "idc-rejected":
            return self.send(400, {"error": "access_denied", "reflected": SECRET + REFRESH})
        result = {
            "accessToken": AWS_TOKEN if len(REQUESTS) == 1 else AWS_TOKEN_2,
            "expiresIn": 1800, "profileArn": PROFILE,
        }
        if CASE != "idc-unrotated":
            result["refreshToken"] = ROTATED
        if CASE == "idc-missing-access":
            result.pop("accessToken")
        if CASE == "idc-fallback-expiry":
            result.pop("expiresIn")
            result.pop("profileArn")
        self.send(200, result)


def ready(child):
    deadline = time.monotonic() + 5
    buffer = bytearray()
    while time.monotonic() < deadline:
        if not select.select([child.stdout], [], [], 1)[0]:
            continue
        chunk = os.read(child.stdout.fileno(), 4096)
        if not chunk:
            break
        buffer.extend(chunk)
        line = next((line for line in buffer.split(b"\n")[:-1] if line.startswith(b"ready ")), None)
        if line:
            return int(line.split()[1])
    raise RuntimeError("OAuth fixture did not start")


def request(port, endpoint, payload, authorized=True):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=4)
    headers = {"Content-Type": "application/json"}
    if authorized:
        headers["x-replay-admin"] = "fixture"
    try:
        body = payload if isinstance(payload, str) else json.dumps(payload)
        conn.request("POST", f"/api/oauth/kiro/{endpoint}", body=body, headers=headers)
        response = conn.getresponse()
        raw = response.read()
        result = json.loads(raw) if response.getheader("Content-Type", "").startswith("application/json") else raw.decode()
        return {"status": response.status, "body": result}
    finally:
        conn.close()


def operations(port, database):
    endpoint = "api-key" if CASE.startswith("key") else "import"
    if endpoint == "api-key":
        data = {"apiKey": "  " + KEY + "  "}
        if CASE == "key-eu":
            data["region"] = "eu-west-2"
        if CASE == "key-jwt-email":
            data["apiKey"] = KEY_JWT
        if CASE == "key-rejected":
            data["apiKey"] = UNKNOWN
        if CASE == "key-invalid-region":
            data["region"] = "eu-west-2.invalid"
        if CASE == "key-missing":
            data = {}
        if CASE == "key-malformed":
            data = '{"broken"'
    else:
        data = {"refreshToken": " " + REFRESH + " ", "clientId": CLIENT, "clientSecret": SECRET}
        if CASE == "idc-regional":
            data["region"] = "eu-west-2"
        if CASE == "idc-bad-region":
            data["region"] = "us-east-1.invalid"
        if CASE == "idc-override-profile":
            data["profileArn"] = OTHER_PROFILE
        if CASE == "idc-proprietary":
            data.pop("clientId")
        if CASE == "idc-bad-profile":
            data["profileArn"] = "not-an-aws-profile"
        if CASE == "idc-malformed-client":
            data["clientId"] = True
        if CASE == "idc-missing-refresh":
            data.pop("refreshToken")
    if CASE.startswith("idc-overlap"):
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            first = executor.submit(request, port, endpoint, data)
            deadline = time.monotonic() + 3
            while not REQUESTS and not first.done() and time.monotonic() < deadline:
                time.sleep(.01)
            assert REQUESTS, "First exchange did not reach the mock"
            second_data = data | {"profileArn": OTHER_PROFILE} if CASE.endswith("-different") else data
            second = executor.submit(request, port, endpoint, second_data)
            return [first.result(), second.result()]
    if CASE == "idc-client-disconnect":
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=4)
        connection.request(
            "POST", "/api/oauth/kiro/import", json.dumps(data),
            {"Content-Type": "application/json", "x-replay-admin": "fixture"},
        )
        deadline = time.monotonic() + 3
        while not REQUESTS and time.monotonic() < deadline:
            time.sleep(.01)
        assert REQUESTS
        connection.close()
        while time.monotonic() < deadline:
            with sqlite3.connect(database) as db:
                if db.execute("SELECT COUNT(*) FROM providerConnections").fetchone()[0] == 1:
                    return []
            time.sleep(.01)
        raise AssertionError("The submitted refresh did not persist after the client disconnected")
    authorized = not CASE.endswith("-unauthorized")
    replies = [request(port, endpoint, data, authorized)]
    if CASE in {"key-repeated", "idc-reimport", "idc-distinct-profile"}:
        second_data = data | {"profileArn": OTHER_PROFILE} if CASE == "idc-distinct-profile" else data
        replies.append(request(port, endpoint, second_data, authorized))
    return replies


def snapshot(database, replies):
    with sqlite3.connect(database) as db:
        rows = db.execute(
            "SELECT id,provider,authType,name,email,priority,isActive,data,updatedAt FROM providerConnections ORDER BY priority"
        ).fetchall()
    entries = []
    ids = {}
    for idx, (id, provider, auth, name, email, priority, active, raw, updated) in enumerate(rows, 1):
        uuid.UUID(id)
        ids[id] = idx
        fields = json.loads(raw)
        access = fields.pop("accessToken")
        assert access in {KEY, KEY_JWT, AWS_TOKEN, AWS_TOKEN_2}
        fields["syntheticValidatedAccess"] = access in {KEY, KEY_JWT, AWS_TOKEN, AWS_TOKEN_2}
        if "refreshToken" in fields:
            refresh = fields.pop("refreshToken")
            assert refresh in {REFRESH, ROTATED}
            fields["refreshRotated"] = refresh == ROTATED
            metadata = fields["providerSpecificData"]
            assert metadata.pop("clientSecret") == SECRET and metadata.pop("clientId") == CLIENT
            metadata["storedClient"] = True
        expires = dt.datetime.fromisoformat(fields.pop("expiresAt"))
        expected = 31536000 if auth == "api_key" else 3600 if CASE == "idc-fallback-expiry" else 1800
        assert abs((expires - dt.datetime.fromisoformat(updated)).total_seconds() - expected) < 4
        fields["expirySeconds"] = expected
        entries.append({
            "id": idx, "provider": provider, "authType": auth, "name": name, "email": email,
            "priority": priority, "active": bool(active), "data": fields,
        })
    for reply in replies:
        if reply["status"] == 200:
            value = reply["body"]
            assert value["success"] is True and set(value["connection"]) == {"id", "provider", "email"}
            value["connection"]["id"] = ids[value["connection"]["id"]]
    serialized = json.dumps(replies)
    assert all(credential not in serialized for credential in [KEY, UNKNOWN, REFRESH, ROTATED, SECRET, AWS_TOKEN, AWS_TOKEN_2])
    return entries


def check(replies, saved):
    status_codes = [reply["status"] for reply in replies]
    if CASE == "idc-proprietary":
        assert status_codes == [501] and REQUESTS == [] and saved == []
    elif CASE in {"idc-bad-profile", "idc-malformed-client"}:
        assert status_codes == [400] and REQUESTS == [] and saved == []
    elif CASE in {"idc-rejected", "idc-missing-access", "key-redirect"}:
        assert status_codes == [500] and len(REQUESTS) == 1 and saved == []
    elif CASE in {"key-repeated", "idc-distinct-profile"}:
        assert status_codes == [200, 200] and len(saved) == len(REQUESTS) == 2
    elif CASE == "idc-overlap-identical":
        assert status_codes == [200, 200] and len(saved) == len(REQUESTS) == 1 and replies[0] == replies[1]
    elif CASE == "idc-overlap-different":
        assert status_codes == [200, 409] and len(saved) == len(REQUESTS) == 1
    elif CASE == "idc-client-disconnect":
        assert len(saved) == len(REQUESTS) == 1
    elif CASE == "idc-reimport":
        assert status_codes == [200, 200] and len(saved) == 1 and len(REQUESTS) == 2 and replies[0] == replies[1]
    elif status_codes == [200]:
        assert len(saved) == len(REQUESTS) == 1
    elif CASE not in {"key-rejected", "key-empty"}:
        assert REQUESTS == [] and saved == []


with tempfile.TemporaryDirectory(prefix="9router-kiro-auth-") as directory:
    with ThreadingHTTPServer(("127.0.0.1", 0), Upstream) as aws:
        worker = threading.Thread(target=aws.serve_forever, kwargs={"poll_interval": .02}, daemon=True)
        worker.start()
        endpoint = f"http://127.0.0.1:{aws.server_port}"
        command = ["node", "--no-warnings", str(ROOT / "original.mjs")] if MODE == "reference" else [ARGS[0]]
        env = {"PATH": os.environ["PATH"], "HOME": directory, "DATA_DIR": directory, "NO_PROXY": "127.0.0.1"}
        with subprocess.Popen(command + [directory, endpoint], cwd=directory, env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as gateway:
            try:
                database = Path(directory) / "db/data.sqlite"
                replies = operations(ready(gateway), database)
                saved = snapshot(database, replies)
                assert all(entry["valid"] for entry in REQUESTS)
                check(replies, saved)
                print(json.dumps({"responses": replies, "requests": REQUESTS, "saved": saved},
                                 sort_keys=True, separators=(",", ":")))
            finally:
                if gateway.poll() is None:
                    gateway.terminate()
                try:
                    gateway.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    gateway.kill()
                    gateway.wait(timeout=3)
                aws.shutdown()
                worker.join()
