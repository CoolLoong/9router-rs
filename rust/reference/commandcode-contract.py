import http.client
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
model = "commandcode/claude-haiku-5-5"
fixture = "offline-commandcode-account"
route = "/v1/chat/completions"


def request(port, path, body=None, session=None, admin=False):
    headers = {"Authorization":"Bearer offline-chat-key", "Content-Type":"application/json"}
    if session:
        headers["x-session-id"] = session
    if admin:
        headers["x-9r-cli-token"] = "68977b6fd9422fbb"
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=16)
    try:
        client.request("POST" if body is not None else "GET", path,
                       body=None if body is None else json.dumps(body), headers=headers)
        response = client.getresponse()
        return response.status, response.getheader("Content-Type"), response.read().decode()
    finally:
        client.close()


def prompt(label, **extra):
    return {"model":model, "messages":[{"role":"user", "content":label}], **extra}


def normalize(chunk, identities):
    assert re.fullmatch(r"chatcmpl-\d+", chunk["id"]), chunk
    assert isinstance(chunk["created"], int)
    identities.add((chunk.pop("id"), chunk.pop("created")))
    return chunk


def read_sse(port, label, body, session=None):
    status, mime, raw = request(port, route, body, session)
    assert status == 200 and mime == "text/event-stream", (label, status, mime, raw)
    events = []
    identities = set()
    for line in raw.splitlines():
        if not line.startswith("data: "):
            continue
        data = line.removeprefix("data: ")
        if data == "[DONE]":
            events.append(data)
        else:
            event = json.loads(data)
            events.append(normalize(event, identities) if "id" in event else event)
    assert len(identities) == 1, (label, raw)
    return events


