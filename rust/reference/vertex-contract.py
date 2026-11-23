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
key = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    private = data / "vertex-fixture-key.pem"
    public = data / "vertex-fixture-key.pub"
    with private.open("w") as output:
        private.chmod(0o600)
        subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048"],
                       check=True, stdout=output, stderr=subprocess.DEVNULL)
    subprocess.run(["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(public)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    service_account = json.dumps({"type": "service_account", "project_id": "fixture-sa-project",
                  "client_email": "vertex-fixture@example.invalid", "private_key": private.read_text().replace("\n", "\\n"),
                  "token_uri": "https://example.invalid/unused-credential-field"})
    private.unlink()
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                          VALUES (?,?,'apikey','Fixture Vertex',1,1,'{}','2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                       [("vertex-fixture", "vertex"), ("vertex-partner-fixture", "vertex-partner")])
    return service_account


def configure(database, provider, **values):
    with sqlite3.connect(database) as db:
        db.execute("UPDATE providerConnections SET data=? WHERE id=?", (json.dumps(values), f"{provider}-fixture"))


def request(base, path, payload=None):
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    try:
        reply = urlopen(Request(base + path, headers=key, data=body), timeout=7)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("content-type"), reply.read().decode()


def run_cases(base, database, service_account):
    snapshots = {}

    def ask(name, model, prompt, expected, stream=False, extra_messages=None):
        payload = {"model": model, "stream": stream, "messages": [{"role": "user", "content": prompt}]}
        if extra_messages:
            payload["messages"].extend(extra_messages)
        status, mime, raw = request(base, chat, payload)
        assert status == 200, (name, status, raw)
        if stream:
            assert mime == "text/event-stream", (mime, raw)
            events = []
            for line in raw.splitlines():
                if line.startswith("data: "):
                    text = line.removeprefix("data: ")
                    if text == "[DONE]":
                        events.append(text)
                    else:
                        entry = json.loads(text)
                        assert isinstance(entry.pop("created"), int)
                        events.append(entry)
            assert len(events) >= 3 and events[-1]["choices"][0]["finish_reason"] == "stop", (name, events, raw)
            text = "".join(event.get("choices", [{}])[0].get("delta", {}).get("content", "")
                           for event in events if isinstance(event, dict) and event.get("choices"))
            assert text == expected, (name, events)
            snapshots[name] = {"status": status, "mime": mime, "events": events}
        else:
            value = json.loads(raw)
            assert value["choices"][0]["message"]["content"] == expected, (name, value)
            assert value["usage"]["total_tokens"] == 2011, (name, value["usage"])
            snapshots[name] = {"status": status, "mime": mime, "value": value}

    configure(database, "vertex", apiKey="offline-vertex-projectless")
    ask("raw-projectless", "vertex/gemini-2.5-flash", "vertex projectless fixture", "raw projectless")
    ask("tool-history", "vx/gemini-2.5-flash", "vertex tool fixture", "vertex tool history", extra_messages=[
        {"role": "assistant", "content": None, "tool_calls": [{"id": "call_fixture_vertex", "type": "function",
            "function": {"name": "fixture_lookup", "arguments": '{"name":"vertex"}'}}]},
        {"role": "tool", "tool_call_id": "call_fixture_vertex", "content": '{"ok":true}'},
    ])
    configure(database, "vertex", apiKey="offline-vertex-scoped", providerSpecificData={"projectId": "fixture-raw-project", "location": "eu"})
    ask("raw-scoped-sse", "vertex/gemini-4-argon", "vertex scoped fixture", "raw scoped", stream=True)
    configure(database, "vertex", apiKey="offline-vertex-unused-key", accessToken="offline-vertex-preset-bearer",
              providerSpecificData={"projectId": "fixture-bearer-project"})
    ask("preset-bearer-global", "vx/gemini-4-argon", "vertex bearer fixture", "configured bearer")
    if implementation == "rust":
        status, _, raw = request(base, chat, {"model": "vx/gemini-4-argon", "stream": False,
                                 "messages": [{"role": "user", "content": "vertex bearer redaction fixture"}]})
        assert status == 422 and "offline-vertex-preset-bearer" not in raw and "[redacted]" in raw, (status, raw)
    configure(database, "vertex", apiKey=service_account, providerSpecificData={"projectId": "fixture-ignored-project"})
    for name in ["service-account", "service-account-token-cache"]:
        ask(name, "vertex/gemini-2.5-flash", "vertex service fixture", "service account")

    adc = json.dumps({"type": "authorized_user", "quota_project_id": "fixture-adc-project", "client_id": "offline-vertex-client",
                      "client_secret": "offline-vertex-client-secret", "refresh_token": "offline-vertex-refresh"})
    configure(database, "vertex", apiKey=adc, providerSpecificData={"projectId": "fixture-ignored-project", "location": "us-east1"})
    ask("adc-authorized-user", "vx/gemini-2.5-flash", "vertex adc fixture", "authorized user")

    model = "vxp/deepseek-ai/deepseek-v3.2-maas"
    configure(database, "vertex-partner", apiKey="offline-vertex-partner-probe")
    ask("partner-project-probe", model, "partner probe fixture", "partner discovered")
    ask("partner-project-cache", model, "partner probe fixture", "partner discovered")
    configure(database, "vertex-partner", apiKey="offline-vertex-partner-project", providerSpecificData={"projectId": "fixture-partner-project"})
    ask("partner-configured-project", model, "partner configured fixture", "partner configured")
    if implementation == "rust":
        configure(database, "vertex", apiKey="offline-vertex-projectless")
        status, _, raw = request(base, chat, {"model": "vertex/gemini-2.5-flash", "stream": False,
                                 "messages": [{"role": "user", "content": "vertex key redaction fixture"}]})
        assert status == 422 and "offline-vertex-projectless" not in raw and "[redacted]" in raw, (status, raw)
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-vertex-contract-") as directory:
    data = Path(directory)
    service_account = seed(data)
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
        snapshots = run_cases(base, data / "db/data.sqlite", service_account)
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
