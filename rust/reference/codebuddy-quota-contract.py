from datetime import datetime, timezone
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
access = "offline-codebuddy-quota-access"
key = "offline-codebuddy-quota-key"
refresh = "offline-codebuddy-quota-refresh"
target = "/codebuddy-quota/cn/v2/billing/meter/get-user-resource"
route = "/api/usage/codebuddy-fixture"
day = 86_400_000
base = 2_000_000_000_000
ledger = []
failures = []


def iso(milliseconds):
    return datetime.fromtimestamp(milliseconds / 1000, timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def envelope(accounts):
    return {"code": 0, "data": {"Response": {"Data": {"Accounts": accounts}}}}


def response_fixture():
    return envelope([
        {"CycleEndTime": base + 12 * day, "DeductionEndTime": base + 12 * day,
         "CapacityUsed": 7, "CapacitySizePrecise": "22.5", "CycleCapacityUsed": 90},
        {"PackageName": "fixture-club", "CycleStartTime": iso(base - day // 2),
         "CycleEndTime": str((base + day) // 1000), "DeductionEndTime": base + 4 * day,
         "CycleCapacityUsedPrecise": "6.54", "CycleCapacityUsed": 99, "CycleCapacitySize": 50,
         "CapacityUsed": 88},
        {"CycleStartTime": base - day // 2, "CycleEndTime": base + day, "DeductionEndTime": str(base + 4 * day),
         "CycleCapacityUsed": 8, "CycleCapacitySizePrecise": "60"},
        {"CycleStartTime": base - 2 * day, "CycleEndTime": base + 8 * day, "DeductionEndTime": base + 30 * day,
         "CycleCapacitySizePrecise": "100.125", "CycleCapacityUsed": "10.1"},
        {"CycleStartTime": base, "CycleEndTime": base + 30 * day, "DeductionEndTime": base + 60 * day,
         "CycleCapacityUsedPrecise": "", "CycleCapacityUsed": 92, "CycleCapacitySize": 200},
        {"CycleEndTime": base, "DeductionEndTime": base + 2 * day,
         "CapacityUsedPrecise": None, "CapacityUsed": 2, "CapacitySize": 18},
        {"CapacityUsed": True, "CapacitySize": 10},
    ])


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        ledger.append(self.path)
        try:
            assert self.path == target
            wanted = account.get("accessToken") or account.get("apiKey")
            assert self.headers.get_all("Authorization") == ["Bearer " + wanted]
            assert self.headers.get("Content-Type") == "application/json"
            assert self.headers.get("Accept") == "application/json"
            if reference:
                assert self.headers.get("User-Agent") == "CLI/2.108.1 CodeBuddy/2.108.1"
            else:
                assert self.headers.get("User-Agent") == "9Router"
                for name in ("x-codebuddy-request", "x-requested-with", "x-ide-type", "x-ide-name", "x-product"):
                    assert self.headers.get(name) is None
            assert self.rfile.read(int(self.headers["Content-Length"])) == b"{}"
            if outcome == "rejected":
                return self.reply(401, {"error": access})
            if outcome == "rate-limit":
                return self.reply(429, {"error": key})
            if outcome == "empty":
                return self.reply(200, envelope([]))
            if outcome == "api-error":
                return self.reply(200, {"code": 1, "msg": "Quota package inactive"})
            if outcome == "reflect-error":
                return self.reply(200, {"code": 2, "msg": f"Issuer echo {access} {key} {refresh}"})
            if outcome == "reflect-plan":
                return self.reply(200, envelope([{"PackageName": key, "CapacityUsed": 2, "CapacitySize": 10}]))
            if outcome == "missing-json":
                return self.reply(200, b"bad provider JSON " + key.encode())
            if outcome == "disconnect":
                self.close_connection = True
                return
            if outcome == "redirect":
                return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/wrong"})
            self.reply(200, response_fixture())
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def reply(self, status, payload, headers=None):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)


def configure(name, fields, provider="codebuddy-cn"):
    global outcome, account
    outcome, account = name, fields
    ledger.clear()
    failures.clear()
    if not reference:
        with sqlite3.connect(database) as db:
            db.execute("DELETE FROM providerConnections WHERE id='codebuddy-fixture'")
            db.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES ('codebuddy-fixture',?,?,'CodeBuddy local fixture',1,1,?,
                         '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""",
                       (provider, fields["authType"], json.dumps(fields)))


def request(path=route, headers=auth):
    conn = HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request("GET", path, headers=headers)
        response = conn.getresponse()
        raw = response.read().decode()
        assert all(credential not in raw for credential in (access, key, refresh))
        return response.status, json.loads(raw)
    finally:
        conn.close()


def read_account():
    with sqlite3.connect(database) as db:
        return db.execute("SELECT * FROM providerConnections WHERE id='codebuddy-fixture'").fetchone()


def run_reference(fields):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/codebuddy-quota-reference.mjs"), json.dumps(fields),
    ], cwd=root, env={"PATH": os.environ["PATH"]}))


