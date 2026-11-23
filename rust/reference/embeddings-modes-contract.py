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
implementation = sys.argv[1]
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
payload = {"model": "fixture-live/fixture-embedding", "input": "fixture embedding", "dimensions": 3}


def bind_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=7)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=3)


def reachable(port, process):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline and process.poll() is None:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.03)
    raise RuntimeError(f"fixture process did not bind port {port}")


def call(port, method, path, headers=None, body=None):
    headers = dict(headers or {})
    if body is not None:
        headers["Content-Type"] = "application/json"
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=8)) as connection:
        connection.request(method, path, json.dumps(body) if body is not None else None, headers)
        result = connection.getresponse()
        return result.status, json.loads(result.read())


def exercise(port, database):
    def config(require_key, observability):
        response = call(port, "PATCH", "/api/settings", cli,
                        {"requireApiKey": require_key, "enableObservability": observability})
        assert response[0] == 200 and response[1]["enableObservability"] == observability, response

    def embed(key=None, path="/v1/embeddings", forwarded=None):
        headers = {"Authorization": "Bearer " + key} if key else {}
        if forwarded:
            headers["X-Forwarded-For"] = forwarded
        return call(port, "POST", path, headers, payload)

    config(True, True)
    snapshot = {"observed": embed("offline-chat-key"),
                "required_unknown": embed("offline-unknown-key"),
                "required_disabled": embed("offline-disabled-key")}
    assert snapshot["required_unknown"][0] == snapshot["required_disabled"][0] == 401

    config(False, True)
    snapshot.update(anonymous=embed(path="/api/v1/embeddings"),
                    optional_unknown=embed("offline-unknown-key", "/v1/v1/embeddings"),
                    optional_restricted=embed("offline-disabled-key"))
    assert snapshot["optional_restricted"][0] == 403, snapshot["optional_restricted"]
    if implementation != "reference":
        assert embed("offline-unknown-key", forwarded="192.0.2.55")[0] == 401

    config(True, False)
    snapshot["unobserved"] = embed("offline-chat-key")
    assert snapshot["observed"] == snapshot["anonymous"] == snapshot["optional_unknown"] == snapshot["unobserved"]
    assert snapshot["observed"][0] == 200 and snapshot["observed"][1]["usage"]["prompt_tokens"] == 5

    deadline = time.monotonic() + 6
    while True:
        with sqlite3.connect(database) as sql:
            rows = sql.execute("""SELECT model,endpoint,promptTokens,completionTokens,status
                FROM usageHistory WHERE model='fixture-embedding' ORDER BY endpoint,promptTokens""").fetchall()
            details = sql.execute("SELECT count(*) FROM requestDetails").fetchone()[0]
        if len(rows) == 4:
            break
        assert time.monotonic() < deadline, rows
        time.sleep(0.03)
    assert all(row[2:] == (5, 0, "success") for row in rows), rows
    assert details == 0
    snapshot["usage"] = rows
    return snapshot


with tempfile.TemporaryDirectory(prefix="9router-embeddings-modes-") as temporary, ExitStack() as stack:
    stage = Path(temporary)
    database = stage / "data/db/data.sqlite"
    empty = stage / "path"
    empty.mkdir()
    common = {"PATH": os.environ["PATH"], "HOME": str(stage), "NODE_NO_WARNINGS": "1"}
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(stage / "data")],
                   cwd=root, env=common, check=True)
    with sqlite3.connect(database) as sql:
        sql.execute("UPDATE apiKeys SET accessRestricted=1,accessAllow='[]' WHERE key='offline-disabled-key'")
        sql.execute("UPDATE providerConnections SET isActive=1 WHERE id='fixture-live-connection'")
    port = bind_port()
    logs = stack.enter_context((stage / "fixture.log").open("w+"))
    env = {**common, "DATA_DIR": str(stage / "data"), "PORT": str(port), "HOSTNAME": "127.0.0.1",
           "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off",
           "RUST_ANYTHING_FALLBACK_LOG": str(stage / "fallback.log")}
    if implementation == "reference":
        command = ["node", "--import", str(root / "rust/reference/mock-models.mjs"),
                   str(root / ".next/standalone/custom-server.js")]
    else:
        mock = subprocess.Popen(["node", "--import", str(root / "rust/reference/mock-models.mjs"),
                                 "--eval", "setInterval(() => {}, 60000)"], cwd=root, env=common,
                                stdin=subprocess.DEVNULL, stdout=logs, stderr=logs)
        stack.callback(stop, mock)
        reachable(52989, mock)
        env["PATH"] = str(empty)
        command = [str(Path(sys.argv[2]).resolve(strict=True)), "--no-browser", "--skip-update"]
    app = subprocess.Popen(command, cwd=stage, env=env, stdin=subprocess.DEVNULL, stdout=logs, stderr=logs)
    stack.callback(stop, app)
    try:
        reachable(port, app)
        snapshot = exercise(port, database)
        if implementation != "reference":
            assert (stage / "fallback.log").read_bytes() == b""
        print(json.dumps(snapshot, sort_keys=True, separators=(",", ":")))
    except BaseException:
        logs.seek(0)
        print(logs.read(), file=sys.stderr)
        raise
