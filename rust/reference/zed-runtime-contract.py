import http.client
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Barrier, Event, Thread
import time
from itertools import count

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
tunneled = implementation in {"connect", "socks5h"}
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
model_route = "/api/providers/zed-a/models"
session = {"x-9r-cli-token":"68977b6fd9422fbb"}
gateway = {"Authorization":"Bearer offline-chat-key"}
messages_auth = {"x-api-key":"offline-chat-key","anthropic-version":"2023-06-01"}
access = {"17":"offline-zed-account-one","42":"offline-zed-account-two"}
organizations = {"17":"org-personal","42":"org-explicit"}
models = [
    {"id":["fixture-claude"],"provider":"anthropic","display_name":"Zed Claude","max_token_count":43210,
     "supports_tools":True,"supports_images":True,"supported_effort_levels":["low","high"]},
    {"id":{"id":"fixture-google"},"provider":"google","displayName":"Zed Google","maxTokenCount":76543,
     "maxOutputTokens":200,"supports_thinking":True,"supports_disabling_thinking":True},
    {"id":"fixture-responses-vision","provider":"open_ai","supports_fast_mode":True,
     "supports_server_side_compaction":True,"supports_streaming_tools":True,
     "supports_tools":True,"supports_images":True,"supports_parallel_tool_calls":True},
    {"id":"fixture-xai","provider":"x_ai","supports_parallel_tool_calls":True},
    {"id":"fixture-disabled","provider":"open_ai","is_disabled":True},
]
ledger = []
failures = []
leases = {}
sequence = count(1)
release = Event()
mode = "ordinary"
image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII="
tool_schema = {"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
tool_payload = {
    "model":"zd/fixture-claude",
    "messages":[
        {"role":"system","content":"Offline test instructions"},
        {"role":"user","content":[{"type":"text","text":"Review the image"},
                                   {"type":"image_url","image_url":{"url":"data:image/png;base64,"+image}}]},
        {"role":"assistant","content":None,"tool_calls":[
            {"id":"call_prior","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"杭州\"}"}},
        ]},
        {"role":"tool","tool_call_id":"call_prior","content":"Previous lookup"},
        {"role":"user","content":"Look up two more cities"},
    ],
    "tools":[{"type":"function","function":{"name":"lookup","description":"Look up a city","parameters":tool_schema}}],
    "tool_choice":"required",
    "max_tokens":512,
}
messages_payload = {
    "model":"zd/fixture-claude",
    "system":[{"type":"text","text":"x-anthropic-billing-header: untrusted"},
              {"type":"text","text":"Offline test instructions"},
              {"type":"text","text":"Use the supplied result"}],
    "messages":[
        {"role":"user","content":[{"type":"text","text":"Review the image"},
                                   {"type":"image","source":{"type":"base64","media_type":"image/png","data":image}}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"call_prior","name":"lookup","input":{"city":"杭州"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_prior","content":"Previous lookup","is_error":False}]},
        {"role":"user","content":"Look up two more cities"},
    ],
    "tools":[{"name":"lookup","description":"Look up a city","input_schema":tool_schema}],
    "tool_choice":{"type":"any"},
    "max_tokens":512,
}
responses_schema = tool_schema | {"additionalProperties":False}
responses_payload = {
    "model":"zd/fixture-responses-vision",
    "messages":[
        tool_payload["messages"][0],
        {"role":"user","content":[{"type":"text","text":"Review the image"},
                                   {"type":"image_url","image_url":{"url":"data:image/png;base64,"+image,"detail":"high"}}]},
        *tool_payload["messages"][2:],
    ],
    "tools":[{"type":"function","function":{"name":"lookup","description":"Look up a city",
                                          "parameters":responses_schema,"strict":True}}],
    "max_tokens":512,
}
direct_responses = {
    "model":"zd/fixture-responses-vision","instructions":"Offline test instructions",
    "max_output_tokens":512,
    "input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"Review the image"},
                  {"type":"input_image","detail":"high","image_url":"data:image/png;base64,"+image}]},
        {"type":"function_call","call_id":"call_prior","name":"lookup","arguments":"{\"city\":\"杭州\"}"},
        {"type":"function_call_output","call_id":"call_prior","output":"Previous lookup"},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"Look up two more cities"}]},
    ],
    "tools":[{"type":"function","name":"lookup","description":"Look up a city",
              "parameters":responses_schema,"strict":True}],
}


