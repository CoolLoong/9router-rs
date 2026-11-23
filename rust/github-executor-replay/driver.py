import datetime
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import uuid

MODE,*ARGS = sys.argv[1:]
HERE = pathlib.Path(__file__).resolve().parent
if MODE in {"native", "strict"}:
    executable, CASE = ARGS
    command = [executable]
else:
    CASE, = ARGS
    command = ["node","--no-warnings",str(HERE/"original.mjs")]
ACCESS = "fixture-github-access"
COPILOT = "fixture-copilot-access"
OBSERVATIONS = []
HEADERS = ("content-type","authorization","copilot-integration-id","editor-version","editor-plugin-version","user-agent",
           "openai-intent","x-github-api-version","x-request-id","x-vscode-user-agent-library-version","x-initiator",
           "anthropic-version","accept","x-fixture-metadata")
fixture = {
    "model":"gpt-5.4",
    "body":{"model":"gpt-5.4","messages":[{"role":"user","content":"Hello"}],
            "temperature":.5,"max_tokens":42,"max_completion_tokens":99,"reasoning_effort":"none"},
    "account":{"accessToken":ACCESS,"providerSpecificData":{"copilotToken":COPILOT}},
    "settings":{},
    "stream":False,
}
if CASE == "oauth-fallback":
    fixture["account"]["providerSpecificData"] = {"copilotToken":""}
    fixture["account"]["copilotToken"] = "fixture-unused-top-level"
if CASE == "gpt6":
    fixture["model"] = fixture["body"]["model"] = "gpt-6.1-sol"
    fixture["body"]["reasoning_effort"] = "extra-high"
if CASE == "suffix":
    fixture["model"] = fixture["body"]["model"] = "O3-mini-plus"
    fixture["body"]["reasoning_effort"] = "medium"
