import http.client
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
cli = {"x-9r-cli-token":"68977b6fd9422fbb"}
key = {"authorization":"Bearer offline-chat-key", "x-synthetic-secret":"never-forward"}
catalog_path = "/tenant%2Falpha/v1/models/metadata?include_in_cli=true"
chat_path = "/static-chat/kimchi"
oauth_token = "offline-kimchi-bearer-credential"
api_key = "offline-kimchi-api-authentic-fixture"
inactive_token = "offline-unused-bearer"
model_json = "fixture-deepseek-backed"
model_sse = "kimi-k2.7"
seen = []
problems = []
usage = {"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}
padded = usage | {"prompt_tokens":2005,"total_tokens":2007}


def response_json(model):
    return {
        "id":"chatcmpl-kimchi-json-fixture", "object":"chat.completion", "created":1760004000,
        "model":model, "choices":[{"index":0,"message":{"role":"assistant","content":"Kimchi JSON"},"finish_reason":"stop"}],
        "usage":usage,
    }


def response_sse(model):
    frames = [
        {"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":None}]},
        {"choices":[{"index":0,"delta":{"content":"Kimchi SSE"},"finish_reason":None}]},
        {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage},
    ]
    return [
        {"id":"chatcmpl-kimchi-stream-fixture","object":"chat.completion.chunk","created":1760004000,"model":model, **frame}
        for frame in frames
    ] + ["[DONE]"]


class Wire(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, payload, content_type="application/json"):
        out = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        try:
            self.wfile.write(out)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if self.path != catalog_path or self.headers.get("authorization") != "Bearer " + oauth_token:
            problems.append("unexpected catalog route")
            return self.reply(422, {"error":"wrong fixture catalog endpoint"})
        seen.append("catalog")
        return self.reply(200, {"models":[{"id":model_json,"provider":"anthropic","reasoning":True}]})

    def do_POST(self):
        if self.path != chat_path:
            problems.append("completion not sent to fixed Kimchi endpoint")
            return self.reply(422, {"error":"wrong completion endpoint"})
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        bearer = self.headers.get("authorization")
        forbidden = ("x-synthetic-secret","x-api-key","anthropic-version","x-9r-cli-token","cookie")
        admitted = (self.headers.get("user-agent","").startswith("9Router/" if mode == "native" else "kimchi/")
                    and not any(self.headers.get(header) for header in forbidden))
        if not admitted:
            problems.append("unexpected completion headers")
            return self.reply(422, {"error":"wrong completion headers"})
        model = body["model"]
        if model == "fixture-kimchi-error" and bearer == "Bearer " + oauth_token:
            seen.append("error")
            return self.reply(400, {"error":{"message":"rejected " + oauth_token}})
        expected = ["system","assistant","assistant","user"]
        messages = body.get("messages") or []
        clean = all(field not in body for field in (
            "system","anthropic_version","anthropic_beta","client_metadata","mcp_servers","stop_sequences","thinking","top_k",
        ))
        clean &= [row.get("role") for row in messages] == expected
        if not clean:
            problems.append({"expected clean request":body})
            return self.reply(422, {"error":"incorrect completion shape"})
        clean &= all("cache_control" not in row for row in messages)
        clean &= "reasoning_content" not in messages[1] and messages[2].get("reasoning_content") == " "
        clean &= messages[3]["content"] == [{"type":"text","text":"Question"}]
        clean &= "cache_control" not in body["tools"][0]
        clean &= messages[0]["content"] == "Added\nSecond\n\nOriginal system"
        if model == model_json and body.get("stream") is False and bearer == "Bearer " + oauth_token:
            clean &= "reasoning_effort" not in body and "reasoning" not in body
            if clean:
                seen.append("json")
                return self.reply(200, response_json(model))
        if model == model_sse and body.get("stream") is True and bearer == "Bearer " + api_key:
            if clean:
                seen.append("sse")
                raw = "".join("data: " + (json.dumps(frame) if isinstance(frame,dict) else frame) + "\n\n"
                              for frame in response_sse(model))
                return self.reply(200, raw.encode(), "text/event-stream")
        problems.append({"completion mismatch":body})
        return self.reply(422, {"error":"incorrect completion body"})


