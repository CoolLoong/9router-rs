import base64
import hashlib
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time


OLD, NEW = [Path(path).resolve() for path in sys.argv[1:]]
SECRET = "synthetic-update-dashboard-secret"
TARGET = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
VERSION_URL = "/api/version"
UPDATE_URL = VERSION_URL + "/update"
MANIFEST = b""
ARTIFACT = NEW
EVENTS = []


def b64(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def digest(path):
    with path.open("rb") as artifact:
        return hashlib.file_digest(artifact, "sha256").hexdigest()


def version(path):
    return subprocess.check_output([path, "--version"], text=True).strip()


CURRENT, REPLACEMENT = version(OLD), version(NEW)


def session():
    header = b64(b'{"alg":"HS256"}')
    claims = b64(json.dumps({"authenticated": True, "sub": "synthetic-updater", "iat": int(time.time()),
                            "exp": int(time.time()) + 600}).encode())
    content = f"{header}.{claims}"
    proof = b64(hmac.new(SECRET.encode(), content.encode(), hashlib.sha256).digest())
    return f"auth_token={content}.{proof}"


def release(directory, binary=NEW, declared=REPLACEMENT, tamper=False, **overrides):
    global MANIFEST, ARTIFACT
    ARTIFACT = binary
    raw = json.dumps({
        "schema": "9router-native-update/v1", "channel": "preview", "version": declared,
        "target": TARGET, "artifact": "/payload", "expiresAt": int(time.time()) + 600,
        "size": binary.stat().st_size, "sha256": digest(binary), **overrides,
    }, separators=(",", ":")).encode()
    payload = directory / "payload.json"
    payload.write_bytes(raw)
    signature = subprocess.check_output(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey",
                                         directory / "fixture-key.pem", "-in", payload])
    if tamper:
        raw += b" "
    MANIFEST = json.dumps({"payload": b64(raw), "signature": b64(signature)}).encode()


class Feed(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        EVENTS.append(self.path)
        if self.path == "/manifest":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(MANIFEST)))
            self.end_headers()
            self.wfile.write(MANIFEST)
        elif self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/payload")
            self.end_headers()
        elif self.path == "/payload":
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(ARTIFACT.stat().st_size))
            self.end_headers()
            with ARTIFACT.open("rb") as artifact:
                shutil.copyfileobj(artifact, self.wfile, 1024 * 1024)
        else:
            self.send_error(404)


def request(port, path, method="GET", *, authorized=True, origin=None):
    headers = {"Cookie": session()} if authorized else {}
    if origin is not None:
        headers["Origin"] = origin
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        conn.request(method, path, headers=headers)
        response = conn.getresponse()
        raw = response.read()
        return response.status, json.loads(raw)
    finally:
        conn.close()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def ready(data, port, ver, different_from=None):
    deadline = time.monotonic() + 19
    record = data / "launcher.pid"
    while time.monotonic() < deadline:
        try:
            identity = json.loads(record.read_text())
            if identity["cliPid"] != different_from and identity["native"]["packageVersion"] == ver:
                if request(port, "/api/health") == (200, {"ok": True}):
                    return identity
        except OSError:
            pass
        time.sleep(.04)
    raise AssertionError(f"Release {ver} did not become healthy on its owned listener")


def protect_live_launcher(installation, env, original, port):
    challenger_env = env | {"NINEROUTER_NATIVE_UPDATE_CHILD": "1"}
    rejected = subprocess.run([installation, "--no-browser", "--port", str(free_port())],
                              env=challenger_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=6)
    assert rejected.returncode != 0 and b"independent launcher" in rejected.stderr
    assert original.poll() is None and request(port, "/api/health")[0] == 200


