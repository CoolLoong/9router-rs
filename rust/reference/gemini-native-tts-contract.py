import base64
from http.client import HTTPConnection
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
from urllib.parse import urlencode

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
client = {"x-goog-api-key": "offline-chat-key"}
canonical = "/v1beta/models/"
pcm_bytes = bytes([1, 0, 2, 0, 255, 127, 0, 128])


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    accounts = [
        ("native-tts-main", "apikey", 1, {"apiKey": "offline-native-tts-primary", "accessToken": "offline-unused"}),
        ("native-tts-next", "oauth", 2, {"accessToken": "offline-native-tts-bearer"}),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,'gemini',?,'Fixture native speech',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(id_, auth, priority, json.dumps(credentials)) for id_, auth, priority, credentials in accounts],
        )


def payload(scenario, audio="AUDIO"):
    generation = {"speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Kore"}}}}
    if audio is not None:
        generation["responseModalities"] = [audio]
    return {"contents": [{"parts": [{"text": f"native {scenario}"}]}], "generationConfig": generation, "fixture": {"numeric": 3.0}}


def connection(port):
    return HTTPConnection("127.0.0.1", port, timeout=7)


def request(port, path, data=None, headers=client):
    conn = connection(port)
    if data is None:
        conn.request("GET", path, headers=headers)
    else:
        conn.request("POST", path, body=json.dumps(data, indent=2).encode(),
                     headers={"Content-Type": "application/json; charset=UTF-8"} | headers)
    try:
        reply = conn.getresponse()
        return reply.status, dict((k.lower(), v) for k, v in reply.getheaders()), reply.read().decode()
    finally:
        conn.close()


def sse(port):
    conn = connection(port)
    path = canonical + "gemini-2.5-flash-preview-tts:streamGenerateContent?alt=sse&tts_fixture=stream-delayed"
    try:
        conn.request("POST", path, body=json.dumps(payload("stream-delayed")).encode(),
                     headers={"Content-Type": "application/json"} | client)
        reply = conn.getresponse()
        assert reply.status == 200 and reply.getheader("Content-Type") == "text/event-stream"
        first = reply.readline().decode()
        assert first.startswith("data: ")
        assert reply.readline() == b"\n"
        mark = time.monotonic()
        final = reply.read().decode()
        assert time.monotonic() - mark >= .25, "response was buffered until the final SSE event"
        assert final == 'data: {"candidates":[{"finishReason":"STOP"}]}\n\n'
        assert audio_connection(first.removeprefix("data: ")) == "apikey"
        return first + "\n" + final
    finally:
        conn.close()


def audio_connection(raw):
    body = json.loads(raw)
    part = body["candidates"][0]["content"]["parts"][0]["inlineData"]
    assert part["mimeType"] == "audio/L16;rate=24000"
    assert base64.b64decode(part["data"]) == pcm_bytes
    return body["fixtureConnection"]


def run_cases(port, database):
    snapshots = {}

    def submit(name, model, scenario, status, mark=None, headers=client, query=None, audio="AUDIO"):
        params = list(query or []) + [("tts_fixture", scenario)]
        result, returned, raw = request(port, canonical + model + ":generateContent?" + urlencode(params), payload(scenario, audio), headers)
        assert result == status, (name, result, raw)
        if mark:
            assert returned["x-fixture-upstream"] == mark and returned["access-control-allow-origin"] == "*"
            assert "content-encoding" not in returned and "content-length" not in returned
        snapshots[name] = {"status": result, "type": returned.get("content-type"), "mark": returned.get("x-fixture-upstream"),
                           "cors": returned.get("access-control-allow-origin"), "raw": raw}
        return raw

    known = submit("gzip-known-model-query-key", "models/gemini-2.5-pro-preview-tts", "known-gzip", 201, "gzip-json", {},
                   [("label", "a b"), ("key", "offline-chat-key"), ("key", "ignored"), ("label", "c+d"), ("empty", "")], None)
    assert known.startswith('{ "candidates":') and known.endswith(' }\n') and audio_connection(known) == "apikey"
    mixed = submit("mixed-case-audio-mode", "fixture-native-audio", "typed-audio", 200, "unknown-audio-model", audio="aUdIo")
    assert audio_connection(mixed) == "apikey"
    buffered_error = submit("documented-raw-error", "gemini-2.5-pro-preview-tts", "upstream-error", 422, "gzip-error")
    assert buffered_error == "fixture synthesis rejected\n"
    snapshots["stream-sends-first-event-without-buffering"] = sse(port)

    alternate = submit("api-key-to-stored-bearer", "fixture-native-rotate", "rotate", 200, "alternate-account")
    assert audio_connection(alternate) == "bearer"
    with sqlite3.connect(database) as db:
        [row] = db.execute("SELECT data FROM providerConnections WHERE id='native-tts-main'").fetchone()
        account = json.loads(row)
        assert account.get("modelLock_fixture-native-rotate") and not account.get("modelLock___all")
    exhausted = submit("all-connections-cooldown", "fixture-native-exhausted", "exhausted", 429)
    assert json.loads(json.loads(exhausted)["error"]["message"]) == {"error": {"message": "bearer fixture quota reached"}}
    assert snapshots["all-connections-cooldown"]["cors"] is None

    if implementation == "rust":
        status, headers, redacted = request(port, canonical + "fixture-native-redaction:generateContent?tts_fixture=redaction", payload("redaction"))
        assert status == 422 and redacted == "fixture key [redacted] and Bearer [redacted]"
        assert "content-encoding" not in headers
    with sqlite3.connect(database) as db:
        db.execute("UPDATE providerConnections SET data='{}' WHERE id='native-tts-main'")
    empty = submit("active-account-without-key", "fixture-native-audio", "typed-audio", 404, audio="aUdIo")
    assert json.loads(empty) == {"error": {"message": "No Gemini API key configured"}}
    with sqlite3.connect(database) as db:
        db.execute("UPDATE providerConnections SET isActive=0 WHERE provider='gemini'")
    absent = submit("no-active-account", "fixture-native-audio", "typed-audio", 503, audio="aUdIo")
    assert json.loads(absent) == {"error": {"message": "No active credentials for provider: gemini"}}
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-native-tts-contract-") as directory:
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
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(port, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        snapshots = run_cases(port, data / "db/data.sqlite")
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
