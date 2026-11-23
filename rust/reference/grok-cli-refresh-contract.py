import http.client
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs

repo = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
models_path = "/grok-cli/v1/models"
discovery_path = "/xai-oauth/auth.x.ai/.well-known/openid-configuration"
private_path = "/xai-oauth/login.x.ai/oauth2/native-token?region=global"
default_path = "/xai-oauth/auth.x.ai/oauth2/token"
identity = "b1a00492-073a-47ea-816f-4c329264a828"
labels = (
    "rejected", "expired", "cas", "ambiguous", "bad-grant", "retain",
    "unsafe", "refused", "published", "proxy-issuer", "bad-expiry", "malformed-rotation",
)
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
events = []
cas_started = threading.Event()
cas_release = threading.Event()
discovery_mode = "trusted"


def old(label):
    return "offline-grok-old-" + label


def access(label):
    return "offline-grok-refreshed-" + label


def grant(label):
    return f"offline-refresh-{label}+opaque/%"


def rotated(label):
    return "offline-grok-rotation-" + label


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if self.path == discovery_path:
            assert not self.headers.get("authorization")
            events.append(("discovery", discovery_mode))
            issuer = "https://auth.x.ai"
            token = "https://login.x.ai/oauth2/native-token?region=global"
            if discovery_mode == "untrusted-host":
                token = "https://login.x.ai.fixtures.invalid/oauth2/native-token"
            elif discovery_mode == "wrong-issuer":
                issuer = "https://login.x.ai"
            elif discovery_mode == "unsafe-port":
                token = "https://login.x.ai:444/oauth2/native-token"
            return self.reply(200, {
                "issuer": issuer, "token_endpoint": token,
                "authorization_endpoint": "https://auth.x.ai/oauth2/authorize",
            })
        assert self.path == models_path
        bearer = self.headers.get("authorization", "").removeprefix("Bearer ")
        if bearer == "offline-new-cas-access":
            events.append(("models", "cas-operator"))
            return self.reply(200, {"models": ["fixture-grok-cas-operator"]})
        for label in labels:
            if bearer == old(label):
                events.append(("old-models", label))
                return self.reply(401, {"error":"model token rejected"})
            if bearer == access(label):
                events.append(("models", label))
                if label == "refused":
                    return self.reply(403, {"error":"rotated token rejected"})
                return self.reply(200, {"models":["fixture-grok-" + label]})
        raise AssertionError("unexpected model fixture authorization")

    def do_POST(self):
        assert self.path in (private_path, default_path)
        assert self.headers.get_content_type() == "application/x-www-form-urlencoded"
        assert self.headers.get("accept") == "application/json"
        assert all(not self.headers.get(key) for key in ("authorization", "cookie", "x-9r-cli-token", "x-grok-client-version"))
        form = parse_qs(self.rfile.read(int(self.headers["content-length"])).decode(), strict_parsing=True)
        matches = [label for label in labels if form.get("refresh_token") == [grant(label)]]
        assert len(matches) == 1 and form == {
            "grant_type": ["refresh_token"], "client_id": [identity], "refresh_token": [grant(matches[0])],
        }
        label, = matches
        events.append(("grant", label, self.path))
        if label == "ambiguous":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.close_connection = True
            return
        if label == "bad-grant":
            return self.reply(400, {"error":"invalid_grant","error_description":"echo " + grant(label)})
        if label == "cas":
            cas_started.set()
            assert cas_release.wait(5)
        if label == "expired":
            time.sleep(.12)
        payload = {"access_token":access(label), "expires_in":3600}
        if label != "retain":
            payload.update(refresh_token=rotated(label), id_token="offline-id-token-" + label)
        if label == "bad-expiry":
            payload["expires_in"] = -1
        if label == "malformed-rotation":
            payload["refresh_token"] = {"bogus":"invalid type"}
        return self.reply(200, payload)


