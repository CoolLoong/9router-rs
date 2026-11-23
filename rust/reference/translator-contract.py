import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
fixture_key = "offline-translator-selected-key"


class SendFixture(BaseHTTPRequestHandler):
    retries = 0

    def log_message(self, *args):
        pass

    def do_POST(self):
        assert self.path in ["/v1/chat/completions", "/v1/messages"]
        assert self.headers.get("authorization") == "Bearer " + fixture_key
        assert "x-forward-probe" not in self.headers
        data = json.loads(self.rfile.read(int(self.headers["content-length"])))
        prompt = data["messages"][0]["content"]
        if self.path == "/v1/messages":
            assert self.headers["x-api-key"] == fixture_key
            assert self.headers["anthropic-version"] == "2023-06-01"
            assert "interleaved-thinking-2025-05-14" in self.headers["anthropic-beta"]
            assert "claude-code-20250219" not in self.headers["anthropic-beta"]
        if prompt == "reject":
            self.respond(403, f"Failed with configured key {fixture_key}".encode())
        elif prompt == "retry" and not SendFixture.retries:
            SendFixture.retries += 1
            self.respond(502, b"Transient synthetic error")
        else:
            if prompt == "retry":
                SendFixture.retries += 1
            raw = f'data: {{"echo":"{fixture_key}"}}\r\n\r\ndata: [DONE]\n\n'.encode()
            midpoint = raw.index(fixture_key.encode()) + 7
            self.respond(200, raw, midpoint)

    def respond(self, status, wire, midpoint=None):
        self.send_response(status)
        self.send_header("content-type", "text/event-stream" if status == 200 else "text/plain")
        self.send_header("content-length", str(len(wire)))
        self.end_headers()
        if midpoint is not None:
            self.wfile.write(wire[:midpoint])
            self.wfile.flush()
            time.sleep(.04)
            self.wfile.write(wire[midpoint:])
        else:
            self.wfile.write(wire)


class ProxyFixture(SendFixture):
    calls = []

    def do_POST(self):
        assert self.headers["authorization"] == "Bearer " + fixture_key
        assert "x-forward-probe" not in self.headers
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        assert body["messages"] == [{"role":"user","content":"use configured egress"}]
        self.calls.append((self.path, self.headers.get("x-relay-target"), self.headers.get("x-relay-path")))
        self.respond(200, b'data: {"transport":"proxy"}\n\ndata: [DONE]\n\n')