def seed(root, stream):
    root.mkdir()
    home, data, commands = root / "home", root / "data", root / "bin"
    home.mkdir()
    commands.mkdir()
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)],
                   cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    stored = {"accessToken":inactive_token if stream else oauth_token, "providerSpecificData":{
        "kimchiEndpoint":"http://127.0.0.1:52989/tenant%2Falpha",
    }}
    if stream:
        stored["apiKey"] = api_key
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("DELETE FROM providerConnections")
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES('selected','kimchi',?,'Selected',1,1,?,'2026-11-01T00:00:00Z','2026-11-01T00:00:00Z')""",
                   ("apikey" if stream else "oauth",json.dumps(stored)))
    return home, data, commands


@contextmanager
def gateway(root, stream):
    home,data,commands = seed(root, stream)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    fallback = root / "fallback"
    env = {"HOME":str(home), "DATA_DIR":str(data), "PATH":str(commands), "HOSTNAME":"127.0.0.1",
           "PORT":str(port),"NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(fallback),
           "MODEL_CATALOG_SYNC":"off", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1"}
    command = [str(Path(args[0]).resolve()),"--no-browser"] if mode == "native" else [
        shutil.which("node"), "--import", str(repo / "rust/reference/mock-kimchi-models.mjs"),
        str(repo / ".next/standalone/custom-server.js"),
    ]
    process = subprocess.Popen(command, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1",port), .2):
                    break
            except OSError:
                time.sleep(.025)
        else:
            raise RuntimeError("Kimchi inference fixture did not start")
        yield port
        if mode == "native":
            assert fallback.is_file() and not fallback.read_text().strip()
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def request(port, path, method="GET", body=None, headers=cli):
    client = http.client.HTTPConnection("127.0.0.1",port,timeout=14)
    try:
        headers = dict(headers)
        if body is not None:
            body = json.dumps(body).encode()
            headers["content-type"] = "application/json"
        client.request(method,path,body=body,headers=headers)
        reply = client.getresponse()
        return reply.status, reply.headers.get_content_type(), reply.read().decode()
    finally:
        client.close()


def inference(port, model, stream):
    body = {
        "model":"kimchi/" + model, "stream":stream, "user":"fixture-openai-format",
        "system":[{"type":"text","text":"Added"},"Second"],"reasoning_effort":"high",
        "anthropic_version":"fixture","anthropic_beta":"fixture","client_metadata":{"user_id":"removed"},
        "stop_sequences":["unused"],"top_k":3,
        "messages":[
            {"role":"system","content":"Original system","cache_control":{"type":"ephemeral"}},
            {"role":"assistant","content":"First answer","reasoning_content":"large reasoning block","cache_control":{"type":"ephemeral"}},
            {"role":"assistant","content":"Second answer","reasoning_content":" "},
            {"role":"user","content":[{"type":"text","text":"Question","cache_control":{"type":"ephemeral"},"signature":"removed"}]},
        ],
        "tools":[{"type":"function","function":{"name":"lookup","description":"Safe fixture","parameters":{"type":"object"}},
                  "cache_control":{"type":"ephemeral"}}],
    }
    return request(port,"/v1/chat/completions","POST",body,key | {
        "accept":"text/event-stream" if stream else "application/json",
    })


def run(root):
    observed = {}
    with gateway(root / "json", False) as port:
        status, _, body = request(port,"/api/providers/selected/models")
        assert status == 200 and json.loads(body)["models"][0]["upstreamProvider"] == "anthropic"
        status, content_type, body = inference(port,model_json,False)
        assert status == 200 and content_type == "application/json", (status, body, problems)
        observed["json"] = json.loads(body)
        assert observed["json"] == response_json(model_json) | {"usage":padded}, observed["json"]
        if mode == "native":
            status, _, body = inference(port,"fixture-kimchi-error",False)
            assert status in (400,503) and oauth_token not in body and "[redacted]" in body
    with gateway(root / "sse", True) as port:
        status, content_type, body = inference(port,model_sse,True)
        assert status == 200 and content_type == "text/event-stream", (status, body, problems)
        frames = [line.removeprefix("data: ").strip() for line in body.splitlines() if line.startswith("data: ")]
        observed["sse"] = [json.loads(line) if line != "[DONE]" else line for line in frames]
        expected = [frame | {"usage":padded} if isinstance(frame,dict) and "usage" in frame else frame
                    for frame in response_sse(model_sse)]
        assert observed["sse"] == expected, observed["sse"]
    assert not problems
    assert seen == ["catalog","json"] + (["error"] if mode == "native" else []) + ["sse"]
    return observed


with tempfile.TemporaryDirectory(prefix="9router-kimchi-inference-") as directory:
    upstream = ThreadingHTTPServer(("127.0.0.1",52989),Wire)
    threading.Thread(target=upstream.serve_forever,daemon=True).start()
    try:
        print(json.dumps(run(Path(directory)),sort_keys=True))
    finally:
        upstream.shutdown()
        upstream.server_close()
