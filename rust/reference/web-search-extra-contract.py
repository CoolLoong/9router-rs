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
admin = {"x-9r-cli-token":"68977b6fd9422fbb"}
user = {"authorization":"Bearer offline-chat-key"}
private = "offline-web-youcom"
captured = []


class OtherOrigin(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        captured.append(self.headers.get("x-api-key"))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b"{}")


def post(port, path, body, headers):
    headers = {**headers, "content-type":"application/json"}
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request("POST", path, body=json.dumps(body).encode(), headers=headers)
        response = connection.getresponse()
        payload = json.loads(response.read())
        return response.status, payload
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-web-search-extra-contract-") as folder:
    storage = Path(folder)
    other = ThreadingHTTPServer(("127.0.0.1", 52982), OtherOrigin)
    threading.Thread(target=other.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallbacks = storage / "fallbacks.txt"
    env = {
        **os.environ, "PORT":str(port), "DATA_DIR":folder, "TMPDIR":folder,
        "NINEROUTER_MIGRATION_DATA_DIR":folder, "NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(fallbacks),
    }
    for key in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "HTTP_PROXY",
                "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]:
        env.pop(key, None)
    process = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("search origin fixture gateway did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        assert post(port, "/api/providers", {
            "provider":"youcom", "name":"Search origin fixture", "apiKey":private,
        }, admin)[0] == 201

        def search(query, **options):
            return post(port, "/v1/search", {"provider":"youcom","query":query, **options}, user)

        status, result = search("same-origin fixture")
        assert status == 200 and result["provider"] == "youcom" and result["results"] == []
        assert search("forbidden override", provider_options={
            "baseUrl":"http://127.0.0.1:52982/capture",
        }) == (400, {"error":{
            "message":"Search base URL must be configured on the provider connection", "code":400,
        }})
        status, result = search("credential fixture")
        assert status == 422 and "[redacted]" in result["error"]["message"]
        assert private not in json.dumps(result)
        status, result = search("redirect fixture")
        assert status == 302 and "returned 302" in result["error"]["message"]
        assert captured == []
        assert not fallbacks.exists() or not fallbacks.read_text().strip()
        print("Search credential origin, redirect scoping and redacted provider errors passed")
    finally:
        other.shutdown()
        other.server_close()
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
