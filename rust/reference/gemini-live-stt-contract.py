import json
import os
from pathlib import Path
import re
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
path = "/v1/audio/transcriptions"
built_in = "gemini-2.5-flash-native-audio-preview-09-17"
custom = "fixture-live-custom"
boundary = "synthetic-voice-data"


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES ('live-stt-fixture','gemini','apikey','Fixture Live STT',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                   (json.dumps({"apiKey":"offline-live-stt"}),))
        models = [custom, "fixture-live-timeout-setup", "fixture-live-timeout-turn",
                  "fixture-live-rejected", "fixture-live-silence", "fixture-live-redaction"]
        for model in models:
            record = json.dumps({"providerAlias":"gemini", "id":model,"name":model,"type":"stt","transport":"gemini-live"})
            db.execute("INSERT INTO kv(scope,key,value) VALUES('customModels',?,?)", (f"gemini|{model}|stt", record))


def form(model, fields, audio, mime, filename):
    output = bytearray()
    for key, value in [("model", "gemini/" + model), *fields]:
        output.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode())
    output.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\nContent-Type: {mime}\r\n\r\n'.encode())
    output.extend(audio)
    output.extend(f"\r\n--{boundary}--\r\n".encode())
    return output


def request(base, destination, data=None, key="offline-chat-key"):
    headers = {"Authorization": "Bearer " + key}
    if data is not None:
        headers["Content-Type"] = f"multipart/form-data; boundary={boundary}"
    try:
        reply = urlopen(Request(base + destination, headers=headers, data=data), timeout=5)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("Access-Control-Allow-Origin"), reply.headers.get("Retry-After"), json.load(reply)


def audio(size):
    return bytes(offset % 113 for offset in range(size))


def run_cases(base):
    snapshots = {}

    def ask(name, model, size, fields, expected, mime="audio/pcm", filename="fixture.pcm", key="offline-chat-key"):
        status, cors, retry, result = request(base, path, form(model, fields, audio(size), mime, filename), key)
        assert status == expected, (name, status, result)
        if retry:
            assert int(retry) > 0 and re.search(r" \(reset after [\dhms ]+\)$", result["error"]["message"]), (name, retry, result)
            result["error"]["message"] = result["error"]["message"].rsplit(" (reset after ", 1)[0]
        snapshots[name] = {"status":status, "cors":cors, "data":result}
        return result

    basic = ask("audio-chunks-verbose", built_in, 20011, [
        ("prompt"," Fixture live basics "), ("prompt","ignored duplicate"), ("language"," en-GB "), ("response_format"," VerBoSe_JsOn ")
    ], 200)
    assert basic == {"text":"fixture speech","segments":[{"id":0,"text":"fixture "},{"id":1,"text":"speech"}]}
    override = ask("custom-transport-system-override", custom, 500, [
        ("prompt","ignored"), ("language","ja-JP"), ("system_instruction"," Fixture live override ")
    ], 200, "application/octet-stream", "fixture.wav")
    assert override == {"text":"override speech"}
    partial = ask("partial-on-close", built_in, 260, [("system_instruction","Fixture live partial"),("response_format","text")], 200)
    assert partial == {"text":"partial "}
    rotated = ask("resume-no-duplicate-chunks", custom, 17200, [
        ("system_instruction","Fixture live goaway"),("response_format","verbose_json")
    ], 200)
    assert rotated == {"text":"first pass","segments":[{"id":0,"text":"first "},{"id":1,"text":"pass"}]}
    empty = ask("empty-audio", built_in, 0, [("system_instruction","Fixture live shared")], 400)
    assert empty["error"]["message"] == "Empty audio file"
    denied = ask("restricted-client", built_in, 230, [("system_instruction","Fixture live silence")], 403, key="offline-restricted-key")
    assert "not allowed to use model" in denied["error"]["message"]

    faults = [
        ("setup-timeout", "fixture-live-timeout-setup", "Fixture live setup timeout", 210, 504, "timed out waiting for setupComplete", [("setup_timeout_ms","120")]),
        ("turn-timeout", "fixture-live-timeout-turn", "Fixture live turn timeout", 320, 504, "transcription timed out", [("turn_timeout_ms","0x78")]),
        ("provider-error", "fixture-live-rejected", "Fixture live rejected", 240, 502, "Gemini Live error (INVALID_ARGUMENT): fixture media rejected", []),
        ("empty-on-close", "fixture-live-silence", "Fixture live silence", 230, 502, "Gemini Live socket closed before completion (code 1000)", []),
    ]
    for name, model, directive, size, status, expected, extras in faults:
        value = ask(name, model, size, [("system_instruction", directive), *extras], status)
        assert value["error"]["message"].endswith(expected), (name, value)

    if implementation == "rust":
        status, _, _, value = request(base, path,
                                      form(custom, [("system_instruction","Fixture live standard ack")], audio(120), "audio/pcm", "fixture.pcm"))
        assert status == 200 and value == {"text":"binary delta"}, (status, value)
        status, _, _, value = request(base, path, form("fixture-live-redaction",
                          [("system_instruction","Fixture live redaction")], audio(190), "audio/pcm", "fixture.pcm"))
        raw = json.dumps(value)
        assert status == 502 and "offline-live-" not in raw and "fixture echoed [redacted] and Bearer [redacted]" in raw, (status, value)
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-gemini-live-stt-") as directory:
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
