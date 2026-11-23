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
from urllib.parse import urlsplit

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
provider = "openai-compatible-detail-fixture"
connection_id = "detail-active"
secret = "offline-provider-detail-key"
ledger = []


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, data):
        content = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def do_GET(self):
        assert self.path == "/v1/models"
        assert self.headers.get("Authorization") == f"Bearer {secret}"
        ledger.append("models")
        self.reply(200, {"data":[{"id":"fixture-live","name":"Fixture Live"}]})

    def do_POST(self):
        assert self.headers.get("Authorization") == f"Bearer {secret}"
        data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/v1/embeddings":
            assert data["input"] in ("test", ["test"]) and data["model"] == "fixture-embedding"
            ledger.append("embedding")
            self.reply(200, {"object":"list","model":"fixture-embedding",
                             "data":[{"object":"embedding","index":0,"embedding":[0.2,0.4]}]})
            return
        assert self.path == "/v1/chat/completions"
        assert data.get("model") == "fixture-live"
        assert data.get("messages") == [{"role":"user","content":"hi"}]
        ledger.append("chat")
        self.reply(200, {"id":"chatcmpl-provider-detail","object":"chat.completion","created":1784419200,
                         "model":"fixture-live","choices":[{"index":0,"finish_reason":"stop",
                         "message":{"role":"assistant","content":"fixture reply"}}]})


def request(port, path, method="GET", body=None, cookie=None, extra=None):
    headers = {"Content-Type":"application/json"}
    if cookie:
        headers["Cookie"] = cookie
    headers.update(extra or {})
    if body is not None:
        body = json.dumps(body)
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        data = response.read().decode()
        if response.headers.get("Content-Type", "").startswith("application/json"):
            data = json.loads(data)
        return response.status, data, response.headers
    finally:
        connection.close()


def ready(port, child):
    deadline = time.monotonic() + 15
    while child.poll() is None and time.monotonic() < deadline:
        try:
            if request(port, "/api/health")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.03)
    raise RuntimeError("fixture did not become ready")


def configure(db_path, host):
    timestamp = "2026-11-19T00:00:00.000Z"
    details = {"baseUrl":host + "/v1","apiType":"chat","prefix":"detail-fixture","nodeName":"Fixture Node"}
    with sqlite3.connect(db_path) as db:
        db.execute("DELETE FROM providerConnections")
        db.execute("""INSERT INTO apiKeys(id,key,name,machineId,isActive,createdAt,accessRestricted,accessAllow)
            VALUES ('fixture-detail-gateway','offline-detail-gateway','Fixture gateway','fixture-machine',1,?,0,'[]')""",
            (timestamp,))
        db.execute("""INSERT INTO providerNodes(id,type,name,data,createdAt,updatedAt)
            VALUES (?,'openai-compatible',?,?,?,?)""",
            (provider, "Fixture <Node>", json.dumps(details), timestamp, timestamp))
        db.execute("""INSERT INTO providerNodes(id,type,name,data,createdAt,updatedAt)
            VALUES ('custom-embedding-detail','custom-embedding','Fixture Embedding',?,?,?)""",
            (json.dumps(details),timestamp,timestamp))
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('detail-embedding','custom-embedding-detail','apikey','Fixture Embedding',1,1,?,?,?)""",
            (json.dumps({"apiKey":secret,"defaultModel":"fixture-embedding","providerSpecificData":details}),
             timestamp,timestamp))
        for identifier, active in [(connection_id, True), ("detail-inactive", False)]:
            data = {"apiKey":secret,"providerSpecificData":details,
                    "defaultModel":"fixture-live","testStatus":"active"}
            if active:
                data["modelLock_fixture-live"] = "2099-07-01T02:00:00.000Z"
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,?,'apikey',?,1,?,?,?,?)""",
                (identifier, provider, identifier, int(active), json.dumps(data), timestamp, timestamp))


