from base64 import b64encode
from collections import deque
from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "ollama_tts_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

REMOTE = "ollama-speech.fixture.example"
HOST = "http://127.0.0.1:52989"
SPEECH, TAGS = "/v1/audio/speech", "/api/tags"
PREFIX, PHONON = "/tenant", "phonon:multilingual"
TEXT = "Fixture render 世界"
WAV = b"RIFF" + bytes(range(128))
PCM = bytes(reversed(range(128)))
KEY = "offline-ollama-configured-key"
RELAY = "https://ollama-speech-fixture.offline-lane.workers.dev"
RELAY_KEY = "O" * 43
AUTH = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json",
        "Cookie": "not forwarded", "x-internal-token": "not forwarded"}
shared.ZED_HOST, shared.CLINE_HOST = REMOTE, "unused.ollama.invalid"
shared.SECRETS = (KEY, RELAY_KEY)
PENDING, OUTBOUND, ERRORS = deque(), [], []


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, mime, location=None):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(body)

    def execute(self):
        try:
            method, path, outbound, key, status, response, mime, hook = PENDING.popleft()
            assert self.command == method and self.path == path
            assert not any(self.headers[name] for name in ("Proxy-Authorization", "Cookie", "x-internal-token",
                                                           "x-relay-auth", "x-relay-target", "x-relay-path"))
            assert self.headers.get_all("Authorization") == (["Bearer " + key] if key else None)
            lane = "direct" if self.server.server_port in (11434, 52989) else "tunnel"
            if lane == "tunnel":
                assert self.headers["Host"] == REMOTE
            if method == "GET":
                assert not self.headers["Content-Length"] and not self.headers["Content-Type"]
            else:
                assert self.headers["Content-Type"] == "application/json"
                received = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert received == outbound, received
            OUTBOUND.append((lane, method, path, outbound))
            if hook:
                hook()
            redirect = f"https://{REMOTE}/must-not-follow" if status == 307 else None
            self.reply(status, response, mime, redirect)
        except (AssertionError, IndexError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((self.command, self.path, repr(error)))
            self.reply(422, {"error": "unexpected fixture request"}, "application/json")

    do_GET = do_POST = execute


def post(model, voice, *, prefix=PREFIX, key=KEY, audio="wav", status=200, body=WAV, mime="audio/wav", hook=None):
    PENDING.append(("POST", prefix + SPEECH, {"model": model, "input": TEXT, "voice": voice, "response_format": audio},
                    key, status, body, mime, hook))


def tags(body, status=200, prefix=PREFIX):
    PENDING.append(("GET", prefix + TAGS, None, None, status, body, "application/json", None))


def save(db, base=HOST + PREFIX, details=None, key=KEY):
    data = {"providerSpecificData": {"baseUrl": base, **(details or {})}}
    if key:
        data["apiKey"] = key
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('ollama-native','ollama-local','apikey','Offline Ollama',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (json.dumps(data),))


def ask(port, model, *, output=None, query=None, expected=200, admin=True):
    path = SPEECH if query is None else SPEECH + "?response_format=" + query
    request = {"model": "ollama-local/" + model, "input": "  " + TEXT + "\t", "voice": "ignored"}
    if output is not None:
        request["response_format"] = output
    with closing(HTTPConnection("127.0.0.1", port, timeout=8)) as connection:
        connection.request("POST", path, json.dumps(request).encode(), AUTH if admin else {"Content-Type": "application/json"})
        response = connection.getresponse()
        data = response.read()
        assert not any(secret.encode() in data for secret in shared.SECRETS)
        assert response.status == expected, (response.status, data, ERRORS)
        assert not PENDING, list(PENDING)
        if expected != 200:
            return json.loads(data)["error"]["message"]
        content_type = response.headers["content-type"]
        assert response.headers["access-control-allow-origin"] == "*"
        if content_type == "application/json":
            return json.loads(data)
        length = response.headers.get("content-length")
        assert length is None or int(length) == len(data)
        return {"mime": content_type, "bytes": b64encode(data).decode()}


@contextmanager
def reference(scratch):
    home = scratch / "reference"
    home.mkdir()
    data = home / "data"
    env = {"DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "HOME": str(home), "PATH": os.environ["PATH"],
           "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, env=env,
                   stderr=subprocess.DEVNULL)
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        env["PORT"] = str(listener.getsockname()[1])
    with subprocess.Popen(["node", ".next/standalone/custom-server.js"], cwd=ROOT, env=env,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            port = int(env["PORT"])
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if shared.get(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise RuntimeError("Pinned Ollama speech server did not start")
            yield port, db
        finally:
            process.terminate()
            process.wait(timeout=7)


def direct_cases(port, db):
    results = {}
    assert ask(port, "phonon", expected=401, admin=False) == "Missing API key"
    post(PHONON, "default", prefix="", key=None)
    results["default-host-model-voice"] = ask(port, "")
    assert results["default-host-model-voice"]["mime"] == "audio/wav"
    save(db, "  " + HOST + PREFIX + "/v1/audio/speech///  ")
    post(PHONON, "zoey")
    results["voice-only-wav"] = ask(port, "zoey")
    post("phonon:en", "default", audio="pcm", body=PCM, mime="audio/pcm")
    results["model-only-pcm-query"] = ask(port, "phonon:en", query="pcm", output="json")
    assert results["model-only-pcm-query"]["mime"] == "audio/pcm"
    post("custom-phonon", "voices/easton")
    results["nested-voice-json"] = ask(port, "custom-phonon//voices/easton", output="json")
    assert results["nested-voice-json"] == {"audio": b64encode(WAV).decode(), "format": "wav"}

    post("phonon:en", "zoey", status=404, body={"error": "model missing"}, mime="application/json")
    tags({"models": [{"name": "non-phonon"}, {"model": "phonon:en"}, {"model": "phonon:installed"}, {"name": "phonon:ignored"}]})
    post("phonon:installed", "zoey", status=400, body={"error": {"message": "voice zoey not found"}}, mime="application/json")
    post("phonon:installed", "default")
    results["discovery-then-voice"] = ask(port, "phonon:en/zoey")
    post("phonon", "default", status=404, body={}, mime="application/json")
    tags(b"{", prefix=PREFIX)
    post(PHONON, "default")
    results["fallback-tag-when-malformed"] = ask(port, "phonon")
    post(PHONON, "easton", status=404, body={}, mime="application/json")
    tags({}, status=503)
    post("phonon:latest", "easton")
    results["fallback-tag-when-unavailable"] = ask(port, "easton")

    for name, model, planned, message in (
        ("too-short", "phonon", lambda: post("phonon", "default", body=WAV[:99]), "Upstream returned empty audio"),
        ("unrelated-model-404", "custom/zoey", lambda: post("custom", "zoey", status=404, body={"detail": "no checkpoint"}),
         "no checkpoint"),
        ("voice-retry-failure", "phonon/zoey", lambda: (
            post("phonon", "zoey", status=400, body={"detail": {"message": "voice requested not found"}}),
            post("phonon", "default", status=503, body={"error": "original safely retained"})),
         "voice requested not found"),
    ):
        save(db)
        planned()
        results[name] = ask(port, model, expected=502)
        assert results[name].startswith(f"[ollama-local/{model}] {message} (reset after "), (name, results[name])
    save(db, "http://127.0.0.1:9")
    results["unreachable-saved-host"] = ask(port, "phonon", expected=502)
    assert results["unreachable-saved-host"].startswith(
        "[ollama-local/phonon] Cannot connect to Ollama at http://127.0.0.1:9. "
        "Please ensure Ollama is running (e.g. 'ollama serve'). (reset after "
    )
    return results | {"outbound": OUTBOUND}


def protected_cases(port, db, connect, http, socks_address, wrong_tls):
    remote = f"https://{REMOTE}{PREFIX}/v1/"
    save(db, remote)
    shared.enable_global_proxy(db, http["connectionProxyUrl"])
    post("phonon:en", "freya", status=404, body={}, mime="application/json",
         hook=lambda: shared.enable_global_proxy(db, socks_address))
    tags({"models": [{"name": "phonon:installed"}]})
    post("phonon:installed", "freya", status=400, body={"message": "voice freya not found"}, mime="application/json")
    post("phonon:installed", "default")
    assert ask(port, "phonon:en/freya")["mime"] == "audio/wav"
    assert shared.TUNNELS == [("connect", REMOTE, 443)] * 4
    assert [lane for lane, *_ in OUTBOUND] == ["tunnel"] * 4
    post("phonon", "default", audio="pcm", body=PCM, mime="audio/pcm")
    assert ask(port, "phonon", output="pcm")["mime"] == "audio/pcm"
    assert shared.TUNNELS[-1] == ("socks", REMOTE, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")

    save(db, HOST + PREFIX, http | {"connectionNoProxy": "127.0.0.1"}, key=None)
    post("phonon", "default", key=None)
    assert ask(port, "phonon")["mime"] == "audio/wav" and OUTBOUND[-1][0] == "direct"
    before = (len(OUTBOUND), len(shared.TUNNELS))
    for base, invalid in ((HOST + PREFIX, http), (remote, {"proxyPoolId": "missing"}), (remote, {"proxyPoolId": "relay"}),
                          (remote, {"vercelRelayUrl": RELAY}), (remote, {"strictProxy": True}),
                          (remote + "?api_key=" + KEY, http)):
        save(db, base, invalid)
        ask(port, "phonon", expected=503)
        assert (len(OUTBOUND), len(shared.TUNNELS)) == before
    save(db, remote, http)
    post("phonon", "default", status=401, body={"error": {"message": "provider echoed " + KEY}})
    assert ask(port, "phonon", expected=502) == "provider echoed [redacted]"
    post("phonon", "default", status=307, body={})
    assert "redirect refused" in ask(port, "phonon", expected=502)
    assert len(OUTBOUND) == before[0] + 2 and len(shared.TUNNELS) == before[1] + 2
    connect.target = wrong_tls
    ask(port, "phonon", expected=502)
    connect.reject = True
    ask(port, "phonon", expected=502)
    assert len(OUTBOUND) == before[0] + 2 and len(shared.TUNNELS) == before[1] + 4
    assert all(hello == REMOTE for hello in shared.HELLOS)


with tempfile.TemporaryDirectory(prefix="9router-ollama-tts-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    for port in (52989, 11434):
        stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", port), Vendor)))
    mode = sys.argv[2]
    if mode in ("original", "native"):
        gateway = reference(scratch) if mode == "original" else shared.gateway(scratch, "native", None, [])
        with gateway as (port, db):
            result = direct_cases(port, db)
        print(json.dumps(result, sort_keys=True, ensure_ascii=False))
    else:
        assert mode == "protected"
        ca, tls, wrong_tls = shared.tls_services(stack, scratch, Vendor)
        connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
        for proxy, kind in ((connect, "connect"), (socks, "socks")):
            proxy.mode, proxy.reject, proxy.target = kind, False, tls
            stack.enter_context(shared.serving(proxy))
        http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
        socks_address = f"socks5h://127.0.0.1:{socks.server_address[1]}"
        with shared.gateway(scratch, "native-protected", ca, [("relay", "cloudflare", RELAY)]) as (port, db):
            with sqlite3.connect(db) as sql:
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                            (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
            protected_cases(port, db, connect, http, socks_address, wrong_tls)
        print("Ollama Local speech/tag/voice retries stayed on the selected protected route without downgrade.")
    assert not ERRORS and not shared.ERRORS and not PENDING, (ERRORS, shared.ERRORS)
