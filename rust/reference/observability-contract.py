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
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]
admin = {"x-9r-cli-token":"68977b6fd9422fbb"}
api = {"authorization": "Bearer offline-chat-key"}


def request(port, method, path, headers, data=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        if data is not None:
            data = json.dumps(data).encode()
            headers = {**headers, "content-type": "application/json"}
        connection.request(method, path, data, headers)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def wait_records(path, predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        with sqlite3.connect(path) as db:
            records = [json.loads(value) for (value,) in db.execute(
                "SELECT data FROM requestDetails ORDER BY timestamp, rowid")]
        if predicate(records):
            return records
        time.sleep(.04)
    raise AssertionError("Stored request details did not reach expected state")


with tempfile.TemporaryDirectory(prefix="9router-observability-contract-") as work:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {**os.environ, "PORT": str(port), "DATA_DIR": work, "NINEROUTER_MIGRATION_DATA_DIR": work,
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(Path(work) / "fallback.log")}
    for key in ["ENABLE_REQUEST_LOGS","OBSERVABILITY_ENABLED"]:
        env.pop(key, None)
    with subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            for _ in range(100):
                if process.poll() is not None:
                    raise RuntimeError("fixture server quit")
                try:
                    if request(port, "GET", "/api/health", {})[0] == 200:
                        break
                except OSError:
                    time.sleep(.07)
            else:
                raise RuntimeError("fixture server did not start")
            assert request(port, "PATCH", "/api/settings", admin, {
                 "enableObservability": True,
                 "observabilityMaxRecords": 2, "observabilityBatchSize": 1,
                 "observabilityFlushIntervalMs": 50, "observabilityMaxJsonSize": 1,
            })[0] == 200
            assert request(port, "PUT", "/api/providers/fixture-live-connection", admin, {"isActive": True})[0] == 200

            def submit(name, stream, message):
                return request(port, "POST", "/v1/chat/completions", api, {
                    "stream": stream, "model": f"fixture-live/{name}",
                    "messages": [{"role": "user","content": message}],
                })

            db = Path(work, "db", "data.sqlite")
            status, _ = submit("gpt-5.6-sol", False, "offline test prompt" * 120)
            assert status == 200, status
            first, = wait_records(db, lambda records: len(records) == 1)
            assert first["request"]["_truncated"] is True
            assert first["providerRequest"]["_truncated"] is True
            assert first["tokens"]["prompt_tokens"] == 4
            assert first["providerResponse"]["choices"][0]["message"]["content"] == "rust migration"

            status, wire = submit("fixture-observe", True, "fixture trace")
            assert status == 200 and b"data: [DONE]" in wire
            records = wait_records(db, lambda records: any(
                row["model"] == "fixture-observe" and row["response"]["content"] == "rust migration"
                for row in records))
            stream, = [row for row in records if row["model"] == "fixture-observe"]
            assert stream["tokens"]["completion_tokens"] == 2
            assert stream["latency"]["total"] >= stream["latency"]["ttft"]
            assert "offline-chat-key" not in json.dumps(records)
            status, body = request(port, "GET", "/api/usage/request-details", admin)
            details = json.loads(body)["details"]
            assert status == 200 and len(details) == 2
            assert all(detail["request"] == {"redacted": True} and detail["providerResponse"] == {"redacted":True}
                       for detail in details)
            status, _ = submit("fixture-invalid-observation", False, "fixture upstream error")
            assert status == 400
            records = wait_records(db, lambda records: len(records) == 2 and records[-1]["status"] == "error")
            assert records[0]["model"] == "fixture-observe"
            assert records[-1]["response"]["status"] == 400
            if server != "original":
                log = Path(work, "fallback.log").read_text()
                assert "/v1/chat/completions" not in log and "/api/usage/request-details" not in log
            print(f"{server}: request detail JSON, streaming completion, redaction and retention passed")
        finally:
            process.terminate()
