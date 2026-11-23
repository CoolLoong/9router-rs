import base64
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
create = "/v1/videos/generations"
model = "veo-3.1-generate-preview"
request_model = "vertex/" + model
bearer_connection = "fixture-vertex-video-bearer"
service_connection = "fixture-vertex-video-service"
bearer_prefix = "projects/fixture-video-bearer/locations/europe-west1/publishers/google/models/" + model
service_prefix = "projects/fixture-sa-project/locations/us-central1/publishers/google/models/" + model
auth = {"Authorization": "Bearer offline-chat-key"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    private = data / "fixture-vertex-video-private.pem"
    public = data / "vertex-fixture-key.pub"
    with private.open("w") as output:
        private.chmod(0o600)
        subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048"],
                       check=True, stdout=output, stderr=subprocess.DEVNULL)
    subprocess.run(["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(public)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    service = json.dumps({"type":"service_account", "project_id":"fixture-sa-project", "client_email":"vertex-fixture@example.invalid",
                          "private_key":private.read_text(), "token_uri":"https://example.invalid/unused"})
    private.unlink()
    accounts = [
        (bearer_connection, "oauth", 1, {"projectId":"fixture-video-bearer","accessToken":"offline-vertex-video-bearer",
             "providerSpecificData":{"projectId":"fixture-should-not-win","location":"europe-west1"}}),
        (service_connection, "apikey", 2, {"projectId":"fixture-should-not-win","apiKey":service}),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                          VALUES (?,'vertex',?,'Fixture Vertex video',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                       [(id_, mode, priority, json.dumps(details)) for id_, mode, priority, details in accounts])


def job(prefix, name):
    return base64.urlsafe_b64encode(f"{prefix}/operations/{name}".encode()).decode().rstrip("=")


def request(base, path, payload=None, preferred=None, authenticated=True, **extra):
    headers = dict(auth) if authenticated else {}
    if preferred:
        headers["x-connection-id"] = preferred
    if payload is not None:
        headers["Content-Type"] = "application/json; charset=utf-8"
    data = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    try:
        reply = urlopen(Request(base + path, headers=headers | extra, data=data), timeout=8)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("x-9router-connection-id"), reply.headers.get("Content-Type"), json.load(reply)


def run_cases(base, database):
    snapshots = {}

    def ask(name, path, payload=None, preferred=None, expected=200, selected=None, **extra):
        status, connection, mime, value = request(base, path, payload, preferred, **extra)
        assert (status, connection) == (expected, selected), (name, status, connection, value)
        assert mime == "application/json", (name, mime)
        snapshots[name] = {"status":status,"connection":connection,"value":value}
        return value

    submitted = ask("bearer-submit", create, {
        "model":request_model, "prompt":"Vertex video primary fixture", "image":"data:image/png;base64,ZmFrZS1pbWFnZQ==",
        "video":{"gcsUri":"gs://example.invalid/input.mp4"}, "n":"0x2", "duration":"4", "aspect_ratio":"16:9", "resolution":"720p",
        "seed":"0009", "negative_prompt":"no overlays", "storage_uri":"gs://example.invalid/output", "generate_audio":False,
    }, bearer_connection, 202, bearer_connection, **{"idempotency-key":"fixture-never-forward"})
    id_ = job(bearer_prefix, "job-pending")
    assert submitted == {"id":id_,"request_id":id_,"status":"pending"}
    path = "/v1/videos/" + id_ + "?provider=openrouter"
    pending = ask("pinned-poll", path, preferred=bearer_connection, selected=bearer_connection)
    assert pending == submitted
    completed = ask("poll-nested-samples", path, preferred=bearer_connection, selected=bearer_connection)
    assert completed["status"] == "completed" and completed["id"] == id_
    assert completed["video"] == {"url":"gs://example.invalid/result.mp4","b64_json":None,"mime_type":"video/mp4"}
    assert completed["videos"][1:] == [
        {"url":None,"b64_json":"ZmFrZS12aWRlbw==","mime_type":"video/webm"},
        {"url":"gs://example.invalid/default.mp4","b64_json":None,"mime_type":"video/mp4"},
    ]
    failed = ask("poll-reported-failure", "/v1/videos/" + job(bearer_prefix,"job-failed") + "?provider=vertex", selected=bearer_connection)
    assert failed["status"] == "failed" and failed["error"] == {"code":3,"message":"fixture rejected"}
    empty = ask("poll-direct-empty-precedes-nested", "/v1/videos/" + job(bearer_prefix,"job-empty"),
                preferred=bearer_connection, selected=bearer_connection)
    assert empty["status"] == "completed" and empty["video"] is None and empty["videos"] == []
    opaque = ask("image-only-opaque-response", create, {"model":request_model,"image_url":"gs://example.invalid/reference.png"},
                 bearer_connection, 200, bearer_connection)
    assert opaque == {"resource":"opaque fixture"}

    invalid = ask("reject-invalid-id", "/v1/videos/not-a-vertex-operation?provider=vertex", expected=400)
    assert invalid["error"]["message"] == "[vertex] Invalid Vertex video job id"
    edits = ask("reject-edits", "/v1/videos/edits", {"model":request_model,"prompt":"ignored"}, bearer_connection, 400)
    assert edits["error"]["message"] == "[vertex] Vertex video supports 'generations' only (got 'edits')"
    no_input = ask("requires-prompt-or-image", create, {"model":request_model}, bearer_connection, 400)
    assert no_input["error"]["message"] == "[vertex] Vertex video requires a prompt or an image"
    missing_key = ask("poll-requires-api-key", path, preferred=bearer_connection, expected=401, authenticated=False)
    assert missing_key["error"]["message"] == "Missing API key"

    service = ask("service-submit", create, {"model":request_model,"prompt":"Vertex video service fixture"},
                  service_connection, 200, service_connection)
    assert service["id"] == job(service_prefix, "job-service") and service["status"] == "pending"
    service_result = ask("service-poll-cache", "/v1/videos/" + service["id"], preferred=service_connection, selected=service_connection)
    assert service_result["video"] == {"url":"gs://example.invalid/service.mp4","b64_json":"ZmFrZS12aWRlbw==","mime_type":"video/mp4"}

    if implementation == "rust":
        status, _, _, value = request(base, create, {"model":request_model,"prompt":"Vertex video redaction fixture"}, service_connection)
        assert status == 422 and "offline-vertex-sa-token" not in json.dumps(value) and "echoed [redacted]" in json.dumps(value), value
    ambiguous = ask("no-replay-after-502", create, {"model":request_model,"prompt":"Vertex video ambiguous fixture"}, bearer_connection, 502)
    assert "fixture may have accepted the job" in ambiguous["error"]["message"]

    with sqlite3.connect(database) as db:
        db.execute("UPDATE providerConnections SET authType='apikey', data=? WHERE id=?",
                   (json.dumps({"apiKey":"offline-vertex-raw-video", "projectId":"fixture-video-bearer"}), bearer_connection))
    raw_key = ask("raw-key-rejected", create, {"model":"vertex/veo-3.0-generate-001","prompt":"ignored"}, bearer_connection, 400)
    assert "raw API keys are not supported" in raw_key["error"]["message"]
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-vertex-video-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    native = Path(sys.argv[2]) if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(native)]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
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
        snapshots = run_cases(base, data / "db/data.sqlite")
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
