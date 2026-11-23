from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from datetime import datetime, timedelta, timezone
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
REFERENCE = sys.argv[1] == "reference"
BINARY = None if REFERENCE else str(Path(sys.argv[2]).resolve())
STRICT = sys.argv[-1] == "strict"
ROUTE = "/api/providers/kiro-test-fixture/test"
ACCESS, REFRESH = "offline-kiro-test-access", "offline-kiro-test-refresh"
NEW_ACCESS, NEW_REFRESH = "offline-kiro-test-renewed", "offline-kiro-test-rotated"
CLIENT, SECRET = "offline-kiro-test-client-id", "offline-kiro-test-client-secret"
FAILED = "Token expired and refresh failed"
LEDGER, ERRORS, REDIRECTS = [], [], []
SCENARIO = "success"
TTL = 3600


def clock():
    return datetime.now(timezone.utc)


def timestamp(seconds):
    return (clock() + timedelta(seconds=seconds)).isoformat()


def details(method="idc", region="eu-central-1"):
    profile = f"arn:aws:codewhisperer:{region}:123456789012:profile/TEST"
    result = {"authMethod": method, "region": region, "profileArn": profile}
    if method in ("builder-id", "idc"):
        result |= {"clientId": CLIENT, "clientSecret": SECRET}
    return result