if CASE == "multimodal":
    fixture["body"]["messages"] = [
        {"role":"user","content":[
            {"type":"text","text":""},
            {"type":"text","text":"Image"},
            {"type":"image_url","image_url":{"url":"https://example.invalid/image.png","detail":"low"}},
            {"type":"tool_result","content":{"20":"two","1":"one","value":3.0}},
            {"type":"tool_use","text":"tool text","content":"discarded"},
            {"type":"thinking","content":["draft",{"visible":False}]},
            {"type":"custom","content":False,"id":"part"},
        ]},
        {"role":"assistant","content":None,"tool_calls":[{"id":"call","type":"function","function":{"name":"demo","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call","content":[{"type":"text","text":""}]},
    ]
if CASE == "stream":
    fixture["body"]["stream"] = fixture["stream"] = True
if CASE == "settings":
    fixture["settings"] = {"providerOverrides":{"github":{"headers":{
        "editor-version":"vscode/fixture","x-initiator":"extension","Accept":"application/x-ndjson","x-fixture-metadata":"fixture-value",
    }}}}
if CASE == "blocked":
    fixture["settings"] = {"providerOverrides":{"github":{"headers":{
        "authorization":"Bearer fixture-override","HOST":"mock.example.invalid","content-length":"150",
        "cookie":"invalid","x-fixture-metadata":"fixture-value",
    }}}}
if CASE.startswith("response-"):
    fixture["action"] = "classification"
    fixture["error"] = "The requested model is not supported"
    if CASE == "response-phrase":
        fixture["error"] = "The model is not accessible via the /chat/completions endpoint"
    if CASE == "response-gemini":
        fixture["model"] = "gemini-3.8-flash"
    if CASE == "response-claude":
        fixture["model"] = "Claude-Fable-5"
    if CASE == "response-codex":
        fixture["model"] = "gpt-5.3-codex"
    if CASE == "response-status":
        fixture["status"] = 404
    if CASE == "response-case-sensitive":
        fixture["error"] = "the requested model is not supported"
if CASE.startswith("monthly"):
    fixture["action"] = "monthly"
    fixture["status"] = 402 if CASE != "monthly-status" else 429
    fixture["error"] = "You've reached your additional usage limit for your plan, please try again" if CASE!="monthly-other" else "Payment required"
    fixture["now"] = int(datetime.datetime(2026,12 if CASE=="monthly-december" else 4,27,18,tzinfo=datetime.timezone.utc).timestamp()*1000)

if CASE.startswith("alternate-"):
    fixture["action"] = "alternate"
    if CASE == "alternate-cache":
        fixture["turns"] = 2
    if CASE in {"alternate-tools","alternate-stream","alternate-cache","alternate-terminal-unterminated"}:
        fixture["stream"] = fixture["body"]["stream"] = True
    if CASE == "alternate-gemini":
        fixture["model"] = fixture["body"]["model"] = "gemini-3.8-flash"
    if CASE in {"alternate-text","alternate-stream"}:
        fixture["body"]["messages"] = [
            {"role":"developer","content":[{"type":"text","text":"Instruction"},{"type":"text","text":"Second line"}]},
            {"role":"system","content":"Ignored"},
            {"role":"user","content":[{"type":"text","text":""},{"type":"image_url","image_url":{"url":"https://example.invalid/photo","detail":"low"}}]},
            {"role":"assistant","reasoning_content":"Earlier thought","encrypted_content":"fixture-encrypted-reasoning","content":"Earlier answer"},
        ]
        fixture["body"]["reasoning_effort"] = "medium"
    if CASE == "alternate-tools":
        fixture["body"]["tools"] = [{"type":"function","function":{"name":" demo ", "strict":True,"parameters":{"type":"object"}}}]
        fixture["body"]["messages"] = [
            {"role":"assistant","content":None,"tool_calls":[{"id":"prior-call","type":"function","function":{"name":"demo","arguments":{"mode":"safe"}}}]},
            {"role":"tool","tool_call_id":"prior-call","content":[{"type":"text","text":"tool reply"}]},
        ]
    if CASE == "alternate-input":
        fixture["body"] = {"model":"untranslated","input":"Native response input","max_tokens":33,"store":True,
            "reasoning_effort":"none","stream":False,"metadata":{"test":"copilot"}}
    if CASE == "alternate-settings":
        fixture["settings"] = {"providerOverrides":{"github":{"headers":{"user-agent":"fixture-agent","x-fixture-metadata":"value"}}}}

if CASE.startswith("claude-"):
    fixture["action"] = "claude"
    fixture["model"] = fixture["body"]["model"] = "claude-sonnet-4.6"
    fixture["body"]["messages"] = [
        {"role":"system","content":"Configure shim"},
        {"role":"user","content":"A short question"},
    ]
    fixture["body"].pop("reasoning_effort")
    fixture["body"].pop("max_completion_tokens")
    if CASE in {"claude-stream","claude-tool"}:
        fixture["body"]["stream"] = fixture["stream"] = True
    if CASE == "claude-thinking":
        fixture["body"]["reasoning_effort"] = "high"
    if CASE == "claude-tool":
        fixture["body"]["tools"] = [
            {"type":"function","function":{"name":"fixture_lookup","description":"Read","parameters":{"type":"object"}}},
            {"type":"web_search_20250305","name":"web_search"},
        ]
        fixture["body"]["tool_choice"] = {"type":"function","function":{"name":"fixture_lookup"}}
        fixture["body"]["messages"].extend([
            {"role":"assistant","content":None,"tool_calls":[
                {"id":"prior-call","type":"function","function":{"name":"fixture_lookup","arguments":"{}"}},
            ]},
            {"role":"tool","tool_call_id":"prior-call","content":"lookup reply"},
        ])
    if CASE == "claude-settings":
        fixture["settings"] = {"providerOverrides":{"github":{"headers":{"user-agent":"fixture-override-agent"}}}}
    if CASE == "claude-media":
        fixture["action"] = "claude-eligibility"
        fixture["body"]["messages"].append({"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/photo.png"}}]})

