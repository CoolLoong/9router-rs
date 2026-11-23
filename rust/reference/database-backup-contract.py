import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
program = ["sh", "rust/reference/server.sh"] if server == "original" else [server]
route = "/api/settings/database"
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(port, path, method="GET", body=None, headers=None):
    headers = {} if headers is None else headers.copy()
    if body is not None:
        headers["content-type"] = "application/json"
        body = json.dumps(body).encode()
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        conn.request(method, path, body=body, headers=headers)
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


def exported(port):
    code, value = request(port, route, headers=cli)
    assert code == 200, code
    return value


def restored(port, payload):
    code, value = request(port, route, "POST", payload, cli)
    assert code == 200 and value == {"success": True}, code


with tempfile.TemporaryDirectory(prefix="9router-backup-contract-") as name:
    scratch = Path(name)
    data = scratch / "data"
    fallback = scratch / "fallback.log"
    port = free_port()
    env = {
        **os.environ,
        "PORT": str(port),
        "TMPDIR": name,
        "DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
    }
    for key in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(key, None)
    proc = subprocess.Popen(program, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if proc.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        before = exported(port)
        db_path = (data / "db" / "data.sqlite").resolve()
        assert db_path.is_relative_to(scratch.resolve())
        with sqlite3.connect(f"{db_path.as_uri()}?mode=rw", uri=True) as db:
            db.execute("INSERT INTO _meta(key, value) VALUES ('backup-contract-meta', 'kept')")
            db.execute(
                "INSERT INTO usageHistory(timestamp, provider, model) VALUES (?, ?, ?)",
                ("2026-11-19T00:00:00.000Z", "backup-contract", "synthetic-model"),
            )
            db.execute(
                "INSERT INTO kv(scope, key, value) VALUES ('disabledModels', ?, ?)",
                ("backup-contract", '["synthetic-model"]'),
            )

        bad = {"settings": {"requireLogin": False}, "apiKeys": [
            {"id": "invalid", "key": "offline-backup-bad", "access": {"restricted": None}},
        ]}
        code, value = request(port, route, "POST", bad, cli)
        assert code == 400 and value["error"].startswith("apiKeys invalid:"), code
        assert exported(port) == before
        assert request(port, "/api/providers/client")[0] == 401

        inputs = root / "rust/reference/database-backup-cases.jsonl"
        cases = (json.loads(line) for line in inputs.read_text().splitlines())
        synthetic = json.loads(next(case["body"] for case in cases if case["name"] == "database-10-restore"))
        synthetic.pop("password")
        restored(port, synthetic)
        snapshot = exported(port)
        assert snapshot["settings"] == {"requireLogin": False, "tunnelDashboardAccess": False}
        assert snapshot["apiKeys"][0]["access"] == {"restricted": False, "allow": []}
        assert snapshot["apiKeys"][1]["access"] == {"restricted": True, "allow": ["g/Name"]}
        assert snapshot["providerConnections"][0]["authType"] == "oauth"
        assert snapshot["proxyPools"][0]["testStatus"] == "unknown"
        assert snapshot["mitmAlias"]["empty"] == {}
        assert request(port, route)[0] == 401
        assert request(port, "/api/providers/client")[0] == 200

        with sqlite3.connect(f"{db_path.as_uri()}?mode=ro", uri=True) as db:
            assert db.execute("SELECT value FROM _meta WHERE key = 'backup-contract-meta'").fetchone() == ("kept",)
            assert db.execute(
                "SELECT count(*) FROM usageHistory WHERE provider = 'backup-contract'"
            ).fetchone() == (1,)
            assert db.execute(
                "SELECT value FROM kv WHERE scope = 'disabledModels' AND key = 'backup-contract'"
            ).fetchone() == ('["synthetic-model"]',)

        proxy = {**snapshot, "settings": {
            **snapshot["settings"], "outboundProxyEnabled": True,
            "outboundProxyUrl": "http://127.0.0.1:9", "outboundNoProxy": "127.0.0.1",
        }}
        restored(port, proxy)
        assert exported(port)["settings"]["outboundProxyEnabled"] is True
        restored(port, before)
        assert exported(port) == before
        assert request(port, "/api/providers/client")[0] == 401
        if server != "original":
            paths = fallback.read_text().splitlines()
            assert paths.count("POST " + route) == 2, "outbound proxy import did not delegate twice"
        print(f"{server}: synthetic backup roundtrip, rollback, metadata and Original cache sharing passed")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