def events(provider, model):
    if provider == "anthropic":
        if mode.startswith("claude-tools") and mode != "claude-tools-error":
            return [
                {"type":"message_start","message":{"id":"tool_fixture","model":model,"usage":{"input_tokens":8}}},
                {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"lookup","id":"toolu_first","input":{}}},
                {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}},
                {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"南京\"}"}},
                {"type":"content_block_stop","index":0},
                {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"lookup","id":"toolu_second","input":{}}},
                {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"上海\"}"}},
                {"type":"content_block_stop","index":2},
                {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}},
                {"type":"message_stop"},
            ]
        return [
            {"type":"message_start","message":{"id":"msg_fixture","model":model,"usage":{"input_tokens":5}}},
            {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},
            {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Zed fixture 世界"}},
            {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}},
            {"type":"message_stop"},
        ]
    if provider == "google":
        return [{"responseId":"fixture-gemini","modelVersion":model,
                 "candidates":[{"content":{"role":"model","parts":[{"text":"Zed fixture 世界"}]},"finishReason":"STOP"}],
                 "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}]
    if provider == "open_ai":
        if mode == "responses-tools":
            first = {"type":"function_call","id":"fc_first","call_id":"call_first","name":"lookup"}
            second = {"type":"function_call","id":"fc_second","call_id":"call_second","name":"lookup"}
            return [
                {"type":"response.created","response":{"id":"resp_fixture","model":model}},
                {"type":"response.output_item.added","output_index":0,"item":first | {"arguments":""}},
                {"type":"response.output_item.added","output_index":1,"item":second | {"arguments":""}},
                {"type":"response.function_call_arguments.delta","item_id":"fc_first","output_index":0,
                 "delta":"{\"city\":"},
                {"type":"response.output_item.done","output_index":1,"item":second | {"arguments":"{\"city\":\"上海\"}"}},
                {"type":"response.function_call_arguments.delta","item_id":"fc_first","output_index":0,
                 "delta":"\"南京\"}"},
                {"type":"response.output_item.done","output_index":0,"item":first | {"arguments":"{\"city\":\"南京\"}"}},
                {"type":"response.completed","response":{"id":"resp_fixture","model":model,
                 "usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}},
            ]
        return [{"type":"response.output_text.delta","delta":"Zed fixture 世界"},
                {"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}]
    return [
        {"id":"fixture-xai","object":"chat.completion.chunk","created":1784419200,"model":model,
         "choices":[{"index":0,"delta":{"role":"assistant","content":"Zed fixture 世界"},"finish_reason":None}]},
        {"id":"fixture-xai","object":"chat.completion.chunk","created":1784419200,"model":model,
         "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
         "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}},
    ]


class Zed(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def saved_account(self):
        user, token = self.headers["Authorization"].split()
        assert access[user] == token and self.headers["x-zed-system-id"] == f"fixture-system-{user}"
        return user

    def scope(self):
        raw = self.headers["Authorization"]
        assert raw.startswith("Bearer ") and self.headers.get("x-zed-system-id") is None
        token = raw.removeprefix("Bearer ")
        return leases[token], token

    def catalog_path(self):
        if not tunneled:
            return self.path
        assert self.headers["Host"] == "cloud.zed.dev"
        assert self.headers["Proxy-Authorization"] is None and self.headers["X-Relay-Target"] is None
        return "/zed-fixture" + self.path

    def do_GET(self):
        try:
            path = self.catalog_path()
            if path == "/zed-fixture/client/users/me":
                user = self.saved_account()
                ledger.append(("user",user))
                return self.reply(200, {"default_organization_id":[organizations[user]]})
            assert path == "/zed-fixture/models"
            assert self.headers["Accept"] == "application/json" and self.headers["x-zed-client-supports-x-ai"] == "true"
            user, token = self.scope()
            ledger.append(("models",user))
            if mode == "model-denied":
                return self.reply(500, {"message":f"Provider echoed {access[user]} and {token}"})
            if mode == "model-refresh" and [call[0] for call in ledger].count("models") == 1:
                return self.reply(401, {"message":"Lease rejected"}, {"x-zed-expired-token":"true"})
            model_items = models if user == "17" else [{"id":"other-catalog","provider":"open_ai"}]
            return self.reply(200, {"models":model_items})
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def stream(self, provider, model, token):
        chunks = [b"\r\n", b'{"status":"started"}\n'] + [
            (("data: " if index % 2 else "") + json.dumps({"event":event}, ensure_ascii=False) + "\r\n").encode()
            for index, event in enumerate(events(provider, model))
        ]
        if mode == "stream-failed":
            chunks.append(json.dumps({"status":{"failed":{"message":"Status echoed " + token}}}).encode() + b"\n")
        else:
            chunks.extend([b'{"status":{"stream_ended":{}}}\n', b"data: [DONE]\n"])
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Content-Length", str(sum(map(len,chunks))))
        if mode == "success-expiry-header":
            self.send_header("x-zed-expired-token","true")
        self.end_headers()
        for index, chunk in enumerate(chunks):
            try:
                self.wfile.write(chunk)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                break
            if mode == "pause" and index == 2:
                assert release.wait(5)

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        try:
            path = self.catalog_path()
            if path == "/zed-fixture/client/llm_tokens":
                user = self.saved_account()
                assert data == {"organization_id":organizations[user]}
                ledger.append(("lease",user))
                token = f"offline-zed-lease-{next(sequence)}"
                leases[token] = user
                return self.reply(200, {"token":[token] if user == "17" else {"value":token}})
            assert path == "/zed-fixture/completions"
            user, token = self.scope()
            assert user == "17" and self.headers.get("User-Agent") == "9router/zed"
            assert self.headers["x-zed-client-supports-status-messages"] == "true"
            assert self.headers["x-zed-client-supports-stream-ended-request-completion-status"] == "true"
            if implementation != "reference":
                assert self.headers.get("x-zed-version") is None
            model, provider, body = data["model"], data["provider"], data["provider_request"]
            expected = {m["id"] if isinstance(m["id"],str) else (m["id"][0] if isinstance(m["id"], list)
                        else m["id"]["id"]):m["provider"] for m in models}
            assert provider == expected[model] and body["model"] == model
            if mode.startswith("claude-tools"):
                choice = "none" if mode == "claude-tools-error" else "any"
                assert provider == "anthropic" and body["tool_choice"] == {"type":choice}, body
                assert body["max_tokens"] == 32000, body
                assert body["tools"] == [{"name":"lookup","description":"Look up a city","input_schema":tool_schema,
                                           "cache_control":{"type":"ephemeral","ttl":"1h"}}], body["tools"]
                instructions = ("Offline test instructions\nUse the supplied result"
                                if mode != "claude-tools" else "Offline test instructions")
                assert any(block["text"] == instructions for block in body["system"]), body
                assert not any("x-anthropic-billing-header" in block["text"] for block in body["system"]), body
                if implementation != "reference":
                    assert len(body["system"]) == 1 and "_toolNameMap" not in body, body
                required = {
                    "image":{"type":"image","source":{"type":"base64","media_type":"image/png","data":image}},
                    "tool_use":{"type":"tool_use","id":"call_prior","name":"lookup","input":{"city":"杭州"}},
                    "tool_result":{"type":"tool_result","tool_use_id":"call_prior","content":"Previous lookup",
                                   "is_error":mode == "claude-tools-error"},
                }
                for message in body["messages"]:
                    for block in message["content"]:
                        if block["type"] in required:
                            expected_block = required.pop(block["type"])
                            assert {key:value for key,value in block.items() if key != "cache_control"} == expected_block, block
                assert not required, required
            elif provider == "anthropic":
                assert body["messages"][0]["content"] in ("Offline message", [{"type":"text","text":"Offline message"}])
                if mode == "messages-text":
                    assert any(block["text"] == "Plain Messages system" for block in body["system"]), body
                    if implementation != "reference":
                        assert len(body["system"]) == 1
            elif provider == "google":
                assert "safetySettings" not in body and body["contents"][0]["parts"] == [{"text":"Offline message"}]
            elif provider == "open_ai":
                if mode == "responses-tools":
                    assert body == direct_responses | {"model":model,"stream":True,"store":False}, body
                elif mode == "responses-history":
                    assert body["input"] == [
                        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Previous narrative"}]},
                        {"type":"function_call","call_id":"call_first","name":"lookup","arguments":"{\"city\":\"南京\"}"},
                        {"type":"function_call","call_id":"call_second","name":"lookup","arguments":"{\"city\":\"上海\"}"},
                        {"type":"function_call_output","call_id":"call_first","output":"First result"},
                        {"type":"function_call_output","call_id":"call_second","output":"Second result"},
                        {"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize both results"}]},
                    ], body
                else:
                    assert body["input"][0]["content"] == [{"type":"input_text","text":"Offline message"}]
                    assert body["store"] is False
            else:
                assert body["messages"] == [{"role":"user","content":"Offline message"}]
            if provider != "google":
                assert body["stream"] is True
            ledger.append(("completion", provider, model))
            if mode == "trial":
                return self.reply(402, {"code":"trial_blocked","message":f"Trial rejected {access[user]} {token}"})
            if mode == "completion-redirect":
                return self.reply(307, {"error": "Unexpected redirection"},
                                  {"Location": "https://api.cline.bot/should-not-follow"})
            if mode == "completion-refresh" and [call[0] for call in ledger].count("completion") == 1:
                return self.reply(401, {"error":"Lease no longer accepted"}, {"x-zed-outdated-token":"true"})
            if mode == "concurrent-refresh" and token == stale_token:
                simultaneous.wait(5)
                return self.reply(401, {"error":"Lease no longer accepted"}, {"x-zed-outdated-token":"true"})
            return self.stream(provider, model, token)
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})


def request(port, path, body=None, headers=session):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        connection.request("GET" if body is None else "POST", path,
                     headers=headers | ({"Content-Type":"application/json"} if body is not None else {}),
                     body=None if body is None else json.dumps(body))
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type", "").split(";")[0], response.read().decode()
    finally:
        connection.close()


def seed(data):
    subprocess.run([sys.executable,"rust/reference/seed.py",str(data)],cwd=root,check=True,stderr=subprocess.DEVNULL)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("UPDATE providerConnections SET isActive=0")
        rows = [("zed-a","17",True),("zed-b","42",False)]
        for identifier,user,active in rows:
            details = {"userId":user,"systemId":f"fixture-system-{user}"}
            if user == "42":
                details["organizationId"] = organizations[user]
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'zed','oauth','Fixture',1,?,?,
                  '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                (identifier,int(active),json.dumps({"accessToken":access[user],"providerSpecificData":details})))


def chat(port, kind, streaming=False):
    return request(port, "/v1/chat/completions", {
        "model":"zd/" + kind,"messages":[{"role":"user","content":"Offline message"}],"stream":streaming,
    }, gateway)


with tempfile.TemporaryDirectory(prefix="9router-zed-runtime-") as temporary, ExitStack() as stack:
    directory = Path(temporary)
    data = directory / "data"
    commands = directory / "bin"
    commands.mkdir()
    fallback = directory / "fallbacks.txt"
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    server = ThreadingHTTPServer(("127.0.0.1",52989), Zed)
    Thread(target=server.serve_forever, daemon=True).start()
    environment = {"PATH":os.environ["PATH"] if implementation == "reference" else str(commands),
                   "HOME":temporary,"DATA_DIR":str(data),"PORT":str(port),"HOSTNAME":"127.0.0.1",
                   "MODEL_CATALOG_SYNC":"off","NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(fallback)}
    if tunneled:
        fixture = runpy.run_path(str(root / "rust/reference/zed-cline-models-proxy-contract.py"))
        anchor, vendor, wrong_host = fixture["tls_services"](stack, directory, Zed)
        proxy = fixture["Proxy"](("127.0.0.1", 0), fixture["Tunnel"])
        proxy.mode = "connect" if implementation == "connect" else "socks"
        proxy.reject, proxy.target, proxy.secrets = False, vendor, tuple(access.values())
        stack.enter_context(fixture["serving"](proxy))
        address = f"{'http' if implementation == 'connect' else 'socks5h'}://127.0.0.1:{proxy.server_address[1]}"
        environment["NINEROUTER_MODEL_LOCAL_PROXY_CA"] = str(anchor)
        with sqlite3.connect(data / "db/data.sqlite") as db:
            db.execute("""UPDATE providerConnections SET data=json_set(data,
                '$.providerSpecificData.connectionProxyEnabled',json('true'),
                '$.providerSpecificData.connectionProxyUrl',?) WHERE provider='zed'""", (address,))
    command = (["node","--import","./rust/reference/mock-zed-runtime.mjs",".next/standalone/custom-server.js"]
               if implementation == "reference" else [str(binary),"--no-browser"])
    process = subprocess.Popen(command,cwd=root,env=environment,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic()+14
        while process.poll() is None and time.monotonic() < deadline:
            try:
                if request(port,"/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.04)
        else:
            raise RuntimeError("Fixture gateway did not start")
        assert request(port,model_route,headers={})[0] == 401 and not ledger
        mode = "model-refresh"
        status,mime,raw = request(port,model_route)
        assert (status,mime) == (200,"application/json"), (status,mime,raw)
        snapshot = {"models":json.loads(raw)}
        assert [model["id"] for model in snapshot["models"]["models"]] == [
            "fixture-claude","fixture-google","fixture-responses-vision","fixture-xai"]
        assert ledger == [("user","17"),("lease","17"),("models","17"),
                          ("user","17"),("lease","17"),("models","17")], ledger
        mode = "ordinary"
        status,mime,raw = request(port,"/api/providers/zed-b/models")
        assert (status,mime) == (200,"application/json")
        snapshot["other"] = json.loads(raw)
        assert [model["id"] for model in snapshot["other"]["models"]] == ["other-catalog"]
        assert ledger[-2:] == [("lease","42"),("models","42")]

        for model in ["fixture-claude","fixture-google","fixture-responses-vision","fixture-xai"]:
            status,mime,raw = chat(port,model)
            assert (status,mime) == (200,"application/json"), (model,status,mime,raw,failures)
            message = json.loads(raw)["choices"][0]["message"]
            assert message["content"] == "Zed fixture 世界", (model,raw)
            snapshot[model] = message["content"]
        status,mime,raw = chat(port,"fixture-responses-vision",True)
        assert (status,mime) == (200,"text/event-stream")
        assert "Zed fixture 世界" in raw and raw.count("data: [DONE]") == 1
        assert not failures, failures
        mode = "claude-tools"
        expected_calls = [
            {"id":"toolu_first","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"南京\"}"}},
            {"id":"toolu_second","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"上海\"}"}},
        ]
        status,mime,raw = request(port,"/v1/chat/completions",tool_payload | {"stream":False},gateway)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        choice = json.loads(raw)["choices"][0]
        assert choice["message"]["tool_calls"] == expected_calls and choice["finish_reason"] == "tool_calls", choice
        snapshot["claude tools JSON"] = choice["message"]["tool_calls"]
        status,mime,raw = request(port,"/v1/chat/completions",tool_payload | {"stream":True},gateway)
        assert (status,mime) == (200,"text/event-stream"), (status,mime,raw,failures)
        assert raw.count("data: [DONE]") == 1
        deltas = [
            json.loads(line[6:])["choices"][0]
            for line in raw.splitlines()
            if line.startswith("data: ") and line != "data: [DONE]" and json.loads(line[6:]).get("choices")
        ]
        starts = [(tool["index"], tool["id"], tool["function"]["name"]) for entry in deltas
                  for tool in entry["delta"].get("tool_calls", []) if "name" in tool["function"]]
        args = [(tool["index"],tool["function"]["arguments"]) for entry in deltas
                for tool in entry["delta"].get("tool_calls", []) if tool["function"]["arguments"]]
        assert starts == [(0,"toolu_first","lookup"),(1,"toolu_second","lookup")], raw
        assert args == [(0,"{\"city\":"),(0,"\"南京\"}"),(1,"{\"city\":\"上海\"}")], raw
        assert [entry["finish_reason"] for entry in deltas if entry["finish_reason"]] == ["tool_calls"], raw
        snapshot["claude tools SSE"] = {"starts":starts,"arguments":args,"finish":"tool_calls"}
        assert not failures, failures
        mode = "responses-tools"
        expected_calls = [
            {"id":"call_first","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"南京\"}"}},
            {"id":"call_second","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"上海\"}"}},
        ]
        status,mime,raw = request(port,"/v1/chat/completions",responses_payload | {"stream":False},gateway)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        response = json.loads(raw)
        choice = response["choices"][0]
        assert choice["message"]["tool_calls"] == expected_calls and choice["finish_reason"] == "tool_calls", response
        assert {key:response["usage"][key] for key in ("prompt_tokens","completion_tokens","total_tokens")} == {
            "prompt_tokens":8,"completion_tokens":4,"total_tokens":12,
        }, response
        snapshot["OpenAI tools JSON"] = {"tool_calls":choice["message"]["tool_calls"],"usage":response["usage"]}
        status,mime,raw = request(port,"/v1/chat/completions",responses_payload | {"stream":True},gateway)
        assert (status,mime) == (200,"text/event-stream"), (status,mime,raw,failures)
        assert raw.count("data: [DONE]") == 1
        response_chunks = [json.loads(line[6:]) for line in raw.splitlines()
                           if line.startswith("data: ") and line != "data: [DONE]"]
        choices = [chunk["choices"][0] for chunk in response_chunks if chunk.get("choices")]
        calls = [tool for entry in choices for tool in entry["delta"].get("tool_calls", [])]
        assert [(tool["index"],tool["id"],tool["function"]["name"]) for tool in calls
                if "name" in tool["function"]] == [(0,"call_first","lookup"),(1,"call_second","lookup")], raw
        assert [(tool["index"],tool["function"]["arguments"]) for tool in calls
                if tool["function"]["arguments"]] == [(0,"{\"city\":"),(1,"{\"city\":\"上海\"}"),(0,"\"南京\"}")], raw
        assert [entry["finish_reason"] for entry in choices if entry["finish_reason"]] == ["tool_calls"], raw
        stream_usage = next(chunk["usage"] for chunk in response_chunks if chunk.get("usage"))
        snapshot["OpenAI tools SSE"] = {"tool_calls":calls,"usage":stream_usage,"finish":"tool_calls"}
        expected_usage = {"input_tokens":8,"output_tokens":4,"total_tokens":12}
        expected_tools = [
            {"type":"function_call","call_id":call["id"],**call["function"]} for call in expected_calls
        ]
        tool_fields = ("type","call_id","name","arguments")
        status,mime,raw = request(port,"/v1/responses",direct_responses | {"stream":False,"store":False},gateway)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        direct = json.loads(raw)
        assert direct["object"] == "response" and direct["status"] == "completed", direct
        assert direct["usage"] == expected_usage, direct
        actual_tools = [{key:item[key] for key in tool_fields} for item in direct["output"]]
        assert actual_tools == expected_tools, direct
        snapshot["direct Responses tools JSON"] = {"output":actual_tools,"usage":direct["usage"]}
        mode = "responses-history"
        history = [
            {"type":"message","id":"msg_narrative","status":"completed","role":"assistant","content":[
                {"type":"output_text","text":"Previous narrative","annotations":[],"logprobs":[]},
            ]},
            *(item | {"status":"completed"} for item in direct["output"]),
            {"type":"function_call_output","call_id":"call_first","output":"First result"},
            {"type":"function_call_output","call_id":"call_second","output":"Second result"},
            {"role":"user","content":"Summarize both results"},
        ]
        status,mime,raw = request(port,"/v1/responses",direct_responses | {"input":history,"stream":False},gateway)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        followup = json.loads(raw)["output"][0]["content"]
        assert followup == [{"type":"output_text","text":"Zed fixture 世界","annotations":[]}], followup
        snapshot["direct Responses history JSON"] = followup

        mode = "responses-tools"
        status,mime,raw = request(port,"/v1/responses",direct_responses | {"stream":True,"store":False},gateway)
        assert (status,mime) == (200,"text/event-stream"), (status,mime,raw,failures)
        response_events = [json.loads(line[6:]) for line in raw.splitlines()
                           if line.startswith("data: ") and line != "data: [DONE]"]
        order = [
            "response.created","response.in_progress",
            "response.output_item.added","response.output_item.added",
            "response.function_call_arguments.delta","response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done","response.output_item.done",
            "response.function_call_arguments.done","response.output_item.done","response.completed",
        ]
        assert [event["type"] for event in response_events] == order, raw
        assert [line[7:] for line in raw.splitlines() if line.startswith("event: ")] == order, raw
        assert "data: [DONE]" not in raw
        assert [
            (event["output_index"],event["delta"]) for event in response_events
            if event["type"] == "response.function_call_arguments.delta"
        ] == [(0,"{\"city\":"),(1,"{\"city\":\"上海\"}"),(0,"\"南京\"}")], raw
        completed = response_events[-1]["response"]
        assert completed["status"] == "completed" and completed["usage"] == expected_usage, raw
        completed_tools = [{key:item[key] for key in tool_fields} for item in completed["output"]]
        assert completed_tools == expected_tools, raw
        snapshot["direct Responses tools SSE"] = {
            "events":response_events[2:-1],"output":completed_tools,"usage":completed["usage"],
        }
        mode = "ordinary"
        status,mime,raw = request(port,"/v1/responses",{
            "model":"zd/fixture-responses-vision","input":"Offline message",
        },gateway)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        direct_text = json.loads(raw)
        assert direct_text["output"][0]["content"] == [{"type":"output_text","text":"Zed fixture 世界","annotations":[]}], direct_text
        snapshot["direct Responses text JSON"] = direct_text["output"][0]["content"]
        assert not failures, failures
        mode = "messages-text"
        status,mime,raw = request(port,"/v1/messages",{"model":"zd/fixture-claude","max_tokens":64,
            "system":"Plain Messages system","messages":[{"role":"user","content":"Offline message"}]},messages_auth)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        plain = json.loads(raw)
        assert plain["type"] == "message" and plain["content"] == [{"type":"text","text":"Zed fixture 世界"}]
        assert plain["stop_reason"] == "end_turn" and plain["usage"] == {"input_tokens":5,"output_tokens":2}, plain
        snapshot["plain Messages JSON"] = {key:plain[key] for key in ("type","content","stop_reason","usage")}

        mode = "claude-tools-messages"
        messages_content = [
            {"type":"tool_use","id":"toolu_first","name":"lookup","input":{"city":"南京"}},
            {"type":"tool_use","id":"toolu_second","name":"lookup","input":{"city":"上海"}},
        ]
        status,mime,raw = request(port,"/v1/messages",messages_payload | {"stream":False},messages_auth)
        assert (status,mime) == (200,"application/json"), (status,mime,raw,failures)
        message = json.loads(raw)
        assert message["type"] == "message" and message["role"] == "assistant" and message["content"] == messages_content
        assert message["stop_reason"] == "tool_use" and message["usage"] == {"input_tokens":8,"output_tokens":4}, message
        snapshot["tool Messages JSON"] = {
            key:message[key] for key in ("type","role","content","stop_reason","usage")
        }
        status,mime,raw = request(port,"/v1/messages",messages_payload | {"stream":True},messages_auth)
        assert (status,mime) == (200,"text/event-stream"), (status,mime,raw,failures)
        assert "data: [DONE]" not in raw
        messages_events = [
            json.loads(line[6:])
            for line in raw.splitlines()
            if line.startswith("data: ") and line != "data: [DONE]"
        ]
        expected_order = [
            ("message_start",None),("content_block_start",0),("content_block_start",1),
            ("content_block_delta",0),("content_block_stop",0),
            ("content_block_delta",1),("content_block_stop",1),("message_delta",None),("message_stop",None),
        ]
        assert [(entry["type"],entry.get("index")) for entry in messages_events] == expected_order, raw
        assert [line[7:] for line in raw.splitlines() if line.startswith("event: ")] == [
            kind for kind,_ in expected_order
        ], raw
        start, first, second, first_args, _, second_args, _, last, _ = messages_events
        assert start["message"]["content"] == [] and start["message"]["role"] == "assistant", start
        for index, (tool, delta) in enumerate(zip((first,second),(first_args,second_args))):
            assert tool["content_block"] | {"input":json.loads(delta["delta"]["partial_json"])} == messages_content[index]
        assert last["delta"]["stop_reason"] == "tool_use", last
        assert {key:last["usage"][key] for key in ("input_tokens","output_tokens")} == {"input_tokens":2008,"output_tokens":4}, last
        snapshot["tool Messages SSE"] = messages_events[1:]
        if implementation == "strict" or tunneled:
            mode = "claude-tools-error"
            failed = json.loads(json.dumps(messages_payload))
            failed["messages"][2]["content"][0]["is_error"] = True
            failed["tool_choice"] = {"type":"none"}
            status,mime,raw = request(port,"/v1/messages",failed,messages_auth)
            assert (status,mime) == (200,"application/json") and json.loads(raw)["stop_reason"] == "end_turn", (raw,failures)
        assert not failures, failures
        mode = "ordinary"
        if implementation == "strict" or tunneled:
            ledger.clear()
            remote = {"model":"zd/fixture-claude","messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"http://127.0.0.1:9/private"}},
            ]}]}
            assert request(port,"/v1/chat/completions",remote,gateway)[0] == 503
            assert request(port,"/v1/chat/completions",tool_payload | {"model":"zd/fixture-google"},gateway)[0] == 503
            assert request(port,"/v1/chat/completions",responses_payload | {"tool_choice":"required"},gateway)[0] == 503
            assert request(port,"/v1/messages",messages_payload | {"model":"zd/fixture-google"},messages_auth)[0] == 503
            assert request(port,"/v1/messages",messages_payload,messages_auth | {"anthropic-beta":"experimental-fixture"})[0] == 503
            assert request(port,"/v1/messages",messages_payload | {"thinking":{"type":"enabled","budget_tokens":1000}},
                           messages_auth)[0] == 503
            remote_messages = messages_payload | {"messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"url","url":"http://127.0.0.1:9/private"}},
            ]}]}
            assert request(port,"/v1/messages",remote_messages,messages_auth)[0] == 503
            for unsupported in [
                {"model":"zd/fixture-claude"},
                {"store":True},
                {"previous_response_id":"resp_elsewhere"},
                {"tool_choice":"required"},
                {"reasoning":{"effort":"medium"}},
                {"input":[{"role":"user","content":[{"type":"input_image","image_url":"http://127.0.0.1:9/private"}]}]},
            ]:
                assert request(port,"/v1/responses",direct_responses | unsupported,gateway)[0] == 503, unsupported
            assert not ledger
            expected_fallback = (["POST /v1/chat/completions"] * 3 + ["POST /v1/messages"] * 4 +
                                 ["POST /v1/responses"] * 6) if not tunneled else []
            assert fallback.read_text().splitlines() == expected_fallback
            fallback.write_text("")
            ledger.clear()
            status,_,raw = request(port,"/v1/models",headers=gateway)
            assert status == 200, (status,raw)
            ids = {model["id"] for model in json.loads(raw)["data"]}
            assert {"zd/fixture-claude","zd/fixture-google","zd/fixture-responses-vision","zd/fixture-xai"} <= ids, ids
            assert "zd/fixture-disabled" not in ids and "zd/other-catalog" not in ids and not ledger
            mode = "completion-refresh"
            organizations["17"] = "org-personal-reassigned"
            assert chat(port,"fixture-xai")[0] == 200
            assert ledger == [("completion","x_ai","fixture-xai"),("user","17"),("lease","17"),
                              ("completion","x_ai","fixture-xai")]
            mode = "success-expiry-header"
            ledger.clear()
            assert chat(port,"fixture-xai")[0] == 200 and ledger == [("completion","x_ai","fixture-xai")]
            stale_token = next(token for token in reversed(leases) if leases[token] == "17")
            simultaneous = Barrier(2)
            mode = "concurrent-refresh"
            ledger.clear()
            with ThreadPoolExecutor(max_workers=2) as pool:
                both = list(pool.map(lambda _: chat(port,"fixture-xai"), range(2)))
            assert all(response[0] == 200 for response in both), both
            assert ledger.count(("lease","17")) == ledger.count(("user","17")) == 1, ledger
            assert ledger.count(("completion","x_ai","fixture-xai")) == 4, ledger
            mode = "model-denied"
            status,_,raw = request(port,model_route)
            assert status == 200 and "[redacted]" in raw and access["17"] not in raw and not any(token in raw for token in leases)
            mode = "trial"
            status,_,raw = chat(port,"fixture-xai")
            assert status == (402 if tunneled else 503) and "[402]" in raw and "trial access is blocked" in raw and access["17"] not in raw, (status,raw)
            assert not any(token in raw for token in leases)
            with sqlite3.connect(data / "db/data.sqlite") as db:
                db.execute("UPDATE providerConnections SET data=json_remove(data,'$.modelLock_fixture-xai') WHERE id='zed-a'")
            mode = "stream-failed"
            status,_,raw = chat(port,"fixture-xai",True)
            assert status == 200 and "[redacted]" in raw and not any(token in raw for token in leases)
            mode = "pause"
            connection = http.client.HTTPConnection("127.0.0.1",port,timeout=5)
            try:
                connection.request("POST", "/v1/chat/completions", json.dumps({
                    "model":"zd/fixture-xai","stream":True,"messages":[{"role":"user","content":"Offline message"}],
                }), gateway | {"Content-Type":"application/json"})
                reply = connection.getresponse()
                assert reply.status == 200 and "Zed fixture 世界" in reply.readline().decode()
                release.set()
                assert "data: [DONE]" in reply.read().decode()
            finally:
                release.set()
                connection.close()
            mode = "ordinary"
            ledger.clear()
            access["17"] = "rotated-" + access["17"][-16:]
            with sqlite3.connect(data / "db/data.sqlite") as db:
                db.execute("UPDATE providerConnections SET data=json_set(data,'$.accessToken',?) WHERE id='zed-a'",
                           (access["17"],))
            assert request(port,model_route)[0] == 200
            assert ledger == [("user","17"),("lease","17"),("models","17")], ledger
        if tunneled:
            tunnels, hellos = fixture["TUNNELS"], fixture["HELLOS"]
            assert all(item[1:] == ("cloud.zed.dev", 443) for item in tunnels)
            assert hellos == ["cloud.zed.dev"] * len(tunnels)
            ledger.clear()
            mode = "completion-redirect"
            before = len(tunnels)
            status,_,raw = chat(port,"fixture-xai")
            assert status == 502 and "Zed upstream redirected" in raw and len(tunnels) == before + 1
            assert ledger == [("completion","x_ai","fixture-xai")], ledger
            mode = "ordinary"
            with sqlite3.connect(data / "db/data.sqlite") as db:
                db.execute("""UPDATE providerConnections SET isActive=1,priority=2,
                    data=json_remove(data, '$.providerSpecificData.connectionProxyEnabled',
                    '$.providerSpecificData.connectionProxyUrl') WHERE id='zed-b'""")
                db.executemany("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                    VALUES (?,?,'llm',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    [(name, name, json.dumps(members)) for name, members in [
                        ("zed-proxy-combo", ["zd/fixture-xai", "zd/fixture-claude"]),
                        ("zed-proxy-nested", ["zed-proxy-combo", "zd/fixture-google"]),
                    ]])
                db.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                    VALUES ('fixture-relay',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps({"type":"cloudflare","proxyUrl":address,"strictProxy":True}),))
            ledger.clear()
            proxy.reject = True
            before = len(tunnels)
            assert chat(port,"fixture-xai")[0] == 502
            for combo in ("zed-proxy-combo", "zed-proxy-nested"):
                assert request(port, "/v1/chat/completions", {"model":combo,
                    "messages":[{"role":"user","content":"Offline message"}]}, gateway)[0] == 502
            assert len(tunnels) == before + 3 and not ledger
            proxy.reject, proxy.target = False, wrong_host
            before = len(hellos)
            assert chat(port,"fixture-xai")[0] == 502 and len(hellos) == before + 1 and not ledger
            proxy.target = vendor
            before = len(tunnels)
            for pool in ("nonexistent-proxy", "fixture-relay"):
                with sqlite3.connect(data / "db/data.sqlite") as db:
                    db.execute("""UPDATE providerConnections SET data=json_set(json_remove(data,
                        '$.providerSpecificData.connectionProxyEnabled','$.providerSpecificData.connectionProxyUrl'),
                        '$.providerSpecificData.proxyPoolId',?) WHERE id='zed-a'""", (pool,))
                assert chat(port,"fixture-xai")[0] == 503
            assert len(tunnels) == before and not ledger and not fixture["ERRORS"]
        if implementation != "reference":
            assert not fallback.read_text().strip()
        assert not failures, failures
        print(json.dumps(snapshot,sort_keys=True,ensure_ascii=False))
    finally:
        release.set()
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        server.shutdown()
        server.server_close()