def scenario(port):
    observations = {}
    status, _, raw = request(port, "/api/providers/commandcode-fixture/models", admin=True)
    assert status == 200
    discovery = json.loads(raw)
    assert discovery["models"] == [
        {"id":"fixture-vision-model","name":"Fixture Vision","description":"Synthetic catalog",
         "contextLength":100000,"maxOutputTokens":9000,"isVL":True,"isReasoning":True},
        {"id":"fixture-basic-model","name":"Fixture Basic","description":None,"isVL":False,"isReasoning":False},
    ]
    observations["catalog-management"] = discovery
    status, _, raw = request(port, "/v1/models")
    assert status == 200
    models = json.loads(raw)["data"]
    custom = [entry for entry in models if entry["id"].startswith("cmc/")]
    assert [entry["id"] for entry in custom] == [
        "cmc/fixture-vision-model", "cmc/fixture-basic-model",
    ], [entry["id"] for entry in custom]
    observations["catalog-public"] = custom
    session = "fixture-command-session"
    for label, extra in [
        ("json", {"stream":False}),
        ("default", {}),
        ("retry-in-band", {"stream":False}),
        ("retry-http", {"stream":False}),
    ]:
        reply = request(port, route, prompt(label, **extra), session if label in ("json", "default") else None)
        assert reply[:2] == (200, "application/json"), (label, reply)
        value = normalize(json.loads(reply[2]), set())
        assert value["choices"][0]["message"]["content"] == "fixture 文 completed", value
        assert value["choices"][0]["finish_reason"] == "stop" and value["usage"]["total_tokens"] == 28, value
        observations[label] = value

    tools = {
        "model":model,
        "stream":True,
        "temperature":0.7,
        "top_p":0.9,
        "max_tokens":88,
        "reasoning_effort":"high",
        "messages":[
            {"role":"system","content":[{"type":"text","text":"first rule"},{"type":"text","text":"second rule"}]},
            {"role":"system","content":"last rule"},
            {"role":"assistant","content":"checking fixture","reasoning_content":"internal fixture",
             "tool_calls":[{"id":"history-1","type":"function","function":{"name":"probe","arguments":'{"q":"fixture"}'}}]},
            {"role":"tool","tool_call_id":"history-1","is_error":True,"content":"previous fixture failed"},
            {"role":"user","content":[{"type":"text","text":"tools"},{"type":"image_url",
             "image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAA=="}}]},
        ],
        "tools":[{"type":"function","function":{"name":"probe","description":"fixture utility",
                  "parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}],
    }
    events = read_sse(port, "tools", tools, session)
    assert "[DONE]" not in events
    choices = [event["choices"][0] for event in events]
    assert choices[0]["delta"] == {"role":"assistant","reasoning_content":"fixture thought"}
    assert choices[1]["delta"]["tool_calls"][0]["id"] == "tool-A"
    fragments = [choice["delta"]["tool_calls"][0] for choice in choices[2:4]]
    assert all("id" not in fragment for fragment in fragments)
    assert "".join(fragment["function"]["arguments"] for fragment in fragments) == '{"q":"fixture"}'
    assert choices[4]["delta"]["tool_calls"][0]["id"] == "tool-B"
    assert choices[5]["finish_reason"] == "tool_calls"
    assert events[-1]["usage"]["prompt_tokens_details"]["cached_tokens"] == 6
    observations["tools"] = events

    events = read_sse(port, "mid-stream-error", prompt("mid-stream-error", stream=True))
    assert events[0]["choices"][0]["delta"]["content"] == "fixture before error"
    assert events[-1] == "[DONE]" and events.count("[DONE]") == 1
    assert events[-2]["error"]["message"] == "upstream connection lost", events
    assert not any(event.get("choices", [{}])[0].get("finish_reason") for event in events[:-1])
    observations["mid-stream-error"] = events

    status, mime, raw = request(port, "/v1/messages", prompt("messages-json", stream=False, max_tokens=100))
    assert (status, mime) == (200, "application/json"), (status, mime, raw)
    messages = json.loads(raw)
    assert messages["type"] == "message" and messages["content"] == [{"type":"text","text":"fixture 文 completed"}]
    assert re.fullmatch(r"\d+", messages.pop("id")), messages
    observations["messages-json"] = messages

    status, mime, raw = request(port, route, prompt("http-error", stream=False))
    assert status == 422 and mime == "application/json" and "[422]: synthetic request rejected" in raw, (status, mime, raw)
    observations["http-error"] = json.loads(raw)

    if implementation == "rust":
        status, _, raw = request(port, route, prompt("secret", stream=False))
        assert status == 422 and fixture not in raw and "[redacted]" in raw, (status, raw)
        frames = read_sse(port, "oversize", prompt("oversize", stream=True))
        assert frames == observations["mid-stream-error"], frames

    status, mime, raw = request(port, route, prompt("early-limit", stream=False))
    assert status == 503 and mime == "application/json" and "[429]: [CommandCode error: too many requests in fixture]" in raw, (
        status, mime, raw
    )
    observations["early-limit"] = json.loads(raw)
    status, _, raw = request(52989, "/commandcode/fixture-attempts")
    ledger = json.loads(raw)
    if implementation == "rust":
        assert ledger.pop("secret") == 1
        assert ledger.pop("oversize") == 1
    assert status == 200 and ledger == {
        "json":1, "default":1, "retry-in-band":2, "retry-http":2,
        "tools":1, "mid-stream-error":1, "messages-json":1, "http-error":1, "early-limit":1,
    }, ledger
    observations["attempts"] = ledger
    return observations


def await_port(port, process):
    deadline = time.monotonic() + 15
    while True:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.15):
                return
        except OSError:
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(f"fixture port {port} did not start")
            time.sleep(0.025)


def stop(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=2)


with tempfile.TemporaryDirectory(prefix="9router-commandcode-") as directory:
    scratch = Path(directory)
    data = scratch / "data"
    home = scratch / "home"
    commands = scratch / "bin"
    home.mkdir()
    commands.mkdir()
    shell = commands / "sh"
    shell.write_text("#!/bin/sh\nexit 47\n")
    shell.chmod(0o700)
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], cwd=root,
                   env=dict(os.environ, NODE_NO_WARNINGS="1"), check=True)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        credentials = json.dumps({"apiKey":fixture,"testStatus":"active"})
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES ('commandcode-fixture','commandcode','apikey','Fixture CommandCode',1,1,?,
                              '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""", (credentials,))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME":str(home), "PATH":str(commands), "PORT":str(port), "HOSTNAME":"127.0.0.1",
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_DATA_DIR":str(data), "NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallback.log"), "MODEL_CATALOG_SYNC":"off",
        "NEXT_TELEMETRY_DISABLED":"1", "NODE_NO_WARNINGS":"1",
    }
    node = shutil.which("node")
    mock = str(root / "rust/reference/mock-commandcode.mjs")
    commands = [[str(native)], [node, mock]] if implementation == "rust" else [[
        node, "--import", mock, str(root / ".next/standalone/custom-server.js"),
    ]]
    with (scratch / "fixture.log").open("w") as log:
        processes = []
        try:
            for command in commands:
                processes.append(subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                                                  stdout=log, stderr=log))
            await_port(port, processes[0])
            await_port(52989, processes[-1])
            snapshots = scenario(port)
            if implementation == "rust":
                assert (scratch / "fallback.log").read_text() == ""
            print(json.dumps(snapshots, sort_keys=True, separators=(",", ":")))
        except Exception:
            print((scratch / "fixture.log").read_text(), file=sys.stderr)
            raise
        finally:
            for process in reversed(processes):
                stop(process)