def save(value, auth="oauth"):
    with sqlite3.connect(DATABASE) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('kiro-test-fixture','kiro',?,'Synthetic Kiro',1,1,?,
                    '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                    (auth, json.dumps(value)))


def saved():
    with sqlite3.connect(DATABASE) as sql:
        row, = sql.execute("SELECT data FROM providerConnections WHERE id='kiro-test-fixture'").fetchone()
        return json.loads(row)


class Server(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, data, location=None):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        try:
            if self.path == "/capture":
                REDIRECTS.append(self.path)
                return self.reply(200, {})
            assert self.headers.get("Authorization") is None
            assert self.headers["Content-Type"] == "application/json"
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            account = saved()
            method = account["providerSpecificData"]["authMethod"]
            region = account["providerSpecificData"]["region"]
            if method in ("builder-id", "idc"):
                assert self.path == f"/kiro-refresh/oidc.{region}.amazonaws.com/token"
                assert payload == {"grantType":"refresh_token","clientId":CLIENT,"clientSecret":SECRET,"refreshToken":REFRESH}
            else:
                assert method in ("google", "github")
                assert self.path == "/kiro-refresh/prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
                assert payload == {"refreshToken":REFRESH}
                assert self.headers["User-Agent"] == ("kiro-cli/1.0.0" if REFERENCE else "9Router")
            if not REFERENCE:
                assert account["kiroRefreshAttempt"] and account["accessToken"] == ACCESS
            LEDGER.append(method)
            if SCENARIO == "concurrent":
                time.sleep(.15)
            if SCENARIO == "rejected":
                return self.reply(401, {"error":"invalid_grant","error_description":REFRESH+" "+SECRET})
            if SCENARIO == "redirect":
                return self.reply(307, {}, "http://127.0.0.1:52989/capture")
            if SCENARIO == "replace":
                save(account | {"accessToken":"offline-manual-replacement", "expiresAt":timestamp(3600),
                                "refreshToken":"offline-manual-grant", "kiroRefreshAttempt":None})
            self.reply(200, {"accessToken":NEW_ACCESS,"refreshToken":NEW_REFRESH,"expiresIn":TTL})
        except (AssertionError, KeyError, ValueError) as err:
            ERRORS.append(repr(err))
            self.reply(500, {})

    do_GET = do_POST


def request(path, body=None, cookie=None):
    headers = {"Content-Type":"application/json"}
    if cookie:
        headers["Cookie"] = cookie
    with closing(http.client.HTTPConnection("127.0.0.1", PORT, timeout=9)) as connection:
        connection.request("GET" if body is None else "POST", path, headers=headers,
                           body=None if body is None else json.dumps(body))
        response = connection.getresponse()
        return response.status, json.loads(response.read()), response.getheaders()


def checked(cookie, problem=None, refreshed=False):
    status, result, _ = request(ROUTE, {}, cookie)
    assert status == 200 and result == {"valid":problem is None,"error":problem,"refreshed":refreshed}, (status, result)
    assert not any(secret in json.dumps(result) for secret in (ACCESS, REFRESH, NEW_ACCESS, NEW_REFRESH, CLIENT, SECRET))
    row = saved()
    assert row["testStatus"] == ("active" if problem is None else "error")
    assert row.get("lastError") == problem
    assert bool(row.get("lastErrorAt")) == (problem is not None)
    return result


def grant(method="idc", region="eu-central-1", seconds=-30):
    return {"accessToken":ACCESS,"refreshToken":REFRESH,"expiresAt":timestamp(seconds),
            "providerSpecificData":details(method, region)}


def verify_rotation(expiry):
    row = saved()
    assert (row["accessToken"], row["refreshToken"], row["expiresIn"]) == (NEW_ACCESS, NEW_REFRESH, expiry)
    remaining = (datetime.fromisoformat(row["expiresAt"]) - clock()).total_seconds()
    assert expiry - 5 < remaining <= expiry + 1
    assert not row.get("kiroRefreshAttempt")
    assert row["providerSpecificData"]["profileArn"].endswith(":profile/TEST")


def compare(cookie):
    global SCENARIO, TTL
    results = {}
    for label, row, auth, error in (
        ("missing", grant() | {"accessToken":None}, "oauth", "No access token"),
        ("legacy-blank", {}, "", "No access token"),
        ("current", grant(seconds=307), "oauth", None),
        ("no-expiration", grant() | {"expiresAt":None}, "oauth", None),
        ("too-soon", grant(seconds=294) | {"refreshToken":None}, "oauth", "Token expired"),
        ("legacy-expiration", grant() | {"refreshToken":None, "expiresAt":None, "tokenExpiresAt":timestamp(-10)},
         "oauth", "Token expired"),
    ):
        save(row, auth)
        results[label] = checked(cookie, error)
    assert not LEDGER
    for method, region in (("idc","eu-central-1"), ("builder-id","us-east-1"),
                           ("google","us-east-1"), ("github","us-east-1")):
        save(grant(method, region, seconds=280 if method == "idc" else -30))
        TTL = 60 if method == "github" else 3600
        results[method] = checked(cookie, refreshed=True)
        verify_rotation(TTL)
    SCENARIO = "rejected"
    save(grant())
    results["rejected"] = checked(cookie, FAILED)
    assert saved()["accessToken"] == ACCESS and saved()["refreshToken"] == REFRESH
    assert LEDGER == ["idc","builder-id","google","github","idc"], (LEDGER, ERRORS)
    return results


def strict(cookie):
    global SCENARIO
    for row in (grant() | {"providerSpecificData":details("external_idp")},
                grant() | {"providerSpecificData":details("idc", "eu-central-1") | {"region":"us-east-1"}}):
        save(row)
        checked(cookie, FAILED)
        assert not LEDGER and not saved().get("kiroRefreshAttempt")
    SCENARIO = "concurrent"
    save(grant())
    with ThreadPoolExecutor(max_workers=6) as workers:
        results = list(workers.map(lambda _: request(ROUTE, {}, cookie)[:2], range(6)))
    assert all(status == 200 and value["valid"] and value["error"] is None for status, value in results), results
    assert any(value["refreshed"] for _, value in results) and LEDGER == ["idc"]
    verify_rotation(3600)
    checked(cookie)
    assert LEDGER == ["idc"]

    SCENARIO = "redirect"
    save(grant())
    checked(cookie, FAILED)
    checked(cookie, FAILED)
    assert not REDIRECTS and LEDGER == ["idc", "idc"]
    assert saved()["refreshToken"] == REFRESH and saved()["kiroRefreshAttempt"] != REFRESH

    SCENARIO = "replace"
    save(grant())
    checked(cookie)
    assert saved()["accessToken"] == "offline-manual-replacement"
    assert saved()["refreshToken"] == "offline-manual-grant" and LEDGER == ["idc"] * 3
    print("Kiro Test single-flight rotation, redirect refusal, issuer eligibility and concurrent credential edits passed.")


with tempfile.TemporaryDirectory(prefix="9router-kiro-test-") as folder:
    directory = Path(folder)
    home, data, empty = (directory / name for name in ("home","data","empty-path"))
    home.mkdir()
    empty.mkdir()
    subprocess.run([sys.executable,str(ROOT/"rust/reference/seed.py"),str(data)], cwd=home, check=True,
                   env={"HOME":str(home),"PATH":os.environ["PATH"],"NODE_NO_WARNINGS":"1"})
    DATABASE = data / "db/data.sqlite"
    server = ThreadingHTTPServer(("127.0.0.1",52989), Server)
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval":.05}, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        PORT = listener.getsockname()[1]
    fallback = directory / "fallback.log"
    env = {"HOME":str(home),"DATA_DIR":str(data),"PORT":str(PORT),"HOSTNAME":"127.0.0.1",
           "PATH":os.environ["PATH"] if REFERENCE else str(empty),"MODEL_CATALOG_SYNC":"off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH":"1","NINEROUTER_MIGRATION_FIXTURE":"1",
           "RUST_ANYTHING_FALLBACK_LOG":str(fallback)}
    args = ["node","--import","./rust/kiro-test-replay/original.mjs",".next/standalone/custom-server.js"] if REFERENCE else [
        BINARY,"--no-browser"]
    with (directory / "server.log").open("w+") as logs:
        app = subprocess.Popen(args,cwd=ROOT,env=env,stdout=logs,stderr=logs)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if request("/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise AssertionError("Kiro fixture gateway did not start")
            assert request(ROUTE,{})[:2] == (401, {"error":"Unauthorized"})
            status, _, headers = request("/api/auth/login", {"password":"123456"})
            assert status == 200
            cookie = next(value.split(";",1)[0] for name,value in headers if name.lower() == "set-cookie")
            assert request(ROUTE,{},cookie)[:2] == (404, {"error":"Connection not found"})
            if STRICT:
                strict(cookie)
            else:
                print(json.dumps(compare(cookie), sort_keys=True))
            assert not ERRORS and (not fallback.exists() or not fallback.read_bytes()), ERRORS
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr)
            print(LEDGER, ERRORS, file=sys.stderr)
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)
            server.shutdown()
            server.server_close()
