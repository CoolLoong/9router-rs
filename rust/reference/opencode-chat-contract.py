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
chat_path = "/v1/chat/completions"
messages_path = "/v1/messages"
responses_path = "/v1/responses"
key = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    (data / "opencode-chat-implementation").write_text(implementation)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                          VALUES (?,?,'apikey','Fixture OpenCode',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                       [(f"fixture-opencode-chat-{lane}", f"opencode-{lane}", json.dumps({"apiKey": f"offline-opencode-chat-{lane}"}))
                        for lane in ("go", "zen")])


def request(base, path, payload=None, headers=None):
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    try:
        reply = urlopen(Request(base + path, headers=key | (headers or {}), data=body), timeout=7)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("Content-Type"), reply.read().decode()


def run_cases(base):
    snapshots = {}

    def submit(name, path, body, expected, headers=None):
        status, mime, raw = request(base, path, body, headers)
        assert status == 200, (name, status, raw)
        if body["stream"]:
            assert mime == "text/event-stream", (name, mime, raw)
            frames = []
            generated = None
            for line in raw.splitlines():
                if not line.startswith("data: "):
                    continue
                value = line.removeprefix("data: ")
                if value == "[DONE]":
                    frames.append(value)
                    continue
                chunk = json.loads(value)
                chunk.pop("created", None)
                identifier = chunk.get("id", "")
                if identifier.startswith("chatcmpl-") and identifier.removeprefix("chatcmpl-").isdigit():
                    assert generated is None or generated == identifier
                    generated = identifier
                    chunk["id"] = "chatcmpl-generated"
                frames.append(chunk)
            assert frames, (name, raw)
            rendered = [frame for frame in frames if isinstance(frame, dict) and (
                frame.get("delta") == expected
                or (isinstance(frame.get("delta"), dict) and frame["delta"].get("text") == expected)
                or (frame.get("choices") and frame["choices"][0]["delta"].get("content") == expected)
            )]
            assert len(rendered) == 1, (name, raw)
            snapshots[name] = {"mime": mime, "frames": frames}
        else:
            value = json.loads(raw)
            content = value["content"][0]["text"] if path == messages_path else value["choices"][0]["message"]["content"]
            assert content == expected
            snapshots[name] = {"mime": mime, "value": value}

    def chat(model, label, stream=False, **fields):
        return {"model": model, "messages": [{"role": "user", "content": label}], "stream": stream, **fields}

    go_session = {"x-opencode-session": "fixture-go-session"}
    submit("go-chat", chat_path, chat("ocg/kimi-k3.1-pro", "Go Chat fixture"), "Go Chat", {"x-session-id": "fixture-client-session"})
    submit("go-messages", messages_path, chat("ocg/minimax-m3", "Go Messages fixture", max_tokens=80), "Go Messages", go_session)
    submit("go-responses", responses_path, {"model": "ocg/grok-4.7", "input": "Go Responses fixture", "stream": True,
           "max_tokens": 42}, "Go Responses", go_session)
    submit("go-chat-to-responses", chat_path, chat("ocg/grok-4.7", "Go Responses pivot fixture", True, max_tokens=42),
           "Go Responses pivot", go_session)
    submit("zen-chat", chat_path, chat("ocz/gemini-3.6-flash", "Zen Chat fixture", True), "Zen Chat")
    submit("zen-messages", messages_path, chat("ocz/claude-opus-4-6", "Zen Messages fixture", True, max_tokens=80), "Zen Messages")
    submit("zen-responses", responses_path, {"model": "ocz/gpt-5.6-sol", "input": "Zen Responses fixture", "stream": True}, "Zen Responses")
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-opencode-chat-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(root / "rust/app/target/debug/server")]
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
