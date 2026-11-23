import base64
from contextlib import ExitStack, closing, contextmanager
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
import threading
import time
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[2]
REFERENCE = sys.argv[1] == "reference"
BINARY = None if REFERENCE else str(Path(sys.argv[2]).resolve())
CASE = sys.argv[-1]
API = "/v1/images/generations"
HEADERS = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}
ASSET = "http://93.184.216.34:52991/sample.png?signature=offline-label"
PNG64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII="
PNG = base64.b64decode(PNG64)
PRIMARY = "offline-BFL-primary"
SECONDARY = "offline-BFL-secondary"
KEYS = {"fixture-bfl-primary": PRIMARY, "fixture-bfl-secondary": SECONDARY}
SUBMITS, POLLS, ASSETS, CAPTURES, ERRORS = [], [], [], [], []
JOBS = {}
UNSAFE = {
    "http": "http://api.bfl.ai/v1/get_result?id=unsafe-http",
    "foreign": "https://api.bfl.ai.invalid/v1/get_result?id=unsafe-foreign",
    "local": "http://127.0.0.1:52989/capture",
    "authority": "https://unexpected.bfl.ai/v1/get_result?id=unsafe-authority",
    "path": "https://api.eu.bfl.ai/v1/credits?id=unsafe-path",
    "fragment": "https://api.bfl.ai/v1/get_result?id=unsafe-fragment#token",
    "userinfo": "https://test@api.bfl.ai/v1/get_result?id=unsafe-userinfo",
    "port": "https://api.bfl.ai:8080/v1/get_result?id=unsafe-port",
    "job": "https://api.bfl.ai/v1/get_result?id=changed-job",
    "query": "https://api.bfl.ai/v1/get_result?id=unsafe-query&id=twice",
}
HOSTS = {"pro": "api.eu.bfl.ai", "dev": "api.us.bfl.ai", "empty": "api.us1.bfl.ai",
         "ultra": "api.bfl.ml", "binary": "api.us2.bfl.ai"}


class Server(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, payload, headers=None):
        raw = json.dumps(payload).encode() if isinstance(payload, (dict, list)) else payload
        self.send_response(status)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Content-Type", "application/json" if isinstance(payload, (dict, list)) else "image/png")
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        try:
            if self.path == "/capture":
                CAPTURES.append(dict(self.headers))
                return self.reply(200, {})
            target = urlsplit(self.path)
            assert target.path.startswith("/bfl/api.bfl.ai/v1/") and not target.query
            model = target.path.rsplit("/", 1)[-1]
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompt = data["prompt"]
            key, = self.headers.get_all("x-key") or []
            assert key in KEYS.values() and self.headers["Authorization"] is None
            assert self.headers["Content-Type"] == "application/json"
            expected = {
                "pro": ("flux-pro-1.1", {"width": 1152, "height": 768}),
                "dev": ("flux-dev", {"image_prompt": PNG64}),
                "binary": ("flux-pro", {}),
                "empty": ("flux-pro-1.1", {}),
                "ultra": ("flux-pro-1.1-ultra", {"width": 1536, "height": 1024, "image_prompt": PNG64}
                          if REFERENCE else {"aspect_ratio": "3:2", "image_prompt": PNG64}),
                "edit": ("flux-kontext-pro", {"width": 1024, "height": 1536, "image_prompt": PNG64}
                         if REFERENCE else {"aspect_ratio": "2:3", "input_image": PNG64}),
                "edit-max": ("flux-kontext-max", {"image_prompt": PNG64} if REFERENCE else {"input_image": PNG64}),
            }.get(prompt, ("flux-pro-1.1", {}))
            assert (model, data) == (expected[0], {"prompt": prompt, **expected[1]}), (model, data)
            SUBMITS.append((prompt, model, key))
            if prompt == "submit-redirect":
                return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/capture"})
            if prompt == "submit-disconnect":
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            if prompt == "submit-ambiguous":
                return self.reply(503, {"error": {"message": "temporary " + key}})
            if prompt == "submit-denied" and key == PRIMARY:
                return self.reply(401, {"error": {"message": "wrong " + key}})
            if prompt == "missing":
                return self.reply(200, {"id": prompt})
            if prompt == "bad-json":
                return self.reply(200, b"not-json")
            if prompt.startswith("unsafe-"):
                return self.reply(200, {"id": prompt, "polling_url": UNSAFE[prompt.removeprefix("unsafe-")]})
            host = HOSTS.get(prompt, "api.bfl.ai")
            JOBS[prompt] = (host, key, 0)
            return self.reply(202, {"id": prompt, "polling_url": f"https://{host}/v1/get_result?id={prompt}"})
        except (AssertionError, ValueError, KeyError) as error:
            ERRORS.append((self.path, repr(error)))
            self.reply(422, {})

    def do_GET(self):
        try:
            if self.path == "/capture":
                CAPTURES.append(dict(self.headers))
                return self.reply(200, {})
            if self.server.server_port == 52991:
                assert self.path == urlsplit(ASSET).path + "?" + urlsplit(ASSET).query
                assert self.headers["Authorization"] is None and self.headers["x-key"] is None
                assert self.headers["Cookie"] is None
                ASSETS.append(self.path)
                return self.reply(200, PNG)

            target = urlsplit(self.path)
            prompt, = parse_qs(target.query)["id"]
            host, key, count = JOBS[prompt]
            assert target.path == f"/bfl/{host}/v1/get_result"
            assert self.headers.get_all("x-key") == [key] and self.headers["Authorization"] is None
            assert self.headers["Accept"] == "application/json"
            JOBS[prompt] = (host, key, count + 1)
            POLLS.append((prompt, host))
            if prompt == "poll-redirect":
                return self.reply(307, {}, {"Location": "http://127.0.0.1:52989/capture"})
            if prompt == "poll-http":
                return self.reply(401, {"error": key})
            if prompt in ("poll-failed", "combo-failed"):
                return self.reply(200, {"status": "Failed", "error": "fixture fail " + key})
            if prompt == "moderated":
                return self.reply(200, {"status": "Content Moderated"})
            if prompt == "slow":
                time.sleep(1.5)
                try:
                    self.reply(200, {"status": "Pending"})
                except OSError:
                    pass
                return
            if prompt == "pending":
                return self.reply(200, {"status": "Reasoning"})
            if count == 0 and prompt == "pro":
                return self.reply(200, {"status": "Pending"})
            sample = ASSET
            if prompt == "secret-sample":
                sample = ASSET + "&token=" + key.replace("-", "%2D")
            if prompt == "local-sample":
                sample = "http://127.0.0.1:52989/capture"
            return self.reply(200, {"status": "Ready", "result": {} if prompt == "empty" else {"sample": sample}})
        except (AssertionError, ValueError, KeyError) as error:
            ERRORS.append((self.path, repr(error)))
            self.reply(422, {})


