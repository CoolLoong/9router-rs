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
IMPLEMENTATION, *ARGS, CASE = sys.argv[1:]
GATEWAY_HEADERS = {"Authorization": "Bearer offline-chat-key"}
MANAGEMENT_HEADERS = {"x-9r-cli-token": "68977b6fd9422fbb"}
PASS_KEY = "clp_fixture_only"
CLINE_ACCESS = "WorkOS:eyJfixture.bearer.mock"
RAW_ACCESS = "eyJfixture.raw.mock"
CLINE_ID = "fixture-cline-transport"
PASS_ID = "fixture-clinepass-transport"
PASS_EMPTY_ID = "fixture-clinepass-empty"
MODEL_CHAT = "anthropic/claude-sonnet-4.6"
MODEL_PASS = "cline-pass/glm-5.2"


def saved_accounts(database):
    db = sqlite3.connect(database)
    db.execute("UPDATE providerConnections SET isActive=0")
    oauth = {"accessToken": CLINE_ACCESS, "expiresAt": "2099-11-19T00:00:00Z"}
    pass_account = {"apiKey": PASS_KEY}
    pass_type = "apikey"
    if CASE in {"oauth-normalized", "oauth-expired-refresh", "oauth-rejected-refresh"}:
        oauth["accessToken"] = RAW_ACCESS
    if CASE == "oauth-expired-refresh":
        oauth.update(refreshToken="fixture-expired-refresh", expiresAt="2000-01-01T00:00:00Z")
    if CASE == "oauth-rejected-refresh":
        oauth.update(refreshToken="fixture-rejected-refresh", expiresAt="2099-11-19T00:00:00Z")
    if CASE == "pass-oauth-refresh":
        pass_type = "oauth"
        pass_account = {"accessToken": RAW_ACCESS, "refreshToken": "fixture-pass-refresh",
                        "expiresAt": "2000-01-01T00:00:00Z"}
    db.executemany(
        """INSERT INTO providerConnections
           (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
           VALUES (?,?,?,?,1,?,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
        [(CLINE_ID, "cline", "oauth", "Fixture Cline", 1, json.dumps(oauth)),
         (PASS_ID, "clinepass", pass_type, "Fixture ClinePass", 1, json.dumps(pass_account)),
         (PASS_EMPTY_ID, "clinepass", "apikey", "Fixture ClinePass fallback", 0,
          json.dumps({"apiKey": "fixture-unknown-pass-key"}))])
    db.commit()
    db.close()


def request(port, path, headers=None, payload=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    headers = dict(headers or {})
    if payload is not None:
        headers["Content-Type"] = "application/json"
    try:
        connection.request("GET" if payload is None else "POST", path,
                           json.dumps(payload) if payload is not None else None, headers)
        response = connection.getresponse()
        return response.status, response.headers.get("Content-Type", "").split(";")[0], response.read()
    finally:
        connection.close()


def discover(port):
    paths = {
        "models-cline": f"/api/providers/{CLINE_ID}/models",
        "models-clinepass": f"/api/providers/{PASS_ID}/models",
        "models-static-fallback": f"/api/providers/{PASS_EMPTY_ID}/models",
        "catalog": "/v1/models",
    }
    status, mime, response = request(
        port, paths[CASE], GATEWAY_HEADERS if CASE == "catalog" else MANAGEMENT_HEADERS)
    assert status == 200 and mime == "application/json", (status, mime)
    value = json.loads(response)
    if CASE == "catalog":
        selected = [entry for entry in value["data"] if entry["id"].startswith(("cl/", "clinepass/"))]
        expected = {
            "cl/anthropic/fixture-live-cline", "cl/cline-pass/fixture-live-pass",
            "cl/cline-pass/fixture-plain-model", "cl/cline-free/fixture-live-free",
            "clinepass/cline-pass/fixture-live-pass", "clinepass/cline-pass/fixture-plain-model",
        }
        assert {entry["id"] for entry in selected} == expected
        assert all(entry["owned_by"] == entry["id"].split("/", 1)[0] for entry in selected)
        return sorted(selected, key=lambda entry: entry["id"])
    assert value["connectionId"] in {CLINE_ID, PASS_ID, PASS_EMPTY_ID}
    models = value["models"]
    if CASE == "models-cline":
        assert models == [
            {"id": "anthropic/fixture-live-cline", "name": "Last Cline entry"},
            {"id": "cline-pass/fixture-live-pass", "name": "Live Pass"},
            {"id": "cline-pass/fixture-plain-model", "name": "cline-pass/fixture-plain-model"},
            {"id": "cline-free/fixture-live-free", "name": "Free Cline"},
        ]
    elif CASE == "models-clinepass":
        assert models == [
            {"id": "cline-pass/fixture-live-pass", "name": "Live Pass"},
            {"id": "cline-pass/fixture-plain-model", "name": "cline-pass/fixture-plain-model"},
        ]
    else:
        assert value["warning"] == "ClinePass returned no live models; falling back to static catalog."
        assert any(model["id"] == MODEL_PASS for model in models)
    return value


def chat(port, database):
    pass_provider = CASE.startswith("pass-") or CASE == "provider-error"
    model = f"clinepass/{MODEL_PASS}" if pass_provider else f"cl/{MODEL_CHAT}"
    stream = CASE.endswith("-stream")
    body = {"model": model, "messages": [{"role": "user", "content": CASE}], "stream": stream}
    if stream:
        body["stream_options"] = {"include_usage": True}
    status, mime, response = request(port, "/v1/chat/completions", GATEWAY_HEADERS, body)
    if CASE == "provider-error":
        value = json.loads(response)
        assert status == 400 and "Fixture provider rejected chat" in value["error"]["message"]
        return {"status": status, "mime": mime, "body": value}
    if CASE == "oauth-normalized" and IMPLEMENTATION == "reference":
        value = json.loads(response)
        assert status == 503 and "Fixture saw bare WorkOS bearer" in value["error"]["message"], (status, mime)
        return {"status": status, "bareJWTRejected": True}
    assert status == 200, (CASE, status, mime)
    text = "fixture " + CASE
    if stream:
        assert mime == "text/event-stream"
        result = [line[6:] for line in response.decode().splitlines() if line.startswith("data: ")]
        assert result[-1] == "[DONE]"
        frames = [json.loads(frame) for frame in result[:-1]]
        assert any(frame.get("choices") and frame["choices"][0]["delta"].get("content") == text for frame in frames)
        assert any(frame.get("usage", {}).get("total_tokens") == 2008 for frame in frames if frame.get("usage"))
        return {"status": status, "mime": mime, "frames": frames}
    assert mime == "application/json"
    value = json.loads(response)
    assert "success" not in value and value["choices"][0]["message"]["content"] == text
    assert value["usage"]["total_tokens"] == 2008

    if CASE.endswith("-refresh"):
        id = PASS_ID if pass_provider else CLINE_ID
        expected = {
            "oauth-expired-refresh": ("workos:eyJfixture.renewed.mock", "fixture-rotated-expired"),
            "oauth-rejected-refresh": ("eyJfixture.retried.mock", "fixture-rejected-refresh"),
            "pass-oauth-refresh": ("eyJfixture.passrenewed.mock", "fixture-rotated-pass"),
        }[CASE]
        with sqlite3.connect(database) as db:
            stored, = db.execute("SELECT data FROM providerConnections WHERE id = ?", (id,)).fetchone()
        saved = json.loads(stored)
        assert (saved["accessToken"], saved["refreshToken"]) == expected
        assert saved["expiresAt"] > "2026-01-01T00:00:00Z"
    return {"status": status, "mime": mime, "body": value}


with tempfile.TemporaryDirectory(prefix="9router-cline-transport-") as directory:
    path = Path(directory)
    data = path / "data"
    home = path / "home"
    home.mkdir()
    log = path / "fallback.log"
    subprocess.run(["python3", str(ROOT / "rust/reference/seed.py"), str(data)], check=True,
                   stderr=subprocess.DEVNULL)
    (data / "cline-transport-implementation").write_text(IMPLEMENTATION)
    database = data / "db/data.sqlite"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if IMPLEMENTATION == "reference" else [str(Path(ARGS[0]).resolve())]
    environment = {
        "PATH": os.environ["PATH"], "PORT": str(port), "HOSTNAME": "127.0.0.1", "HOME": str(home),
        "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data), "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(log),
    }
    process = subprocess.Popen(command, cwd=ROOT, env=environment, start_new_session=True,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and process.poll() is None:
            try:
                if request(port, "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.05)
        else:
            raise RuntimeError("Fixture did not start")
        saved_accounts(database)
        result = discover(port) if CASE.startswith("models-") or CASE == "catalog" else chat(port, database)
        if IMPLEMENTATION == "rust":
            assert log.read_bytes() == b"", log.read_text()
        print(json.dumps(result, separators=(",", ":"), sort_keys=True))
    finally:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=2)