def claude_body():
    def event(name, **data):
        return ("event: "+name+"\ndata: "+json.dumps({"type":name, **data},separators=(",",":"))+"\n\n").encode()
    if CASE == "claude-http-error":
        return 424, "application/json", b'{"error":{"message":"Fixture Claude error"}}'
    body = event("message_start",message={"id":"msg_fixture","model":fixture["model"],
        "usage":{"input_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":1}})
    if CASE == "claude-refusal":
        body += event("message_delta", delta={"stop_reason":"refusal","stop_details":{"explanation":"Fixture declined the turn"}}, usage={"output_tokens":3})
    elif CASE == "claude-tool":
        body += event("content_block_start",index=0,content_block={"type":"thinking","thinking":""})
        body += event("content_block_delta",index=0,delta={"type":"thinking_delta","thinking":"Fixture thinking"})
        body += event("content_block_stop",index=0)
        body += event("content_block_start",index=1,content_block={"type":"tool_use","id":"tool_fixture","name":"fixture_lookup","input":{}})
        body += event("content_block_delta",index=1,delta={"type":"input_json_delta","partial_json":'{"key":'})
        body += event("content_block_delta",index=1,delta={"type":"input_json_delta","partial_json":'"one"}'})
        body += event("content_block_stop",index=1)
        body += event("message_delta",delta={"stop_reason":"tool_use"},usage={"output_tokens":4})
    else:
        body += event("content_block_start",index=0,content_block={"type":"text","text":""})
        body += event("content_block_delta",index=0,delta={"type":"text_delta","text":"Fixture Claude text"})
        body += event("content_block_stop",index=0)
        if CASE != "claude-no-delta":
            body += event("message_delta",delta={"stop_reason":"end_turn"},usage={"output_tokens":3})
    body += event("message_stop") + b"data: [DONE]\n\n"
    return 200, "text/event-stream", body

def alternate_body():
    def event(name, **data):
        return ("event: "+name+"\ndata: "+json.dumps({"type":name, **data},separators=(",",":"))+"\n\n").encode()
    if CASE == "alternate-upstream-error":
        return 422, "application/json", b'{"error":{"message":"Alternate provider failure"}}'
    if CASE == "alternate-error-event":
        body = event("error", error={"message":"Fixture provider rejected alternate"})
        body += event("response.failed",response={"error":{"message":"Duplicate"}})
    elif CASE == "alternate-tools":
        body = event("response.output_item.added", item={"id":"fc_a","type":"function_call","call_id":"call-a","name":"lookup"})
        body += event("response.output_item.added", item={"id":"fc_b","type":"custom_tool_call","call_id":"call-b","name":"custom"})
        body += event("response.function_call_arguments.delta",item_id="fc_a",delta='{"name":')
        body += event("response.custom_tool_call_input.delta",item_id="fc_b",delta="custom argument")
        body += event("response.function_call_arguments.delta",item_id="fc_a",delta='"fixture"}')
        body += event("response.output_item.done", item={"id":"fc_a","type":"function_call","arguments":'{"name":"fixture"}'})
        body += event("response.output_item.added", item={"id":"fc_c","type":"function_call","call_id":"call-c","name":"finish"})
        body += event("response.output_item.done", item={"id":"fc_c","type":"function_call","arguments":'{"last":true}'})
        body += event("response.done",response={"usage":{"input_tokens":9,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}})
    else:
        body = event("response.created")
        body += event("response.reasoning_summary_text.delta",delta="Fixture reasoning")
        body += event("response.output_text.delta",delta="Hello")
        body += event("response.output_text.done",text="Should not duplicate content")
        body += event("response.output_text.delta",delta=" world")
        body += event("response.completed",response={"usage":{"input_tokens":7,"output_tokens":3,"input_tokens_details":{"cached_tokens":2}}})
    return 200, "text/event-stream", body + (b"data: [DONE]" if CASE=="alternate-terminal-unterminated" else b"data: [DONE]\n\n")

