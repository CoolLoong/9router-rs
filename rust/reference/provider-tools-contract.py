import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
provider_key = "offline-provider-tools-key"
claude_key = "offline-claude-tools-key"


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, body):
        wire = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    def do_GET(self):
        if self.path == "/openai/v1/models":
            if self.headers.get("Authorization") != f"Bearer {provider_key}":
                self.send(401, {"error": f"Rejected {provider_key}"})
                return
            self.send(200, {"data": [
                {"id": "unit-test", "name": "Fixture model", "metadata": {"echo": f"Key was {provider_key}"}},
            ]})
        elif self.path == "/claude/models":
            if self.headers.get("x-api-key") != claude_key or self.headers.get("anthropic-version") != "2023-06-01":
                self.send(400, {"error": "Invalid Anthropic discovery headers"})
                return
            self.send(200, {"models": [{"id": "claude-fixture"}]})
        elif self.path == "/ollama/api/tags":
            self.send(200, {"models": [{"name": "test:local", "size": 7}]})
        else:
            self.send(404, {"error": "Unknown fixture path"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/openai/v1/chat/completions":
            if self.headers.get("Authorization") != f"Bearer {provider_key}":
                return self.send(401, {"error": {"message": "Unexpected provider authentication"}})
            if body.get("model") != "unit-test" or body.get("messages") != [{"role": "user", "content": "hi"}]:
                return self.send(422, {"error": {"message": f"Unexpected {provider_key}"}})
            return self.send(200, {
                "id": "fixture-probe", "object": "chat.completion", "created": 1784419200,
                "model": "unit-test", "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "fixture probe complete"}}],
            })
        if self.path == "/claude/v1/messages" and self.headers.get("x-api-key") == claude_key:
            if body.get("model") != "claude-fixture" or body.get("max_tokens") != 1:
                return self.send(401, {"error": "Unexpected test payload"})
            return self.send(400, {"error": "An admitted 400 proves credentials were accepted"})
        return self.send(404, {"error": "Unknown fixture path"})


def request(port, path, method="GET", body=None, headers=auth):
    headers = headers.copy()
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body)
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, path, body=data, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        client.close()


with tempfile.TemporaryDirectory(prefix="9router-provider-tools-contract-") as directory:
    fixture = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    target = f"http://127.0.0.1:{fixture.server_port}"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    log = Path(directory) / "fallbacks.log"
    env = {
        **os.environ, "PORT": "0", "DATA_DIR": directory, "TMPDIR": directory,
        "NINEROUTER_MIGRATION_DATA_DIR": directory, "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(log),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(name, None)
    process = subprocess.Popen([binary, "--port", str(port)], cwd=root, env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("provider tools fixture did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        def call(path, method="GET", body=None):
            return request(port, path, method, body)

        def connect(kind, prefix, endpoint, key, selected=None):
            create = {"type": kind, "prefix": prefix, "name": prefix, "baseUrl": endpoint}
            if kind == "openai-compatible":
                create["apiType"] = "chat"
            status, node = call("/api/provider-nodes", "POST", create)
            assert status == 201
            status, result = call("/api/providers", "POST", {
                "provider": node["node"]["id"], "name": prefix,
                "apiKey": key, "defaultModel": selected,
            })
            assert status == 201 and key not in json.dumps(result)
            return result["connection"]["id"], node["node"]["id"]

        id, provider = connect("openai-compatible", "provider-tools-fixture", target + "/openai/v1", provider_key)
        path = f"/api/providers/{id}"
        assert request(port, path + "/models", headers={})[0] == 401
        assert request(port, path + "/test", "POST", headers={})[0] == 401
        code, rows = call(path + "/models")
        assert code == 200 and rows["provider"] == provider and rows["connectionId"] == id
        assert rows["models"][0]["name"] == "Fixture model"
        assert rows["models"][0]["metadata"]["echo"] == "Key was [redacted]"
        assert call(path + "/test", "POST") == (200, {"valid": True, "error": None, "refreshed": False})
        code, saved = call(path)
        assert code == 200 and saved["connection"]["testStatus"] == "active"
        assert saved["connection"]["lastError"] is None and "apiKey" not in saved["connection"]

        code, probe = call("/api/models/test", "POST", {"model": "provider-tools-fixture/unit-test"})
        assert code == 200 and probe["ok"] and probe["status"] == 200 and probe["error"] is None
        code, batch = call(path + "/test-models", "POST")
        assert code == 200 and batch["provider"] == provider and batch["connectionId"] == id
        assert batch["results"][0]["modelId"] == "unit-test" and batch["results"][0]["ok"] is True

        assert call(path, "PUT", {"apiKey": "offline-bad-key"})[0] == 200
        assert call(path + "/models") == (401, {"error": "Failed to fetch models: 401"})
        code, failed = call(path + "/test", "POST")
        assert code == 200 and failed["valid"] is False and failed["error"] == "Invalid API key or base URL"
        code, saved = call(path)
        assert code == 200 and saved["connection"]["isActive"] is True
        assert saved["connection"]["testStatus"] == "error" and saved["connection"]["lastErrorAt"]

        claude, _ = connect("anthropic-compatible", "claude-tools-fixture", target + "/claude/messages", claude_key, "claude-fixture")
        code, rows = call(f"/api/providers/{claude}/models")
        assert code == 200 and rows["models"] == [{"id": "claude-fixture"}]
        assert call(f"/api/providers/{claude}/test", "POST") == (200, {"valid": True, "error": None, "refreshed": False})

        status, ollama = call("/api/providers", "POST", {
            "provider": "ollama-local", "name": "Offline Ollama", "baseUrl": target + "/ollama/",
        })
        assert status == 201
        local = f"/api/providers/{ollama['connection']['id']}"
        assert call(local + "/models")[1]["models"] == [{"name": "test:local", "size": 7}]
        assert call(local + "/test", "POST")[1]["valid"] is True
        private_endpoint = target.replace("http://", "http://offline-user:offline-password@") + "/missing"
        assert call(local, "PUT", {"providerSpecificData": {"baseUrl": private_endpoint}})[0] == 200
        code, result = call(local + "/test", "POST")
        assert code == 200 and result["error"] == f"Ollama not reachable at {target}/missing"
        assert not log.exists() or not log.read_text().strip()
        print("Provider model discovery, credential redaction, testing, persisted outcomes and gateway pings passed")
    finally:
        fixture.shutdown()
        fixture.server_close()
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