def prepare(root):
    root.mkdir()
    (root / "home").mkdir()
    (root / "bin").mkdir()
    data = root / "data"
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)],
                   cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("DELETE FROM providerConnections")
        for index, label in enumerate(labels, 1):
            stored = {
                "accessToken": old(label), "refreshToken": grant(label),
                "expiresAt": "2099-01-01T00:00:00.000Z" if label == "rejected" else "2025-01-01T00:00:00.000Z",
                "providerSpecificData": {"authMethod":"device_code","note":"keep this metadata"},
            }
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                        VALUES(?,'grok-cli','oauth',?,?,1,?,'2026-11-01T00:00:00Z','2026-11-01T00:00:00Z')""",
                       (label, label, index, json.dumps(stored)))


def stored(root, label):
    with sqlite3.connect(root / "data/db/data.sqlite") as db:
        return json.loads(db.execute("SELECT data FROM providerConnections WHERE id=?", [label]).fetchone()[0])


def write(root, label, patch):
    with sqlite3.connect(root / "data/db/data.sqlite") as db:
        current = json.loads(db.execute("SELECT data FROM providerConnections WHERE id=?", [label]).fetchone()[0])
        db.execute("UPDATE providerConnections SET data=? WHERE id=?", [json.dumps(current | patch), label])


@contextmanager
def gateway(root, legacy=False):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    paths = root / "bin"
    if legacy:
        shell = paths / "sh"
        shell.write_text(f"#!{sys.executable}\n" + """import http.server, os