with tempfile.TemporaryDirectory(prefix="9router-provider-detail-") as path:
    directory = Path(path)
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    Thread(target=upstream.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    log = directory / "fallbacks.txt"
    env = {"PATH":os.environ["PATH"], "HOME":path, "DATA_DIR":path, "PORT":str(port),
           "HOSTNAME":"127.0.0.1", "NINEROUTER_MIGRATION_FIXTURE":"1",
           "NINEROUTER_MIGRATION_DATA_DIR":path, "MODEL_CATALOG_SYNC":"off",
           "RUST_ANYTHING_FALLBACK_LOG":str(log)}
    if mode == "standalone":
        commands = directory / "empty"
        commands.mkdir()
        env["PATH"] = str(commands)
    command = ["sh","rust/reference/server.sh"] if mode == "original" else [str(binary),"--no-browser"]
    process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        ready(port, process)
        path = "/dashboard/providers/" + provider
        status, _, headers = request(port, path)
        assert status == 307 and urlsplit(headers["location"]).path == "/login"
        status, _, headers = request(port, "/api/auth/login", "POST", {"password":"123456"})
        assert status == 200
        cookie = headers["set-cookie"].split(";", 1)[0]
        configure(directory / "db/data.sqlite", f"http://127.0.0.1:{upstream.server_port}")
        status, detail, headers = request(port, path, cookie=cookie)
        assert status == 200
        if mode != "original":
            assert headers["cache-control"] == "no-store" and secret not in detail
            assert 'Fixture &lt;Node&gt;' in detail and 'Fixture <Node>' not in detail
            assert all(marker in detail for marker in [
                'id="provider-detail"', f'data-provider="{provider}"', f'data-alias="{provider}"',
                f'href="/dashboard/providers/new?provider={provider}"', 'id="provider-detail-test-all"',
                'id="provider-detail-discovery"', 'id="provider-detail-models"', 'id="provider-detail-availability"',
                'id="provider-detail-routing-fields"',
                f'href="{path}?panel=legacy"', '/__9router-ui/provider-detail.js',
            ])
            status, builtin, _ = request(port, "/dashboard/providers/gemini-cli", cookie=cookie)
            assert status == 200 and '/__9router-ui/provider-detail.js' in builtin and '/_next/' not in builtin
            assert 'data-provider="gemini-cli"' in builtin and 'OAuth' in builtin
            status, embedding, _ = request(port, "/dashboard/providers/custom-embedding-detail", cookie=cookie)
            assert status == 200 and 'data-model-kind="embedding"' in embedding
            assert request(port, "/dashboard/providers/not-a-provider", cookie=cookie)[0] == 404
            status, script, headers = request(port, "/__9router-ui/provider-detail.js")
            assert status == 200 and headers["content-type"].startswith("text/javascript")
            for marker in ["/test-batch", "clearCooldown", "/availability", "modelApi + \"/test\""]:
                assert marker in script, marker
            providers_with_accounts = ("github","gitlab","grok-cli","xiaomi-mimo","zed","commandcode","kiro","cursor")
            for id_ in providers_with_accounts:
                code, setup, _ = request(port, "/dashboard/providers/" + id_, cookie=cookie)
                assert code == 200 and f'id="provider-account" data-provider="{id_}" data-local="true"' in setup, id_
                assert 'src="/__9router-ui/provider-account.js"' in setup, id_
            code, remote, _ = request(port, "/dashboard/providers/kiro", cookie=cookie,
                                       extra={"X-Forwarded-For":"192.0.2.1"})
            assert code == 200 and 'id="provider-account" data-provider="kiro" data-local="false"' in remote
            code, asset, headers = request(port, "/__9router-ui/provider-account.js")
            assert code == 200 and headers["content-type"].startswith("text/javascript")
            assert 'deviceAuthorization("github", "GitHub")' in asset and "window.open(" not in asset

        def api(route, method="GET", body=None, status=200):
            code, data, _ = request(port, route, method, body, cookie)
            assert code == status and secret not in json.dumps(data), (route, code, data)
            return data

        target = "/api/providers/" + connection_id
        accounts = [row["id"] for row in api("/api/providers")["connections"] if row["provider"] == provider]
        assert accounts == [connection_id,"detail-inactive"]
        locks = [row for row in api("/api/models/availability")["models"] if row["provider"] == provider]
        assert [(row["connectionId"],row["model"],row["status"]) for row in locks] == [
            (connection_id,"fixture-live","cooldown")]
        assert api("/api/models/availability","POST",{"action":"clearCooldown","provider":provider,
                                                       "model":"fixture-live"}) == {"ok":True}
        assert not [row for row in api("/api/models/availability")["models"] if row["provider"] == provider]
        discovered = api(target + "/models")["models"]
        assert discovered[0]["id"] == "fixture-live" and ledger == ["models"]
        batch = api("/api/providers/test-batch","POST",{"mode":"provider","providerId":provider})
        assert batch["summary"] == {"total":1,"passed":1,"failed":0} and batch["results"][0]["connectionId"] == connection_id
        assert ledger == ["models","models"]
        strategy = {provider:{"fallbackStrategy":"round-robin","stickyRoundRobinLimit":2}}
        assert api("/api/settings","PATCH",{"providerStrategies":strategy})["providerStrategies"] == strategy
        assert api("/api/settings")["providerStrategies"] == strategy
        custom = {"providerAlias":provider,"id":"fixture-live","name":"Fixture Live","type":"llm"}
        status, _, _ = request(port, "/api/models/custom", "POST", custom, cookie)
        assert status in {200,201}, status
        catalog = api("/api/models")["models"]
        assert any(model["fullModel"] == provider + "/fixture-live" for model in catalog)
        assert api("/api/models/disabled","POST",{"providerAlias":provider,"ids":["fixture-live"]})["success"]
        assert api("/api/models/disabled?providerAlias=" + provider)["ids"] == ["fixture-live"]
        api("/api/models/disabled?providerAlias=" + provider + "&id=fixture-live","DELETE")
        assert api("/api/models/disabled?providerAlias=" + provider)["ids"] == []
        probe = api("/api/models/test","POST",{"model":provider + "/fixture-live"})
        assert probe["ok"], probe
        assert ledger == ["models","models","chat"]
        embedding_probe = api("/api/models/test","POST",{
            "model":"custom-embedding-detail/fixture-embedding","kind":"embedding"})
        assert embedding_probe["ok"] and ledger == ["models","models","chat","embedding"], embedding_probe
        if mode != "original":
            assert not log.read_text().strip(), log.read_text()
            code, legacy, _ = request(port, path + "?panel=legacy", cookie=cookie)
            if mode == "standalone":
                assert code == 503 and legacy["error"] == "Legacy runtime unavailable for this route"
            else:
                assert code == 200 and "/_next/" in legacy
            assert log.read_text().splitlines() == ["GET " + path]
        print(json.dumps({"accounts":accounts,"test":batch["summary"],"outbound":ledger,
                          "availableAfterClear":True,"modelEnabled":True},sort_keys=True))
    finally:
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        upstream.shutdown()
        upstream.server_close()