def quotas(result):
    assert result["plan"] == "fixture-club"
    rows = result["quotas"]
    assert list(rows) == ["Daily", "Daily 2", "Weekly", "Monthly", "Bonus Pack 1", "Bonus Pack 2", "Bonus Pack 3"]
    expected = [(6.54, 50, base + day, True), (8, 60, base + day, True), (10.1, 100.125, base + 8 * day, True),
                (0, 200, base + 30 * day, True), (2, 18, base, False), (7, 22.5, base + 12 * day, False), (1, 10, None, False)]
    assert list(rows.values()) == [{"used": used, "total": total, "resetAt": iso(reset) if reset is not None else None,
                                    "unlimited": False, "recurring": recurring} for used, total, reset, recurring in expected]


def compare():
    reports = {}
    api = {"authType": "apikey", "apiKey": key}
    oauth = {"authType": "oauth", "accessToken": access, "apiKey": key, "refreshToken": refresh, "expiresAt": "2000-01-01T00:00:00Z"}
    for name, fields, expected in [
        ("resources", api, None),
        ("oauth-resources", oauth, None),
        ("rejected", oauth, "CodeBuddy CN credential invalid or expired."),
        ("rate-limit", api, "CodeBuddy CN quota API error (429)."),
        ("empty", api, "CodeBuddy CN connected. No credit package found."),
        ("api-error", api, "CodeBuddy CN quota error: Quota package inactive"),
        ("missing-credential", {"authType": "apikey"}, "CodeBuddy (codebuddy-cn) credential not available."),
    ]:
        configure(name, fields)
        if reference:
            result = run_reference(fields)
        else:
            before = read_account()
            status, result = request()
            assert status == 200 and read_account() == before
        assert not failures and ledger == ([] if name == "missing-credential" else [target]), (name, failures, ledger)
        if expected is None:
            quotas(result)
        else:
            assert result == {"message": expected}, (name, result)
        reports[name] = result
    if not reference:
        assert fallback_log.read_bytes() == b""
    print(json.dumps(reports, ensure_ascii=False, sort_keys=True))


def strict():
    oauth = {"authType": "oauth", "accessToken": access, "apiKey": key, "refreshToken": refresh, "expiresAt": "2000-01-01T00:00:00Z",
             "providerSpecificData": {"usageUrl": "https://example.invalid/receive-token", "baseUrl": "https://www.codebuddy.ai"}}
    configure("resources", oauth)
    assert request(headers={}) == (401, {"error": "Unauthorized"}) and not ledger
    before = read_account()
    status, result = request(route + "?force=1")
    assert status == 200 and read_account() == before and ledger == [target] and not failures
    quotas(result)

    for name, message in [("reflect-error", "CodeBuddy CN quota error: Issuer echo [redacted] [redacted] [redacted]"),
                          ("missing-json", "CodeBuddy (codebuddy-cn) error: invalid provider response"),
                          ("disconnect", "CodeBuddy (codebuddy-cn) error: fetch failed"),
                          ("redirect", "CodeBuddy CN quota API error (307).")]:
        configure(name, oauth)
        before = read_account()
        assert request() == (200, {"message": message})
        assert read_account() == before and ledger == [target] and not failures

    configure("reflect-plan", {"authType": "api_key", "apiKey": key})
    status, result = request()
    assert status == 200 and result["plan"] == "[redacted]" and ledger == [target] and not failures
    assert result["quotas"]["Bonus Pack 1"]["recurring"] is False
    assert fallback_log.read_bytes() == b""
    assert all(value.encode() not in output_log.read_bytes() for value in [access, key, refresh])

    configure("proxy-fallback", dict(oauth, providerSpecificData={
        "connectionProxyEnabled": True, "connectionProxyUrl": "http://127.0.0.1:52989",
    }))
    assert request()[0] == 503 and not ledger
    configure("intl-fallback", oauth, "codebuddy-intl")
    assert request()[0] == 503 and not ledger
    assert fallback_log.read_text().splitlines() == [f"GET {route}"] * 2
    print("CodeBuddy CN quota verified: read-only bearer request, exact refill and bonus mapping, stable sorting, no credential leakage; Intl stays delegated")


def boot(directory, binary):
    global port, database, fallback_log, output_log
    location = Path(directory)
    home = location / "home"
    home.mkdir()
    data = location / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
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
            if request("/api/health")[0] == 200:
                return process
        except OSError:
            pass
        time.sleep(.03)
    process.terminate()
    process.wait(timeout=8)
    raise RuntimeError("Native gateway did not start")


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as provider, tempfile.TemporaryDirectory(prefix="9router-codebuddy-quota-") as directory:
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
