import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Thread
import time
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
server = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
key = "offline-provider-validate-key"
bad_key = "offline-provider-validate-bad-key"
secret = "offline-provider-validate-url-secret"
ledger = []
failures = []
proxy_ledger = []
route = "/api/providers/validate"
chat_probes = {
    "glm": ("api.z.ai/api/anthropic/v1/messages", "glm-5.3", True),
    "glm-cn": ("open.bigmodel.cn/api/coding/paas/v4/chat/completions", "glm-5.3", False),
    "kimi": ("api.kimi.com/coding/v1/messages", "kimi-k3", True),
    "minimax": ("api.minimax.io/anthropic/v1/messages", "MiniMax-M3", True),
    "minimax-cn": ("api.minimaxi.com/anthropic/v1/messages", "MiniMax-M3", True),
    "volcengine-ark": ("ark.cn-beijing.volces.com/api/coding/v3/chat/completions", "Doubao-Seed-2.0-Code", False),
    "byteplus": ("ark.ap-southeast.bytepluses.com/api/coding/v3/chat/completions", "seed-2-0-pro-260328", False),
}
chat_paths = {"/validate/" + path: (model, claude) for path, model, claude in chat_probes.values()}


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, redirect=None):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2")
        if redirect:
            self.send_header("Location", redirect)
        self.end_headers()
        self.wfile.write(b"{}")

    def track(self, check):
        if not check:
            failures.append(self.path)

    def do_GET(self):
        ledger.append(self.path)
        bearer = self.headers.get("Authorization")
        if self.path == "/cross-origin/models":
            self.reply(307, trap)
            return
        if self.path == "/target":
            failures.append("redirect was followed")
            self.reply(200)
            return
        if self.path in {"/validate/api.x.ai/v1/models", "/validate/api.atria-asi.ai/v1/models"}:
            self.track(bearer in {f"Bearer {key}", f"Bearer {bad_key}"})
            if self.path.startswith("/validate/api.x.ai/"):
                self.reply(403 if bearer == f"Bearer {key}" else 400)
            else:
                self.reply(404 if bearer == f"Bearer {key}" else 403)
            return
        if self.path.startswith("/validate/www.searchapi.io/"):
            query = parse_qs(urlsplit(self.path).query)
            self.track(query == {"api_key":[key],"q":["ping"],"engine":["google"]})
            self.track(bearer is None)
            self.reply(400)
            return
        if self.path in {"/local/api/tags", "/local-auth/api/tags"}:
            self.track(bearer == (f"Bearer {key}" if self.path == "/local-auth/api/tags" else None))
            self.reply(200)
            return
        self.track(bearer in {f"Bearer {key}", f"Bearer {bad_key}"})
        if self.path in {"/compatible/models", "/embedding-models/models"}:
            self.reply(200 if bearer == f"Bearer {key}" else 401)
        elif self.path == "/embedding-no-models/models":
            self.reply(405 if bearer == f"Bearer {key}" else 403)
        elif self.path == "/embedding-forbidden/models":
            self.reply(404)
        else:
            self.reply(404)

    def do_POST(self):
        ledger.append(self.path)
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/claude/v1/messages":
            sent = self.headers.get("x-api-key")
            self.track(sent in {key, bad_key} and self.headers.get("Authorization") == f"Bearer {sent}")
            self.track(self.headers.get("Anthropic-Version") == "2023-06-01")
            self.track(body == {"model":"fixture-claude","max_tokens":1,
                                "messages":[{"role":"user","content":"test"}]})
            self.reply(400 if sent == key else 403)
        elif self.path.startswith("/embedding-"):
            self.track(body == {"model":"test","input":"ping"})
            self.track(self.headers.get("Authorization") == f"Bearer {key}")
            self.reply(403 if self.path == "/embedding-forbidden/embeddings" else 400)
        elif self.path.startswith("/openai/deployments/"):
            self.track(self.headers.get("api-key") in {key, bad_key})
            self.track(body == {"messages":[{"role":"user","content":"test"}],"max_tokens":1})
            if self.path == "/openai/deployments/fixture-model/chat/completions?api-version=2026-01-01":
                self.track(self.headers.get("OpenAI-Organization") == "fixture-org")
            else:
                self.track(self.path == "/openai/deployments/gpt-4/chat/completions?api-version=2024-10-01-preview")
                self.track(self.headers.get("OpenAI-Organization") is None)
            self.reply(400 if self.headers.get("api-key") == key else 401)
        elif self.path == "/validate/api.deepgram.com/v1/listen":
            access = self.headers.get("Authorization")
            self.track(access in {f"Token {key}", f"Token {bad_key}"})
            self.track(body == {"input":"ping","text":"ping","prompt":"ping","model":"nova-3"})
            self.reply(400 if access == f"Token {key}" else 401)
        elif self.path == "/validate/api.atria-asi.ai/v1/chat/completions":
            self.track(self.headers.get("Authorization") == f"Bearer {key}")
            self.track(body == {"model":"Atria-Dawn-Preview","messages":[{"role":"user","content":"ping"}],
                                "max_tokens":1})
            self.reply(400)
        elif self.path in chat_paths:
            model, claude = chat_paths[self.path]
            self.track(body == {"model":model,"max_tokens":1,"messages":[{"role":"user","content":"test"}]})
            if claude:
                sent = self.headers.get("X-API-Key")
                version = self.headers.get("Anthropic-Version", "")
                self.track(all(value.strip() == "2023-06-01" for value in version.split(",")))
                self.track(self.headers.get("Anthropic-Beta") == "claude-code-20250219,interleaved-thinking-2025-05-14")
                self.track(self.headers.get("Authorization") is None)
            else:
                sent = self.headers.get("Authorization", "").removeprefix("Bearer ")
                self.track(self.headers.get("X-API-Key") is None)
            self.track(sent in (key, bad_key))
            self.reply(400 if sent == key else 401)
        else:
            self.reply(404)