def request(port, path, method="GET", payload=None, cookie=None):
    headers = {}
    body = None
    if cookie is not None:
        headers["cookie"] = cookie
    if payload is not None:
        body = json.dumps(payload).encode()
        headers["content-type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        content = response.read().decode()
        if (response.headers.get("content-type") or "").startswith("application/json"):
            content = json.loads(content)
        return response.status, content, response.headers
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-translator-contract-") as directory:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    log = Path(directory) / "fallbacks.txt"
    env = {
        **os.environ,
        "PORT": str(port),
        "DATA_DIR": directory,
        "TMPDIR": directory,
        "NINEROUTER_MIGRATION_DATA_DIR": directory,
        "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(log),
        "ENABLE_TRANSLATOR": "true",
    }
    for key in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(key, None)
    process = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    upstream = None
    proxy = None
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("translator fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        assert request(port, "/dashboard/translator")[0] == 307
        assert request(port, "/api/translator/load?file=1_req_client.json")[0] == 401
        code, _, headers = request(port, "/api/auth/login", "POST", {"password": "123456"})
        assert code == 200
        cookie = headers["set-cookie"].split(";", 1)[0]

        def call(path, method="GET", payload=None):
            return request(port, path, method, payload, cookie)

        status, html, _ = call("/dashboard/translator")
        assert status == 200 and "/_next/" not in html
        assert all(f'id="translator-stage-{n}"' in html for n in range(1, 8))
        assert '/__9router-ui/translator.js' in html and 'href="/dashboard/translator"' in html
        assert 'href="/dashboard/translator"' in call("/dashboard")[1]
        status, script, headers = call("/__9router-ui/translator.js")
        assert status == 200 and headers["content-type"].startswith("text/javascript")
        assert 'fetch(`${base}/send`' in script and 'save(panel, read(panel))' in script

        source = {"body": {"model": "fixture-live/gpt-5.6-sol", "stream": True,
                           "messages": [{"role": "user", "content": "离线样本"}]}}
        file = "1_req_client.json"
        raw = json.dumps(source, ensure_ascii=False, indent=2) + "\r\n"
        assert call("/api/translator/save", "POST", {"file": file, "content": raw})[:2] == (200, {"success": True})
        assert call("/api/translator/load?file=" + file)[:2] == (200, {"success": True, "content": raw})
        assert (Path(directory) / "logs" / "translator" / file).read_bytes() == raw.encode()
        assert call("/api/translator/save", "POST", {"file": "../settings.json", "content": raw})[0] == 400
        modified = raw.replace("离线样本", "替换完成")
        assert call("/api/translator/save", "POST", {"file": file, "content": modified})[0] == 200
        assert call("/api/translator/load?file=" + file)[1]["content"] == modified

        def translate(step, body):
            status, response, _ = call("/api/translator/translate", "POST", {"step": step, "body": body})
            assert status == 200 and response["success"] is True
            return response["result"]

        detected = translate(1, source)
        assert detected == {
            "provider": "openai-compatible-chat-fixture-live", "model": "gpt-5.6-sol",
            "sourceFormat": "openai", "targetFormat": "openai",
        }
        converted = translate(2, source)["body"]
        assert converted == source["body"]
        assert call("/api/providers/fixture-live-connection", "PUT", {"isActive": True})[0] == 200
        preview = translate(3, {"provider": detected["provider"], "model": detected["model"], "body": {
            **converted, "model": detected["model"],
        }})
        assert preview["url"] == "http://127.0.0.1:52989/v1/chat/completions"
        assert preview["headers"] == {
            "Content-Type": "application/json", "Authorization": "[redacted]", "Accept": "text/event-stream",
        }
        assert "offline-models-provider" not in json.dumps(preview)

        assert call("/api/providers/fixture-responses-connection", "PUT", {"isActive": True})[0] == 200
        preview = translate(3, {"provider": "openai-compatible-responses-fixture", "model": "fixture-response-model",
                                "body": {"model": "fixture-response-model", "messages": converted["messages"], "stream": False}})
        assert preview["url"] == "http://127.0.0.1:52989/v1/responses"
        assert preview["headers"]["Authorization"] == "[redacted]"
        assert preview["body"]["input"][0]["content"][0]["text"] == "离线样本"

        assert not log.exists() or not log.read_text().strip()
        for provider, model, endpoint in [
            ("openai","gpt-4o-mini","https://api.openai.com/v1/chat/completions"),
            ("groq","llama-3.1-8b-instant","https://api.groq.com/openai/v1/chat/completions"),
            ("openrouter","openai/gpt-4o-mini","https://openrouter.ai/api/v1/chat/completions"),
            ("vercel-ai-gateway","openai/gpt-4o-mini","https://ai-gateway.vercel.sh/v1/chat/completions"),
        ]:
            key = f"offline-selected-{provider}-preview"
            assert call("/api/providers", "POST", {"provider":provider,"apiKey":key,"name":"Offline static preview"})[0] == 201
            body = {"model":model,"stream":False,"messages":[{"role":"user","content":"Static translator preview fixture"}]}
            previewed = translate(3,{"provider":provider,"model":model,"body":body})
            assert previewed["url"] == endpoint and previewed["body"] == body, (provider,previewed)
            assert previewed["headers"]["Authorization"] == "[redacted]" and key not in json.dumps(previewed)
            assert "Accept" not in previewed["headers"]
        tool_fixture = json.loads((root / "rust/reference/translator-tools-http.jsonl").read_text().splitlines()[0])
        tool_body = json.loads(tool_fixture["body"])["body"]
        tool_body["model"] = "gpt-4o-mini"
        expected = translate(2, tool_body)["body"]
        assert expected["messages"][0]["role"] == "system" and expected["messages"][-1]["role"] == "tool"
        for provider in ("openai", "openai-compatible-chat-fixture-live"):
            previewed = translate(3, {"provider":provider,"model":"gpt-4o-mini","body":tool_body})
            assert previewed["body"] == expected and previewed["headers"]["Authorization"] == "[redacted]"
        previewed = translate(3, {"provider":"openai-compatible-responses-fixture",
            "model":"fixture-response-model","body":tool_body})
        assert previewed["url"] == "http://127.0.0.1:52989/v1/responses"
        assert previewed["headers"]["Authorization"] == "[redacted]"
        items = previewed["body"]["input"]
        assert any(item["type"] == "function_call" and item["call_id"] == "call_01" for item in items)
        assert any(item["type"] == "function_call_output" and item["call_id"] == "call_01" for item in items)
        assert previewed["body"]["tools"][0]["name"] == "lookup"
        converted_elsewhere = translate(2, {
            "model": "gemini/gemini-2.5-flash", "contents": [{"role": "user", "parts": [{"text": "fixture"}]}],
        })
        assert converted_elsewhere["body"] == {
            "model": "gemini-2.5-flash", "stream": True, "messages": [{"role": "user", "content": "fixture"}],
        }
        status, problem, _ = call("/api/translator/translate", "POST", {
            "step": 3, "body": {"provider": "gemini", "model": "gemini-2.5-flash", **converted_elsewhere},
        })
        assert status == 400 and problem["error"] == "No active connection for provider: gemini"
        status, response, headers = call("/api/translator/send", "POST", {
            "provider": detected["provider"], "model": detected["model"], "body": {
                **converted, "model": detected["model"],
            },
        })
        assert status == 200 and headers["content-type"].startswith("text/event-stream")
        assert 'data: [DONE]' in response and 'migration' in response
        for stream, label in [(True, "direct native SSE"), (False, "direct genuine JSON")]:
            model = "fixture-response-model"
            code, wire, headers = call("/api/translator/send", "POST", {
                "provider": "openai-compatible-responses-fixture", "model": model,
                "body": {"model": model, "input": label, "stream": stream},
            })
            assert code == 200 and headers["content-type"] == "text/event-stream"
            if stream:
                assert "event: response.completed" in wire
            else:
                assert json.loads(wire)["output"][0]["content"][0]["text"] == "native responses migration"
        standard = [
            ("anthropic", "offline-official-anthropic", "claude-migration-fixture", {
                "messages": [{"role": "user", "content": "Anthropic translator fixture"}],
                "max_tokens": 64,
            }, "native Anthropic "),
            ("gemini", "offline-provider-placeholder", "gemini-2.5-flash", {
                "contents": [{"role": "user", "parts": [{"text": "Gemini translator fixture"}]}],
                "safetySettings": [
                    {"category": category, "threshold": "BLOCK_NONE"} for category in [
                        "HARM_CATEGORY_HARASSMENT", "HARM_CATEGORY_HATE_SPEECH",
                        "HARM_CATEGORY_SEXUALLY_EXPLICIT", "HARM_CATEGORY_DANGEROUS_CONTENT",
                        "HARM_CATEGORY_CIVIC_INTEGRITY",
                    ]
                ],
            }, "Gemini migration"),
        ]
        for provider, key, model, body, expected in standard:
            assert call("/api/providers", "POST", {
                "provider":provider, "name":"Offline translator send", "apiKey":key,
            })[0] == 201
            code, wire, headers = call("/api/translator/send", "POST", {
                "provider":provider, "model":model, "body":{"model":model, "stream":True, **body},
            })
            assert code == 200 and expected in wire and headers["content-type"] == "text/event-stream", (provider, code)
        model = "claude-sonnet-4-20250514"
        anthropic_preview = translate(3, {"provider":"anthropic", "model":model, "body": {
            "model":model, "stream":False, "max_tokens":64, "temperature":0.5,
            "messages":[{"role":"system","content":"Brief"},{"role":"user","content":"Anthropic preview fixture"}],
        }})
        assert anthropic_preview["url"] == "https://api.anthropic.com/v1/messages"
        assert anthropic_preview["headers"] == {
            "Content-Type":"application/json", "x-api-key":"[redacted]", "anthropic-version":"2023-06-01",
            "Anthropic-Beta":"claude-code-20250219,interleaved-thinking-2025-05-14",
        }
        assert anthropic_preview["body"]["max_tokens"] == 64 and "temperature" not in anthropic_preview["body"]
        assert anthropic_preview["body"]["system"][-1] == {"type":"text","text":"Brief",
            "cache_control":{"type":"ephemeral","ttl":"1h"}}
        for stream, suffix in [(True, "streamGenerateContent?alt=sse"), (False, "generateContent")]:
            model = "gemini-2.5-flash"
            gemini_preview = translate(3, {"provider":"gemini", "model":model, "body": {
                "model":model, "stream":stream, "messages":[{"role":"user","content":"Gemini translator fixture"}],
            }})
            assert gemini_preview["url"] == f"https://generativelanguage.googleapis.com/v1beta/models/{model}:{suffix}"
            assert gemini_preview["headers"] == {
                "Content-Type":"application/json", "x-goog-api-key":"[redacted]",
                **({"Accept":"text/event-stream"} if stream else {}),
            }
            assert gemini_preview["body"]["contents"] == standard[1][3]["contents"]
            assert {setting["threshold"] for setting in gemini_preview["body"]["safetySettings"]} == {"OFF"}
        assert call("/api/providers/fixture-muse-oauth", "PUT", {"isActive": True})[0] == 200
        for stream in (False, True):
            model = "fixture-muse-chat"
            payload = {"provider":"muse", "model":model, "body":{
                "model":model, "messages":[{"role":"user","content":"Muse shared Chat"}], "stream":stream,
            }}
            previewed = translate(3,payload)
            assert previewed == {"url":"https://api.meta.ai/v1/chat/completions", "body":payload["body"],
                "headers":{"Content-Type":"application/json", "Authorization":"[redacted]", "x-api-version":"1.0.0",
                    **({"Accept":"text/event-stream"} if stream else {})}}
            code, raw, _ = call("/api/translator/send", "POST", {
                **payload, "url":"https://other.example.invalid", "headers":{"Authorization":"Bearer offline-unselected"},
            })
            assert code == 200 and "Muse fixture response" in raw and "LLM|fixture-muse-seat" not in raw
            assert ("data: [DONE]" in raw) is stream
        assert call("/api/providers/fixture-muse-oauth", "PUT", {"isActive": False})[0] == 200
        assert call("/api/providers/fixture-kimi-translator-oauth", "PUT", {"isActive": True})[0] == 200
        preview = translate(3, {"provider":"kimi", "model":"k3", "body":{
            "model":"k3", "max_tokens":32, "messages":[{"role":"user","content":"Kimi preview fixture"}], "stream":False,
        }})
        assert preview["url"] == "https://api.kimi.com/coding/v1/messages?beta=true"
        assert preview["headers"]["x-api-key"] == "[redacted]" and preview["headers"]["X-Msh-Device-Id"] == "[redacted]"
        assert preview["headers"]["User-Agent"].startswith("9Router/") and preview["headers"]["X-Msh-Platform"] == "9router"
        assert preview["body"]["messages"][0]["content"][0]["text"] == "Kimi preview fixture"
        assert "0139b5c9-506c-48fb-bef1-956bfdc9880b" not in json.dumps(preview)
        code, raw, _ = call("/api/translator/send", "POST", {
            "provider":"kimi", "model":"k3", "body":{
                "model":"k3", "max_tokens":32, "messages":[{"role":"user","content":[
                    {"type":"text","text":"fixture Kimi Translator refresh"},
                ]}], "stream":False,
            },
        })
        assert code == 200, (code,raw)
        assert json.loads(raw)["content"][0]["text"] == "Kimi refresh echoed [redacted] and [redacted]"
        with sqlite3.connect(Path(directory)/"db/data.sqlite") as store:
            saved = json.loads(store.execute("SELECT data FROM providerConnections WHERE id='fixture-kimi-translator-oauth'").fetchone()[0])
        assert saved["accessToken"] == "offline-kimi-translator-new" and saved["refreshToken"] == "offline-kimi-translator-rotated"
        assert call("/api/providers/fixture-kimi-translator-oauth", "PUT", {"isActive": False})[0] == 200
        assert not log.exists() or not log.read_text().strip()
        for filename, content in [("4_req_target.json", json.dumps(preview)), ("5_res_provider.txt", response)]:
            assert call("/api/translator/save", "POST", {"file": filename, "content": content})[0] == 200
            assert call("/api/translator/load?file=" + filename)[1]["content"] == content

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), SendFixture)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{upstream.server_port}/v1"
        selected_ids = {}

        def create(kind, prefix):
            node = {"type": kind, "name": prefix, "prefix": prefix, "baseUrl": base}
            if kind == "openai-compatible":
                node["apiType"] = "chat"
            status, body, _ = call("/api/provider-nodes", "POST", node)
            assert status == 201
            provider = body["node"]["id"]
            for name, key, active in [
                ("inactive", "offline-unused-credential", False), ("selected", fixture_key, True),
            ]:
                status, connection, _ = call("/api/providers", "POST", {
                    "provider": provider, "name": name, "apiKey": key,
                })
                assert status == 201
                if not active:
                    assert call(f"/api/providers/{connection['connection']['id']}", "PUT", {"isActive": False})[0] == 200
                else:
                    selected_ids[provider] = connection["connection"]["id"]
            return provider

        provider = create("openai-compatible", "translator-send-fixture")
        claude = create("anthropic-compatible", "translator-claude-fixture")
        preview = translate(3, {"provider":claude,"model":"claude-sonnet-test","body": {
            "model":"claude-sonnet-test", "messages":[{"role":"user","content":"preview stored origin"}],
        }})
        assert preview["url"] == base + "/messages"
        assert preview["headers"] == {
            "Content-Type":"application/json", "x-api-key":"[redacted]", "Authorization":"[redacted]",
            "anthropic-version":"2023-06-01", "Accept":"text/event-stream",
        }

        def send(provider, model, prompt):
            return call("/api/translator/send", "POST", {
                "provider": provider, "model":model, "url": "https://example.invalid/",
                "headers":{"Authorization": "Bearer offline-false", "x-forward-probe": "not forwarded"},
                "body": {"model": model, "messages": [{"role": "user", "content": prompt}], "stream": True},
            })

        for entry, model in [(provider, "fixture-model"), (claude, "claude-sonnet-test")]:
            code, wire, headers = send(entry, model, "echo")
            assert code == 200 and headers["content-type"] == "text/event-stream"
            assert headers["cache-control"] == "no-cache"
            assert wire == 'data: {"echo":"[redacted]"}\r\n\r\ndata: [DONE]\n\n'
        code, problem, _ = send(provider, "fixture-model", "reject")
        assert code == 403 and problem == {
            "success": False, "error":"Provider error: 403", "details":"Failed with configured key [redacted]",
        }
        assert send(provider, "fixture-model", "retry")[0] == 200 and SendFixture.retries == 2
        proxy = ThreadingHTTPServer(("127.0.0.1", 0), ProxyFixture)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        proxy_url = f"http://127.0.0.1:{proxy.server_port}"

        def bind_proxy(kind, address, no_proxy=""):
            code, pool, _ = call("/api/proxy-pools", "POST", {
                "name":"Translator fixture", "type":kind,"proxyUrl":address, "strictProxy":True, "noProxy":no_proxy,
            })
            assert code == 201
            code, _, _ = call(f"/api/providers/{selected_ids[provider]}", "PUT", {
                "providerSpecificData":{"baseUrl":base,"apiType":"chat","proxyPoolId":pool["proxyPool"]["id"]},
            })
            assert code == 200

        for kind, address, expected in [
            ("http", proxy_url, (base + "/chat/completions",None,None)),
            ("vercel", proxy_url + "/relay", ("/relay",base.removesuffix("/v1"),"/v1/chat/completions")),
        ]:
            bind_proxy(kind, address)
            code, wire, _ = send(provider,"fixture-model","use configured egress")
            assert code == 200 and '"transport":"proxy"' in wire
            assert ProxyFixture.calls == [expected]
            ProxyFixture.calls.clear()
        bind_proxy("http", proxy_url, "127.0.0.1")
        code, problem, _ = send(provider,"fixture-model","use configured egress")
        assert code == 502 and problem["error"] == "Required provider proxy is unavailable"
        assert not ProxyFixture.calls
        assert not log.exists() or not log.read_text().strip()
        print("Translator Send streams, selected credentials, retries, proxies, redaction and scoped history passed")
    finally:
        if proxy is not None:
            proxy.shutdown()
            proxy.server_close()
        if upstream is not None:
            upstream.shutdown()
            upstream.server_close()
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
