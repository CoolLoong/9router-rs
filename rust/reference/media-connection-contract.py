import base64
import hashlib
import hmac
from html.parser import HTMLParser
import http.client
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time


BINARY = str(Path(sys.argv[1]).resolve())
ROOT = Path(__file__).resolve().parents[2]
SECRET = "synthetic-media-form-session"
BASE = "/dashboard/media-providers"
SETUP = "provider-account"
FORM = "media-credential-form"


class Document(HTMLParser):
    def __init__(self, html):
        super().__init__()
        self.elements = {}
        self.scripts = []
        self.feed(html)

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        if "id" in attributes:
            self.elements[attributes["id"]] = attributes
        if tag == "script" and "src" in attributes:
            self.scripts.append(attributes["src"])


def cookie():
    b64 = lambda raw: base64.urlsafe_b64encode(raw).decode().rstrip("=")
    header = b64(b'{"alg":"HS256"}')
    payload = b64(json.dumps({"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 600}).encode())
    pair = f"{header}.{payload}"
    return {"Cookie": "auth_token=" + pair + "." + b64(hmac.new(SECRET.encode(), pair.encode(), hashlib.sha256).digest())}


def request(port, path, body=None, method=None, headers=None):
    headers = dict(cookie() if headers is None else headers)
    if body is not None:
        headers["Content-Type"] = "application/json"
        body = json.dumps(body).encode()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request(method or ("POST" if body is not None else "GET"), path, body=body, headers=headers)
        response = connection.getresponse()
        raw = response.read().decode()
        if response.getheader("Content-Type", "").startswith("application/json"):
            raw = json.loads(raw)
        return response.status, raw, response.headers
    finally:
        connection.close()


def pages(port):
    assert request(port, BASE + "/video/vertex", headers={})[0] == 307
    for path, scheme, oauth, basic in [
        ("video/vertex", "vertex", False, False),
        ("imageToText/vertex", "vertex", False, True),
        ("systemone/cloudflare-ai", "cloudflare", False, False),
        ("image/cloudflare-ai", "cloudflare", False, False),
        ("stt/selfhosted-stt", "selfhosted", False, False),
        ("tts/selfhosted-tts", "selfhosted", False, False),
        ("embedding/selfhosted-embedding", "selfhosted", False, False),
        ("tts/local-device", None, False, False),
        ("tts/ollama-local", None, False, True),
        ("stt/codex", None, True, False),
        ("image/codex", None, False, False),
        ("imageToText/xai", None, True, True),
        ("imageToText/nous", None, True, True),
        ("imageToText/kimchi", None, True, True),
        ("image/xai", None, False, True),
        ("video/xai", None, False, True),
        ("webSearch/xai", None, False, True),
        ("embedding/github", None, False, False),
        ("image/grok-web", None, False, False),
    ]:
        status, html, headers = request(port, BASE + "/" + path)
        assert status == 200 and headers.get("Cache-Control") == "no-store", (path, status)
        assert "panel=legacy" in html and "/_next/" not in html
        page = Document(html)
        assert (page.elements.get(FORM, {}).get("data-scheme") == scheme), path
        assert (SETUP in page.elements) == oauth, path
        assert ("hidden" not in page.elements["media-create-connection"]) == basic, path
        if oauth:
            assert page.elements[SETUP]["data-provider"] == path.split("/")[-1]
            assert "/__9router-ui/provider-account.js" in page.scripts
        if scheme == "vertex":
            assert page.elements["media-credential-file"]["type"] == "file"
        if scheme in {"selfhosted", "cloudflare"}:
            assert page.elements["media-credential-key"]["type"] == "password"
        if path == "tts/local-device":
            assert "This provider needs no account credentials." in html
        if path == "tts/ollama-local":
            assert "Add Ollama server" in html
    status, html, _ = request(port, "/dashboard/providers/codex")
    assert status == 200 and Document(html).elements[SETUP]["data-provider"] == "codex"
    status, asset, headers = request(port, "/__9router-ui/integrations.js")
    assert status == 200 and headers.get("Content-Type", "").startswith("text/javascript")
    assert asset == (ROOT / "rust/app/src/dashboard_assets/integrations.js").read_text()


def save_and_reload(port, database):
    service_account = json.dumps({"type": "service_account", "project_id": "media-demo-project", "client_email":
                                  "media-fixture@example.invalid", "private_key": "synthetic-media-private-key"})
    for provider, secret, details, changed in [
        ("cloudflare-ai", "synthetic-cloudflare-key", {"accountId": "synthetic-first"}, {"accountId": "synthetic-second"}),
        ("selfhosted-stt", "synthetic-stt-key", {"baseUrl": "http://127.0.0.1:52989/stt"}, {"baseUrl": "http://127.0.0.1:52989/stt-new"}),
        ("selfhosted-tts", "synthetic-tts-key", {"baseUrl": "http://127.0.0.1:8880"}, {"baseUrl": "http://127.0.0.1:9000"}),
        ("selfhosted-embedding", "synthetic-embed-key", {"baseUrl": "http://127.0.0.1:8080/v1"}, {"baseUrl": "http://127.0.0.1:9000/v1"}),
        ("vertex", service_account, {"location": "europe-west4"}, {"location": None}),
    ]:
        status, created, _ = request(port, "/api/providers", {"name": "Media fixture", "provider": provider,
                                                              "apiKey": secret, "providerSpecificData": details})
        public = created["connection"]
        assert status == 201 and "apiKey" not in public and public["testStatus"] == "unknown"
        path = "/api/providers/" + public["id"]
        status, updated, _ = request(port, path, {"name": "Updated fixture", "providerSpecificData": changed}, method="PUT")
        assert status == 200 and "apiKey" not in updated["connection"]
        status, reloaded, _ = request(port, path)
        prior_details = public["providerSpecificData"]
        saved_details = reloaded["connection"]["providerSpecificData"]
        assert status == 200 and all(saved_details.get(field) == value for field, value in {**prior_details, **changed}.items())
        with sqlite3.connect(database) as sql:
            (raw,), = sql.execute("SELECT data FROM providerConnections WHERE id=?", (public["id"],))
        assert json.loads(raw)["apiKey"] == secret


with tempfile.TemporaryDirectory(prefix="9router-media-form-") as directory:
    temporary = Path(directory)
    home, data, empty = (temporary / name for name in ("home", "data", "empty-path"))
    home.mkdir()
    empty.mkdir()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallbacks = temporary / "fallbacks"
    env = {"PATH": str(empty), "HOME": str(home), "DATA_DIR": str(data), "PORT": str(port), "JWT_SECRET": SECRET,
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks)}
    with (temporary / "app.log").open("w+") as log:
        app = subprocess.Popen([BINARY, "--no-browser"], cwd=home, env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while app.poll() is None and time.monotonic() < deadline:
                try:
                    if request(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.03)
            else:
                raise AssertionError("Native media form gateway did not start")
            pages(port)
            save_and_reload(port, data / "db/data.sqlite")
            assert fallbacks.read_bytes() == b""
            print("Media pages exposed only scoped native forms. HTTP save/edit retained redacted credentials with zero fallback.")
        except Exception:
            log.seek(0)
            print(log.read(), file=sys.stderr, end="")
            raise
        finally:
            app.terminate()
            app.wait(timeout=5)
