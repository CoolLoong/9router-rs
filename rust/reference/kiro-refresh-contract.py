from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
from threading import Thread
import time
from urllib.parse import parse_qs
import zlib

root = Path(__file__).resolve().parents[2]
reference = sys.argv[1] == "reference"
binary = None if reference else str(Path(sys.argv[1]).resolve())
compare = reference or sys.argv[2:] == ["compare"]
before = "offline-kiro-refresh-before"
after = "offline-kiro-refresh-after"
refresh_token = "offline-kiro-refresh-grant"
client_secret = "offline-kiro-refresh-client"
next_token = "offline-kiro-refresh-rotated"
auth = {"Authorization":"Bearer offline-chat-key","Content-Type":"application/json"}
ledger = []
failures = []
cases = [("idc","eu-central-1"),("builder-id","us-east-1"),
         ("google","us-east-1"),("github","us-east-1"),("external_idp","us-east-1")]


def frame(kind, payload):
    metadata = b""
    for name, value in {":message-type":"event",":event-type":kind,":content-type":"application/json"}.items():
        name, value = name.encode(), value.encode()
        metadata += bytes([len(name)]) + name + b"\x07" + struct.pack(">H", len(value)) + value
    data = json.dumps(payload).encode()
    prelude = struct.pack(">II", len(metadata) + len(data) + 16, len(metadata))
    full = prelude + struct.pack(">I", zlib.crc32(prelude)) + metadata + data
    return full + struct.pack(">I", zlib.crc32(full))


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, mime="application/json", extra=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            if self.path.startswith("/kiro-refresh/"):
                self.renew(raw)
            elif self.path.startswith("/kiro/"):
                self.chat(raw)
            else:
                raise AssertionError("Unexpected outbound destination")
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, b"{}")

    def renew(self, raw):
        ledger.append(("refresh", self.path))
        assert self.path == issued_url
        assert self.headers.get("Accept") == "application/json"
        assert self.headers.get("Authorization") is None
        assert self.headers.get("x-amz-sso-bearer") is None
        if method == "external_idp":
            assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
            assert parse_qs(raw.decode()) == {
                "grant_type":["refresh_token"],"refresh_token":[refresh_token],
                "client_id":["fixture-id"],"scope":["one offline_access"],
            }
            data = {"access_token":after,"refresh_token":next_token,"expires_in":3600}
        else:
            assert self.headers.get("Content-Type") == "application/json"
            if method in {"google","github"}:
                assert json.loads(raw) == {"refreshToken":refresh_token}
                assert self.headers.get("User-Agent") == ("kiro-cli/1.0.0" if reference else "9Router")
            else:
                assert json.loads(raw) == {"clientId":"fixture-id","clientSecret":client_secret,
                                           "refreshToken":refresh_token,"grantType":"refresh_token"}
            data = {"accessToken":after,"refreshToken":next_token,"expiresIn":3600}
        if not reference:
            assert stored()["kiroRefreshAttempt"] and stored()["accessToken"] == before
        if behavior == "concurrent":
            time.sleep(.15)
        if behavior == "disconnect":
            self.close_connection = True
            return
        if behavior == "redirect":
            return self.reply(307, b"{}", extra={"Location":"http://127.0.0.1:52989/never-follow"})
        if behavior == "negative-ttl":
            data["expiresIn"] = -10
        if behavior == "throttled" and sum(call[0] == "refresh" for call in ledger) == 1:
            return self.reply(429, b"{}")
        if behavior == "editing":
            with sqlite3.connect(database) as db:
                db.execute("""UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData.profileArn',?)
                    WHERE id='fixture-progress'""", (profile.replace("fixture-profile","changed-profile"),))
        data["profileArn"] = profile.replace("fixture-profile","other-profile")
        self.reply(200, json.dumps(data).encode())

    def chat(self, raw):
        path = self.path.split("/")
        token = self.headers.get("Authorization", "").removeprefix("Bearer ")
        name = "before" if token == before else "after"
        ledger.append(("chat", path[2], name))
        assert token in {before, after}
        assert self.headers.get("x-amz-sso-bearer") == token
        assert self.headers.get("x-amzn-codewhisperer-profile-arn") == profile
        assert json.loads(raw).get("profileArn") == profile
        assert (self.headers.get("TokenType") == "EXTERNAL_IDP") == (method == "external_idp")
        assert path[3] == region
        if behavior == "rejected" and token == before or behavior == "post-refresh-rejection":
            self.reply(401, json.dumps({"error":{"message":"Rejected " + token}}).encode())
            return
        assert token == after
        wire = frame("assistantResponseEvent", {"content":"OAuth fixture ready"}) + frame(
            "messageStopEvent", {"stopReason":"endTurn"})
        self.reply(200, wire, "application/vnd.amazon.eventstream")


