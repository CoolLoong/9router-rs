from concurrent.futures import ThreadPoolExecutor
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
from threading import Thread
import time

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
one = "sk-ws-fixture-first-local"
two = "sk-ws-fixture-second-local"
firebase = "eyJoffline-firebase.e30.synthetic"
endpoint = "/api/oauth/windsurf/exchange"
vendor = "https://server.codeium.com"
attempted = []


class Trap(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.reject()

    def do_POST(self):
        self.reject()

    def reject(self):
        attempted.append(self.path)
        self.send_response(500)
        self.send_header("Content-Length", "0")
        self.end_headers()


def call(path, body=None, headers=None):
    client = HTTPConnection("127.0.0.1", port, timeout=8)
    request_headers = dict(auth if headers is None else headers)
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    try:
        client.request("POST" if body is not None else "GET", path,
                       None if body is None else json.dumps(body), request_headers)
        response = client.getresponse()
        raw = response.read().decode()
        assert all(secret not in raw for secret in (one, two, firebase))
        if path == endpoint and response.status != 503:
            assert response.getheader("Cache-Control") == "no-store"
        return response.status, json.loads(raw)
    finally:
        client.close()


def saved():
    with sqlite3.connect(database) as db:
        db.row_factory = sqlite3.Row
        rows = db.execute("""SELECT provider,authType,name,email,priority,isActive,data
                             FROM providerConnections WHERE provider='windsurf' ORDER BY priority""").fetchall()
    return [dict(row) | {"data": json.loads(row["data"])} for row in rows]


def import_key(body):
    status, result = call(endpoint, body)
    assert status == 200 and result["success"] is True
    connection = result["connection"]
    assert set(result) == {"success", "connection"} and set(connection) == {"id", "provider"}
    assert connection["provider"] == "windsurf"
    return connection["id"]


def compare():
    given = [{"code": one}, {"code": "  bEaReR\u00a0" + two + "\n", "apiServerUrl": "http://127.0.0.1:52989/illegal"}, {"code": one}]
    if reference:
        output = subprocess.check_output([
            "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
            str(root / "rust/reference/windsurf-import-reference.mjs"), json.dumps(given),
        ], cwd=root, env={"PATH": os.environ["PATH"]})
        rows = json.loads(output)
    else:
        ids = [import_key(body) for body in given]
        assert len(set(ids)) == len(ids)
        rows = saved()
        assert fallback_log.read_bytes() == b""
    assert len(rows) == 3
    for index, (row, key) in enumerate(zip(rows, (one, two, one)), 1):
        assert row["data"].pop("accessToken") == key
        assert row == {"provider": "windsurf", "authType": "api_key", "name": None, "email": None,
                       "priority": index, "isActive": 1, "data": {"testStatus": "active", "providerSpecificData": {
                           "authMethod": "imported", "apiServerUrl": vendor, "firebaseIdToken": None,
                       }}}
    assert not attempted
    print(json.dumps(rows, sort_keys=True))


def strict():
    assert call(endpoint, {"code": one}, headers={}) == (401, {"error": "Unauthorized"})
    assert call(endpoint, {}) == (400, {"error": "Missing token or callback URL"})
    assert call(endpoint, {"code": "mistyped"}) == (400, {"error": "Enter a Windsurf API key beginning with sk-ws-"})
    for malformed in ("sk-ws-", "sk-ws-abc#fragment", "sk-ws-abc?callback", one + "\nextra", "sk-ws-错误"):
        assert call(endpoint, {"code": malformed}) == (400, {"error": "Invalid Windsurf API key"})
    assert not saved() and fallback_log.read_bytes() == b""

    with ThreadPoolExecutor(4) as pool:
        ids = list(pool.map(import_key, [{"code": one, "baseUrl": "http://127.0.0.1:52989/forbidden"}] * 4))
    assert len(set(ids)) == 4
    rows = saved()
    assert [row["priority"] for row in rows] == [1, 2, 3, 4]
    assert all(row["data"]["accessToken"] == one and row["name"] is None for row in rows)
    assert all(row["data"]["providerSpecificData"]["apiServerUrl"] == vendor for row in rows)

    with sqlite3.connect(database) as db:
        db.execute("""UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData.firebaseIdToken',?)
                      WHERE id=?""", (firebase, ids[0]))
    status, result = call(f"/api/providers/{ids[0]}")
    assert status == 200 and "firebaseIdToken" not in json.dumps(result)
    status, result = call("/api/providers")
    assert status == 200 and ids[0] in json.dumps(result) and "firebaseIdToken" not in json.dumps(result)
    assert not attempted and fallback_log.read_bytes() == b""
    assert all(secret.encode() not in output_log.read_bytes() for secret in (one, two, firebase))

    rejected = (500, {"error": "Windsurf RegisterUser failed"})
    assert call(endpoint, {"code": firebase}) == rejected
    assert call(endpoint, {"code": f"http://localhost:8080/windsurf-auth-callback?access_token={firebase}"}) == rejected
    status, authorized = call("/api/oauth/windsurf/authorize")
    assert status == 200 and authorized["authUrl"].startswith("https://www.windsurf.com/windsurf/signin?")
    assert authorized["state"] and authorized["redirectUri"] == "http://localhost:8080/callback"
    assert fallback_log.read_bytes() == b""
    assert len(saved()) == 4 and attempted == ["/windsurf/exa.seat_management_pb.SeatManagementService/RegisterUser"] * 2
    print("Windsurf offline imports verified: authenticated, pinned endpoint/storage, isolated concurrent writes, native JWT rejection and private credentials")


def boot(directory, binary):
    global port, database, fallback_log, output_log
    location = Path(directory)
    home = location / "home"
    home.mkdir()
    data = location / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
    with sqlite3.connect(database) as db:
        db.execute("DELETE FROM providerConnections WHERE provider='windsurf'")
    fallback_log = location / "fallback.log"
    output_log = location / "output.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with output_log.open("wb") as out:
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
            "PATH": "", "HOME": str(home), "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
            "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
            "PORT": str(port), "HOSTNAME": "127.0.0.1",
        }, stdout=out, stderr=subprocess.STDOUT)
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


with ThreadingHTTPServer(("127.0.0.1", 52989), Trap) as trap, tempfile.TemporaryDirectory(prefix="9router-windsurf-import-") as directory:
    thread = Thread(target=trap.serve_forever, daemon=True)
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
        trap.shutdown()
        thread.join()
