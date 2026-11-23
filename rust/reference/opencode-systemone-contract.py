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
systemone = "/v1/systemone"
combo = "fixture-opencode-default-smart"
candidates = ["fixture-live/fixture-combo-a", "fixture-live/fixture-combo-b"]
headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey','Fixture decision',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(id_, provider, json.dumps({"apiKey": key})) for id_, provider, key in [
                ("fixture-opencode-stored-free", "opencode", "offline-systemone-unused"),
                ("fixture-opencode-stored-zen", "opencode-zen", "offline-systemone-opencode-zen"),
            ]],
        )
        db.execute("UPDATE providerConnections SET isActive=1 WHERE id='fixture-live-connection'")
        db.execute("""INSERT INTO combos(id,name,kind,models,createdAt,updatedAt)
                      VALUES (?,?,'llm',?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                   (combo, combo, json.dumps(candidates)))
        settings = {"comboStrategies": {combo: {"fallbackStrategy": "smart", "sessionSticky": False}}}
        db.execute("INSERT INTO settings(id,data) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                   (json.dumps(settings),))


def ask(base, path, body=None):
    data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
    try:
        reply = urlopen(Request(base + path, data=data, headers=headers), timeout=7)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, json.load(reply)


def run_cases(base, database):
    snapshots = {}

    def record(name, path, body, status=200):
        result, value = ask(base, path, body)
        assert result == status, (name, result, value)
        snapshots[name] = value
        return value

    for name, model, receipt in [("anonymous", "oc/jev-1.13-free", "anonymous"),
                                 ("stored-zen", "ocz/jev-1.13", "stored-key")]:
        result = record(name, systemone, {"model": model, "state": "OpenCode direct fixture",
                                          "questions": {"route": {"type": "choice"}}, "fixtureUsage": True})
        assert result["receipt"] == receipt and result["answers"]["route"]["choice"] == "b"

    for prompt, name, expected in [
        ("OpenCode Smart decision fixture", "smart-classifier", "fixture-combo-b"),
        ("OpenCode Smart fallback fixture", "smart-fail-open", "fixture-combo-a"),
    ]:
        result = record(name, "/v1/chat/completions",
                        {"model": combo, "messages": [{"role": "user", "content": prompt}], "stream": False})
        assert result["model"] == expected and result["choices"][0]["message"]["content"] == expected, result

    failure = record("free-quota-response", systemone,
                     {"model": "oc/jev-1.13-free", "state": "OpenCode direct quota fixture",
                      "questions": {"route": {"type": "choice"}}}, 429)
    assert failure["error"]["message"] == "[429]: fixture public decision quota"
    result = record("free-continues-after-quota", systemone,
                    {"model": "oc/jev-1.13-free", "state": "OpenCode direct fixture", "questions": {"route": {"type": "choice"}}})
    assert result["receipt"] == "anonymous"

    rows = []
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        with sqlite3.connect(database) as db:
            rows = db.execute("""SELECT provider,model,connectionId,endpoint,status,tokens
                                 FROM usageHistory WHERE provider IN ('opencode','opencode-zen') ORDER BY provider""").fetchall()
        if len(rows) == 2:
            break
        time.sleep(0.025)
    assert len(rows) == 2, rows
    assert [row[2] for row in rows] == [None, "fixture-opencode-stored-zen"], rows
    detail = {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
    assert all(row[3:5] == ("/v1/systemone", "success") and json.loads(row[5]) == detail for row in rows), rows
    snapshots["usage"] = [list(row[:5]) + [json.loads(row[5])] for row in rows]
    with sqlite3.connect(database) as db:
        daily = [json.loads(raw)["byAccount"] for (raw,) in db.execute("SELECT data FROM usageDaily")]
        assert all(not ({"", "noauth", "fixture-opencode-stored-free"} & set(item)) for item in daily), daily
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-opencode-systemone-") as directory:
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
                ask(base, "/api/health")
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