def cases(directory, installation, data, env, port, original, kind):
    previous = ready(data, port, CURRENT)
    old_pid = previous["cliPid"]
    assert old_pid == original.pid
    before = digest(installation)
    database = data / "db/data.sqlite"
    with sqlite3.connect(database) as sql:
        sql.execute("""INSERT INTO settings (id,data) VALUES (1,'{"updateFixture":"kept"}')
            ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.updateFixture','kept')""")
    if kind == "unconfigured":
        assert request(port, VERSION_URL) == (200, {"currentVersion": CURRENT,
                                                   "latestVersion": None, "hasUpdate": False})
        assert request(port, UPDATE_URL, "POST")[0] == 503 and not EVENTS
        assert digest(installation) == before and original.poll() is None
        return

    if kind == "upgrade":
        protect_live_launcher(installation, env, original, port)
        assert request(port, UPDATE_URL, "POST", authorized=False)[0] == 401
        assert request(port, UPDATE_URL, "POST", origin="https://untrusted.invalid")[0] == 403
        assert not EVENTS
        release(directory)
        mode = installation.parent.stat().st_mode & 0o777
        try:
            installation.parent.chmod(0o777)
            assert request(port, UPDATE_URL, "POST")[0] == 409
            assert EVENTS == ["/manifest"] and digest(installation) == before
        finally:
            installation.parent.chmod(mode)
        for status, values in [
            (502, {"tamper": True}), (502, {"target": "wrong-linux-target"}),
            (502, {"channel": "stable"}), (502, {"expiresAt": 1}),
            (502, {"artifact": "http://127.0.0.2/payload"}), (409, {"declared": CURRENT}),
        ]:
            release(directory, **values)
            EVENTS.clear()
            assert request(port, UPDATE_URL, "POST")[0] == status
            assert EVENTS == ["/manifest"] and digest(installation) == before and original.poll() is None
        release(directory, binary=Path("/usr/bin/false"), sha256="0" * 64)
        EVENTS.clear()
        assert request(port, UPDATE_URL, "POST")[0] == 502
        assert EVENTS == ["/manifest", "/payload"]
        release(directory, binary=Path("/usr/bin/false"), size=10)
        EVENTS.clear()
        assert request(port, UPDATE_URL, "POST")[0] == 502
        assert EVENTS == ["/manifest", "/payload"]
        release(directory, binary=Path("/usr/bin/false"), artifact="/redirect")
        EVENTS.clear()
        assert request(port, UPDATE_URL, "POST")[0] == 502
        assert EVENTS == ["/manifest", "/redirect"] and original.poll() is None
        assert digest(installation) == before and not list(installation.parent.glob(".9router-update-*"))
        release(directory)
        assert request(port, VERSION_URL) == (200, {"currentVersion": CURRENT,
                                                   "latestVersion": REPLACEMENT, "hasUpdate": True})
        expected, installed_digest = REPLACEMENT, digest(NEW)
    elif kind == "rollback":
        release(directory, binary=Path("/usr/bin/false"))
        expected, installed_digest = CURRENT, before
    elif kind == "interrupted":
        release(directory, declared="999.0.0")
        expected, installed_digest = REPLACEMENT, digest(NEW)
    else:
        release(directory, binary=OLD)
        expected, installed_digest = CURRENT, before

    EVENTS.clear()
    status, accepted = request(port, UPDATE_URL, "POST", origin=f"http://127.0.0.1:{port}")
    assert status == 200 and accepted["success"] is True, accepted
    assert EVENTS == ["/manifest", "/payload"]
    if kind == "interrupted":
        ready(data, port, expected, different_from=old_pid)
        assert digest(installation) == installed_digest
        (recovery,) = installation.parent.glob(".9router-update-*")
        assert digest(recovery / "previous") == before
        original.kill()
        assert original.wait(timeout=4) < 0
        assert request(port, VERSION_URL + "/shutdown", "POST")[0] == 200
        deadline = time.monotonic() + 8
        while (data / "launcher.pid").exists() and time.monotonic() < deadline:
            time.sleep(.05)
        assert not (data / "launcher.pid").exists()
        os.replace(recovery / "previous", installation)
        assert digest(installation) == before and version(installation) == CURRENT
        return
    failed = ready(data, port, CURRENT, different_from=old_pid)["cliPid"] if kind == "wrong-running-version" else None
    assert original.wait(timeout=24) == 0
    current = ready(data, port, expected, different_from=failed or old_pid)
    assert current["cliPid"] != old_pid and current["native"]["listener"] != previous["native"]["listener"]
    assert digest(installation) == installed_digest and version(installation) == expected
    assert not list(installation.parent.glob(".9router-update-*"))
    with sqlite3.connect(database) as sql:
        (sentinel,), = sql.execute("SELECT json_extract(data,'$.updateFixture') FROM settings WHERE id=1")
        assert sentinel == "kept"
    if kind == "upgrade":
        assert request(port, VERSION_URL) == (200, {"currentVersion": REPLACEMENT,
                                                   "latestVersion": REPLACEMENT, "hasUpdate": False})
        assert request(port, UPDATE_URL, "POST")[0] == 409


def exercise(workspace, feed, public_key, kind):
    directory = workspace / kind
    directory.mkdir()
    home, data, empty = [directory / name for name in ("home", "data", "empty-path")]
    for path in (home, data, empty):
        path.mkdir()
    installation = directory / "9router"
    shutil.copyfile(OLD, installation)
    installation.chmod(0o755)
    env = {"PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data), "JWT_SECRET": SECRET,
           "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off"}
    if kind != "unconfigured":
        env.update({"NINEROUTER_NATIVE_UPDATE_MANIFEST": f"http://127.0.0.1:{feed.server_port}/manifest",
                    "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY": public_key})
    port = free_port()
    EVENTS.clear()
    with (directory / "launcher.log").open("w+") as log:
        original = subprocess.Popen([installation, "--host", "127.0.0.1", "--port", str(port), "--no-browser", "--skip-update"], cwd=directory,
                                    env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            cases(workspace, installation, data, env, port, original, kind)
            print(f"{kind}: passed")
        except Exception:
            log.seek(0)
            print(log.read(), file=sys.stderr, end="")
            raise
        finally:
            try:
                request(port, VERSION_URL + "/shutdown", "POST")
            except OSError:
                if original.poll() is None:
                    original.terminate()
            original.wait(timeout=6)


with tempfile.TemporaryDirectory(prefix="9router-update-fixture-") as temporary:
    workspace = Path(temporary)
    key = workspace / "fixture-key.pem"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", key], check=True)
    certificate = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
    assert len(certificate) == 44
    feed = ThreadingHTTPServer(("127.0.0.1", 0), Feed)
    threading.Thread(target=feed.serve_forever, daemon=True).start()
    try:
        for kind in ("unconfigured", "upgrade", "rollback", "wrong-running-version", "interrupted"):
            exercise(workspace, feed, b64(certificate[-32:]), kind)
    finally:
        feed.shutdown()
        feed.server_close()
