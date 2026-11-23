from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]


def free():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait(proc, port):
    deadline = time.monotonic() + 15
    while proc.poll() is None and time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=.2):
                return
        except OSError:
            time.sleep(.04)
    raise RuntimeError("fixture server did not become ready")


def get_frame(response, until):
    for _ in range(20):
        line = response.readline()
        if line.startswith(b"data: ") and until(frame := json.loads(line[6:])):
            return frame
    raise AssertionError("expected usage event not seen")


with tempfile.TemporaryDirectory(prefix="9router-activity-contract-") as work:
    port = free()
    env = {**os.environ, "PORT": str(port), "DATA_DIR": work, "NINEROUTER_MIGRATION_DATA_DIR": work,
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(Path(work) / "fallback.log")}
    admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
    with subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as proc:
        feed = HTTPConnection("127.0.0.1", port, timeout=8)
        try:
            wait(proc, port)
            admin_client = HTTPConnection("127.0.0.1", port, timeout=8)
            admin_client.request("PUT", "/api/providers/fixture-live-connection",
                                 body=b'{"isActive":true}', headers={**admin, "content-type":"application/json"})
            response = admin_client.getresponse()
            assert response.status == 200, response.status
            response.read()
            admin_client.close()
            feed.request("GET", "/api/usage/stream", headers=admin)
            stream = feed.getresponse()
            assert stream.status == 200 and stream.headers["content-type"] == "text/event-stream", stream.status
            initial = get_frame(stream, lambda frame: not frame["activeRequests"])
            assert initial["totalRequests"] == 0

            def generate():
                client = HTTPConnection("127.0.0.1", port, timeout=8)
                data = json.dumps({"model":"fixture-live/fixture-observe","stream":True,
                                   "messages":[{"role":"user","content":"usage replay"}]}).encode()
                client.request("POST", "/v1/chat/completions", data,
                               headers={"authorization":"Bearer offline-chat-key","content-type":"application/json"})
                response = client.getresponse()
                assert response.status == 200
                raw = response.read()
                client.close()
                return raw

            with ThreadPoolExecutor(max_workers=1) as pool:
                generated = pool.submit(generate)
                pending = get_frame(stream, lambda frame: len(frame["activeRequests"]) == 1)
                assert pending["activeRequests"][0] == {
                    "model":"fixture-observe", "provider":"openai-compatible-chat-fixture-live",
                    "account":"Fixture live discovery", "count":1,
                }
                assert b"rust " in generated.result()
                finished = get_frame(stream,
                                     lambda frame: frame["totalRequests"] == 1 and not frame["activeRequests"])
                assert finished["recentRequests"][0]["model"] == "fixture-observe"
                assert finished["recentRequests"][0]["promptTokens"] == 4
                assert finished["recentRequests"][0]["completionTokens"] == 2
                if server != "original":
                    assert "/api/usage/stream" not in Path(work,"fallback.log").read_text()
                print(f"{server}: usage stream pending → completed, persisted request and live metadata passed")
        finally:
            feed.close()
            proc.terminate()
