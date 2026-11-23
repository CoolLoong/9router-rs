import http.client
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

ROOT = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
api = {"Authorization": "Bearer offline-chat-key"}
api_id = "fixture-kiro-key-models"
oauth_id = "fixture-kiro-profile-models"
empty_id = "fixture-kiro-no-models"


def request(port, path, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        conn.request("GET", path, headers=headers or {})
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


def insert_accounts(db):
    api_details = {
        "accessToken": "ksk_fixture_router_only",
        "providerSpecificData": {"authMethod": "api_key", "region": "eu-west-2"},
    }
    oauth_details = {
        "accessToken": "fixture-kiro-profile-token",
        "providerSpecificData": {
            "authMethod": "idc",
            "profileArn": "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE",
        },
    }
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
        sql.executemany(
            """INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,'kiro',?,'Synthetic Kiro',?,?,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [
                (api_id, "api_key", 1, 1, json.dumps(api_details)),
                (oauth_id, "oauth", 2, 1, json.dumps(oauth_details)),
                (empty_id, "api_key", 3, 0, json.dumps(api_details | {"accessToken": "fixture-ineligible-key"})),
            ],
        )


with tempfile.TemporaryDirectory(prefix="9router-kiro-models-http-") as directory:
    home = Path(directory) / "home"
    home.mkdir()
    data = Path(directory) / "data"
    fallbacks = Path(directory) / "fallback.log"
    subprocess.run(
        [sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
        check=True, stderr=subprocess.DEVNULL,
    )
    database = data / "db/data.sqlite"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    environment = {
        "PATH": os.environ["PATH"], "PORT": str(port), "HOSTNAME": "127.0.0.1", "HOME": str(home),
        "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    server = subprocess.Popen(
        [binary, "--no-browser"], cwd=ROOT, env=environment, start_new_session=True,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and server.poll() is None:
            try:
                if request(port, "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.05)
        else:
            raise RuntimeError("Native gateway did not start")
        insert_accounts(database)
        scopes = (api_id, oauth_id)
        expected = {"fixture-key-model": (300000, 1.25), "fixture-profile-model": (100000, 1)}
        for identifier, upstream_id in zip(scopes, expected):
            route = f"/api/providers/{identifier}/models"
            assert request(port, route)[0] == 401
            status, result = request(port, route, admin)
            assert status == 200 and result["provider"] == "kiro" and "warning" not in result
            by_id = {model["id"]: model for model in result["models"]}
            base = by_id[upstream_id]
            assert (base["contextLength"], base["rateMultiplier"]) == expected[upstream_id]
            assert by_id[upstream_id + "-thinking-agentic"]["capabilities"] == {"thinking": True, "agentic": True}
        status, result = request(port, f"/api/providers/{empty_id}/models", admin)
        assert status == 200 and result["models"] == []
        assert result["warning"] == "Kiro returned no models; falling back to static catalog."
        status, catalog = request(port, "/v1/models", api)
        assert status == 200
        kiro = {model["id"]: model for model in catalog["data"] if model["owned_by"] == "kr"}
        assert kiro["kr/fixture-key-model-thinking"]["capabilities"] == {"thinking": True, "agentic": False}
        assert kiro["kr/fixture-key-model"]["context_length"] > 0
        assert "kr/auto" in kiro and "kr/auto-agentic" not in kiro
        assert "kr/fixture-profile-model" not in kiro
        with sqlite3.connect(database) as sql:
            sql.execute(
                """UPDATE providerConnections
                      SET data=json_set(data,'$.providerSpecificData.enabledModels',json(?))
                    WHERE id=?""",
                (json.dumps(["fixture-configured-only"]), api_id),
            )
        status, catalog = request(port, "/v1/models", api)
        assert status == 200
        kiro = [model["id"] for model in catalog["data"] if model["owned_by"] == "kr"]
        assert kiro == ["kr/fixture-configured-only"], kiro
        assert fallbacks.read_bytes() == b""
        print("Native Kiro dashboard and public discovery passed with zero route fallbacks")
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
