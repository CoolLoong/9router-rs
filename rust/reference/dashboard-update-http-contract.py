import base64
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
from threading import Thread
import time

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
current = json.loads((root / "cli/package.json").read_text())["version"]
next_version = "90.0.1"
asset = "/__9router-ui/updates.js"


class Feed(BaseHTTPRequestHandler):
    payload = b""
    requests = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.requests.append(self.path)
        assert self.path == "/manifest", self.path
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(self.payload)))
        self.end_headers()
        self.wfile.write(self.payload)


def b64(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def signer(workspace):
    secret = workspace / "ephemeral-fixture-key.pem"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", secret], check=True)
    public = subprocess.check_output(["openssl", "pkey", "-in", secret, "-pubout", "-outform", "DER"])
    target = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
    payload = workspace / "manifest.json"
    payload.write_text(json.dumps({
        "schema": "9router-native-update/v1", "channel": "preview", "target": target,
        "version": next_version, "expiresAt": int(time.time()) + 300,
        "artifact": "/updater-fixture.exe", "size": 1, "sha256": "01" * 32,
    }))
    signature = subprocess.check_output(
        ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", secret, "-in", payload]
    )
    Feed.payload = json.dumps({"payload": b64(payload.read_bytes()), "signature": b64(signature)}).encode()
    return b64(public[-32:])


def request(port, path, method="GET", cookie=None, payload=None):
    headers = {"Content-Type": "application/json"}
    if cookie is not None:
        headers["Cookie"] = cookie
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=6)
    try:
        client.request(method, path, body=json.dumps(payload) if payload is not None else None, headers=headers)
        reply = client.getresponse()
        return reply.status, reply.read().decode(), reply.headers
    finally:
        client.close()


def check(port, workspace):
    assert request(port, "/dashboard/endpoint")[0] == 307
    code, _, headers = request(port, "/api/auth/login", "POST", payload={"password": "123456"})
    assert code == 200
    cookie = headers["Set-Cookie"].split(";", 1)[0]
    for path in ("/dashboard", "/dashboard/endpoint"):
        code, body, headers = request(port, path, cookie=cookie)
        assert code == 200 and headers["Cache-Control"] == "no-store"
        assert "script-src 'self'" in headers["Content-Security-Policy"]
        assert 'href="/dashboard/endpoint#native-update"' in body
        assert 'id="native-update" data-can-install="true" data-signed-in="true"' in body
        assert 'id="native-update-install" hidden' in body and f'src="{asset}"' in body
    assert not Feed.requests
    code, recorded, _ = request(port, "/api/version", cookie=cookie)
    assert code == 200 and json.loads(recorded) == {
        "currentVersion": current, "latestVersion": next_version, "hasUpdate": True,
    }
    code, script, headers = request(port, asset, cookie=cookie)
    assert code == 200 and headers["Content-Type"].startswith("text/javascript")
    javascript = workspace / "served-updates.js"
    javascript.write_text(script)
    subprocess.run(["node", str(root / "rust/reference/dashboard-update-contract.mjs"), str(javascript)], check=True)

    assert request(port, "/api/settings", "PATCH", cookie, {"requireLogin": False})[0] == 200
    code, body, _ = request(port, "/dashboard")
    assert code == 200 and 'data-can-install="true" data-signed-in="false"' in body
    assert 'href="/login?sign-in=1" id="native-update-sign-in"' in body
    assert 'data-ready="true"' in request(port, "/login")[1]
    assert 'data-ready="false"' in request(port, "/login?sign-in=1")[1]
    assert 'data-ready="true"' in request(port, "/login?sign-in=1", cookie=cookie)[1]
    assert Feed.requests == ["/manifest"]
    assert request(port, "/api/version/shutdown", "POST", cookie=cookie)[0] == 200
    print("Native update HTML, scoped sign-in, served asset and signed discovery passed.")


with tempfile.TemporaryDirectory(prefix="9router-dashboard-update-") as directory:
    workspace = Path(directory)
    data = workspace / "data"
    home = workspace / "home"
    empty = workspace / "empty-path"
    for path in (data, home, empty):
        path.mkdir()
    key = signer(workspace)
    feed = ThreadingHTTPServer(("127.0.0.1", 0), Feed)
    Thread(target=feed.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off",
        "NINEROUTER_NATIVE_UPDATE_MANIFEST": f"http://127.0.0.1:{feed.server_port}/manifest",
        "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY": key,
    }
    log = workspace / "native.log"
    with log.open("w") as stream:
        server = subprocess.Popen(
            [str(binary), "--port", str(port), "--no-browser", "--skip-update"],
            cwd=workspace, env=env, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream,
        )
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline and server.poll() is None:
                try:
                    if request(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.04)
            else:
                raise RuntimeError("fixture server did not start")
            check(port, workspace)
            assert server.wait(timeout=10) == 0
        except Exception:
            print(log.read_text(errors="replace"), file=sys.stderr)
            raise
        finally:
            if server.poll() is None:
                server.terminate()
                server.wait(timeout=5)
            feed.shutdown()
            feed.server_close()
