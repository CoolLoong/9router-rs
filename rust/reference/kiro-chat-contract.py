import http.client
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

ROOT = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
route = "/v1/chat/completions"
key = "ksk_fixture_kiro_chat_only"
api = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}


def request(port, path, headers=None, payload=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request("POST" if payload is not None else "GET", path,
                     headers=headers or {}, body=json.dumps(payload) if payload is not None else None)
        response = conn.getresponse()
        return response.status, response.getheader("Content-Type", "").split(";")[0], response.read().decode()
    finally:
        conn.close()


def body(text, stream):
    return {"model": "kr/auto", "messages": [{"role": "user", "content": f"fixture {text}"}], "stream": stream}


def credential(db_path, kind, token, data, **extra):
    with sqlite3.connect(db_path) as db:
        db.execute("DELETE FROM providerConnections WHERE provider='kiro'")
        db.execute(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES ('fixture-kiro-chat','kiro',?,'Synthetic Kiro',1,1,?,
                       '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            (kind,json.dumps({"accessToken":token,"providerSpecificData":data,**extra})),
        )


def oauth(db_path, method, token, region):
    profile = f"arn:aws:codewhisperer:{region}:123456789012:profile/chat-fixture"
    credential(db_path, "oauth", token, {"authMethod":method,"region":region,"profileArn":profile},
               refreshToken="offline-fixture-kiro-refresh", expiresAt="2099-01-01T00:00:00.000Z")
    return profile


with tempfile.TemporaryDirectory(prefix="9router-kiro-chat-http-") as directory:
    home = Path(directory) / "home"
    home.mkdir()
    data = Path(directory) / "data"
    fallbacks = Path(directory) / "fallback.log"
    subprocess.run(
        [sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
        check=True, stderr=subprocess.DEVNULL,
    )
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    environment = {
        "PATH": os.environ["PATH"], "PORT": str(port), "HOSTNAME": "127.0.0.1", "HOME": str(home),
        "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    server = subprocess.Popen([binary, "--no-browser"], cwd=ROOT, env=environment,
                              start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and server.poll() is None:
            try:
                if request(port, "/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.05)
        else:
            raise RuntimeError("Native gateway did not start")
        db_path = data / "db/data.sqlite"
        credential(db_path, "api_key", key, {"authMethod":"api_key","region":"us-east-1"})
        status, mime, raw = request(port, route, api, body("json", False))
        assert (status, mime) == (200, "application/json"), (status, mime, raw)
        result = json.loads(raw)
        assert result["choices"][0]["message"]["content"] == "Kiro fixture 世界"
        assert result["choices"][0]["finish_reason"] == "stop"
        assert result["usage"]["completion_tokens"] == 2
        status, mime, raw = request(port, route, api, body("sse", True))
        assert (status, mime) == (200, "text/event-stream"), (status, mime, raw)
        sse = [line[6:] for line in raw.splitlines() if line.startswith("data: ")]
        assert sse.pop() == "[DONE]"
        frames = [json.loads(event) for event in sse]
        assert "".join(frame["choices"][0]["delta"].get("content", "") for frame in frames) == "Kiro fixture 世界"
        assert frames[-1]["choices"][0]["finish_reason"] == "stop"
        assert frames[-1]["usage"]["completion_tokens"] == 2
        tool_body = body("tool calls", False)
        tool_body["tools"] = [{"type":"function","function":{"name":"lookup_ticket",
            "parameters":{"type":"object","properties":{"ticket":{"type":"string"}}}}}]
        status, mime, raw = request(port, route, api, tool_body)
        assert (status, mime) == (200, "application/json"), (status, mime, raw)
        choice = json.loads(raw)["choices"][0]
        call, = choice["message"]["tool_calls"]
        assert choice["finish_reason"] == "tool_calls" and call["id"] == "fixture_ticket_call"
        assert call["function"]["name"] == "lookup_ticket" and json.loads(call["function"]["arguments"]) == {"ticket":"T3"}
        status, mime, raw = request(port, route, api, {**tool_body,"stream":True})
        assert (status, mime) == (200, "text/event-stream") and "fixture_ticket_call" in raw and "[DONE]" in raw
        tool_body["messages"].extend([{"role":"assistant","content":None,"tool_calls":[call]},
                                        {"role":"tool","tool_call_id":call["id"],"content":"Ticket T3 is open"}])
        status, mime, raw = request(port, route, api, tool_body)
        assert (status, mime) == (200, "application/json"), (status, mime, raw)
        assert json.loads(raw)["choices"][0]["message"]["content"] == "Kiro fixture 世界"
        image_body = body("image", False)
        image_body["messages"][0]["content"] = [{"type":"text","text":"fixture image"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,ZmFrZS1pbWFnZQ=="}}]
        status, mime, raw = request(port, route, api, image_body)
        assert (status, mime) == (200, "application/json") and json.loads(raw)["choices"][0]["message"]["content"] == "Kiro fixture 世界"
        reasoning = {**body("GPT reasoning", False),"model":"kr/gpt-5.6-luna","reasoning":{"effort":"max"}}
        status, mime, raw = request(port, route, api, reasoning)
        assert (status, mime) == (200, "application/json") and json.loads(raw)["choices"][0]["message"]["content"] == "Kiro fixture 世界", raw
        session = {**api,"X-Session-Id":"fixture-application-session"}
        assert request(port, route, session, body("session first", False))[0] == 200
        assert request(port, route, session, body("session no history", False))[0] == 200
        history = body("session history", False)
        history["messages"][:0] = [{"role":"user","content":"Another copy of the first turn"},
                                  {"role":"assistant","content":"Known previous answer"}]
        assert request(port, route, session, history)[0] == 200
        another = {**session,"Authorization":"Bearer offline-active-key"}
        assert request(port, route, another, body("session another owner", False))[0] == 200
        body_id = {"prompt_cache_key":"fixture-body-session"}
        assert request(port, route, api, {**body("session first", False),**body_id})[0] == 200
        assert request(port, route, api, {**body("session no history", False),**body_id})[0] == 200
        status, mime, raw = request(port, route, api, body("refusal", True))
        assert (status, mime) == (200, "text/event-stream")
        assert '"kiro_terminal_refusal"' in raw and "Buffered private response" not in raw
        status, mime, raw = request(port, route, api, body("unauthorized", False))
        assert status in {401, 503} and "[redacted]" in raw and key not in raw

        with sqlite3.connect(db_path) as db:
            overrides = json.dumps({"providerOverrides":{"kiro":{"headers":{
                "tokentype":"incorrect_type","x-amz-sso-bearer":"incorrect_bearer",
                "x-amzn-codewhisperer-profile-arn":"incorrect_profile","x-amz-target":"incorrect_target",
            }}}})
            db.execute("INSERT INTO settings(id,data) VALUES(1,?) "
                       "ON CONFLICT(id) DO UPDATE SET data=json_patch(data, excluded.data)", (overrides,))
        profile = oauth(db_path, "idc", "offline-fixture-kiro-idc", "eu-central-1")
        status, mime, raw = request(port, route, api, body("oauth idc", False))
        assert (status, mime) == (200, "application/json"), (status, mime, raw)
        assert json.loads(raw)["choices"][0]["message"]["content"] == "Kiro fixture 世界"
        status, mime, raw = request(port, route, api, body("unauthorized", False))
        assert status in {401,503} and "[redacted]" in raw and profile not in raw and "offline-fixture-kiro-idc" not in raw
        oauth(db_path, "github", "offline-fixture-kiro-social", "us-east-1")
        status, mime, raw = request(port, route, api, body("oauth social", True))
        assert (status, mime) == (200, "text/event-stream"), (status, mime, raw)
        assert '"content":"世界"' in raw and "data: [DONE]" in raw
        oauth(db_path, "external_idp", "offline-fixture-kiro-external", "us-east-1")
        status, mime, raw = request(port, route, api, body("external alternate", False))
        assert (status,mime) == (200,"application/json"), (status,mime,raw)
        assert json.loads(raw)["choices"][0]["message"]["content"] == "Kiro alternate 世界"
        assert request(port, route, session, body("session alternate", False))[0] == 200
        status, mime, raw = request(port, route, api, body("bad payload", False))
        assert status == 400 and "Fixture payload rejected" in raw
        assert fallbacks.read_bytes() == b""
        print("Native Kiro API-key and OAuth JSON/SSE, tool calls, images, reasoning, scoped session replay, alternate endpoint and rejection passed without fallbacks")
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
