import datetime as dt
from copy import deepcopy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zlib

MODE, *ARGS, CASE = sys.argv[1:]
ROOT = Path(__file__).resolve().parent
VERSION = json.loads((ROOT.parent.parent / "package.json").read_text())["version"]
SECRET = "ksk_fixture_chat_key"
PRIVATE_TEXT = "Buffered content that must stay private"
REQUESTS = []
SESSION_IDS = []
SESSION_CASES = {"session-continuity", "session-body-continuity", "session-claude-continuity"}


def frame(kind, payload, message_type="event"):
    fields = {":message-type": message_type, ":event-type": kind, ":content-type": "application/json"}
    headers = b""
    for name, value in fields.items():
        key = name.encode()
        val = value.encode()
        headers += bytes([len(key)]) + key + bytes([7]) + struct.pack(">H", len(val)) + val
    raw = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()
    prelude = struct.pack(">II", 16 + len(headers) + len(raw), len(headers))
    body = prelude + struct.pack(">I", zlib.crc32(prelude)) + headers + raw
    return body + struct.pack(">I", zlib.crc32(body))


def event_body():
    assistant = lambda text: frame("assistantResponseEvent", {"content": text})
    stop = lambda reason: frame("messageStopEvent", {"stopReason": reason})
    if CASE in {"refusal", "cancelled", "unknown-stop", "exception", "corrupt", "truncated", "tool-output"}:
        after = {
            "refusal": lambda: stop("guardrail_blocked"),
            "cancelled": lambda: stop("cancelled"),
            "unknown-stop": lambda: stop("new_status"),
            "exception": lambda: frame("serviceException", {"message": SECRET}, "exception"),
            "corrupt": lambda: stop("end_turn")[:-1] + bytes([stop("end_turn")[-1] ^ 0x92]),
            "truncated": lambda: stop("end_turn")[:-4],
            "tool-output": lambda: frame("toolUseEvent", {"name": "unavailable", "input": {}}),
        }
        return assistant(PRIVATE_TEXT) + after[CASE]()
    if CASE == "empty-event":
        return b""
    if CASE == "split-thinking":
        parts = ["Hello <thi", "nking>hidden detail</think", "ing>\nWorld"]
        return b"".join(map(assistant, parts)) + stop("end_turn")
    if CASE == "context":
        return assistant("A short answer") + frame("contextUsageEvent", {"contextUsagePercentage": 5}) + (
            frame("meteringEvent", {"meteringEvent": {"usage": 1.25, "unit": "credit"}}) + stop("endTurn"))
    if CASE == "usage":
        return frame("reasoningContentEvent", {"reasoningContentEvent": {"text": "reasoning"}}) + (
            assistant("A visible <thinking>hidden detail</thinking> answer") +
            frame("metricsEvent", {"metricsEvent": {"inputTokens": 16, "outputTokens": "4",
                "cacheReadInputTokens": 2, "cache_creation_input_tokens": 1}}) +
            frame("meteringEvent", {"usage": "1.25"}) +
            frame("MetadataEvent", {"metadata": {"stop_reason": "max_tokens"}}))
    if CASE == "clean-eof":
        return assistant("Completed reply with implicit end")
    if CASE in {"tool-declared", "tool-parallel", "tool-partial", "tool-incomplete", "tool-unknown"}:
        tool = lambda value: frame("toolUseEvent", value)
        common = {"name":"lookup_weather","toolUseId":"fixture_call_weather"}
        partial = tool({**common,"input":'{"city":'})
        if CASE == "tool-incomplete":
            return partial + stop("tool_use")
        if CASE == "tool-unknown":
            return tool({**common,"name":"unexpected_name","input":{}}) + stop("tool_use")
        result = partial + (b"" if CASE == "tool-partial" else tool({**common,"input":'"上海"}'}))
        if CASE in {"tool-parallel","tool-partial"}:
            result = assistant("Preparing requests") + result + tool([{
                "name":"lookup_time","toolUseId":"fixture_call_time","input":{"zone":"UTC"},
            }])
        return result + stop("toolUse") + frame("contextUsageEvent",{"contextUsagePercentage":5}) + frame(
            "meteringEvent",{"meteringEvent":{"usage":0.15,"unit":"credit"}})
    return assistant("Hello ") + assistant("世界 🙂") + stop("endTurn") + frame(
        "metricsEvent", {"inputTokens": 12, "outputTokens": 2})


