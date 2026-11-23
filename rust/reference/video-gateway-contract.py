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
connection_header = "x-9router-connection-id"
create_path = "/v1/videos/generations"
encoded_job = "/v1/videos/fixture-job%2Fpart%3Fq"


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    connections = [
        ("fixture-video-primary", "xai", 1, "offline-xai-image"),
        ("fixture-video-secondary", "xai", 2, "offline-xai-video-secondary"),
        ("fixture-video-openrouter", "openrouter", 1, "offline-static-openrouter"),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as database:
        database.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey','Fixture video',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(id_, provider, priority, json.dumps({"apiKey": key})) for id_, provider, priority, key in connections],
        )


def request(base, path, data=None, preferred=None, content_type="application/json", authorize=True):
    headers = {"Authorization": "Bearer offline-chat-key"} if authorize else {}
    if preferred:
        headers["x-connection-id"] = preferred
    if data is not None:
        headers["Content-Type"] = content_type
        if isinstance(data, dict):
            data = json.dumps(data, separators=(",", ":")).encode()
    try:
        response = urlopen(Request(base + path, data=data, headers=headers), timeout=5)
    except HTTPError as failure:
        response = failure
    with response:
        return {
            "status": response.status,
            "connection": response.headers.get(connection_header),
            "contentType": response.headers.get("Content-Type"),
            "raw": response.read().decode(),
        }


def run_cases(base):
    secondary = "fixture-video-secondary"
    primary = "fixture-video-primary"
    openrouter = "fixture-video-openrouter"
    snapshots = {}

    def send(name, path, *args, status, connection=None, **kwargs):
        result = request(base, path, *args, **kwargs)
        assert (result["status"], result["connection"]) == (status, connection), (name, result)
        snapshots[name] = result
        return json.loads(result["raw"])

    def create(name, model, prompt, **kwargs):
        return send(name, create_path, {"model": model, "prompt": prompt}, **kwargs)

    submitted = create("preferred-submit", "xai/fixture-video-affinity", "affinity fixture",
                       preferred=secondary, status=202, connection=secondary)
    assert submitted["status"] == "pending" and submitted["account"] == "secondary"
    stopped = send("missing-poll-auth", encoded_job, preferred=secondary, authorize=False, status=401)
    assert stopped["error"]["message"] == "Missing API key"
    unpinned = send("default-poll", encoded_job, status=200, connection=primary)
    assert unpinned["account"] == "primary"
    pinned = send("preferred-poll", encoded_job + "?provider=openrouter", preferred=secondary,
                  status=200, connection=secondary)
    assert pinned["account"] == "secondary" and pinned["status"] == "completed"

    routed = create("openrouter-submit", "openrouter/google/veo-3.1", "openrouter fixture", status=201,
                    connection=openrouter)
    assert routed["status"] == "queued"
    checked = send("openrouter-pinned-poll", "/v1/videos/fixture-openrouter-job", preferred=openrouter,
                   status=200, connection=openrouter)
    assert checked["unsigned_urls"] == ["https://example.invalid/fixture.mp4"]

    binary = (b'--video-fixture\r\nContent-Disposition: form-data; name="image"; filename="fake.bin"\r\n\r\n'
              b"\x00\x7f\x80\xff\r\n--video-fixture--\r\n")
    multipart = send("multipart-exact-bytes", create_path, binary, content_type="multipart/form-data; boundary=video-fixture",
                     status=202, connection=primary)
    assert multipart["request_id"] == "fixture-multipart"

    rotated = create("quota-rotation", "xai/fixture-video-rotate", "rotate fixture", status=202, connection=secondary)
    assert rotated["account"] == "secondary"
    ambiguous = create("no-5xx-replay", "xai/fixture-video-unavailable", "unavailable fixture", status=502)
    assert "fixture may have accepted job" in ambiguous["error"]["message"]
    network = request(base, create_path, {"model": "xai/fixture-video-network", "prompt": "network fixture"})
    network_body = json.loads(network["raw"])
    if implementation == "reference":
        assert network["status"] == 202 and network["connection"] == primary and network_body["account"] == "primary", network
    else:
        assert network["status"] == 502 and network["connection"] is None, network
        assert network_body["error"]["message"] == "[xai] video upstream fetch failed: fetch failed"
    poll_failure = send("no-poll-rotation", "/v1/videos/fixture-poll-no-rotation", preferred=secondary, status=429)
    assert "fixture polling rate limited" in poll_failure["error"]["message"]
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-video-contract-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
    cmd = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(binary)]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(cmd, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(base, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        snapshots = run_cases(base)
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
