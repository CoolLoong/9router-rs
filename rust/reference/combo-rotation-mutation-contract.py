from contextlib import ExitStack, closing
import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
original = sys.argv[1] == "reference"
binary = None if original else Path(sys.argv[2]).resolve()
admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
inference = {"Authorization": "Bearer offline-chat-key"}
a, b = "fixture-combo-a", "fixture-combo-b"
models = {model: "fixture-live/" + model for model in (a, b)}
name = "rotation-mutation-source"
destination = "rotation-mutation-destination"
neighbor = "rotation-mutation-unrelated"
collision = "rotation-mutation-taken"


def request(port, method, path, data=None, headers=None):
    headers = admin.copy() if headers is None else headers.copy()
    if data is not None:
        headers["Content-Type"] = "application/json"
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=8)) as client:
        client.request(method, path, None if data is None else json.dumps(data), headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())


def verify(port, db):
    records = {}
    resources = "/api/combos"
    enabled = request(port, "PUT", "/api/providers/fixture-live-connection", {"isActive": True})
    assert enabled[0] == 200, enabled
    settings = {"comboStrategy": "round-robin", "comboStickyRoundRobinLimit": 2, "comboStrategies": {}}
    assert request(port, "PATCH", "/api/settings", settings)[0] == 200

    def create(label, order):
        status, payload = request(port, "POST", resources,
                                  {"name": label, "models": [models[model] for model in order], "kind": "llm"})
        assert status == 201, (status, payload)
        return resources + "/" + payload["id"], payload["id"]

    def update(path, fields, expected):
        result = request(port, "PUT", path, fields)
        assert result[0] == expected, result
        return result[1]

    def sequence(label, model, order):
        observed = []
        for _ in order:
            status, payload = request(port, "POST", "/v1/chat/completions",
                                      {"model": model, "stream": False, "messages": [{"role": "user", "content": "test combo"}]},
                                      inference)
            assert status == 200, (label, status, payload)
            observed.append(payload["choices"][0]["message"]["content"])
        assert observed == list(order), (label, observed, order)
        records[label] = observed

    source, source_id = create(name, [a, b])
    create(collision, [a, b])
    create(neighbor, [a, b])
    sequence("original order", name, [a, a])
    sequence("unrelated warmup", neighbor, [a, a])

    updated = update(source, {"models": [models[b], models[a]]}, 200)
    assert updated["models"] == [models[b], models[a]]
    sequence("updated order reset", name, [b, b, a])
    sequence("unrelated order retained", neighbor, [b])
    assert update(source, {"name": collision}, 400)["error"] == "Combo name already exists"
    sequence("duplicate rename retains count", name, [a, b, b])
    assert update(source, {"name": "invalid name"}, 400)["error"] == "Name can only contain letters, numbers, -, _ and ."
    sequence("invalid rename retains state", name, [a])

    with sqlite3.connect(db) as sql:
        sql.execute("""CREATE TRIGGER fixture_combo_update_abort
            BEFORE UPDATE ON combos WHEN NEW.name='rotation-db-abort'
            BEGIN SELECT RAISE(ABORT, 'synthetic update failure'); END""")
    assert update(source, {"name": "rotation-db-abort"}, 500)["error"] == "Failed to update combo"
    sequence("rolled back update retains count", name, [a, b, b])

    stale, stale_id = create(destination, [a, b])
    sequence("destination warmup", destination, [a, a])
    with sqlite3.connect(db) as sql:
        sql.execute("DELETE FROM combos WHERE id=?", (stale_id,))
    assert request(port, "GET", stale)[0] == 404
    changed = update(source, {"name": destination}, 200)
    assert changed["name"] == destination and changed["id"] == source_id
    sequence("rename clears destination state", destination, [b, b])
    create(name, [b, a])
    sequence("rename clears previous name state", name, [b])

    with sqlite3.connect(db) as sql:
        sql.execute("""CREATE TRIGGER fixture_combo_delete_abort
            BEFORE DELETE ON combos WHEN OLD.name='rotation-mutation-destination'
            BEGIN SELECT RAISE(ABORT, 'synthetic delete failure'); END""")
    rejected = request(port, "DELETE", source)
    assert rejected == (500, {"error": "Failed to delete combo"}), rejected
    sequence("rolled back delete retains state", destination, [a])
    with sqlite3.connect(db) as sql:
        sql.execute("DROP TRIGGER fixture_combo_delete_abort")
    assert request(port, "DELETE", source) == (200, {"success": True})
    assert request(port, "DELETE", source) == (404, {"error": "Combo not found"})
    create(destination, [b, a])
    sequence("delete clears reused name state", destination, [b, b])
    print(json.dumps(records, sort_keys=True))


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


with tempfile.TemporaryDirectory(prefix="9router-combo-mutation-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    data = scratch / "data"
    env = {"HOME": str(scratch), "PATH": os.environ["PATH"], "DATA_DIR": str(data),
           "NODE_NO_WARNINGS": "1", "NEXT_TELEMETRY_DISABLED": "1", "MODEL_CATALOG_SYNC": "off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], cwd=root, env=env, check=True)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    mock = str(root / "rust/reference/mock-models.mjs")
    logs = stack.enter_context((scratch / "server.log").open("w+"))
    server_env = {**env, "PORT": str(port), "HOSTNAME": "127.0.0.1"}
    cmd = ["node", "--import", mock, str(root / ".next/standalone/custom-server.js")]
    if not original:
        simulator = subprocess.Popen(["node", "--import", mock, "--eval", "setInterval(() => {}, 60000)"],
                                     cwd=root, env=env, stdout=logs, stderr=logs)
        stack.callback(stop, simulator)
        limit = time.monotonic() + 8
        while True:
            try:
                with socket.create_connection(("127.0.0.1", 52989), timeout=.25):
                    break
            except OSError:
                assert time.monotonic() < limit and simulator.poll() is None
                time.sleep(.025)
        fallback = scratch / "fallback.log"
        server_env.update({"PATH": "", "NINEROUTER_MIGRATION_FIXTURE": "1",
                           "RUST_ANYTHING_FALLBACK_LOG": str(fallback)})
        cmd = [str(binary), "--no-browser", "--no-tray", "--skip-update"]
    server = subprocess.Popen(cmd, cwd=scratch, env=server_env, stdout=logs, stderr=logs)
    stack.callback(stop, server)
    limit = time.monotonic() + 10
    try:
        while True:
            try:
                if request(port, "GET", "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            assert time.monotonic() < limit and server.poll() is None
            time.sleep(.03)
        verify(port, data / "db/data.sqlite")
        assert original or fallback.read_bytes() == b""
    except BaseException:
        logs.seek(0)
        print(logs.read(), file=sys.stderr)
        raise