class Legacy(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type","application/json")
        self.end_headers()
        self.wfile.write(b'{"legacy_refresh_owner":true}')
http.server.HTTPServer(("127.0.0.1",int(os.environ["PORT"])),Legacy).serve_forever()
""")
        shell.chmod(0o700)
    env = {"HOME":str(root / "home"), "PATH":str(paths), "DATA_DIR":str(root / "data"),
           "PORT":str(port), "HOSTNAME":"127.0.0.1", "NINEROUTER_MIGRATION_FIXTURE":"1",
           "MODEL_CATALOG_SYNC":"off", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1",
           "RUST_ANYTHING_FALLBACK_LOG":str(root / "fallbacks")}
    command = [str(Path(args[0]).resolve()), "--no-browser"] if mode == "native" else [
        shutil.which("node"), "--import", str(repo / "rust/reference/mock-grok-cli-models.mjs"),
        str(repo / ".next/standalone/custom-server.js"),
    ]
    process = subprocess.Popen(command, env=env, cwd=repo, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), .2):
                    break
            except OSError:
                time.sleep(.025)
        else:
            raise RuntimeError("Grok OAuth fixture did not start")
        yield port
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def http_get(port, path, headers):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=25)
    try:
        connection.request("GET", path, headers=headers)
        reply = connection.getresponse()
        return reply.status, json.loads(reply.read())
    finally:
        connection.close()


def get(port, label, suffix="/models"):
    return http_get(port, f"/api/providers/{label}{suffix}", auth)


def model(port, label):
    expected = "fixture-grok-" + label
    actual = get(port, label)
    assert actual[0] == 200 and actual[1]["models"] == [{"id":expected,"name":expected}]
    return actual


def refreshed_state(root, label):
    state = stored(root, label)
    return all([
        state["accessToken"] == access(label), state["refreshToken"] == rotated(label),
        state["idToken"] == "offline-id-token-" + label, state["expiresIn"] == 3600,
        state["expiresAt"] > "2026-11-01T00:00:00Z",
        state["providerSpecificData"]["note"] == "keep this metadata",
    ])


def scenarios(root):
    global discovery_mode
    work = root / "main"
    prepare(work)
    with gateway(work) as port:
        observed = {
            "after-401": model(port, "rejected"),
            "reused-rotation": model(port, "rejected"),
            "stored-rotation": refreshed_state(work, "rejected"),
            "wire": events.copy(),
        }
        assert observed["stored-rotation"]
        assert observed["wire"] == [
            ("old-models", "rejected"), ("discovery", "trusted"),
            ("grant", "rejected", private_path), ("models", "rejected"), ("models", "rejected"),
        ]
        if mode == "native":
            with ThreadPoolExecutor(max_workers=6) as pool:
                outputs = list(pool.map(lambda _: model(port, "expired"), range(6)))
            assert len(outputs) == 6 and refreshed_state(work, "expired")
            assert events.count(("grant", "expired", private_path)) == 1
            assert ("old-models", "expired") not in events

            with ThreadPoolExecutor() as pool:
                pending = pool.submit(get, port, "cas")
                assert cas_started.wait(5)
                write(work, "cas", {"accessToken":"offline-new-cas-access",
                                   "refreshToken":"offline-new-cas-refresh", "expiresAt":"2099-01-01T00:00:00Z"})
                cas_release.set()
                status, body = pending.result(timeout=8)
            assert status == 200 and body["models"][0]["id"] == "fixture-grok-cas-operator"
            assert stored(work, "cas")["accessToken"] == "offline-new-cas-access"
            assert stored(work, "cas")["refreshToken"] == "offline-new-cas-refresh"

            for label in ("ambiguous", "bad-grant", "bad-expiry", "malformed-rotation"):
                first = get(port, label)
                second = get(port, label)
                assert first == second and first[0] == 200
                assert first[1]["warning"].endswith("The provider account must sign in again.")
                assert stored(work, label)["accessToken"] == old(label)
                assert stored(work, label)["refreshToken"] == grant(label)
                assert stored(work, label)["grokCliRefreshAttempt"]
                assert events.count(("grant", label, private_path)) == 1
                assert "grokCliRefreshAttempt" not in json.dumps(get(port, label, "")[1])
                assert grant(label) not in json.dumps(first)

            model(port, "retain")
            model(port, "retain")
            assert stored(work, "retain")["refreshToken"] == grant("retain")
            assert events.count(("grant", "retain", private_path)) == 1
            rejected, response = get(port, "refused")
            assert rejected == 200 and "(403)" in response["warning"]
            assert events.count(("grant", "refused", private_path)) == 1
            assert (work / "fallbacks").read_text() == ""

            with sqlite3.connect(work / "data/db/data.sqlite") as db:
                db.execute("UPDATE providerConnections SET isActive=(id='published')")
            status, catalog = http_get(port, "/v1/models", {"authorization":"Bearer offline-active-key"})
            assert status == 200 and "gcli/fixture-grok-published" in {row["id"] for row in catalog["data"]}
            assert events[-2:] == [("grant", "published", private_path), ("models", "published")]
            with sqlite3.connect(work / "data/db/data.sqlite") as db:
                db.execute("UPDATE providerConnections SET isActive=1")

            before = len(events)
            write(work, "proxy-issuer", {"providerSpecificData":{
                "connectionProxyEnabled":True,"connectionProxyUrl":"http://127.0.0.1:52989",
                "connectionNoProxy":"cli-chat-proxy.grok.com",
            }})
            status, response = get(port, "proxy-issuer")
            assert status == 200 and response["warning"].endswith("Provider credential refresh failed.")
            assert not stored(work, "proxy-issuer").get("grokCliRefreshAttempt")
            assert len(events) == before and (work / "fallbacks").read_text() == ""
    if mode == "native":
        before = len(events)
        with gateway(work) as port:
            assert get(port, "ambiguous")[1]["warning"].endswith("The provider account must sign in again.")
        assert len(events) == before

        for invalid in ("untrusted-host", "wrong-issuer", "unsafe-port"):
            discovery_mode = invalid
            alternate = root / invalid
            prepare(alternate)
            with gateway(alternate) as port:
                model(port, "unsafe")
            assert events[-3:] == [("discovery", invalid), ("grant", "unsafe", default_path), ("models", "unsafe")]

        coexist = root / "coexist"
        prepare(coexist)
        discovery_mode = "trusted"
        before = len(events)
        with gateway(coexist, legacy=True) as port:
            model(port, "expired")
            write(coexist, "proxy-issuer", {"providerSpecificData":{
                "connectionProxyEnabled": True, "connectionProxyUrl": "http://127.0.0.1:52989",
            }})
            status, response = get(port, "proxy-issuer")
            assert status == 200 and response["warning"].endswith("Provider credential refresh failed.")
        assert events[before:] == [("discovery", "trusted"), ("grant", "expired", private_path), ("models", "expired")]
        assert not (coexist / "fallbacks").read_text()
    return observed


with tempfile.TemporaryDirectory(prefix="9router-grok-oauth-") as directory:
    fixture = ThreadingHTTPServer(("127.0.0.1", 52989), Handler)
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    try:
        print(json.dumps(scenarios(Path(directory)), sort_keys=True))
    finally:
        fixture.shutdown()
        fixture.server_close()
