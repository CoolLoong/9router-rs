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
native = Path(sys.argv[2]) if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
path = "/v1/images/generations"
sd = "sdwebui/sdxl-base-1.0"
comfy = "comfyui/flux-dev"
credential = "fixture-ignored-sd-account"


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    with sqlite3.connect(data / "db/data.sqlite") as db:
        payload = {"testStatus":"unavailable", "modelLock_sdxl-base-1.0":"2099-01-01T00:00:00Z",
                   "accessToken":"offline-local-fixture-never-use", "providerSpecificData":{"baseUrl":"https://example.invalid/ignored-account"}}
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES (?,'sdwebui','oauth','Ignored fixture account',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                   (credential, json.dumps(payload)))


def request(base, dest, input_=None, key="offline-chat-key", extra=None):
    try:
        data = None if input_ is None else json.dumps(input_, separators=(",", ":")).encode()
        headers = {"Authorization":"Bearer " + key,"Content-Type":"application/json"} | (extra or {})
        response = urlopen(Request(base + dest, data=data, headers=headers), timeout=5)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.headers.get("Content-Type"), response.headers.get("Content-Disposition"), response.read()


def payload(model, prompt, **extra):
    return {"model":model,"prompt":prompt, **extra}


def encoded(text):
    return base64.b64encode(text.encode()).decode()


def ledger(data):
    file = data / "fixture-local-image-requests"
    return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []


def run_cases(base, data):
    snapshots = {}

    def json_call(name, body, expected=200, dest=path, key="offline-chat-key", extra=None):
        status, mime, disposition, text = request(base, dest, body, key, extra)
        assert status == expected and mime == "application/json" and disposition is None, (name, status, mime, text)
        value = json.loads(text)
        snapshots[name] = {"status":status,"body":value}
        return value

    def binary(name, body, media, filename, expected):
        result = request(base, path + "?response_format=binary", body)
        assert result == (200, media, f'inline; filename="{filename}"', expected), (name, result)
        snapshots[name] = {"mime":media,"filename":filename,"base64":base64.b64encode(expected).decode()}

    database = data / "db/data.sqlite"
    with sqlite3.connect(database) as db:
        before = db.execute("SELECT data,updatedAt FROM providerConnections WHERE id=?", (credential,)).fetchone()
    start = json_call("sd-ignores-account-and-overrides", payload(sd, "Fixture SD defaults",
                      negative_prompt="ignored", steps=99, endpoint="https://example.invalid/ignored-body"),
                      extra={"x-connection-id":credential})
    assert isinstance(start.pop("created"), int)
    assert start["data"] == [{"b64_json":encoded("first fake SD image")},{"b64_json":encoded("second fake SD image")}]
    binary("sd-size-fallback-and-batch-null", payload(sd, "Fixture SD binary", size="0x720", n=None),
           "image/png", "image.png", b"fake SD binary\0")
    empty = json_call("sd-empty-images", payload(sd, "Fixture SD empty"))
    assert isinstance(empty.pop("created"), int) and empty["data"] == []

    before_invalid = len(ledger(data))
    invalid = json_call("sd-size-null", payload(sd, "Fixture SD defaults", size=None), 400)
    assert invalid["error"]["message"] == "Cannot read properties of null (reading 'split')"
    assert len(ledger(data)) == before_invalid
    denied = json_call("source-key-model-policy", payload(sd, "Fixture SD defaults"), 403, key="offline-restricted-key")
    assert "not allowed to use model" in denied["error"]["message"] and len(ledger(data)) == before_invalid
    rejected = json_call("sd-upstream-status", payload(sd, "Fixture SD rejection"), 429)
    assert rejected["error"]["message"] == "[429]: fixture generation is busy"

    before_queued = len(ledger(data))
    pending = json_call("comfy-pending-receipt", payload(comfy, {"3":{"class_type":"KSampler","inputs":{"seed":7}}}),
                        dest=path + "?response_format=binary")
    assert pending == {"prompt_id":"fixture-queued","number":2,"node_errors":{}}
    assert ledger(data)[before_queued:] == [{"method":"POST","path":"/comfyui/","query":""}]
    inline = json_call("comfy-opaque-json", payload(comfy, "Fixture Comfy inline"))
    assert inline["receipt"] == "opaque" and inline["data"][0]["revised_prompt"] == "synthetic"
    binary("comfy-inline-no-download", payload(comfy, "Fixture Comfy inline", output_format="webp"),
           "image/webp", "image.webp", b"fake inline Comfy image")
    previous = len(ledger(data))
    binary("comfy-same-origin-download", payload(comfy, "Fixture Comfy artifact", output_format="JpEg"),
           "image/jpeg", "image.jpg", b"fixture staged artifact\0")
    assert ledger(data)[previous:] == [
        {"method":"POST","path":"/comfyui/","query":""},
        {"method":"GET","path":"/comfyui/fixture-local-artifact","query":"?name=demo"},
    ]
    malformed = json_call("comfy-empty-json", payload(comfy, "Fixture Comfy invalid JSON"), 502)
    assert malformed["error"]["message"] == "Unexpected end of JSON input"

    if implementation == "rust":
        prior = len(ledger(data))
        status, mime, _, body = request(base, path + "?response_format=binary", payload(comfy, "Fixture Comfy off-origin"))
        assert status == 200 and mime == "application/json"
        assert json.loads(body)["data"][0]["url"] == "https://example.invalid/fixture-only"
        assert ledger(data)[prior:] == [{"method":"POST","path":"/comfyui/","query":""}]
    with sqlite3.connect(database) as db:
        after = db.execute("SELECT data,updatedAt FROM providerConnections WHERE id=?", (credential,)).fetchone()
    assert before == after, "no-auth generation mutated an unrelated connection"
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-local-image-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(native)]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", MODEL_CATALOG_SYNC="off", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
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
        snapshots = run_cases(base, data)
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
