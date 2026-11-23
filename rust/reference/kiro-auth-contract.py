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
authorized = {"x-9r-cli-token": "68977b6fd9422fbb"}
key = "ksk_fixture_router_only"
client_secret = "fixture-kiro-client-secret"
profile = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE"


def request(port, method, path, headers=None, payload=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        forwarded = dict(headers or {})
        if payload is not None:
            forwarded["Content-Type"] = "application/json"
        conn.request(method, path, body=json.dumps(payload) if payload is not None else None, headers=forwarded)
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix="9router-kiro-auth-http-") as directory:
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
                if request(port, "GET", "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.05)
        else:
            raise RuntimeError("Native gateway did not start")
        api = "/api/oauth/kiro/api-key"
        idc = "/api/oauth/kiro/import"
        creds = {"apiKey": key, "region": "eu-west-2"}
        grant = {
            "clientId": "fixture-kiro-client", "clientSecret": client_secret,
            "refreshToken": "fixture-kiro-refresh", "region": "eu-west-2",
        }
        for route, value in [(api, creds), (idc, grant)]:
            status, result = request(port, "POST", route, payload=value)
            assert status == 401 and result == {"error": "Unauthorized"}
        status, result = request(port, "POST", api, authorized, creds | {"region": "eu-west-2.invalid"})
        assert status == 500 and result == {"error": "API key validation failed"}
        status, result = request(port, "POST", api, authorized, creds)
        assert status == 200 and result["success"] and set(result["connection"]) == {"id", "provider", "email"}
        key_id = result["connection"]["id"]
        status, models = request(port, "GET", f"/api/providers/{key_id}/models", authorized)
        assert status == 200 and models["provider"] == "kiro" and any(
            model["id"] == "fixture-key-model" for model in models["models"])
        status, result = request(port, "POST", idc, authorized, grant)
        assert status == 200 and result["connection"]["email"] == "fixture-kiro@example.invalid"
        assert client_secret not in json.dumps(result)
        idc_id = result["connection"]["id"]
        with sqlite3.connect(database) as sql:
            rows = sql.execute(
                """SELECT authType,name,priority,data FROM providerConnections
                   WHERE id IN (?,?) ORDER BY priority""",
                (key_id, idc_id),
            ).fetchall()
        assert [row[:2] for row in rows] == [("api_key", None), ("oauth", "fixture-kiro@example.invalid")]
        key_data, idc_data = (json.loads(row[-1]) for row in rows)
        assert key_data["accessToken"] == key and "refreshToken" not in key_data
        assert key_data["providerSpecificData"]["region"] == "eu-west-2"
        assert idc_data["refreshToken"] == "fixture-kiro-renewed-refresh"
        assert idc_data["providerSpecificData"]["profileArn"] == profile
        assert idc_data["providerSpecificData"]["clientSecret"] == client_secret
        assert fallbacks.read_bytes() == b""
        print("Native Kiro API-key and IDC import persisted correct account rows, zero route fallbacks")
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
