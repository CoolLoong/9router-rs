import http.client
import http.server
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

repository = Path(__file__).resolve().parents[2]
gateway_key = "offline-chat-key"
backend_key = "offline-ollama-embeddings"
admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
embedding = "ollama-local/nomic-embed-text:latest"
connection = "/api/providers/fixture-ollama-local"
configured_port = 52984
untrusted_port = 52985
captured = []


def send(response, status, value):
    raw = json.dumps(value, ensure_ascii=False).encode()
    response.send_response(status)
    response.send_header("Content-Type", "application/json")
    response.send_header("Content-Length", str(len(raw)))
    response.end_headers()
    response.wfile.write(raw)


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        port = self.server.server_port
        if port == untrusted_port:
            captured.append(self.path)
            return send(self, 418, {"error": {"message": "unexpected second origin"}})
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
        label = body.get("input")
        key = self.headers.get("authorization")
        fields = set(body)
        expected_path = "/v1/embeddings" if port == 11434 else "/ollama-fixture/v1/embeddings"
        if self.path != expected_path or "x-api-key" in self.headers or body.get("model") != "nomic-embed-text:latest":
            return send(self, 422, {"error": {"message": "unexpected Ollama embedding endpoint"}})
        if port == 11434:
            if key is not None or label != "fixture local no auth" or fields != {"model", "input", "encoding_format"}:
                return send(self, 422, {"error": {"message": "unexpected default Ollama embedding request"}})
            if body["encoding_format"] != "float":
                return send(self, 422, {"error": {"message": "expected default float encoding"}})
            data = [{"object": "embedding", "index": 0, "embedding": [0.25, -0.5, 0.75]}]
        else:
            if key != f"Bearer {backend_key}":
                return send(self, 422, {"error": {"message": "unexpected stored Ollama embedding credential"}})
            if label == "fixture cross origin":
                self.send_response(307)
                self.send_header("Location", f"http://127.0.0.1:{untrusted_port}/captured")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if label == "fixture credential echo":
                return send(self, 422, {"error": {"message": "received key " + backend_key}})
            if label == "fixture ollama rejection":
                return send(self, 422, {"error": {"message": "fixture Ollama embedding rejection"}})
            if label != ["fixture stored batch", "数组 🛰️"] or fields != {"model", "input", "encoding_format", "dimensions"}:
                return send(self, 422, {"error": {"message": "unexpected stored Ollama embedding payload"}})
            if body["encoding_format"] != "base64" or body["dimensions"] != 3:
                return send(self, 422, {"error": {"message": "unexpected Ollama embedding shape"}})
            data = [{"object": "embedding", "index": 0, "embedding": "AACAPw=="},
                    {"object": "embedding", "index": 1, "embedding": "AAAAQA=="}]
        send(self, 200, {"object": "list", "data": data, "model": "fixture-ollama-upstream",
                         "usage": {"prompt_tokens": 5, "total_tokens": 5}})

    do_GET = do_POST


class IPv6Server(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        super().server_bind()


def serve():
    servers = [http.server.ThreadingHTTPServer(("127.0.0.1", port), Upstream)
               for port in (11434, configured_port, untrusted_port)]
    try:
        servers.append(IPv6Server(("::1", 11434), Upstream))
    except OSError:
        pass
    for server in servers:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    return servers


def request(port, method, path, body, headers):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        headers = {**headers, "content-type": "application/json"}
        conn.request(method, path, body=json.dumps(body), headers=headers)
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


def contract(binary):
    with tempfile.TemporaryDirectory(prefix="9router-ollama-embeddings-") as folder:
        data = Path(folder)
        subprocess.run([sys.executable, str(repository / "rust/reference/seed.py"), folder], check=True)
        database = data / "db" / "data.sqlite"
        with sqlite3.connect(database) as db:
            db.execute(
                "UPDATE providerConnections SET data = json_set(data, '$.accessToken', ?) WHERE id = ?",
                ("offline-never-forward-local-access-token", "fixture-ollama-local"),
            )
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        fallback = data / "fallbacks"
        env = {**os.environ, "PORT":str(port), "DATA_DIR":folder, "HOME":folder,
               "NINEROUTER_MIGRATION_FIXTURE":"1", "RUST_ANYTHING_FALLBACK_LOG":str(fallback)}
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            env.pop(name, None)
        process = subprocess.Popen([binary, "--no-browser"], cwd=repository, env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 15
            while True:
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Ollama embedding fixture gateway did not start")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.2):
                        break
                except OSError:
                    time.sleep(.04)
            state = {"isActive":True, "apiKey":backend_key, "providerSpecificData":{
                "baseUrl":f"http://127.0.0.1:{configured_port}/ollama-fixture/v1",
            }}

            def embed(label):
                return request(port, "POST", "/v1/embeddings", {"model":embedding,"input":label},
                               {"authorization":f"Bearer {gateway_key}"})

            assert request(port, "PUT", connection, {"isActive": True}, admin)[0] == 200
            status, result = embed("fixture local no auth")
            assert status == 200 and result["data"][0]["embedding"] == [0.25, -0.5, 0.75]
            deadline = time.monotonic() + 4
            while True:
                with sqlite3.connect(database) as db:
                    usage = db.execute(
                        "SELECT connectionId, endpoint, promptTokens, completionTokens, "
                        "json_extract(tokens, '$.total_tokens') "
                        "FROM usageHistory WHERE provider = 'ollama-local'"
                    ).fetchall()
                if usage or time.monotonic() >= deadline:
                    break
                time.sleep(.05)
            assert usage == [("fixture-ollama-local", "/v1/embeddings", 5, 0, 5)], usage

            assert request(port, "PUT", connection, state, admin)[0] == 200
            status, result = embed("fixture credential echo")
            assert status == 422
            assert "[redacted]" in result["error"]["message"] and backend_key not in json.dumps(result)
            status, _ = embed("fixture cross origin")
            assert status == 307 and not captured
            assert not fallback.exists() or not fallback.read_text().strip()
            print("Ollama embedding usage, optional authentication, redaction and cross-origin isolation passed")
        finally:
            process.terminate()
            try:
                process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


servers = serve()
try:
    if sys.argv[1] == "serve":
        Path(sys.argv[2]).touch()
        threading.Event().wait()
    else:
        contract(str(Path(sys.argv[1]).resolve()))
finally:
    for server in servers:
        server.shutdown()
        server.server_close()
