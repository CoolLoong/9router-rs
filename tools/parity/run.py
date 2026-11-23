import http.cookiejar
import http.server
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PROFILE = json.loads((ROOT / "profile.json").read_text())
GATEWAY = "http://127.0.0.1:20128"
MOCK = "http://127.0.0.1:52989"
KEY = "fixture-gateway-key"
PASSWORD = "fixture-dashboard-password"
STAMP = "2026-01-01T00:00:00.000Z"
VOLATILE = {
    "timestamp", "created", "created_at", "createdAt", "updatedAt", "lastUsedAt",
    "latency", "latencyMs", "durationMs", "uptime", "prompt_cache_key", "system_fingerprint", "lastUsed",
}
SECRETS = {"key", "apiKey", "accessToken", "refreshToken", "idToken", "passwordHash", "machineId"}
OAUTH_RANDOM = {"state", "codeVerifier", "codeChallenge", "code_challenge", "nonce"}
UUID = re.compile(r"^[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$", re.I)


def normalize(value, key=None):
    if isinstance(value, dict):
        return {k: normalize(v, k) for k, v in value.items() if k not in VOLATILE}
    if isinstance(value, list):
        return [normalize(item, key) for item in value]
    if isinstance(value, str) and value and key in SECRETS | OAUTH_RANDOM:
        return "<fixture-secret>"
    if isinstance(value, str) and key == "authUrl":
        url = urllib.parse.urlsplit(value)
        query = urllib.parse.parse_qsl(url.query, keep_blank_values=True)
        return urllib.parse.urlunsplit(url._replace(
            query=urllib.parse.urlencode(sorted((k, normalize(v, k)) for k, v in query))
        ))
    if isinstance(value, str) and (
        key in {"request_id", "response_id", "item_id", "id"}
        and (UUID.fullmatch(value) or value.startswith(("chatcmpl-", "msg_", "resp_"))
             or (len(value) == 13 and value.isdecimal()))
    ):
        return "<generated-id>"
    if isinstance(value, str) and key == "id" and value.startswith("openai-compatible-"):
        kind, _, suffix = value.partition("compatible-")
        mode, _, uid = suffix.partition("-")
        if UUID.fullmatch(uid):
            return f"{kind}compatible-{mode}-<generated-id>"
    return value


