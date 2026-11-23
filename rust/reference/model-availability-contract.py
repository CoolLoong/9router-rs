import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
route = "/api/models/availability"
cli = {"x-9r-cli-token": "68977b6fd9422fbb", "Content-Type": "application/json"}
provider = "availability-fixture"
other = "availability-other"
future = "2099-05-01T10:00:00.000Z"
expired = "2000-05-01T10:00:00.000Z"
stamp = "2026-11-19T00:00:00.000Z"


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    rows = [
        ("av-a-reset", provider, "Reset all", None, 1, -6,
         {"testStatus":"unavailable", "modelLock_target":future, "modelLock_other":future,
          "modelLock_expired":expired, "modelLock_blank":None, "lastError":"fixture limited",
          "lastErrorAt":expired, "errorCode":429, "backoffLevel":3, "rateLimitedUntil":future,
          "unrelated":{"kept":True}, "apiKey":"offline-availability-key"}),
        ("av-b-active", provider, "", "display@example.invalid", 1, -5,
         {"testStatus":"active", "modelLock_target":expired, "modelLock_numeric":4_100_000_000_000,
          "modelLock_invalid":"malformed", "modelLock_out_of_range":9_000_000_000_000_000,
          "lastError":"fixture diagnostic", "lastErrorAt":expired, "errorCode":400,
          "backoffLevel":1, "rateLimitedUntil":expired}),
        ("av-c-disabled", provider, None, None, 0, -4,
         {"testStatus":"unknown", "modelLock_target":future, "modelLock_other":future, "lastError":""}),
        ("av-d-no-target", provider, "Waiting", None, 1, -3,
         {"testStatus":"unavailable", "modelLock_invalid":"malformed", "modelLock_expired":expired,
          "modelLock_falsy":False, "lastError":None}),
        ("av-e-different", other, "Other provider", None, 1, -2,
         {"testStatus":"active", "modelLock_target":future}),
        ("av-f-all", provider, "Global locks", None, 1, -1,
         {"testStatus":"active", "modelLock___all":future, "modelLock_":future}),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections(id,provider,authType,name,email,isActive,priority,data,createdAt,updatedAt)
                          VALUES (?,?,'apikey',?,?,?,?,?,?,?)""",
                       [(id_, name, display, email, active, priority, json.dumps(record), stamp, stamp)
                        for id_, name, display, email, active, priority, record in rows])


def request(base, path=route, payload=None, authenticated=True):
    body = None if payload is None else (payload.encode() if isinstance(payload, str)
           else json.dumps(payload, separators=(",", ":")).encode())
    try:
        response = urlopen(Request(base + path, headers=cli if authenticated else {}, data=body), timeout=5)
    except HTTPError as error:
        response = error
    with response:
        return response.status, json.load(response)


def read_db(database):
    with sqlite3.connect(database) as db:
        return {id_: dict(data=json.loads(raw), updated_at=stamp, active=active)
                for id_, raw, stamp, active in db.execute("SELECT id,data,updatedAt,isActive FROM providerConnections WHERE id LIKE 'av-%'")}


def run_cases(base, database):
    snapshots = {}
    previous = read_db(database)

    def get(name):
        status, value = request(base)
        assert status == 200 and value["unavailableCount"] == len(value["models"]), (status, value)
        rows = [row for row in value["models"] if row["connectionId"].startswith("av-")]
        assert all(set(row) <= {"provider", "model", "status", "until", "connectionId", "connectionName", "lastError"} for row in rows)
        snapshots[name] = rows
        return rows

    def clear(name, selection):
        status, value = request(base, payload={"action":"clearCooldown", "provider":provider, "model":selection})
        assert (status, value) == (200, {"ok":True}), (name, status, value)
        snapshots[name] = value

    status, value = request(base, authenticated=False)
    assert (status, value) == (401, {"error":"Unauthorized"})
    before = get("active-locks")
    assert [(row["connectionId"], row["model"], row["status"]) for row in before] == [
        ("av-a-reset", "target", "cooldown"), ("av-a-reset", "other", "cooldown"),
        ("av-b-active", "numeric", "cooldown"), ("av-c-disabled", "target", "cooldown"),
        ("av-c-disabled", "other", "cooldown"), ("av-d-no-target", "__all", "unavailable"),
        ("av-e-different", "target", "cooldown"), ("av-f-all", "__all", "cooldown"),
        ("av-f-all", "__all", "cooldown"),
    ], before
    assert before[2]["connectionName"] == "display@example.invalid" and before[2]["until"] == 4_100_000_000_000
    assert before[3]["connectionName"] == "av-c-disabled" and before[3]["lastError"] is None
    assert "until" not in before[5]
    for selector, expected in [({}, 200), ([], 200), (True, 500), ([provider], 500)]:
        status, _ = request(base, payload={"action":"clearCooldown", "provider":selector, "model":"target"})
        assert status == expected and read_db(database) == previous, (selector, status)

    clear("clear-target", ["target"])
    after = read_db(database)
    reset = after["av-a-reset"]["data"]
    assert (reset["testStatus"], reset["backoffLevel"], reset["unrelated"]) == ("active", 0, {"kept":True})
    assert reset["apiKey"] == previous["av-a-reset"]["data"]["apiKey"]
    assert all(value is None for key, value in reset.items() if key.startswith("modelLock_") or key in {"errorCode","lastError","lastErrorAt","rateLimitedUntil"})
    assert after["av-b-active"]["data"] == previous["av-b-active"]["data"] | {"modelLock_target":None}
    assert after["av-c-disabled"]["data"] == previous["av-c-disabled"]["data"] | {"modelLock_target":None}
    assert after["av-c-disabled"]["active"] == 0
    for id_ in ("av-d-no-target", "av-e-different", "av-f-all"):
        assert after[id_] == previous[id_]
    assert all(after[id_]["updated_at"] > previous[id_]["updated_at"] for id_ in ("av-a-reset", "av-b-active", "av-c-disabled"))
    get("filtered-after-target")
    clear("repeat-target", "target")
    assert read_db(database) == after
    clear("explicit-account-lock", "__all")
    final = read_db(database)
    assert final["av-f-all"]["data"] == after["av-f-all"]["data"] | {"modelLock___all":None}
    assert [(row["connectionId"], row["model"]) for row in get("filtered-after-account-lock") if row["connectionId"] == "av-f-all"] == [("av-f-all", "__all")]
    snapshots["persisted-fields"] = {id_: {key:value for key,value in record["data"].items() if key != "apiKey"}
                                     for id_, record in final.items()}

    if implementation == "rust":
        with sqlite3.connect(database) as db:
            echo = final["av-c-disabled"]["data"] | {"apiKey":"offline-availability-redaction",
                    "lastError":"fixture echoed offline-availability-redaction and Bearer offline-auth-synthetic"}
            db.execute("UPDATE providerConnections SET data=? WHERE id='av-c-disabled'", (json.dumps(echo),))
        _, value = request(base)
        assert any(row["connectionId"] == "av-c-disabled" and row["lastError"] == "fixture echoed [redacted] and Bearer [redacted]" for row in value["models"])
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-model-availability-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(root / "rust/app/target/debug/server")]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(base, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        snapshots = run_cases(base, data / "db/data.sqlite")
        if implementation == "rust":
            assert (data / "fallback.log").read_text() == ""
        print(json.dumps(snapshots, separators=(",", ":"), sort_keys=True))
    finally:
        try:
            os.killpg(server.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            server.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(server.pid, signal.SIGKILL)
            server.wait(timeout=2)
