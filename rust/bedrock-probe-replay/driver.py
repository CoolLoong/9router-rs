from collections import defaultdict, deque
import hashlib
import hmac
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


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS.pop(0)).resolve())
CHECK = ARGS[0]
AUTH = {"x-9r-cli-token": "68977b6fd9422fbb"}
AWS = {
    "us-west-2": ("ASIA_STATIC_PROBE_FIXTURE", "offline-probe-static-secret", "offline-probe-static-session"),
    "eu-central-1": ("ASIA_NAMED_PROBE_FIXTURE", "offline-probe-named-secret", "offline-probe-named-session"),
    "us-east-2": ("AKIA_XAI_PROBE_FIXTURE", "offline-probe-xai-secret", None),
}
LEDGER = []
PROBLEMS = []
QUEUE = defaultdict(deque)


def settings(region, profile=False):
    if profile:
        return {"providerSpecificData":{"region":region,"profile":"dashboard"}}
    access, secret, session = AWS[region]
    return {"apiKey":secret,"providerSpecificData":{"region":region,"accessKeyId":access,"sessionToken":session}}


def seed(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
                   check=True, cwd=ROOT, stderr=subprocess.DEVNULL)
    credentials = home / ".aws/credentials"
    credentials.parent.mkdir()
    access, secret, session = AWS["eu-central-1"]
    marker = home / "ran"
    credentials.write_text(
        f"[dashboard]\naws_access_key_id={access}\naws_secret_access_key={secret}\naws_session_token={session}\n"
        f"[process]\ncredential_process=/usr/bin/touch {marker}\n"
    )
    database = data / "db/data.sqlite"
    accounts = [
        ("bedrock-static", "bedrock", settings("us-west-2"), 1),
        ("bedrock-profile", "bedrock", settings("eu-central-1", True), 2),
        ("bedrock-xai-static", "bedrock-xai", settings("us-east-2"), 1),
    ]
    with sqlite3.connect(database) as sql:
        sql.executemany(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,?,'apikey','Fixture Bedrock',?,1,?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""",
            [(identifier, provider, priority, json.dumps(body)) for identifier, provider, body, priority in accounts]
        )
    return database, marker


def signed(request, region):
    expected_host = f"bedrock.{region}.amazonaws.com"
    assert request.headers["Host"] in (expected_host, "127.0.0.1:52989")
    assert request.headers["Accept"] == "application/json"
    assert request.headers.get("Content-Length", "0") == "0"
    access, secret, token = AWS[region]
    assert request.headers["X-Amz-Security-Token"] == token
    auth = request.headers["Authorization"]
    parsed = re.fullmatch(r"AWS4-HMAC-SHA256 Credential=([^/]+)/([^,]+), SignedHeaders=([^,]+), Signature=([0-9a-f]{64})", auth)
    assert parsed, "Missing Bedrock authorization"
    key, scope, signed_headers, signature = parsed.groups()
    assert key == access
    date, scope_region, service, suffix = scope.split("/")
    assert (scope_region, service, suffix) == (region, "bedrock", "aws4_request")
    names = signed_headers.split(";")
    assert set(names) == {"accept","host","x-amz-date"} | ({"x-amz-security-token"} if token else set())
    headers = "".join(
        name + ":" + (expected_host if name == "host" else request.headers[name]) + "\n"
        for name in names
    )
    body_hash = hashlib.sha256(b"").hexdigest()
    canonical = f"GET\n/foundation-models\n\n{headers}\n{signed_headers}\n{body_hash}"
    stamp = request.headers["x-amz-date"]
    payload = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n" + hashlib.sha256(canonical.encode()).hexdigest()
    current = ("AWS4" + secret).encode()
    for item in (date, region, service, suffix):
        current = hmac.new(current, item.encode(), hashlib.sha256).digest()
    assert hmac.compare_digest(hmac.new(current, payload.encode(), hashlib.sha256).hexdigest(), signature)


class MockAWS(ThreadingHTTPServer):
    allow_reuse_address = True


class AWSHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, extra=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        PROBLEMS.append("Unexpected POST to AWS")
        self.reply(422, {})

    def do_GET(self):
        try:
            match = re.fullmatch(r"/bedrock-probe/([^/]+)/foundation-models", self.path)
            assert match, "Unexpected provider route " + self.path
            region = match.group(1)
            signed(self, region)
            stage = QUEUE[region].popleft()
            LEDGER.append((region, stage))
            if stage == "success":
                self.reply(200, {"modelSummaries":[]})
            elif stage == "access-denied":
                self.reply(403, {"message":"No ListFoundationModels permission"},
                           {"X-Amzn-Errortype":"AccessDeniedException:aws"})
            elif stage == "signature":
                self.reply(403, {"message":"The fixture signature was rejected"}, {"X-Amzn-Errortype":"InvalidSignatureException:aws"})
            elif stage == "generic":
                self.reply(403, {})
            elif stage == "redaction":
                access, secret, session = AWS[region]
                self.reply(403, {"message":f"echo {access} {secret} {session} {self.headers['Authorization']}"},
                           {"X-Amzn-Errortype":"UnrecognizedClientException"})
            elif stage == "redirect":
                self.reply(307, {}, {"Location":"http://127.0.0.1:52989/never"})
            elif stage == "timeout":
                time.sleep(8.5)
                try:
                    self.reply(200, {})
                except OSError:
                    pass
            else:
                raise AssertionError("Unknown synthetic probe stage")
        except (AssertionError, KeyError, IndexError, ValueError) as error:
            PROBLEMS.append(str(error) or self.path)
            self.reply(403, {"message":"Mock AWS probe invariant failed"})


def request(port, path, payload=None, headers=AUTH, method=None):
    method = method or ("POST" if payload is not None else "GET")
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=13)
    try:
        calculated = dict(headers)
        if payload is not None:
            calculated["Content-Type"] = "application/json"
            payload = json.dumps(payload)
        connection.request(method, path, payload, calculated)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def wait(child, port):
    deadline = time.monotonic() + 10
    while child.poll() is None and time.monotonic() < deadline:
        if REFERENCE and select.select([child.stdout], [], [], .25)[0]:
            line = child.stdout.readline().decode()
            if line.startswith("ready "):
                return int(line.split()[1])
        if not REFERENCE:
            try:
                if request(port, "/api/health")[0] == 200:
                    return port
            except OSError:
                time.sleep(.025)
    raise RuntimeError("Bedrock fixture process did not start")


def stored(database, identifier):
    with sqlite3.connect(database) as sql:
        row, = sql.execute("SELECT data FROM providerConnections WHERE id=?", (identifier,)).fetchone()
    fields = json.loads(row)
    return {"status":fields.get("testStatus"),"error":fields.get("lastError"),"dated":bool(fields.get("lastErrorAt"))}


def compare(port, database):
    QUEUE["us-west-2"].extend(["success","signature","generic","success"])
    QUEUE["eu-central-1"].extend(["access-denied","success","signature"])
    QUEUE["us-east-2"].extend(["access-denied"])
    recent = {}
    for tag, provider, account, outcome in [
        ("static","bedrock",settings("us-west-2"),{"valid":True,"error":None}),
        ("named","bedrock",settings("eu-central-1", True),{"valid":True,"error":None}),
        ("iam-denied","bedrock-xai",settings("us-east-2"),{"valid":True,"error":None}),
        ("signature","bedrock",settings("us-west-2"),{
            "valid":False,"error":"AWS rejected the credentials (InvalidSignatureException): The fixture signature was rejected",
        }),
    ]:
        actual = request(port, "/api/providers/validate", {"provider":provider} | account)
        assert actual == (200,outcome), (tag,actual,PROBLEMS)
        recent[tag] = outcome
    fail = request(port, "/api/providers/bedrock-static/test", {}, method="POST")
    assert fail == (200,{"valid":False,"error":"AWS rejected the credentials (HTTP 403)","refreshed":False}), fail
    assert stored(database, "bedrock-static") == {"status":"error","error":fail[1]["error"],"dated":True}
    valid = request(port, "/api/providers/bedrock-profile/test", {}, method="POST")
    assert valid == (200,{"valid":True,"error":None,"refreshed":False}), valid
    assert stored(database, "bedrock-profile") == {"status":"active","error":None,"dated":False}
    status, batch = request(port, "/api/providers/test-batch", {"mode":"provider","providerId":"bedrock"})
    assert status == 200 and batch["summary"] == {"total":2,"passed":1,"failed":1}, batch
    rows = [[row["connectionId"],row["valid"],row["error"]] for row in batch["results"]]
    assert stored(database, "bedrock-static") == {"status":"active","error":None,"dated":False}
    assert stored(database, "bedrock-profile") == {"status":"error","error":rows[1][2],"dated":True}
    count = len(LEDGER)
    for provider, identifier in [("bedrock","bedrock-static"),("bedrock-xai","bedrock-xai-static")]:
        observed = request(port, f"/api/providers/{identifier}/models")
        assert observed == (400,{"error":f"Provider {provider} does not support models listing"})
    assert len(LEDGER) == count
    print(json.dumps({"validation":recent,"batch":rows,"calls":LEDGER,"known_listing":400},
                     separators=(",", ":"), sort_keys=True))


def strict(port, database, marker):
    assert not REFERENCE
    before = len(LEDGER)
    status, catalog = request(port, "/v1/models", headers={"Authorization": "Bearer offline-chat-key"})
    assert status == 200
    identifiers = [model["id"] for model in catalog["data"]]
    assert any(name.startswith("br/") for name in identifiers)
    assert any(name.startswith("brx/") for name in identifiers)
    candidate = {"provider":"bedrock","providerSpecificData":{"profile":"process","region":"us-east-1"}}
    unauthorized = request(port, "/api/providers/validate", candidate, {})
    assert unauthorized == (401,{"error":"Unauthorized"})
    assert not marker.exists()
    invalid = request(port, "/api/providers/validate", candidate)
    assert invalid == (200,{"valid":False,"error":"AWS credential_process returned invalid credentials"})
    assert marker.exists() and len(LEDGER) == before
    assert request(port, "/api/providers/validate", {"provider":"bedrock","apiKey":"test","providerSpecificData":{"region":"invalid.example"}}) == (
        200,{"valid":False,"error":"Invalid AWS region"},
    )
    assert len(LEDGER) == before
    QUEUE["eu-central-1"].extend(["redaction","redirect","timeout"])
    redacted = request(port, "/api/providers/bedrock-profile/test", {})
    status, value = redacted
    assert status == 200 and value["valid"] is False and "[redacted]" in value["error"], redacted
    assert all(text not in value["error"] for text in AWS["eu-central-1"])
    assert stored(database, "bedrock-profile")["error"] == value["error"]
    assert request(port, "/api/providers/validate", {"provider":"bedrock"} | settings("eu-central-1", True)) == (
        200,{"valid":False,"error":"fetch failed"},
    )
    assert request(port, "/api/providers/validate", {"provider":"bedrock"} | settings("eu-central-1", True)) == (
        200,{"valid":False,"error":"AWS credential probe timed out"},
    )
    assert len(LEDGER) == before + 3
    print("Bedrock probes used read-only SigV4, rejected unauthorized process requests and invalid output, and redacted saved errors")


with tempfile.TemporaryDirectory(prefix="9router-bedrock-probe-") as temp:
    scratch = Path(temp)
    home = scratch / "home"
    home.mkdir()
    data = scratch / "data"
    empty_path = scratch / "empty-path"
    empty_path.mkdir()
    database, marker = seed(data, home)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    environment = {
        "PATH":os.environ["PATH"] if REFERENCE else str(empty_path),
        "HOME":str(home),"USERPROFILE":str(home),"DATA_DIR":str(data),"PORT":str(port),
        "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallbacks"),
    }
    mock = MockAWS(("127.0.0.1", 52989), AWSHandler)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    command = ["node","--no-warnings",str(Path(__file__).with_name("original.mjs")),str(data)] if REFERENCE else [BINARY,"--no-browser"]
    app = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        port = wait(app, port)
        if CHECK == "strict":
            strict(port, database, marker)
        else:
            compare(port, database)
        assert not PROBLEMS, PROBLEMS
        assert all(not steps for steps in QUEUE.values()), "Unconsumed local probe fixtures"
        if not REFERENCE:
            assert (scratch / "fallbacks").read_bytes() == b""
    finally:
        app.terminate()
        try:
            app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            app.kill()
            app.wait(timeout=3)
        mock.shutdown()
        mock.server_close()