def model_input():
    model = "auto"
    region = "eu-central-1" if CASE == "regional" else "us-east-1"
    messages = [{"role": "user", "content": " fixture prompt "}]
    data = {
        "account": {"provider": "kiro", "authType": "api_key", "accessToken": SECRET,
                    "providerSpecificData": {"authMethod": "api_key", "region": region}},
        "body": {"model": model, "stream": CASE != "json-request", "messages": messages},
        "model": model, "headers": {},
    }
    if CASE == "history":
        model = "claude-sonnet-4-5-20250929"
        messages[:] = [
            {"role": "system", "content": " Keep it brief. "},
            {"role": "user", "content": " first"},
            {"role": "user", "content": "second "},
            {"role": "assistant", "content": " old reply "},
            {"role": "assistant", "content": " another part "},
            {"role": "user", "content": [{"type": "text", "text": " follow"},
                                         {"type": "text", "text": "up "}]},
        ]
        data["body"].update(temperature=0.25, top_p=0.95, max_tokens=40)
    if CASE == "blocks":
        model = "MiniMax-M2.5"
        messages[0]["content"] = [{"type": "text", "text": "alpha"}, {"type": "text", "text": "beta"}]
    if CASE == "tools-input":
        data["body"]["tools"] = [{"type": "function", "function": {"name": "fixture needs renaming"}}]
    if CASE in {"tool-declared", "tool-parallel", "tool-partial", "tool-history", "tool-incomplete", "tool-unknown", "tool-unpaired-input"}:
        data["body"]["tool_choice"] = "auto"
        data["body"]["tools"] = [{"type":"function", "function":{
            "name":"lookup_weather","description":"Look up the forecast", "parameters":{
                "type":"object","additionalProperties":False,
                "properties":{"city":{"type":"string"},"metadata":{"type":"object","required":[],
                    "additionalProperties":False,"properties":{"unit":{"type":"string"}}}},
                "required":["city","unknown","city"],
            }}}, {"type":"function","function":{"name":"lookup_time"}}]
        if CASE in {"tool-history","tool-unpaired-input"}:
            messages.append({"role":"assistant","content":None,"tool_calls":[
                {"id":"fixture_historical_weather","type":"function","function":{
                    "name":"lookup_weather","arguments":'{"city":"Shanghai"}'}},
                {"id":"fixture_historical_time","type":"function","function":{
                    "name":"lookup_time","arguments":'{"zone":"UTC"}'}},
            ]})
            messages.append({"role":"tool","tool_call_id":"fixture_historical_time","status":"error","content":"Clock unavailable"})
            if CASE == "tool-history":
                messages.append({"role":"tool","tool_call_id":"fixture_historical_weather","content":"Clear skies"})
    if CASE == "explicit-session":
        data["headers"]["x-session-id"] = "fixture-client-session"
    if CASE in SESSION_CASES:
        if CASE == "session-continuity":
            data["headers"]["x-session-id"] = "fixture-session-replay"
        elif CASE == "session-body-continuity":
            data["body"]["prompt_cache_key"] = "fixture-session-replay"
        else:
            data["body"]["metadata"] = {"user_id":json.dumps({"session_id":"fixture-session-replay"})}
        data["sessionOwner"] = "offline-key-to-scope-the-session"
        data["account"]["id"] = "fixture-account"
        data["account"]["connectionId"] = "fixture-account"
    if CASE == "unsupported-region":
        data["account"]["providerSpecificData"]["region"] = "eu-west-2"
    if CASE == "oauth-input":
        data["account"]["authType"] = "oauth"
        data["account"]["providerSpecificData"]["authMethod"] = "idc"
    if CASE.startswith("oauth-") and CASE != "oauth-input":
        data["account"]["authType"] = "oauth"
        data["account"]["accessToken"] = "offline-kiro-chat-oauth"
        data["account"]["refreshToken"] = "offline-kiro-chat-refresh"
        data["account"]["expiresAt"] = "2099-01-01T00:00:00.000Z"
        details = data["account"]["providerSpecificData"]
        if CASE == "oauth-idc-eu":
            details["region"] = "eu-central-1"
            del data["account"]["expiresAt"]
            data["account"]["tokenExpiresAt"] = 4070908800
        details["profileArn"] = f"arn:aws:codewhisperer:{details['region']}:123456789012:profile/isolated-kiro"
        details["authMethod"] = {
            "oauth-google": "google", "oauth-github": "github", "oauth-builder": "builder-id",
            "oauth-external": "external_idp", "oauth-external-alternate": "external_idp",
        }.get(CASE, "idc")
        if CASE in {"oauth-expired-input", "oauth-refresh-input"}:
            now = dt.datetime.now(dt.timezone.utc)
            expiry = (-10 if CASE == "oauth-expired-input" else 25)
            data["account"]["expiresAt"] = (now + dt.timedelta(seconds=expiry)).isoformat()
        if CASE == "oauth-cross-region-input":
            details["profileArn"] = details["profileArn"].replace("us-east-1", "eu-central-1")
        if CASE == "oauth-proxy-input":
            details["connectionProxyEnabled"] = True
        if CASE == "oauth-missing-profile-input":
            del details["profileArn"]
        if CASE == "oauth-invalid-profile-input":
            details["profileArn"] += "/unexpected"
        if CASE == "oauth-invalid-method-input":
            details["authMethod"] = "unexpected"
    if CASE == "vision-input":
        messages[0]["content"] = [{"type": "image_url", "image_url": {"url": "data:text/html;base64,PHRlc3Q+"}}]
    if CASE == "vision-inline":
        messages[0]["content"] = [
            {"type":"text","text":"Describe the image"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,ZmFrZS1pbWFnZQ=="}},
            {"type":"image_url","image_url":{"url":"https://example.invalid/reference.png"}},
        ]
    if CASE == "vision-history":
        messages[:] = [{"role":"user","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"ZmFrZS1qcGVn"}},
        ]},{"role":"assistant","content":"prior reply"},{"role":"user","content":"Compare another image"},
            {"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:image/gif;base64,ZmFrZS1naWY="}},
            ]}]
    if CASE == "thinking-input":
        model = "claude-sonnet-5(invalid)"
    if CASE == "reasoning-level":
        model = "claude-sonnet-4.6(xhigh)"
        data["body"]["reasoning_effort"] = "low"
    if CASE == "reasoning-gpt":
        model = "gpt-5.6-luna"
        data["body"]["reasoning"] = {"effort":"max"}
    if CASE == "reasoning-budget":
        model = "claude-sonnet-4.5"
        data["body"]["thinking"] = {"type":"enabled","budget_tokens":2048}
    if CASE == "reasoning-disabled":
        model = "claude-sonnet-5-thinking(off)"
        data["body"]["reasoning_effort"] = "high"
        data["headers"]["anthropic-beta"] = "interleaved-thinking-2025-05-14"
    if CASE == "reasoning-agentic":
        model = "claude-sonnet-5-thinking-agentic"
        messages[:] = [{"role":"user","content":"first"}, {"role":"assistant","content":"prior"},
                       {"role":"user","content":"current"}]
    data["model"] = data["body"]["model"] = model
    return data


