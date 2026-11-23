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
chat = "/v1/chat/completions"
messages = "/v1/messages"
claude = "br/us.anthropic.claude-sonnet-4-5-20250929-v1:0"
profile = "br/arn:aws:bedrock:us-west-2:123456789000:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0"
grok = "brx/us.xai.grok-4.6"
headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    accounts = [
        ("bedrock-fixture", "bedrock", {"apiKey":"offline-bedrock-secret", "providerSpecificData":{
            "region":" us-west-2 ", "accessKeyId":"ASIAFIXTUREBEDROCKKEY", "sessionToken":"offline-bedrock-session"}}),
        ("bedrock-xai-fixture", "bedrock-xai", {"apiKey":"offline-bedrock-grok-secret", "providerSpecificData":{
            "accessKeyId":"AKIAFIXTUREGROKKEY"}}),
    ]
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                          VALUES (?,?,'apikey','Fixture Bedrock',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                       [(id_, provider, json.dumps(details)) for id_, provider, details in accounts])


def request(base, path, payload=None):
    data = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    try:
        reply = urlopen(Request(base + path, headers=headers, data=data), timeout=7)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("Content-Type"), reply.read().decode()


def make(model, label, stream=False):
    return {"model":model, "messages":[{"role":"user", "content":label}], "stream":stream, "max_tokens":90}


def native_text(value, wire):
    return value["content"][0]["text"] if wire == messages else value["choices"][0]["message"]["content"]


def run_cases(base):
    snapshots = {}
    for name, action, payload, expected in [
        ("messages-sts-arn", messages, make(profile, "Bedrock Messages JSON fixture") | {"anthropic_version":"mismatch-fixture-version"}, "Bedrock Messages JSON"),
        ("chat-to-messages", chat, make(claude, "Bedrock Chat pivot fixture"), "Bedrock Messages JSON"),
        ("xai-default-region", chat, make(grok, "Bedrock xAI JSON fixture"), "Bedrock xAI JSON"),
    ]:
        status, mime, raw = request(base, action, payload)
        assert status == 200, (name, status, raw)
        response = json.loads(raw)
        assert native_text(response, action) == expected, response
        if "created" in response and action == chat and name == "chat-to-messages":
            assert isinstance(response.pop("created"), int)
        snapshots[name] = {"mime": mime, "body": response}

    streaming = [
        ("messages-stream", messages, claude, "Bedrock Messages stream fixture", "Bedrock Messages SSE"),
        ("xai-stream", chat, grok, "Bedrock xAI stream fixture", "Bedrock xAI SSE"),
        ("message-crc-error", messages, claude, "Bedrock CRC fixture", "AWS EventStream message CRC mismatch"),
        ("xai-exception", chat, grok, "Bedrock exception fixture", "fixture throttled"),
        ("xai-truncation", chat, grok, "Bedrock truncation fixture", "Bedrock stream ended before any choice reported a finish_reason"),
    ]
    for name, action, model, label, expected in streaming:
        status, mime, raw = request(base, action, make(model, label, True))
        assert status == 200 and mime == "text/event-stream", (name, status, mime, raw)
        events = []
        for line in raw.splitlines():
            if not line.startswith("data: "):
                continue
            data = line.removeprefix("data: ")
            events.append(data if data == "[DONE]" else json.loads(data))
        assert expected in raw, (name, raw)
        if name == "messages-stream":
            assert [event for event in events if isinstance(event, dict)][-1]["type"] == "message_stop"
        if name == "xai-stream":
            assert events[-1] == "[DONE]"
        snapshots[name] = {"mime":mime, "events":events}

    if implementation == "rust":
        status, _, raw = request(base, chat, make(claude, "Bedrock redaction fixture"))
        assert status == 422 and "offline-bedrock-secret" not in raw and "offline-bedrock-session" not in raw, (status, raw)
        assert "ASIAFIXTUREBEDROCKKEY" not in raw and "Signature=[redacted]" in raw, raw
    status, _, raw = request(base, chat, make(claude, "Bedrock redirect fixture"))
    assert status in (502, 503) and "incorrectly followed redirect" not in raw, (status, raw)
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-bedrock-contract-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [
        str(Path(sys.argv[2]).resolve()) if len(sys.argv) > 2 else str(root / "rust/app/target/debug/server"),
    ]
    home = data / "home"
    home.mkdir()
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"),
               HOME=str(home), USERPROFILE=str(home))
    for key in [key for key in env if key.startswith("AWS_")]:
        del env[key]
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