class Proxy(Endpoint):
    mode = "forward"

    def forward(self, handler):
        parts = urlsplit(self.path)
        self.track(parts.scheme == "http" and parts.hostname == "127.0.0.1")
        self.track(parts.port in {52979, mock.server_port})
        self.track(self.headers["Cookie"] is None and self.headers["X-9r-Cli-Token"] is None)
        self.path = parts.path + ("?" + parts.query if parts.query else "")
        proxy_ledger.append((self.command, self.path))
        if self.mode == "auth":
            return self.reply(407)
        if self.mode == "disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.close_connection = True
            return
        handler(self)

    def do_GET(self):
        self.forward(Endpoint.do_GET)

    def do_POST(self):
        self.forward(Endpoint.do_POST)


def request(port, path, method="POST", body=None, cookie=None):
    headers = {"Content-Type":"application/json"}
    if cookie:
        headers["Cookie"] = cookie
    if body is not None and not isinstance(body, bytes):
        body = json.dumps(body)
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read()), response.headers
    finally:
        connection.close()


def ready(port, process):
    deadline = time.monotonic() + 15
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if request(port, "/api/health", "GET")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.02)
    raise RuntimeError("fixture service did not become ready")


def configure(db, base):
    rows = {
        "openai-compatible-contract": {"baseUrl":base + "/compatible/"},
        "openai-compatible-private": {"baseUrl":base.replace("://", f"://fixture:{secret}@") + "/compatible"},
        "openai-compatible-malformed": {"baseUrl":"not-a-valid-url"},
        "openai-compatible-redirect": {"baseUrl":base + "/cross-origin"},
        "anthropic-compatible-contract": {"baseUrl":f" {base}/claude/messages/ ", "defaultModel":"fixture-claude"},
        "custom-embedding-models": {"baseUrl":base + "/embedding-models"},
        "custom-embedding-post": {"baseUrl":base + "/embedding-no-models/"},
        "custom-embedding-post-forbidden": {"baseUrl":base + "/embedding-forbidden"},
    }
    with sqlite3.connect(db) as connection:
        for id_, data in rows.items():
            connection.execute(
                "INSERT INTO providerNodes (id,type,name,data,createdAt,updatedAt) VALUES (?,?,?,?,?,?)",
                (id_, id_.rsplit("-", 1)[0], id_, json.dumps(data), "2026-11-19", "2026-11-19"),
            )


