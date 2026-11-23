import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
now = int(time.time() * 1000)
DAY = 86_400_000


def request(port, path, method="GET", payload=None, headers=cli):
    headers = headers.copy()
    if payload is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(payload).encode()
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        client.request(method, path, body=payload, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        client.close()


def fake_package(directory):
    package = directory / "node_modules" / "pxpipe-proxy"
    core = package / "dist" / "core"
    core.mkdir(parents=True)
    (package / "package.json").write_text('{"name":"pxpipe-proxy","version":"0.0.0-fixture","type":"module"}')
    (core / "library.js").write_text(
        "export const transformAnthropicMessages = ({ body }) => ({ applied: false, body, reason: 'fixture' });"
    )
    return package


def run(kind, directory):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    home = directory / "home"
    logs = directory / "data" / "pxpipe"
    logs.mkdir(parents=True)
    home.mkdir()
    fallbacks = directory / "fallbacks.txt"
    env = {
        **os.environ,
        "PORT": str(port), "TMPDIR": str(directory), "HOME": str(home),
        "DATA_DIR": str(logs.parent), "NINEROUTER_MIGRATION_DATA_DIR": str(logs.parent),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "TZ"]:
        env.pop(name, None)
    entry = ["sh", "rust/reference/server.sh"] if kind == "original" else [binary]
    process = subprocess.Popen(entry, env=env, cwd=root, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"{kind} PXPIPE fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        def call(operation, method="GET", payload=None):
            return request(port, f"/api/pxpipe/{operation}", method, payload)

        for operation in ["status", "stats", "logs", "health"]:
            assert request(port, f"/api/pxpipe/{operation}", headers={})[0] == 401
        assert request(port, "/api/settings", "PATCH", {
            "pxpipeEnabled": True, "pxpipeAutoInstall": False,
            "pxpipeMinChars": 401, "pxpipeTimeoutMs": 830,
        })[0] == 200
        status, empty = call("status")
        assert status == 200 and empty["mode"] == "library" and empty["enabled"] is True
        assert empty["autoInstall"] is False and empty["minChars"] == 401 and empty["timeoutMs"] == 830
        assert not empty["installed"] and not empty["running"] and empty["loadedAt"] is None
        assert call("start", "POST") == (409, {"error":"PXPIPE is not installed","code":"NOT_INSTALLED"})
        assert call("restart", "POST") == (500, {"error":"PXPIPE is not installed","code":"NOT_INSTALLED"})
        status, stopped = call("stop", "POST")
        assert status == 200 and stopped == {
            "stopped":False,
            **{field: empty[field] for field in [
                "installed","installing","version","path","running","loadedAt","uptimeMs","npmAvailable","mode",
            ]},
        }
        for method in ["GET", "POST"]:
            assert call("health", method) == (200, {
                "healthy": False,
                "checks": [{"id":"installed","label":"PXPIPE installed","ok":False,"detail":None}],
                "error":"pxpipe not installed",
            })

        records = [
            {"ts":now - 36 * DAY, "applied": True, "tokensBeforeEst":110, "tokensAfterEst":65,
             "tokensSavedEst":45, "durationMs":3, "imageCount":1, "model":"old"},
            {"ts":now - 9 * DAY, "applied": True, "tokensBeforeEst":90, "tokensAfterEst":85,
             "tokensSavedEst":5, "durationMs":2, "imageCount":0, "model":"warm"},
            {"ts":now - 3000, "applied": True, "tokensBeforeEst":101, "tokensAfterEst":78,
             "tokensSavedEst":23, "durationMs":4, "imageCount":2, "model":"same-time-rotated"},
            {"ts":now - 3000, "applied": False, "reason":"threshold", "model":"same-time-active"},
            {"ts":now - 2000, "applied":False, "reason":"timeout", "model":"timeout"},
            {"ts":now - 1000, "applied":False, "reason":"transform_error", "model":"error"},
        ]
        def text(values):
            return "\n".join([json.dumps(value) for value in values]) + "\n"

        (logs / "events.jsonl.1").write_text(text([records[1], records[0], records[2]]) + "{malformed\n")
        (logs / "events.jsonl").write_text(text([records[5], records[3], records[4]]))
        (logs / "install.log").write_bytes("\r\n\r\n".join(f"fixture {i}" for i in range(207)).encode())
        recorded = {}

        for limit in ["2", "0", "0.8", "-1", "0x2", "1000"]:
            status, value = call(f"stats?limit={limit}")
            assert status == 200
            totals = value["windows"]["all"]
            assert totals["requests"] == 6 and totals["compressed"] == 3 and totals["errors"] == 2
            assert totals["bypassed"] == 1 and totals["tokensSavedEst"] == 73 and totals["savedPct"] == 24.25
            assert totals["imagesGenerated"] == 3 and totals["avgCompressionMs"] == 3
            assert len(value["timeline"]) == 30 and value["recent"][0]["model"] == "error"
            status, entries = call(f"logs?limit={limit}")
            assert status == 200 and entries["events"] == value["recent"]
            assert entries["installLog"].splitlines() == [f"fixture {i}" for i in range(7, 207)]
            if limit == "2":
                assert [item["model"] for item in entries["events"]] == ["error", "timeout"]
            if limit == "0":
                assert [item["model"] for item in entries["events"]][2:4] == ["same-time-active", "same-time-rotated"]
            recorded[limit] = {"stats":value,"logs":entries}
        if kind == "rust":
            assert not fallbacks.exists() or not fallbacks.read_text().strip()

        package = fake_package(logs)
        status, installed = call("status")
        assert status == 200 and installed["installed"] and not installed["running"]
        assert installed["path"] == str(package) and installed["version"] == "0.0.0-fixture"
        status, started = call("start", "POST")
        assert status == 200 and started["running"] and started["loadedAt"] > 0
        status, healthy = call("health", "POST")
        assert status == 200 and healthy["healthy"] and healthy["checks"][-1]["detail"].endswith("(fixture)")
        status, restarted = call("restart", "POST")
        assert status == 200 and restarted["running"] and restarted["loadedAt"] >= started["loadedAt"]

        (package / "dist" / "core" / "library.js").unlink()
        status, vanished = call("status")
        assert status == 200 and not vanished["installed"] and vanished["running"]
        status, stopped = call("stop", "POST")
        assert status == 200 and stopped["stopped"] and not stopped["running"]
        if kind == "rust":
            assert fallbacks.read_text().splitlines() == [
                "GET /api/pxpipe/status", "POST /api/pxpipe/start", "POST /api/pxpipe/health",
                "POST /api/pxpipe/restart", "GET /api/pxpipe/status", "POST /api/pxpipe/stop",
            ]
        return {"empty": empty, "metrics": recorded}
    finally:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


with tempfile.TemporaryDirectory(prefix="9router-pxpipe-fixture-") as temporary:
    path = Path(temporary)
    assert run("original", path / "original") == run("rust", path / "rust")
    print("PXPIPE native status, malformed line filtering, limits, timelines and library lifecycle bridge passed")