def snapshot(response, capture):
    raw = response.read()
    code = response.status
    media = response.headers.get("Content-Type", "").split(";")[0]
    out = {"status": code}
    if media:
        out["contentType"] = media
    uid = None
    if redirect := response.headers.get("Location"):
        out["location"] = redirect.removeprefix(GATEWAY)
    if cookies := response.headers.get_all("Set-Cookie"):
        out["cookies"] = sorted(cookie.split("=", 1)[0] for cookie in cookies)
    if media == "application/json" and raw:
        value = json.loads(raw)
        if capture and isinstance(value,dict):
            uid = value.get("id") or next(
                (item["id"] for item in value.values() if isinstance(item,dict) and "id" in item), None
            )
        out["body"] = normalize(value)
    elif media == "text/event-stream":
        out["events"] = [
            data if data == "[DONE]" else normalize(json.loads(data))
            for line in raw.decode().splitlines()
            if line.startswith("data:") and (data := line[5:].strip())
        ]
    elif media == "text/html":
        out["html"] = bool(re.search(rb"<html[\s>]", raw, re.IGNORECASE))
    elif raw:
        out["body"] = raw.decode(errors="replace")
    return out, uid


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class Mock(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        path = self.headers.get("x-relay-path", self.path).removeprefix("/codex")
        wire = ("gemini" if "generatecontent" in path.lower() else
                "responses" if path.rstrip("/").endswith("/responses") else "chat")
        self.server.calls.append({"path": path.split("?", 1)[0], "request": normalize(body)})
        if "parity-mock-reject" in json.dumps(body):
            self.emit(400, {"error": {"message": "fixture upstream rejected request", "type": "invalid_request_error"}})
            return
        if wire == "responses":
            self.codex(body)
        elif wire == "gemini":
            self.gemini(path)
        else:
            self.chat(body)

    def do_GET(self):
        path = self.headers.get("x-relay-path", self.path)
        self.server.calls.append({"path": path.split("?", 1)[0], "request":None})
        if "models" in path:
            self.emit(200, {"object":"list","data":[
                {"id": item.split("/", 1)[-1], "object": "model", "owned_by": "fixture"}
                for item in PROFILE["models"] if item.startswith("dp/")
            ]})
        else:
            self.emit(200, {})

    def emit(self, code, value, sse=False, done=False):
        if sse:
            data = "".join(
                (f"event: {item['type']}\n" if "type" in item else "")
                + f"data: {json.dumps(item)}\n\n" for item in value
            )
            if done:
                data += "data: [DONE]\n\n"
        else:
            data = json.dumps(value)
        payload = data.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/event-stream" if sse else "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def codex(self, body):
        item = {"id":"msg_fixture","type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"parity-pong","annotations":[]}]}
        response = {"id":"resp_fixture","object":"response","created_at":1,"model":body.get("model"),
                    "status":"completed","output":[item],
                    "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}
        events = [
            {"type":"response.created","response":{**response,"status":"in_progress","output":[]}},
            {"type":"response.output_item.added","output_index":0,"item":{**item,"status":"in_progress","content":[]}},
            {"type":"response.content_part.added","output_index":0,"item_id":item["id"],"content_index":0,
             "part":{"type":"output_text","text":"","annotations":[]}},
            {"type":"response.output_text.delta","output_index":0,"item_id":item["id"],"content_index":0,"delta":"parity-pong"},
            {"type":"response.output_text.done","output_index":0,"item_id":item["id"],"content_index":0,"text":"parity-pong"},
            {"type":"response.output_item.done","output_index":0,"item":item},
            {"type":"response.completed","response":response},
        ]
        if "parity-tool-call" in json.dumps(body):
            item = {"id":"fc_fixture","type":"function_call","call_id":"call_fixture",
                    "name":"lookup","arguments":'{"query":"probe"}',"status":"completed"}
            response["output"] = [item]
            added = {"type":"response.output_item.added","output_index":0,
                     "item":{**item,"arguments":"","status":"in_progress"}}
            delta = {"type":"response.function_call_arguments.delta","output_index":0,"item_id":item["id"],
                     "delta":item["arguments"]}
            done = {"type":"response.function_call_arguments.done","output_index":0,"item_id":item["id"],
                    "arguments":item["arguments"]}
            events = [events[0],added,delta,done,{**events[-2],"item":item},events[-1]]
        self.emit(200, events, sse=True)

    def chat(self, body):
        base = {"id":"chatcmpl-fixture", "created":1,"model":body.get("model","fixture")}
        usage = {"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
        if body.get("stream"):
            base["object"] = "chat.completion.chunk"
            chunks = [
                {**base, "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":None}]},
                {**base, "choices":[{"index":0,"delta":{"content":"parity-pong"},"finish_reason":None}]},
                {**base, "choices":[{"index":0,"delta":{},"finish_reason":"stop"}], "usage":usage},
            ]
            self.emit(200, chunks, sse=True, done=True)
        else:
            self.emit(200, {**base,"object":"chat.completion","usage":usage,"choices":[
                {"index":0,"message":{"role":"assistant","content":"parity-pong"},"finish_reason":"stop"}]})

    def gemini(self, path):
        data = {"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"parity-pong"}]},
                               "finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1,"totalTokenCount":3}}
        if "streamGenerateContent" in path:
            self.emit(200, [data], sse=True)
        else:
            self.emit(200, data)

    def log_message(self, *args):
        return


def seed(data, scenario):
    node = PROFILE["node"] | {"name":"Fixture compatible node","baseUrl":MOCK + "/v1"}
    settings = {
        "requireApiKey": True, "requireLogin": scenario.startswith("auth-"),
        "enableRequestLogs": False, "rtkEnabled": PROFILE["rtkEnabled"],
        "providerThinking": PROFILE["providerThinking"], "codexAutoPing": {"enabled":False},
    }
    if scenario == "advertised-models-oc":
        settings["providerStrategies"] = {
            "opencode": {"rotateStrategy":"round-robin","proxyPoolId":"fixture-relay"},
        }
    accounts = []
    for number, account in enumerate(PROFILE["accounts"]):
        connection = account | {"id":f"fixture-connection-{number}","priority":1,"name":account["provider"],
                                "createdAt":STAMP,"updatedAt":STAMP}
        connection["apiKey" if account["authType"] == "apikey" else "accessToken"] = "fixture-provider-key"
        if account["authType"] == "oauth":
            connection["expiresAt"] = "2099-01-01T00:00:00.000Z"
        if account["provider"] in ("codex", "gemini"):
            connection["providerSpecificData"] = {"proxyPoolId":"fixture-relay"}
        if account["provider"] == node["id"]:
            connection["providerSpecificData"] = {key:node[key] for key in ("prefix","apiType","baseUrl")}
        accounts.append(connection)
    fixture = {
        "settings":settings,"providerConnections":accounts,"providerNodes":[node],
        "proxyPools":[{"id":"fixture-relay","name":"Fixture relay","type":"vercel","isActive":True,"proxyUrl":MOCK + "/relay"}],
        "apiKeys":[{"id":"fixture-key","name":"Fixture","key":KEY,"isActive":True,"createdAt":STAMP}],
        "combos":[combo | {"id":f"fixture-combo-{index}"} for index,combo in enumerate(PROFILE["combos"])],
        "customModels":[{"providerAlias":m["provider"],"id":m["id"],"name":m["name"],"type":"llm"}
                        for m in PROFILE["customModels"]],
    }
    (data / "db.json").write_text(json.dumps(fixture))
    (data / "disabledModels.json").write_text(json.dumps(PROFILE["disabledModels"]))


def gateway_command(engine):
    if engine == "node":
        path = Path(os.environ["PARITY_NODE_SERVER"]).resolve()
        return ["node", str(path)], path.parent
    path = Path(os.environ["PARITY_RUST_BIN"]).resolve()
    return [str(path), "--host", "127.0.0.1", "--port", "20128", "--no-browser", "--skip-update"], None


def cli_show(engine, home):
    (home / ".claude").mkdir()
    (home / ".claude/settings.json").write_text(json.dumps({
        "model":"opus","env":{"ANTHROPIC_BASE_URL":GATEWAY+"/v1","ANTHROPIC_AUTH_TOKEN":KEY,
                              "ANTHROPIC_DEFAULT_OPUS_MODEL":"cx/gpt-6.2-sol"}
    }))
    (home / ".codex").mkdir()
    (home / ".codex/config.toml").write_text(
        f'model = "gpt-6.2-sol"\nmodel_provider = "9router"\n'
        f'[model_providers.9router]\nname = "9Router"\nbase_url = "{GATEWAY}/v1"\n'
        'wire_api = "responses"\n'
    )
    cmd, _ = gateway_command(engine)
    launcher = ["node", str(Path(cmd[1]).parent.parent / "cli.js")] if engine == "node" else cmd[:1]
    result = subprocess.run(
        [*launcher,"show","claude","codex","--json"],
        env={"PATH":os.environ["PATH"],"HOME":str(home),"DATA_DIR":str(home),"NO_COLOR":"1"},
        capture_output=True,text=True,timeout=8,
    )
    output = result.stdout.replace(KEY,"<fixture-secret>").replace(str(home),"<fixture-home>")
    if output.strip():
        output = json.loads(output)
    print(json.dumps({"exit":result.returncode,"stdout":output,
                      "stderr":result.stderr.replace(KEY,"<fixture-secret>")},sort_keys=True))


def body_for(endpoint, model, stream, variant):
    messages = [{"role":"user","content":"Reply parity-pong."}]
    if variant == "reject":
        messages[0]["content"] = "parity-mock-reject"
    if variant == "tool-call":
        messages[0]["content"] = "parity-tool-call"
    body = {"model":model,"stream":stream}
    if endpoint.endswith("/responses"):
        body["input"] = messages
        body["reasoning"] = {"effort":"high"}
    elif endpoint.endswith("/messages"):
        body.update(messages=messages,max_tokens=32000)
        if variant in {"adaptive","disabled","budget","claude"}:
            body["thinking"] = (
                {"type":"disabled"} if variant == "disabled" else
                {"type":"enabled","budget_tokens":15000} if variant == "budget" else
                {"type":"adaptive","display":"omitted"}
            )
        if variant in {"adaptive","claude"}:
            body["output_config"] = {"effort":"high"}
    else:
        body["messages"] = messages
    if variant in {"tool","tool-call"}:
        body["tools"] = [{"name":"lookup","description":"Look up a value.","input_schema":{
            "type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}]
    if variant == "tool":
        messages += [
            {"role":"assistant","content":[{"type":"tool_use","id":"toolu_fixture","name":"lookup","input":{"query":"probe"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fixture","content":"fixture-result"}]},
        ]
    if variant == "claude":
        body["system"] = [{"type":"text","text":"You are a local fixture client."}]
        body["tools"] = [{"name":"lookup","description":"Look up a value.","input_schema":{"type":"object"}}]
    return body


def run_steps(opener, server, steps, data_dir, app_dir):
    ids = {}
    for index, step in enumerate(steps, 1):
        path = step["path"].format_map(ids)
        headers = {"Accept":"application/json"}
        if step.get("key", True):
            headers["Authorization"] = "Bearer " + step.get("token", KEY)
        headers.update(step.get("headers",{}))
        payload = step.get("body")
        if payload is not None:
            headers["Content-Type"] = "application/json"
            if path.split("?")[0].endswith("/messages"):
                headers.update({"anthropic-version":"2023-06-01","User-Agent":"claude-code/parity-fixture"})
        request = urllib.request.Request(
            GATEWAY + path, data=json.dumps(payload).encode() if payload is not None else None,
            method=step.get("method","GET"), headers=headers,
        )
        server.calls.clear()
        capture = step.get("capture")
        try:
            with opener.open(request, timeout=step.get("timeout",8)) as response:
                summary, uid = snapshot(response, capture)
        except urllib.error.HTTPError as response:
            summary, uid = snapshot(response, capture)
        except TimeoutError:
            summary, uid = {"timeout": True}, None
        if capture:
            ids[capture] = urllib.parse.quote(str(uid or "missing-id"),safe="")
        label = f"{index:02d} {step.get('name', step['path'])}"
        output = json.dumps({"step":label,"response":summary,"upstream":server.calls},
                            sort_keys=True,separators=(",",":"),ensure_ascii=False)
        output = output.replace(str(data_dir),"<fixture-home>")
        if app_dir is not None:
            output = output.replace(str(app_dir),"<fixture-app>")
        print(output)


def run(engine, scenario):
    from scenarios import scenarios
    steps = scenarios(PROFILE, body_for, PASSWORD)[scenario]
    with tempfile.TemporaryDirectory(prefix="9router-parity-") as directory:
        root = Path(directory)
        if scenario == "cli-show":
            cli_show(engine,root)
            return
        seed(root, scenario)
        with http.server.ThreadingHTTPServer(("127.0.0.1",52989),Mock) as server:
            server.calls = []
            thread = threading.Thread(target=server.serve_forever,daemon=True)
            thread.start()
            command, cwd = gateway_command(engine)
            env = {
                "PATH":os.environ["PATH"],"HOME":directory,"DATA_DIR":directory,
                "TMPDIR":directory,"HOSTNAME":"127.0.0.1","PORT":"20128",
                "INITIAL_PASSWORD":PASSWORD,"JWT_SECRET":"fixture-jwt-secret-for-local-parity-gateway",
                "DISABLE_BACKGROUND_TOKEN_REFRESH":"1","NEXT_TELEMETRY_DISABLED":"1",
            }
            opener = urllib.request.build_opener(NoRedirect, urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
            with tempfile.TemporaryFile() as log:
                with subprocess.Popen(command,cwd=cwd,env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log) as process:
                    try:
                        for _ in range(100):
                            if process.poll() is not None:
                                raise RuntimeError(f"{engine} exited during startup")
                            try:
                                with opener.open(GATEWAY+"/api/health",timeout=1):
                                    break
                            except OSError:
                                time.sleep(0.05)
                        else:
                            raise RuntimeError(f"{engine} did not become healthy")
                        run_steps(opener,server,steps,root,cwd)
                    finally:
                        process.terminate()
                        process.wait(timeout=5)
                        server.shutdown()


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "cases":
        from scenarios import scenarios
        target = Path(sys.argv[2])
        target.write_text("".join(json.dumps({"name":name,"args":[name]})+"\n"
                                  for name in scenarios(PROFILE,body_for,PASSWORD)))
        return
    if len(sys.argv) == 4 and sys.argv[1] == "isolated":
        run(*sys.argv[2:])
        return
    if len(sys.argv) != 3 or sys.argv[1] not in ("node", "rust"):
        raise SystemExit("Usage: run.py cases OUTPUT or run.py {node,rust} CASE")
    os.execvp("unshare", ["unshare","-Urn","sh","-c",'ip link set lo up && exec "$@"',
                         "parity",sys.executable,str(Path(__file__).resolve()),"isolated",*sys.argv[1:]])


if __name__ == "__main__":
    main()