def request(port, body=None, path=API, headers=HEADERS):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=8)) as client:
        client.request("GET" if body is None else "POST", path,
                       body=None if body is None else json.dumps(body), headers=headers)
        response = client.getresponse()
        return response.status, response.getheader("Content-Type", "").split(";", 1)[0], response.read()


def seed(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
                   cwd=home, env={"PATH": os.environ["PATH"], "HOME": str(home)},
                   check=True, stderr=subprocess.DEVNULL)
    path = data / "db/data.sqlite"
    with sqlite3.connect(path) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
        sql.executemany("""INSERT INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'black-forest-labs','apikey','Synthetic BFL',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                        [(name, priority, json.dumps({"apiKey": key})) for priority, (name, key) in enumerate(KEYS.items(), 1)])
        sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES ('fixture-bfl-combo','fixture-bfl-combo','image',?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                    (json.dumps(["bfl/flux-pro-1.1", "bfl/flux-dev"]),))
    return path


def reset(path):
    with sqlite3.connect(path) as sql:
        sql.executemany("UPDATE providerConnections SET data=? WHERE id=?",
                        [(json.dumps({"apiKey": key}), name) for name, key in KEYS.items()])


@contextmanager
def serving(server):
    threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .05}, daemon=True).start()
    try:
        yield
    finally:
        server.shutdown()
        server.server_close()


def outputs(port):
    def image(model, prompt, **options):
        return request(port, {"model": model, "prompt": prompt, **options})

    snapshots = {}
    for model, prompt, params in (
        ("bfl/flux-pro-1.1", "pro", {"size": "1152x768", "n": 2, "quality": "hd"}),
        ("black-forest-labs/flux-dev", "dev", {"image": PNG64}),
        ("bfl/flux-pro-1.1", "empty", {}),
        ("bfl/flux-pro-1.1-ultra", "ultra", {"size": "1536x1024", "image": PNG64}),
        ("bfl/flux-kontext-pro", "edit", {"size": "1024x1536", "image": PNG64}),
        ("bfl/flux-kontext-max", "edit-max", {"image": PNG64}),
    ):
        status, mime, raw = image(model, prompt, **params)
        assert (status, mime) == (200, "application/json"), (status, mime, raw, ERRORS)
        result = json.loads(raw)
        assert isinstance(result["created"], int) and result["data"] == ([] if prompt == "empty" else [{"url": ASSET}])
        snapshots[prompt] = result["data"]
    status, mime, raw = request(port, {"model": "bfl/flux-pro", "prompt": "binary"},
                                "/api/v1/images/generations?response_format=binary")
    assert (status, mime, raw) == (200, "image/png", PNG), (status, mime, raw, ERRORS)
    snapshots["binary"] = [mime, base64.b64encode(raw).decode()]
    assert len(SUBMITS) == 7 and len(POLLS) == 8 and len(ASSETS) == 1
    assert all(key == PRIMARY for _, _, key in SUBMITS)
    assert not CAPTURES and not ERRORS, (CAPTURES, ERRORS)
    return {"images": snapshots, "submits": [(prompt, model) for prompt, model, _ in SUBMITS], "polls": POLLS}


def strict(port, db):
    def image(prompt, model="bfl/flux-pro-1.1", path=API, **options):
        return request(port, {"model": model, "prompt": prompt, **options}, path)

    no_poll = {
        "missing": "no polling_url returned",
        "bad-json": "Invalid response",
        "submit-redirect": "redirect refused",
        "submit-disconnect": "fetch failed",
        "submit-ambiguous": "[redacted]",
        **{"unsafe-" + label: "invalid polling_url" for label in UNSAFE},
    }
    for prompt, message in no_poll.items():
        reset(db)
        count = len(SUBMITS)
        status, _, raw = image(prompt)
        assert status == (503 if prompt == "submit-ambiguous" else 502) and message.encode() in raw, (prompt, status, raw)
        assert len(SUBMITS) == count + 1 and not POLLS and not CAPTURES, (SUBMITS, POLLS, CAPTURES)
        assert PRIMARY.encode() not in raw and SECONDARY.encode() not in raw
    for prompt, message, model in [
        ("poll-redirect", "BFL status 307", "bfl/flux-pro-1.1"),
        ("poll-http", "BFL status 401", "bfl/flux-pro-1.1"),
        ("poll-failed", "fixture fail [redacted]", "bfl/flux-pro-1.1"),
        ("combo-failed", "fixture fail [redacted]", "fixture-bfl-combo"),
        ("moderated", "Content Moderated", "bfl/flux-pro-1.1"),
        ("slow", "BFL polling timeout", "bfl/flux-pro-1.1"),
        ("pending", "BFL polling timeout", "bfl/flux-pro-1.1"),
    ]:
        reset(db)
        count = len(SUBMITS)
        start = time.monotonic()
        status, _, raw = image(prompt, model)
        assert status == 502 and message.encode() in raw and len(SUBMITS) == count + 1, (prompt, status, raw)
        assert not CAPTURES and any(label == prompt for label, _ in POLLS)
        assert PRIMARY.encode() not in raw and SECONDARY.encode() not in raw
        if prompt in ("slow", "pending"):
            assert .40 < time.monotonic() - start < 1.4

    reset(db)
    before = len(SUBMITS)
    status, _, _ = image("submit-denied")
    assert status == 200 and len(SUBMITS) == before + 2
    assert [key for _, _, key in SUBMITS[-2:]] == [PRIMARY, SECONDARY]

    reset(db)
    before = len(SUBMITS)
    status, _, raw = image("empty", "fixture-bfl-combo")
    assert status == 200 and json.loads(raw)["data"] == [] and len(SUBMITS) == before + 1

    reset(db)
    for prompt in ("secret-sample", "local-sample"):
        status, mime, raw = image(prompt, path=API + "?response_format=binary")
        assert (status, mime) == (200, "application/json") and not ASSETS and not CAPTURES
        assert PRIMARY.encode() not in raw
        if prompt == "secret-sample":
            assert json.loads(raw)["data"] == []
    before = len(SUBMITS)
    assert image("bad size", size=100)[0] == 400
    assert image("unknown-model", "bfl/stolen-credentials")[0] == 400
    assert len(SUBMITS) == before
    assert not ERRORS, ERRORS
    print("BFL exact regional polling, no key on assets, one submission across poll/transport errors and combos, explicit pre-acceptance account fallback.")


with tempfile.TemporaryDirectory(prefix="9router-bfl-images-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    home = scratch / "home"
    home.mkdir()
    data = scratch / "data"
    empty_path = scratch / "bin"
    empty_path.mkdir()
    db = seed(data, home)
    stack.enter_context(serving(ThreadingHTTPServer(("127.0.0.1", 52989), Server)))
    stack.enter_context(serving(ThreadingHTTPServer(("93.184.216.34", 52991), Server)))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback = scratch / "fallback.log"
    env = {"PATH": os.environ["PATH"] if REFERENCE else str(empty_path), "HOME": str(home),
           "DATA_DIR": str(data), "HOST": "127.0.0.1", "HOSTNAME": "127.0.0.1", "PORT": str(port),
           "MODEL_CATALOG_SYNC": "off", "NINEROUTER_MIGRATION_FIXTURE": "1",
           "RUST_ANYTHING_FALLBACK_LOG": str(fallback)}
    args = ["node", "--import", "./rust/bfl-image-replay/original.mjs", ".next/standalone/custom-server.js"] if REFERENCE else [
        BINARY, "--no-browser",
    ]
    with (scratch / "server.log").open("w+") as logs:
        app = subprocess.Popen(args, cwd=ROOT, env=env, stdout=logs, stderr=logs)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if request(port, path="/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("BFL fixture gateway did not start")
            if CASE == "strict":
                strict(port, db)
            else:
                print(json.dumps(outputs(port), sort_keys=True))
            assert not fallback.exists() or not fallback.read_bytes()
        except Exception:
            logs.seek(0)
            print(logs.read(), file=sys.stderr)
            print(SUBMITS, POLLS, ERRORS, file=sys.stderr)
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)