def normalize(result):
    for response in result.get("responses", []):
        raw = response["body"]
        if not raw.startswith("data:"):
            continue
        output = []
        identifier = None
        created = None
        for line in raw.splitlines():
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if payload == "[DONE]":
                output.append(payload)
                continue
            data = json.loads(payload)
            identifier = identifier or data["id"]
            created = created or data["created"]
            assert data["id"] == identifier and data["created"] == created
            if response["url"].endswith("/v1/messages"):
                assert identifier == "chatcmpl-msg_fixture"
            else:
                assert identifier.startswith("chatcmpl-") and identifier.removeprefix("chatcmpl-").isdigit()
            data["id"] = "chatcmpl-fixture"
            data["created"] = 1
            output.append(data)
        response["body"] = output
    return result

class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self,*_args):
        pass

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers.get("Content-Length","0"))))
        seen = {}
        for name in HEADERS:
            values = self.headers.get_all(name) or []
            if values:
                seen[name] = ", ".join(values)
        bearer = seen.pop("authorization")
        assert bearer in {"Bearer " + ACCESS,"Bearer " + COPILOT}
        request_id = seen.pop("x-request-id")
        assert uuid.UUID(request_id).version == 4
        assert ("cookie" not in self.headers) and self.headers["Host"].startswith("127.0.0.1:")
        OBSERVATIONS.append({"headers":seen,"hasCopilot":bearer=="Bearer "+COPILOT,"requestId":True,"path":self.path,"body":data})
        if fixture.get("action") == "alternate" and self.path == "/chat/completions":
            status, media, body = 400, "application/json", b'{"error":{"message":"The requested model is not supported"}}'
        elif fixture.get("action") == "alternate" and self.path == "/responses":
            status, media, body = alternate_body()
        elif fixture.get("action") == "claude" and self.path == "/v1/messages":
            status, media, body = claude_body()
        else:
            status = 200
            media = "text/event-stream" if fixture["stream"] else "application/json"
            body = b'data: {"choices":[{"delta":{"content":"Hello"}}]}\n\ndata: [DONE]\n\n' if fixture["stream"] else b'{"choices":[{"message":{"content":"Hello"}}]}'
        self.send_response(status)
        self.send_header("Content-Type",media)
        self.send_header("Content-Length",str(len(body)))
        self.end_headers()
        self.wfile.write(body)

with tempfile.TemporaryDirectory(prefix="9router-github-executor-fixture-") as home:
    fixture_server = http.server.ThreadingHTTPServer(("127.0.0.1",0),Mock)
    thread = threading.Thread(target=lambda:fixture_server.serve_forever(poll_interval=.02),daemon=True)
    thread.start()
    fixture["endpoint"] = f"http://127.0.0.1:{fixture_server.server_port}/chat/completions"
    fixture["responsesEndpoint"] = f"http://127.0.0.1:{fixture_server.server_port}/responses"
    fixture["messagesEndpoint"] = f"http://127.0.0.1:{fixture_server.server_port}/v1/messages"
    profile = dict(os.environ, HOME=home, DATA_DIR=home, NO_PROXY="localhost,127.0.0.1")
    try:
        result = subprocess.run(command, input=json.dumps(fixture), cwd=home, env=profile, timeout=7, text=True, capture_output=True)
    finally:
        fixture_server.shutdown()
        fixture_server.server_close()
    assert result.returncode == 0, "fixture execution failed"
    value = json.loads(result.stdout.strip().splitlines()[-1])
    if CASE == "claude-media":
        assert value == {"supported":False} and not OBSERVATIONS
    if CASE == "blocked":
        assert OBSERVATIONS[0]["hasCopilot"]
        assert OBSERVATIONS[0]["headers"]["content-type"] == "application/json"
    print(json.dumps({"wire":OBSERVATIONS,"result":normalize(value)},sort_keys=True))