def fixture_input():
    first = model_input()
    if CASE not in SESSION_CASES:
        return first
    next_ = deepcopy(first)
    last = deepcopy(first)
    next_["body"]["messages"] = [{"role":"user","content":"next turn"}]
    last["body"]["messages"] = [{"role":"user","content":"changed client copy of the first turn"},
        {"role":"assistant","content":"past answer"},{"role":"user","content":"last turn"}]
    return {"requests":[first,next_,last]}


def clean_request(body):
    state = body["conversationState"]
    identifier = state.pop("conversationId")
    if CASE in SESSION_CASES:
        if MODE == "reference":
            assert identifier == ("claude:fixture-session-replay" if CASE == "session-claude-continuity" else "fixture-session-replay")
        else:
            uuid.UUID(identifier)
            assert identifier != "fixture-session-replay" and "_nativeKiroSession" not in body
        SESSION_IDS.append(identifier)
        assert len(set(SESSION_IDS)) == 1
        state["conversationId"] = "<scoped stable session>"
    else:
        uuid.UUID(identifier[:36])
        assert identifier[36:].isdigit()
        state["conversationId"] = "<unique>"
    turns = state["history"] + [state["currentMessage"]]
    for turn in turns:
        if "userInputMessage" not in turn:
            continue
        item = turn["userInputMessage"]
        match = re.search(r"\[Context: Current time is ([^\]]+)\]", item["content"])
        if match:
            assert dt.datetime.fromisoformat(match[1]).utcoffset() == dt.timedelta()
            item["content"] = item["content"][:match.start()] + "[Context: Current time is <utc>]" + item["content"][match.end():]
    return body


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        account = model_input()["account"]
        details = account["providerSpecificData"]
        region = details["region"]
        profile = details.get("profileArn")
        secondary = self.path.endswith("/codewhisperer/generateAssistantResponse")
        token_type = "API_KEY" if account["authType"] == "api_key" else (
            "EXTERNAL_IDP" if details["authMethod"] == "external_idp" else None)
        valid = (
            self.path == (f"/{region}/codewhisperer/generateAssistantResponse" if secondary
                          else f"/{region}/generateAssistantResponse")
            and self.headers.get("Accept") == "application/vnd.amazon.eventstream"
            and self.headers.get("Authorization") == "Bearer " + account["accessToken"]
            and self.headers.get("TokenType") == token_type
            and self.headers.get("User-Agent") == f"9Router/{VERSION}"
            and self.headers.get("Content-Type") == "application/json"
            and all(not self.headers.get(field) for field in (
                "x-amz-user-agent", "x-amzn-kiro-agent-mode", "x-amzn-codewhisperer-machine-id",
            ))
            and self.headers.get("x-amzn-codewhisperer-profile-arn") == profile
            and self.headers.get("x-amz-sso-bearer") == (account["accessToken"] if profile else None)
            and self.headers.get("X-Amz-Target") == (
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse" if secondary else None)
            and body.get("profileArn") == profile
        )
        REQUESTS.append({"path": self.path, "valid": valid, "body": clean_request(body)})
        if CASE in {"unauthorized", "redirect", "oauth-rejected", "oauth-bad-payload"} or (
                CASE == "oauth-external-alternate" and not secondary):
            status = {"redirect":302,"oauth-bad-payload":400,"oauth-external-alternate":403}.get(CASE,401)
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            if CASE == "redirect":
                self.send_header("Location", "http://127.0.0.1:1/unsafe-redirection")
            self.end_headers()
            self.wfile.write(json.dumps({"error": {"message": "Denied " + account["accessToken"]}}).encode())
            return
        raw = event_body()
        self.send_response(200)
        self.send_header("Content-Type", "application/json" if CASE == "wrong-content-type" else "application/vnd.amazon.eventstream")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        for first, last in [(0, 4), (4, 25), (25, 83), (83, len(raw))]:
            try:
                self.wfile.write(raw[first:last])
                self.wfile.flush()
            except BrokenPipeError:
                break
            time.sleep(0.003)


def transcript(wire):
    chunks = []
    identifier = None
    for line in wire.splitlines():
        if not line.startswith("data: "):
            continue
        event = line.removeprefix("data: ")
        if event == "[DONE]":
            chunks.append("[DONE]")
            continue
        parsed = json.loads(event)
        if "id" in parsed:
            id = parsed["id"]
            assert id.startswith("chatcmpl-") and id.removeprefix("chatcmpl-").isdigit()
            identifier = identifier or id
            assert id == identifier
            parsed["id"] = "<generated>"
            assert parsed["created"] > 0
            parsed["created"] = "<now>"
        chunks.append(parsed)
    return chunks


eligible = not CASE.endswith("-input") and CASE not in {"unsupported-region", "explicit-session"}
with tempfile.TemporaryDirectory(prefix="9router-kiro-chat-") as directory, ThreadingHTTPServer(("127.0.0.1", 0), Upstream) as aws:
    worker = threading.Thread(target=aws.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    worker.start()
    base = f"http://127.0.0.1:{aws.server_port}"
    command = ["node", "--no-warnings", str(ROOT / "original.mjs")] if MODE == "reference" else [ARGS[0]]
    env = {"PATH": os.environ["PATH"], "HOME": directory, "DATA_DIR": directory, "NO_PROXY": "127.0.0.1"}
    result = subprocess.run([*command, base], input=json.dumps(fixture_input()), env=env, check=True,
                            text=True, capture_output=True, timeout=8)
    response = json.loads(result.stdout)
    aws.shutdown()
    worker.join()
if CASE in SESSION_CASES:
    assert len(REQUESTS) == len(response["responses"]) == 3 and all(req["valid"] for req in REQUESTS)
    frames = []
    for reply in response["responses"]:
        assert reply["native"] and reply["status"] == 200 and SECRET not in reply["wire"]
        frames.append(transcript(reply["wire"]))
    starts = [req["body"]["conversationState"] for req in REQUESTS]
    assert starts[0]["history"] == [] and all("fixture prompt" in turn["history"][0]["userInputMessage"]["content"] for turn in starts[1:])
    assert all(items[-1] == "[DONE]" for items in frames)
    print(json.dumps({"upstream":REQUESTS,"frames":frames},sort_keys=True,separators=(",", ":")))
    sys.exit(0)
assert response["native"] == eligible
expected_attempts = 2 if CASE in {"oauth-external-alternate", "oauth-rejected"} else int(eligible)
assert len(REQUESTS) == expected_attempts and all(req["valid"] for req in REQUESTS)
if eligible:
    wire = response.pop("wire")
    assert all(not token or token not in wire for token in [SECRET, model_input()["account"]["accessToken"]])
    if CASE in {"unauthorized", "redirect", "oauth-rejected", "oauth-bad-payload"}:
        assert response["status"] == {"redirect":302,"oauth-bad-payload":400}.get(CASE,401)
        assert "[redacted]" in wire
        response["error"] = json.loads(wire)
    else:
        assert response["status"] == 200
        response["frames"] = transcript(wire)
        assert response["frames"][-1] == "[DONE]"
        failed = CASE in {"refusal", "cancelled", "unknown-stop", "exception", "corrupt",
                          "truncated", "tool-output", "wrong-content-type", "empty-event", "tool-incomplete", "tool-unknown"}
        if failed:
            assert PRIVATE_TEXT not in wire and len(response["frames"]) == 2
            assert response["frames"][0]["error"]["status"] == 502
        else:
            assert all("error" not in record for record in response["frames"][:-1])
            finish = response["frames"][-2]["choices"][0]["finish_reason"]
            assert finish == ("length" if CASE == "usage" else "tool_calls" if CASE in {"tool-declared", "tool-parallel", "tool-partial"} else "stop")
        if CASE == "split-thinking":
            assert "hidden detail" not in wire
            text = "".join(item["choices"][0]["delta"].get("content", "") for item in response["frames"][:-1])
            assert text == "Hello World"
print(json.dumps({"upstream": REQUESTS, "response": response}, sort_keys=True, separators=(",", ":")))
