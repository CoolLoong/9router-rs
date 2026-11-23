import base64
import datetime
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
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS, CASE = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS[0]).resolve())
AUTH = {"X-9r-Cli-Token": "68977b6fd9422fbb"}
USAGE = "/api/usage/grok-weekly-connection"
MEDIA = "application/grpc-web+proto"
REQUESTS = []
FAILURES = []
MODE_REPLY = CASE


def b64(value):
    return base64.urlsafe_b64encode(value).decode().rstrip("=")


JWT = "fixture." + b64(b'{"tier":5.0,"sub":"fixture-user"}') + ".signature"
BEARER = "synthetic-grok-weekly-token" if CASE == "raw" else JWT


def varint(value):
    raw = bytearray()
    while value >= 128:
        raw.append((value & 127) | 128)
        value >>= 7
    raw.append(value)
    return bytes(raw)


def field(number, payload):
    return varint(number << 3 | 2) + varint(len(payload)) + payload


def frame(flag, payload):
    return bytes([flag]) + struct.pack(">I", len(payload)) + payload


def weekly(case):
    if case == "raw":
        return field(1, b"\x09" + struct.pack("<d", .42))
    if case == "zero":
        return frame(0, field(1, b""))
    if case == "malformed":
        return frame(0, b"\x0a\x08\x01")
    timestamp = b"\x08" + varint(1893456000) + b"\x10" + varint(123_499_999)
    payload = field(1, b"\x0d" + struct.pack("<f", .35) + field(5, timestamp))
    trailers = b"grpc-status: 7\r\n" if case == "grpc-denied" else b"grpc-status: 0\r\n"
    return frame(0, payload) + frame(0x80, trailers)


class Credits(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        FAILURES.append("Unexpected quota GET")
        self.send_error(422)

    def do_POST(self):
        try:
            assert self.path == "/grok-build-weekly", self.path
            assert self.headers.get_all("Authorization") == ["Bearer " + BEARER]
            assert self.headers["Content-Type"] == MEDIA and self.headers["Accept"] == MEDIA
            assert self.headers["X-Grpc-Web"] == "1"
            assert self.rfile.read(int(self.headers["Content-Length"])) == b"\x00" * 5
            assert all(not name.lower().startswith("x-grok-client") for name in self.headers)
            assert all(self.headers.get(name) is None for name in (
                "X-Xai-Token-Auth", "X-Email", "X-Userid", "Referer", "Cookie", "Origin", "Proxy-Authorization"))
            if not REFERENCE:
                assert self.headers["User-Agent"] == "9Router"
            REQUESTS.append({"protocol": "grpc-web", "readOnly": True, "requestBytes": 5})
            data = weekly(MODE_REPLY)
            status = 403 if MODE_REPLY == "denied" else 307 if MODE_REPLY == "redirect" else 200
            self.send_response(status)
            if MODE_REPLY == "redirect":
                self.send_header("Location", "http://127.0.0.1:52989/never")
            self.send_header("Content-Type", MEDIA)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (AssertionError, KeyError, ValueError) as error:
            FAILURES.append(str(error) or "Unexpected weekly usage request")
            self.send_error(422)


def request(port, path=USAGE, headers=AUTH):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        connection.request("GET", path, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def record(database, state="active"):
    with sqlite3.connect(database) as sql:
        fields = {"providerSpecificData": {"authMethod": "device_code", "email": "grok-fixture@example.invalid",
                     "userId": "fixture-account-id", "subscriptionTier": "grok_build_pro"},
                  "refreshToken": "unused-fixture-grok-refresh"}
        if state != "missing":
            fields["accessToken"] = BEARER
        fields["expiresAt"] = "2000-01-01T00:00:00Z" if state == "expired" else (
            datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=60)).isoformat()
        sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES ('grok-weekly-connection','grok-cli','oauth','Grok fixture',1,1,?,'2026-01-01','2026-01-01')
                       ON CONFLICT(id) DO UPDATE SET data=excluded.data""", (json.dumps(fields),))


def run_native(scratch):
    global MODE_REPLY
    home, data, empty = (scratch / name for name in ("home", "data", "empty-path"))
    home.mkdir()
    empty.mkdir()
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, cwd=ROOT,
                   env={"PATH": os.environ["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
    database = data / "db/data.sqlite"
    record(database)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data), "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallbacks")}
    with (scratch / "app.log").open("w+") as logs:
        app = subprocess.Popen([BINARY, "--no-browser"], cwd=ROOT, env=env, stdout=logs, stderr=logs)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if request(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.03)
            else:
                raise AssertionError("Native Grok quota gateway did not start")
            if CASE == "strict":
                assert request(port, headers={}) == (401, {"error": "Unauthorized"})
                assert not REQUESTS
                for state, message in [("missing", "not available"), ("expired", "expired")]:
                    record(database, state)
                    status, response = request(port)
                    assert status == 200 and message in response["message"] and not REQUESTS
                record(database)
                for reply in ("grpc-denied", "redirect"):
                    MODE_REPLY = reply
                    status, response = request(port)
                    assert status == 200 and response["quotas"] == {} and "unavailable" in response["message"]
                assert len(REQUESTS) == 2
                result = None
            else:
                status, response = request(port)
                assert status == 200, (status, response, FAILURES)
                if CASE == "denied":
                    assert "re-authorize" in response["message"]
                else:
                    assert response["plan"] == ("Grok Build Pro" if CASE == "raw" else "SuperGrok Heavy")
                result = {"quota": response.get("quotas", {}).get("Weekly SuperGrok")}
                assert len(REQUESTS) == 1
            with sqlite3.connect(database) as sql:
                (raw,), = sql.execute("SELECT data FROM providerConnections WHERE id='grok-weekly-connection'")
                assert json.loads(raw)["refreshToken"] == "unused-fixture-grok-refresh"
                assert not json.loads(raw).get("grokCliRefreshAttempt")
            assert not FAILURES and not (scratch / "fallbacks").read_bytes()
            return result
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr, end="")
            raise
        finally:
            app.terminate()
            app.wait(timeout=4)


with tempfile.TemporaryDirectory(prefix="9router-grok-weekly-") as directory:
    scratch = Path(directory)
    upstream = ThreadingHTTPServer(("127.0.0.1", 52989), Credits)
    threading.Thread(target=upstream.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    try:
        if REFERENCE:
            program = Path(__file__).with_name("original.mjs")
            raw = subprocess.check_output(["node", "--no-warnings", str(program)], input=json.dumps(BEARER), text=True,
                                          env={"PATH": os.environ["PATH"], "HOME": str(scratch)}, timeout=6)
            result = json.loads(raw)
            assert len(REQUESTS) == 1 and not FAILURES
        else:
            result = run_native(scratch)
        if CASE == "strict":
            print("Grok weekly usage accepts only the saved bearer, skips expired credentials, refuses redirect and gRPC failure, and leaves credentials unchanged.")
        else:
            assert (result["quota"] is not None) == (CASE in {"framed", "raw", "zero"}), result
            print(json.dumps({**result, "requests": REQUESTS}, sort_keys=True))
    finally:
        upstream.shutdown()
        upstream.server_close()
