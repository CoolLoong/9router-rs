import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve(strict=True)
key = "offline-embeddings-proxy-secret"
body = {"model": "fixture-live/fixture-embedding", "input": ["test", "samples"], "dimensions": 2}
expected_request = {"model": "fixture-embedding", "input": ["test", "samples"], "dimensions": 2, "encoding_format": "float"}
answer = {"object":"list", "data":[{"object":"embedding","index":0,"embedding":[0.25,0.75]}], "model":"fixture-embedding"}
received = []


class ProviderFixture(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        received.append((self.path, self.headers.get("Authorization"), self.headers.get("x-relay-target"),
                         self.headers.get("x-relay-path"), self.headers.get("x-forward-probe"), payload))
        rejected = self.server.reject
        output = json.dumps({"error":{"message":"credential " + key}} if rejected else answer).encode()
        self.send_response(403 if rejected else 200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(output)))
        self.end_headers()
        self.wfile.write(output)


def call(port, path, method="GET", payload=None, headers=None):
    headers = {"x-9r-cli-token":"68977b6fd9422fbb"} if headers is None else headers
    if payload is not None:
        headers = {**headers, "Content-Type":"application/json"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=8)) as connection:
        connection.request(method, path, None if payload is None else json.dumps(payload), headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())


def pool(port, address, kind, strict=False):
    status, result = call(port, "/api/proxy-pools", "POST", {
        "name":kind, "type":kind, "strictProxy":strict, "proxyUrl":address,
    })
    assert status == 201, result
    return result["proxyPool"]["id"]


with tempfile.TemporaryDirectory(prefix="9router-embeddings-proxy-") as directory, \
     ThreadingHTTPServer(("127.0.0.1",0), ProviderFixture) as direct, \
     ThreadingHTTPServer(("127.0.0.1",0), ProviderFixture) as proxy:
    direct.reject = proxy.reject = False
    for listener in [direct, proxy]:
        Thread(target=listener.serve_forever, daemon=True).start()
    with socket.socket() as address, socket.socket() as refused:
        address.bind(("127.0.0.1",0))
        refused.bind(("127.0.0.1",0))
        port = address.getsockname()[1]
        refusal_port = refused.getsockname()[1]

    work = Path(directory)
    data = work / "data"
    empty = work / "empty-path"
    empty.mkdir()
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], cwd=root, check=True, stdout=subprocess.DEVNULL)
    proc = subprocess.Popen([binary,"--no-browser"], cwd=work, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={
        "PATH":str(empty), "HOME":str(work), "DATA_DIR":str(data), "PORT":str(port),
        "MODEL_CATALOG_SYNC":"off", "NINEROUTER_MIGRATION_FIXTURE":"1", "RUST_ANYTHING_FALLBACK_LOG":str(work/"fallbacks"),
    })
    try:
        origin = f"http://127.0.0.1:{direct.server_port}"
        proxy_url = f"http://127.0.0.1:{proxy.server_port}"
        deadline = time.monotonic() + 12
        while True:
            assert proc.poll() is None and time.monotonic() < deadline
            try:
                if call(port, "/api/health")[0] == 200:
                    break
            except OSError:
                time.sleep(.04)

        def configure(pool_id, strict=False):
            with sqlite3.connect(data / "db/data.sqlite") as db:
                db.execute("""UPDATE providerConnections SET isActive=1, data=?
                              WHERE id='fixture-live-connection'""", (json.dumps({
                    "apiKey":key, "providerSpecificData":{"baseUrl":origin + "/v1", "proxyPoolId":pool_id, "strictProxy":strict},
                }),))

        def embed():
            return call(port,"/v1/embeddings","POST",body,{
                "Authorization":"Bearer offline-active-key", "x-forward-probe":"must not forward",
            })

        def expect(path, relay_target=None, relay_path=None):
            assert embed() == (200,answer)
            assert received.pop() == (path,"Bearer " + key,relay_target,relay_path,None,expected_request)
            assert not received

        configure("__none__")
        expect("/v1/embeddings")

        assert call(port,"/api/settings","PATCH",{"outboundProxyEnabled":True,"outboundProxyUrl":proxy_url})[0] == 200
        expect(origin + "/v1/embeddings")
        assert call(port,"/api/settings","PATCH",{"outboundNoProxy":"127.0.0.1"})[0] == 200
        expect("/v1/embeddings")
        assert call(port,"/api/settings","PATCH",{"outboundProxyEnabled":False,"outboundNoProxy":""})[0] == 200

        configure(pool(port,proxy_url,"http",True))
        expect(origin + "/v1/embeddings")
        configure(pool(port,proxy_url + "/relay","vercel"))
        expect("/relay", origin, "/v1/embeddings")

        configure(pool(port, f"http://127.0.0.1:{refusal_port}","http",True))
        assert embed()[0] == 502 and not received
        configure("__none__", True)
        assert embed()[0] == 502 and not received
        configure("__none__")
        direct.reject = True
        code, problem = embed()
        assert code == 403 and "[redacted]" in json.dumps(problem) and key not in json.dumps(problem)
        assert not (work/"fallbacks").exists() or not (work/"fallbacks").read_text().strip()
        print("Embeddings proxy, bypass, relay, strict failure and credential redaction passed without Node")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        direct.shutdown()
        proxy.shutdown()
