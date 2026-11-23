import concurrent.futures
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]) if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
path = "/api/models/catalog-sync"
cli = {"x-9r-cli-token":"68977b6fd9422fbb"}
first_tag = '"catalog-fixture-1"'
second_tag = '"catalog-fixture-2"'


def request(base, method="GET", authorized=True, endpoint=path):
    try:
        response = urlopen(Request(base + endpoint, headers=cli if authorized else {}, method=method), timeout=8)
    except HTTPError as failure:
        response = failure
    with response:
        return response.status, json.load(response)


@contextmanager
def launch(data):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["node", "--import", str(root / "rust/reference/mock-models.mjs"),
               str(root / ".next/standalone/custom-server.js")] if implementation == "reference" else [str(native)]
    env = dict(os.environ, PORT=str(port), HOSTNAME="127.0.0.1", DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               MODEL_CATALOG_SYNC="off", NINEROUTER_MIGRATION_FIXTURE="1", NEXT_TELEMETRY_DISABLED="1",
               RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(base, endpoint="/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        yield base
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


def mode(data, label):
    (data / "catalog-fixture-mode").write_text(label)


def recorded(data):
    file = data / "catalog-fixture-requests"
    return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []


def await_request(data, count):
    end = time.monotonic() + 3
    while len(recorded(data)) < count:
        assert time.monotonic() < end, "catalog fake provider did not receive HTTP request"
        time.sleep(.01)


def status_of(base, data):
    status, value = request(base)
    assert status == 200 and value.pop("file") == str(data / "model-catalog.json"), (status, value)
    assert value["intervalMs"] == 86400000 and value["url"] == "https://models.dev/api.json"
    return value


def stable(value):
    if value.get("lastSync") is not None:
        assert isinstance(value["lastSync"], (int, float))
        value["lastSync"] = "fixture time"
    if value.get("catalog") is not None and value["catalog"].get("syncedAt") is not None:
        assert isinstance(value["catalog"]["syncedAt"], int)
        value["catalog"]["syncedAt"] = "fixture time"
    return value


def fingerprint(paths):
    return [(path.read_bytes(), path.stat().st_mtime_ns) for path in paths]


def exercise(data):
    snapshots = {}
    catalog = data / "model-catalog.json"
    raw = data / "model-catalog-raw.json"
    publish_paths = [catalog, raw]

    with launch(data) as base:
        for verb in ("GET", "POST"):
            assert request(base, verb, authorized=False) == (401, {"error":"Unauthorized"})
        assert recorded(data) == []
        initial = status_of(base, data)
        assert initial["catalog"] is None and initial["running"] is False and initial["etag"] is None
        snapshots["initial"] = initial

        mode(data, "hold")
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as worker:
            first = worker.submit(request, base, "POST")
            await_request(data, 1)
            during = status_of(base, data)
            assert during["running"] is True and during["catalog"] is None
            snapshots["while-running"] = during
            assert request(base, "POST") == (503, {"error":"sync in progress"})
            assert len(recorded(data)) == 1 and not catalog.exists() and not raw.exists()
            (data / "catalog-fixture-release").touch()
            code, payload = first.result(timeout=5)
        assert code == 200 and payload["success"] is True and payload["result"]["status"] == "updated", (code, payload)
        overlay = json.loads(catalog.read_bytes())
        slim = json.loads(raw.read_bytes())
        assert overlay["v"] == 2 and overlay["etag"] == first_tag
        assert overlay["models"]["gemini:gemini-2.5-flash"] == {"vision":True,"audioInput":True}
        assert overlay["models"]["google:gemini-2.5-flash"] == {"vision":True,"audioInput":True}
        assert overlay["models"]["claude:claude-sonnet-4-6"] == {"pdf":True}
        assert overlay["models"]["fixture-upstream:special"] == {"audioInput":True,"videoInput":True}
        assert overlay["providers"]["gemini"]["gemini-2.5-flash"] == {"contextWindow":5000000,"maxOutput":6000}
        assert overlay["providers"]["kiro"]["gpt-5.6-sol"] == {"maxOutput":4321}
        assert slim["google"]["vendor/GEMINI-2.5-FLASH:free"] == {"i":["image","audio"],"c":1200000,"o":17000,"r":True}
        assert slim["google"]["gemini-2.5-flash"] == {"i":["pdf"],"c":5000000,"o":6000}
        assert slim["fixture-upstream"]["vendor/Special:large"]["i"] == ["audio","video","unknown"]
        assert payload["result"]["bytes"] == catalog.stat().st_size and payload["result"]["models"] == len(overlay["models"])
        assert payload["result"]["providers"] == len(overlay["providers"])
        snapshots["first-result"] = payload
        snapshots["first-overlay"] = overlay | {"syncedAt":"fixture time"}
        snapshots["first-slim"] = slim
        snapshots["first-status"] = stable(status_of(base, data))

        before = fingerprint(publish_paths)
        mode(data, "initial")
        assert request(base, "POST") == (200, {"success":True,"result":{"status":"unchanged"}})
        assert fingerprint(publish_paths) == before
        assert recorded(data)[-1]["tag"] == first_tag
        snapshots["after-304"] = stable(status_of(base, data))

        mode(data, "fail")
        assert request(base, "POST") == (503, {"error":"HTTP 503"})
        assert fingerprint(publish_paths) == before
        failed = status_of(base, data)
        assert failed["lastError"] == "HTTP 503" and failed["etag"] == first_tag and failed["running"] is False
        snapshots["after-failure"] = stable(failed)
        mode(data, "invalid")
        code, invalid = request(base, "POST")
        assert code == 503 and invalid.get("error") and fingerprint(publish_paths) == before

        mode(data, "updated")
        code, update = request(base, "POST")
        assert code == 200 and update["result"]["etag"] == second_tag
        assert recorded(data)[-1]["tag"] == first_tag
        latest = json.loads(catalog.read_bytes())
        assert latest["models"]["gemini:gemini-2.5-flash"] == {"videoInput":True}
        assert "kiro" not in latest["providers"]
        snapshots["second-overlay"] = latest | {"syncedAt":"fixture time"}
        snapshots["second-status"] = stable(status_of(base, data))

    before_restart = fingerprint(publish_paths)
    with launch(data) as base:
        restarted = status_of(base, data)
        count = len(recorded(data))
        assert restarted["catalog"]["bytes"] == catalog.stat().st_size and restarted["running"] is False
        if implementation == "rust":
            assert restarted["etag"] == second_tag and restarted["fileVersion"] == 2 and restarted["lastSync"] is not None
            assert request(base, "POST") == (200, {"success":True,"result":{"status":"unchanged"}})
            assert recorded(data)[count]["tag"] == second_tag and fingerprint(publish_paths) == before_restart
        else:
            assert restarted["etag"] is None and restarted["fileVersion"] is None
            assert request(base, "POST")[1]["result"]["status"] == "updated"
            assert recorded(data)[count]["tag"] is None

    outdated = json.loads(catalog.read_bytes()) | {"v":1}
    catalog.write_text(json.dumps(outdated))
    with launch(data) as base:
        old = status_of(base, data)
        assert old["fileVersion"] == (1 if implementation == "rust" else None)
        count = len(recorded(data))
        assert request(base, "POST")[1]["result"]["status"] == "updated"
        assert recorded(data)[count]["tag"] is None and json.loads(catalog.read_bytes())["v"] == 2

        if implementation == "rust":
            mode(data, "initial")
            before_rollback = fingerprint([raw])
            catalog.rename(data / "saved-overlay.json")
            catalog.mkdir()
            assert request(base, "POST") == (503, {"error":"Could not write model catalog files"})
            assert raw.read_bytes() == before_rollback[0][0] and catalog.is_dir()
            assert status_of(base, data)["running"] is False
            assert (data / "fallback.log").read_text() == ""
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-catalog-sync-contract-") as directory:
    data = Path(directory)
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    output = exercise(data)
    print(json.dumps(output, separators=(",", ":"), sort_keys=True))