with tempfile.TemporaryDirectory(prefix="9router-provider-validate-") as temporary:
    directory = Path(temporary)
    mock = ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    sentinel = ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    registry = ThreadingHTTPServer(("127.0.0.1", 52979), Endpoint)
    proxy = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    trap = f"http://127.0.0.1:{sentinel.server_port}/target"
    for listener in [mock, sentinel, registry, proxy]:
        Thread(target=listener.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{mock.server_port}"
    with socket.socket() as socket_:
        socket_.bind(("127.0.0.1", 0))
        port = socket_.getsockname()[1]
    fallbacks = directory / "fallbacks.txt"
    env = {"PATH":os.environ["PATH"], "HOME":str(directory), "DATA_DIR":str(directory),
           "PORT":str(port), "HOSTNAME":"127.0.0.1", "NEXT_TELEMETRY_DISABLED":"1", "MODEL_CATALOG_SYNC":"off",
           "NINEROUTER_MIGRATION_DATA_DIR":str(directory), "NINEROUTER_MIGRATION_FIXTURE":"1",
           "RUST_ANYTHING_FALLBACK_LOG":str(fallbacks)}
    if mode == "standalone":
        path = directory / "bin"
        path.mkdir()
        env["PATH"] = str(path)
    if mode == "original":
        subprocess.run(["python3","rust/reference/seed.py",str(directory)], cwd=root,
                       env={**env,"NODE_NO_WARNINGS":"1"}, check=True)
        command = ["node","--import","./rust/reference/mock-provider-validate.mjs",".next/standalone/custom-server.js"]
    else:
        command = [str(server), "--no-browser"]
    child = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        ready(port, child)
        assert request(port, route, body={"provider":"ollama-local"})[:2] == (401, {"error":"Unauthorized"})
        assert not ledger
        code, _, headers = request(port, "/api/auth/login", body={"password":"123456"})
        assert code == 200
        cookie = headers["Set-Cookie"].split(";", 1)[0]
        db = directory / "db/data.sqlite"
        configure(db, base)
        captures = {}

        def call(label, body, expected_status=200, outbound=()):
            ledger.clear()
            status, response, _ = request(port, route, body=body, cookie=cookie)
            assert status == expected_status and ledger == list(outbound), (label, status, response, ledger)
            assert not failures, failures
            assert all(secret_ not in json.dumps(response) for secret_ in (key, bad_key, secret))
            captures[label] = response
            return response

        assert call("missing", {}, 400) == {"error":"Provider and API key required"}
        assert call("invalid", b"{", 500) == {"error":"Validation failed"}
        assert call("null", b"null", 500) == {"error":"Validation failed"}
        assert call("unknown-provider", {"provider":"missing-provider","apiKey":key}, 400) == {
            "error":"Provider validation not supported"}
        assert call("type-provider", {"provider":{},"apiKey":key}, 400) == {
            "error":"Provider validation not supported"}
        for provider, error in [("openai-compatible-absent","OpenAI Compatible node not found"),
                                ("anthropic-compatible-absent","Anthropic Compatible node not found"),
                                ("custom-embedding-absent","Custom Embedding node not found")]:
            assert call(provider, {"provider":provider,"apiKey":key}, 404) == {"error":error}
        for name, provider, selected, paths, verdict in [
            ("openai-good","openai-compatible-contract",key,["/compatible/models"],True),
            ("openai-bad","openai-compatible-contract",bad_key,["/compatible/models"],False),
            ("anthropic-accepted","anthropic-compatible-contract",key,["/claude/v1/messages"],True),
            ("anthropic-bad","anthropic-compatible-contract",bad_key,["/claude/v1/messages"],False),
            ("embedding-models-denied","custom-embedding-models",bad_key,["/embedding-models/models"],False),
            ("embedding-models-403","custom-embedding-post",bad_key,["/embedding-no-models/models"],False),
            ("embedding-accepted","custom-embedding-post",key,
             ["/embedding-no-models/models","/embedding-no-models/embeddings"], True),
            ("embedding-denied","custom-embedding-post-forbidden",key,
             ["/embedding-forbidden/models","/embedding-forbidden/embeddings"], False),
        ]:
            result = call(name, {"provider":provider,"apiKey":selected,
                                 "providerSpecificData":{"baseUrl":base + "/ignored"}}, outbound=paths)
            assert result == {"valid":verdict,"error":None if verdict else "Invalid API key"}
        assert call("embedding-open-models", {"provider":"custom-embedding-models","apiKey":key},
                    outbound=["/embedding-models/models"]) == {"valid":True}
        assert call("azure", {"provider":"azure","apiKey":key,"providerSpecificData":{
            "azureEndpoint":base + "/", "deployment":"fixture-model", "apiVersion":"2026-01-01",
            "organization":"fixture-org"}},
            outbound=["/openai/deployments/fixture-model/chat/completions?api-version=2026-01-01"])["valid"]
        assert call("azure-bad", {"provider":"azure","apiKey":bad_key,"providerSpecificData":{"azureEndpoint":base}},
                    outbound=["/openai/deployments/gpt-4/chat/completions?api-version=2024-10-01-preview"]) == {
            "valid":False,"error":"Invalid API key or Azure configuration"}
        assert call("ollama", {"provider":" Ollama Local ","providerSpecificData":{"baseUrl":f" {base}/local/ "}},
                    outbound=["/local/api/tags"]) == {"valid":True,"error":None}
        assert call("ollama-key", {"provider":"ollama-local","apiKey":key,"providerSpecificData":{"baseUrl":base + "/local-auth"}},
                    outbound=["/local-auth/api/tags"]) == {"valid":True,"error":None}
        assert call("cloudflare", {"provider":"Cloudflare AI","apiKey":key}) == {"valid":False,"error":"Missing Account ID"}
        for provider in ["edge-tts","coqui","searxng"]:
            assert call(provider, {"provider":provider}) == {"valid":True,"error":None}
        sa = {"type":"service_account","client_email":"test@example.invalid","private_key":key,"project_id":"fixture"}
        assert call("vertex-shape", {"provider":"vertex","apiKey":json.dumps(sa)}) == {"valid":True,"error":None}
        del sa["project_id"]
        assert call("vertex-incomplete", {"provider":"vertex","apiKey":json.dumps(sa)}) == {"valid":False,"error":"Invalid API key"}
        for label, provider, provided, paths, valid in [
            ("xai-no-credit","xai",key,["/validate/api.x.ai/v1/models"],True),
            ("xai-bad","xai",bad_key,["/validate/api.x.ai/v1/models"],False),
            ("media-good","deepgram",key,["/validate/api.deepgram.com/v1/listen"],True),
            ("media-bad","deepgram",bad_key,["/validate/api.deepgram.com/v1/listen"],False),
            ("generic-fallback","atria",key,["/validate/api.atria-asi.ai/v1/models",
                                           "/validate/api.atria-asi.ai/v1/chat/completions"],True),
            ("generic-denied","atria",bad_key,["/validate/api.atria-asi.ai/v1/models"],False),
            ("web-query","searchapi",key,[f"/validate/www.searchapi.io/api/v1/search?api_key={key}&q=ping&engine=google"],True),
        ]:
            assert call(label, {"provider":provider,"apiKey":provided}, outbound=paths) == {
                "valid":valid,"error":None if valid else "Invalid API key"}
        for provider, (path, _, _) in chat_probes.items():
            for provided, accepted in ((key, True), (bad_key, False)):
                label = provider + ("-accepted" if accepted else "-denied")
                assert call(label, {"provider":provider,"apiKey":provided}, outbound=("/validate/" + path,)) == {
                    "valid":accepted,"error":None if accepted else "Invalid API key"}

        assert call("cursor-unsupported", {"provider":"cursor","apiKey":key}, 400) == {
            "error":"Provider validation not supported"}
        if mode != "original":
            assert not fallbacks.exists() or not fallbacks.read_text().strip()
            for provider, outbound in [("openai-compatible-private", []), ("openai-compatible-malformed", []),
                                       ("openai-compatible-redirect", ["/cross-origin/models"])]:
                row = call("strict-" + provider, {"provider":provider,"apiKey":key}, outbound=outbound)
                assert row["valid"] is False
                captures.pop("strict-" + provider)
        if mode == "standalone":
            with sqlite3.connect(db) as connection:
                connection.execute("""INSERT INTO settings(id,data) VALUES(1,'{"outboundProxyEnabled":true}')
                    ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.outboundProxyEnabled',json('true'))""")
            assert call("global-proxy", {"provider":"openai-compatible-contract","apiKey":key}) == {
                "valid":False,"error":"fetch failed"}
            assert call("global-proxy-default", {"provider":"atria","apiKey":key}) == {
                "valid":False,"error":"fetch failed"}
            with sqlite3.connect(db) as connection:
                connection.execute("UPDATE settings SET data=json_set(data,'$.outboundProxyUrl',?) WHERE id=1",
                                   (f"http://127.0.0.1:{proxy.server_port}",))
            for label, provider, provided, details, outgoing, expected in [
                ("proxy-web", "searchapi", key, {},
                 [f"/validate/www.searchapi.io/api/v1/search?api_key={key}&q=ping&engine=google"], True),
                ("proxy-media", "deepgram", key, {}, ["/validate/api.deepgram.com/v1/listen"], True),
                ("proxy-default", "atria", key, {}, ["/validate/api.atria-asi.ai/v1/models",
                                                     "/validate/api.atria-asi.ai/v1/chat/completions"], True),
                ("proxy-default-denied", "atria", bad_key, {}, ["/validate/api.atria-asi.ai/v1/models"], False),
                ("proxy-azure", "azure", key, {"azureEndpoint":base + "/"},
                 ["/openai/deployments/gpt-4/chat/completions?api-version=2024-10-01-preview"], True),
                ("proxy-kimi", "kimi", key, {}, ["/validate/" + chat_probes["kimi"][0]], True),
                ("proxy-ark", "volcengine-ark", key, {}, ["/validate/" + chat_probes["volcengine-ark"][0]], True),
            ]:
                start = len(proxy_ledger)
                assert call(label, {"provider":provider,"apiKey":provided,"providerSpecificData":details},
                            outbound=outgoing) == {"valid":expected,"error":None if expected else "Invalid API key"}
                assert [path for _, path in proxy_ledger[start:]] == outgoing
            for label, message in (("auth", "Proxy authentication required"), ("disconnect", "fetch failed")):
                Proxy.mode = label
                start = len(proxy_ledger)
                assert call("proxy-default-" + label, {"provider":"atria","apiKey":key}) == {
                    "valid":False,"error":message}
                assert proxy_ledger[start:] == [("GET", "/validate/api.atria-asi.ai/v1/models")]
            Proxy.mode = "forward"
            with sqlite3.connect(db) as connection:
                connection.execute("UPDATE settings SET data=json_set(data,'$.outboundNoProxy','127.0.0.1') WHERE id=1")
            start = len(proxy_ledger)
            assert call("proxy-bypass", {"provider":"atria","apiKey":key}, outbound=(
                "/validate/api.atria-asi.ai/v1/models", "/validate/api.atria-asi.ai/v1/chat/completions")) == {
                    "valid":True,"error":None}
            assert len(proxy_ledger) == start
        print(json.dumps(captures, sort_keys=True))
    finally:
        child.terminate()
        try:
            child.wait(timeout=7)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        for listener in [mock, sentinel, registry]:
            listener.shutdown()
            listener.server_close()
