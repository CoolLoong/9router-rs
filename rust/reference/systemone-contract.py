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
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
canonical = "/v1/systemone"
auth = {"Authorization": "Bearer offline-chat-key"}
restricted = {"Authorization": "Bearer offline-restricted-key"}
fixture_state = {"text": "offline fixture", "revision": 2}
questions = {"route": {"type": "choice", "criterion": {"a": "first", "b": "second"}}}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    accounts = [
        ("v1m", "v1m", 1, {}),
        ("v1m", "v1m-backup", 2, {}),
        ("openrouter", "openrouter", 1, {}),
        ("nace", "nace", 1, {}),
        ("ollama", "ollama", 1, {}),
        ("ollama-local", "ollama-local", 1, {}),
        ("cloudflare-ai", "cloudflare", 1, {"accountId": "fixture-account"}),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey','Fixture decision',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(f"sys-{suffix}", provider, priority,
              json.dumps({"apiKey": f"offline-systemone-{suffix}", "providerSpecificData": settings}))
             for provider, suffix, priority, settings in accounts]
        )
        db.execute("UPDATE apiKeys SET accessAllow = ? WHERE id = 'fixture-restricted'",
                   (json.dumps(["v1m/rev-latest"]),))


def request(base, path, payload=None, headers=auth):
    headers = dict(headers)
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    else:
        data = None
    try:
        reply = urlopen(Request(base + path, data=data, headers=headers), timeout=6)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, json.load(reply)


def configure(database, suffix, settings):
    with sqlite3.connect(database) as db:
        [raw] = db.execute("SELECT data FROM providerConnections WHERE id = ?", (f"sys-{suffix}",)).fetchone()
        payload = json.loads(raw)
        payload["providerSpecificData"] = settings
        db.execute("UPDATE providerConnections SET data = ? WHERE id = ?", (json.dumps(payload), f"sys-{suffix}"))


def run_cases(base, database):
    snapshots = {}

    def ask(name, model, path=canonical, headers=auth, status=200, payload=None):
        body = dict(model=model, state=fixture_state, questions=questions, response_mode="strict")
        if payload:
            body.update(payload)
        received, value = request(base, path, body, headers)
        assert received == status, (name, received, value)
        assert "offline-systemone-" not in json.dumps(value), name
        snapshots[name] = value
        return value

    primary = ask("provider-alias-key-grant", "jev/rev-latest", "/systemone", restricted)
    assert primary["answers"]["route"]["choice"] == "a" and primary["model"] == "rev-latest"
    cloud = ask("ollama-cloud", "ollama/clef", "/v1/v1/systemone")
    assert cloud["provider_extension"]["received"] == "ollama"
    local = ask("ollama-local-zero-usage", "ollama-local/clef-flash", "/api/v1/systemone",
                payload={"state": False, "questions": {}})
    assert local == {"answers": {}, "usage": {}}
    router = ask("openrouter", "openrouter/typesafe/jev-1.13")
    assert router["model"] == "typesafe/jev-1.13"
    cf = ask("cloudflare-model-map", "cf/@cf/cloudflare/clef-flash")
    assert cf["model"] == "clef-flash" and cf["provider_extension"]["received"] == "cloudflare"
    nace = ask("nace-default", "nace/drex-v2.0")
    assert nace["provider_extension"]["received"] == "nace"

    for index, suffix in enumerate(["", "/v1/", "/v1/systemone/"]):
        configure(database, "nace", {"baseUrl": f" http://127.0.0.1:52989/nace-custom{suffix} "})
        custom = ask(f"nace-override-{index}", "nace/drex-v2.0", payload={"state": True, "questions": {}})
        assert custom["answers"]["configured"]["reached"] and "usage" not in custom
    configure(database, "cloudflare", {})
    cf_without_id = ask("cloudflare-requires-id", "cf/@cf/cloudflare/clef-flash", status=400)
    assert "requires accountId" in cf_without_id["error"]["message"]

    rotated = ask("quota-rotation", "v1m/fixture-systemone-rotate")
    assert rotated["answers"]["route"]["choice"] == "b"
    with sqlite3.connect(database) as db:
        [raw] = db.execute("SELECT data FROM providerConnections WHERE id='sys-v1m'").fetchone()
        account = json.loads(raw)
        assert account.get("modelLock_fixture-systemone-rotate") and not account.get("modelLock___all")

    error = ask("nested-provider-error", "v1m/fixture-systemone-error", status=422)
    assert error["error"]["message"] == "[422]: invalid fixture condition"
    invalid = ask("malformed-provider-json", "v1m/fixture-systemone-malformed", status=502)
    hint, marker, remaining = invalid["error"]["message"].partition(" (reset after ")
    assert hint == "[v1m/fixture-systemone-malformed] Invalid JSON response from v1m" and marker and remaining.endswith(")")
    invalid["error"]["message"] = hint

    if implementation == "rust":
        status, message = request(base, canonical, dict(model="v1m/fixture-systemone-redaction",
                                  state=fixture_state, questions=questions, response_mode="strict"))
        assert status == 422 and message["error"]["message"] == "[422]: Rejected [redacted] and Bearer [redacted]"

    rows = []
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        with sqlite3.connect(database) as db:
            rows = db.execute("""SELECT provider, model, connectionId, endpoint, promptTokens, completionTokens, status, tokens
                                 FROM usageHistory ORDER BY provider, model""").fetchall()
        if len(rows) == 7:
            break
        time.sleep(0.025)
    assert len(rows) == 7, rows
    usage = {f"{provider}/{model}": dict(provider=provider, connection=connection, endpoint=endpoint,
                       input=input_tokens, output=output_tokens, status=status, detail=json.loads(detail))
             for provider, model, connection, endpoint, input_tokens, output_tokens, status, detail in rows}
    assert usage["ollama-local/clef-flash"]["detail"] == {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    assert usage["v1m/fixture-systemone-rotate"]["connection"] == "sys-v1m-backup"
    assert usage["v1m/rev-latest"]["detail"] == {"prompt_tokens": 9, "completion_tokens": 3, "total_tokens": 12}
    assert all(entry["status"] == "success" for entry in usage.values())
    snapshots["usage"] = usage
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-systemone-contract-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(binary)]
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