def request(port, path, data=None, headers=auth):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        client.request("GET" if data is None else "POST", path,
                       body=None if data is None else json.dumps(data), headers=headers)
        response = client.getresponse()
        return response.status, response.read().decode()
    finally:
        client.close()


def send(port):
    code, raw = request(port, "/v1/chat/completions", {
        "model":"kr/auto","messages":[{"role":"user","content":"Synthetic query"}],"stream":False,
    })
    assert all(token not in raw for token in (before, after, refresh_token, next_token, client_secret))
    return code, json.loads(raw)


def boot(run_env):
    proc = subprocess.Popen([binary, "--no-browser"], cwd=root, env=run_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 14
    while time.monotonic() < deadline and proc.poll() is None:
        try:
            if request(int(run_env["PORT"]), "/api/health")[0] == 200:
                return proc
        except OSError:
            pass
        time.sleep(.025)
    raise RuntimeError("Fixture launcher did not start")


def stop(process):
    process.terminate()
    process.wait(timeout=8)


def stored():
    with sqlite3.connect(database) as db:
        row = db.execute("SELECT data FROM providerConnections WHERE id='fixture-progress'").fetchone()
        return json.loads(row[0])


def seed(auth_method, selected_region, outcome, lifetime=-10, token_url=None):
    global method, region, behavior, issued_url, profile
    method, region, behavior = auth_method, selected_region, outcome
    profile = f"arn:aws:codewhisperer:{region}:123456789012:profile/fixture-profile"
    details = {"authMethod":method,"profileArn":profile,"region":region,"keep":"preserved"}
    if method == "external_idp":
        endpoint = token_url or "https://login.microsoftonline.com/fixture/oauth2/v2.0/token"
        details.update(clientId="fixture-id", tokenEndpoint=endpoint, scopes=["one","offline_access"])
        issued_url = "/kiro-refresh/login.microsoftonline.com/fixture/oauth2/v2.0/token"
    elif method in {"google","github"}:
        issued_url = "/kiro-refresh/prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
    else:
        details.update(clientId="fixture-id", clientSecret=client_secret)
        issued_url = f"/kiro-refresh/oidc.{region if method == 'idc' else 'us-east-1'}.amazonaws.com/token"
    account = {"accessToken":before,"refreshToken":refresh_token,"providerSpecificData":details,
               "expiresAt":(datetime.now(timezone.utc) + timedelta(seconds=lifetime)).isoformat()}
    with sqlite3.connect(database) as db:
        db.execute("DELETE FROM providerConnections WHERE provider='kiro'")
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
             VALUES ('fixture-progress','kiro','oauth','Fixture',1,1,?,
               '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""", (json.dumps(account),))
    ledger.clear()
    failures.clear()


def success(result):
    code, value = result
    assert code == 200 and value["choices"][0]["message"]["content"] == "OAuth fixture ready", result
    assert not failures, failures
    assert stored()["accessToken"] == after and stored()["refreshToken"] == next_token
    assert stored()["kiroRefreshAttempt"] is None and stored()["expiresIn"] == 3600
    assert stored()["providerSpecificData"]["profileArn"] == profile
    assert stored()["providerSpecificData"]["keep"] == "preserved"


with tempfile.TemporaryDirectory(prefix="9router-kiro-refresh-") as temporary:
    directory = Path(temporary)
    no_node = directory / "bin"
    no_node.mkdir()
    data = directory / "data"
    database = data / "db/data.sqlite"
    fallback = directory / "fallbacks"
    subprocess.run([sys.executable, "rust/reference/seed.py", str(data)], cwd=root,
                   env={"PATH":os.environ["PATH"], "HOME":str(directory),"NODE_NO_WARNINGS":"1"}, check=True)
    with socket.socket() as port_probe:
        port_probe.bind(("127.0.0.1",0))
        port = port_probe.getsockname()[1]
    env = {"PATH":str(no_node), "HOME":str(directory), "PORT":str(port), "DATA_DIR":str(data),
           "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(fallback)}
    endpoint = ThreadingHTTPServer(("127.0.0.1",52989),Provider)
    Thread(target=endpoint.serve_forever, daemon=True).start()
    if reference:
        results = []
        try:
            for auth_method, selected_region in cases:
                seed(auth_method, selected_region, "normal", lifetime=20)
                result = subprocess.check_output(
                    ["node","--no-warnings","rust/reference/kiro-refresh-reference.mjs"],
                    cwd=root, input=json.dumps(stored()), text=True, timeout=8,
                )
                assert ledger == [("refresh",issued_url)] and not failures, failures
                results.append({"method":auth_method,"issuer":issued_url,**json.loads(result)})
        finally:
            endpoint.shutdown()
            endpoint.server_close()
        print(json.dumps(results, sort_keys=True))
        sys.exit()
    process = boot(env)
    try:
        results = []
        for auth_method, selected_region in cases:
            seed(auth_method, selected_region, "normal", lifetime=20)
            success(send(port))
            assert ledger == [("refresh",issued_url),("chat","q","after")]
            results.append({"method":auth_method,"issuer":issued_url,"accepted":True,
                            "rotated":True,"preservedProfile":True,"ttl":stored()["expiresIn"]})
        if compare:
            print(json.dumps(results, sort_keys=True))
            sys.exit()
        seed("idc","us-east-1","concurrent")
        with ThreadPoolExecutor(max_workers=2) as executor:
            both = list(executor.map(send, [port,port]))
        for result in both:
            success(result)
        assert ledger == [("refresh",issued_url),("chat","q","after"),("chat","q","after")]

        seed("idc","us-east-1","rejected",lifetime=7200)
        success(send(port))
        assert ledger == [("chat","q","before"),("chat","codewhisperer","before"),
                          ("refresh",issued_url),("chat","q","after")]
        seed("idc","us-east-1","post-refresh-rejection")
        assert send(port)[0] in {401,503}
        assert ledger == [("refresh",issued_url),("chat","q","after"),("chat","codewhisperer","after")]

        seed("idc","us-east-1","throttled")
        assert send(port)[0] == 503 and stored()["kiroRefreshAttempt"] is None
        success(send(port))
        assert ledger == [("refresh",issued_url),("refresh",issued_url),("chat","q","after")]
        for event in ["disconnect","redirect","negative-ttl","editing"]:
            seed("idc","us-east-1",event)
            first = send(port)
            assert first[0] == 401 and stored()["kiroRefreshAttempt"], first
            assert send(port)[0] == 401
            assert stored()["accessToken"] == before and stored()["refreshToken"] == refresh_token
            assert ledger == [("refresh",issued_url)] and not failures, (ledger,failures)
            if event == "editing":
                assert "changed-profile" in stored()["providerSpecificData"]["profileArn"]
        stop(process)
        process = boot(env)
        assert send(port)[0] == 401 and ledger == [("refresh",issued_url)]
        assert not fallback.read_text().strip()
        status, public = request(port, "/api/providers/fixture-progress",
                                 headers={"x-9r-cli-token":"68977b6fd9422fbb"})
        assert status == 200 and all(field not in public for field in (
            "kiroRefreshAttempt","clientSecret",client_secret,refresh_token,before,
        ))

        seed("external_idp","us-east-1","invalid",token_url="https://login.microsoftonline.com.invalid/oauth2/token")
        assert send(port)[0] == 503 and not ledger
        assert fallback.read_text().splitlines() == ["POST /v1/chat/completions"]
        print("Kiro OAuth refresh passed issuer scope, concurrency, cooldown, CAS and durable ambiguity checks.")
    finally:
        if process.poll() is None:
            stop(process)
        endpoint.shutdown()
        endpoint.server_close()
