import gzip
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
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
cli = {
    "x-9r-cli-token": "68977b6fd9422fbb",
    "cookie": "fixture-only-session=not-forwarded",
    "authorization": "Bearer not-the-grok-token",
    "x-api-key": "not-an-upstream-key",
}
public = {"authorization": "Bearer offline-active-key"}
path = "/grok-cli/v1/models"
labels = (
    "primary", "mapping", "results", "empty", "denied", "bad-gateway", "missing",
    "invalid-json", "oversize", "echo", "error-echo", "status-refresh", "expired",
    "expired-refresh", "proxy", "relay", "gzip", "unexpected-redirect",
)
events = []


def token(label):
    return f"opaque-grok-{label}-fixture-only"


def email(label):
    return f"catalog-{label}@fixture.invalid"


def user(label):
    return f"catalog-{label}-principal"


def payload(label):
    if label == "primary":
        return [
            "grok-build",
            {"model_id": " fixture-grok-premium ", "display_name": "Premium",
             "context_window": "262144", "max_output_tokens": "32768", "version": "offline-metadata",
             "capabilities": {"vision": True}},
            {"id": "fixture-grok-premium", "displayName": "Ignored duplicate"},
            {"id": "", "slug": "ignored-empty-id"},
            {"modelId": "fixture-grok-short", "displayName": "", "name": "Ignored fallback", "contextLength": 12},
            None, [], {"slug": "fixture-grok-slug"}, {"name": "fixture-grok-name-only"},
        ]
    if label == "mapping":
        return {"data": None, "models": {
            "entry": {"displayName": "Label", "context_window": "0x80000"},
            "4": {"display_name": "Four"}, "2": {"display_name": "Two"},
            "x": {"id": "fixture-grok-map", "maxOutputTokens": 24},
        }, "results": [{"id": "unused"}]}
    if label == "results":
        return {"results": {"grok-build": {"maxOutputTokens": "already supplied"}}}
    if label == "echo":
        return {"data": [
            {"id": token(label)},
            {"id": "fixture-grok-scrubbed",
             "name": token(label),
             "nested": {"email": email(label), "api_key": "unknown-credential-by-key",
                        "client_secret": "unknown-secret-by-key",
                        "note": "public metadata " + user(label)}},
        ]}
    if label == "gzip":
        return [{"id": "fixture-grok-compressed"}]
    return {"data": []}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        authorization = self.headers.get("authorization")
        label = next((item for item in labels if authorization == "Bearer " + token(item)), None)
        denied = ["cookie", "x-api-key", "x-9r-cli-token", "proxy-authorization"]
        if mode == "native":
            denied.extend(["x-grok-client-version", "x-grok-client-identifier"])
        agent = self.headers.get("user-agent", "")
        admitted = (
            bool(label) and not any(self.headers.get(header) for header in denied)
            and self.path == path and self.headers.get("x-xai-token-auth") == "xai-grok-cli"
            and self.headers.get("x-grok-client-mode") == "headless"
            and self.headers.get("accept") == "application/json"
            and self.headers.get("x-email") == email(label) and self.headers.get("x-userid") == user(label)
            and agent.startswith("9Router/" if mode == "native" else "grok-shell/")
        )
        events.append((label, admitted))
        status, body, headers = 200, json.dumps(payload(label)).encode(), {}
        if not admitted:
            status, body = 422, b"unexpected Grok fixture request"
        elif label == "denied":
            status, body = 403, b"fixture scope denied"
        elif label == "bad-gateway":
            status, body = 502, b""
        elif label == "status-refresh":
            status, body = 401, b"fixture requires reauthorization"
        elif label == "invalid-json":
            body = b"{"
        elif label == "oversize":
            body = b""
            headers["content-length"] = str(4 * 1024 * 1024 + 1)
        elif label == "error-echo":
            status, body = 403, (f"{token(label)} {email(label)} {user(label)}").encode()
        elif label == "gzip":
            body = gzip.compress(body)
            headers["content-encoding"] = "gzip"
        elif label == "unexpected-redirect":
            status, body = 307, b""
            headers["location"] = "http://127.0.0.1:52989/grok-cli/do-not-follow"
        self.send_response(status)
        self.send_header("content-type", "application/json")
        headers.setdefault("content-length", str(len(body)))
        for key, value in headers.items():
            self.send_header(key, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass


def seed(data):
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)],
                   cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("DELETE FROM providerConnections")
        for priority, label in enumerate(labels, 1):
            details = {"email": email(label), "principalId": user(label)}
            if label == "primary":
                details["userId"] = details.pop("principalId")
                details["principalId"] = "wrong-fallback-principal"
            if label == "proxy":
                details.update(connectionProxyEnabled=True, connectionProxyUrl="http://127.0.0.1:52989")
            if label == "relay":
                details["vercelRelayUrl"] = "http://127.0.0.1:52989/relay"
            expiry = "2025-01-01T00:00:00.000Z" if label.startswith("expired") else "2099-01-01T00:00:00.000Z"
            account = {"accessToken": None if label == "missing" else token(label),
                       "expiresAt": expiry, "providerSpecificData": details}
            if label in ("status-refresh", "expired-refresh"):
                account["refreshToken"] = "offline-refresh-fixture-" + label
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                          VALUES(?,'grok-cli','oauth',?,?,?,?,'2026-11-01T00:00:00Z','2026-11-01T00:00:00Z')""",
                       ("fixture-grok-" + label, label, priority, int(label == "primary"), json.dumps(account)))


def request(port, url, headers=cli):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        connection.request("GET", url, headers=headers)
        reply = connection.getresponse()
        return reply.status, json.loads(reply.read())
    finally:
        connection.close()


def scenario(port, fallbacks):
    prefix = "/api/providers/fixture-grok-"

    def models(label, headers=cli):
        return request(port, prefix + label + "/models", headers)

    observed = {"protected": models("primary", {}), "not-found": models("absent")}
    assert observed["protected"][0] == 401 and observed["not-found"][0] == 404
    assert not events
    for label in ("primary", "mapping", "results", "empty", "denied", "bad-gateway", "missing", "gzip"):
        observed[label] = models(label)
        assert observed[label][0] == 200
    primary = observed["primary"][1]["models"]
    assert [item["id"] for item in primary] == [
        "grok-build", "fixture-grok-premium", "fixture-grok-short", "fixture-grok-slug", "fixture-grok-name-only",
    ]
    assert primary[0] == {"id":"grok-build", "name":"grok-build", "contextLength":500000, "maxOutputTokens":64000}
    assert primary[1]["contextLength"] == 262144 and primary[1]["maxOutputTokens"] == 32768
    assert primary[1]["version"] == "offline-metadata" and primary[2]["name"] == ""
    mapping = observed["mapping"][1]["models"]
    assert [item["id"] for item in mapping] == ["2", "4", "entry", "fixture-grok-map"]
    assert mapping[2]["contextLength"] == 524288
    assert observed["results"][1]["models"][0]["maxOutputTokens"] == "already supplied"
    assert observed["gzip"][1]["models"] == [{"id":"fixture-grok-compressed", "name":"fixture-grok-compressed"}]
    for label, warning in {
        "empty": "Grok CLI returned no selectable models.",
        "denied": "Grok CLI model discovery failed (403): fixture scope denied",
        "bad-gateway": "Grok CLI model discovery failed (502)",
        "missing": "Grok CLI access token is missing.",
    }.items():
        response = observed[label][1]
        assert response["warning"] == warning and response["models"][0]["id"] == "grok-build"

    status, response = request(port, "/v1/models", headers=public)
    ids = sorted(item["id"] for item in response["data"] if item["id"].startswith("gcli/"))
    assert status == 200 and ids == sorted("gcli/" + item["id"] for item in primary)
    premium = next(item for item in response["data"] if item["id"] == "gcli/fixture-grok-premium")
    assert premium["capabilities"] == {"vision": True}
    observed["published"] = {"ids":ids, "premium":premium["capabilities"]}
    assert all(admitted for _, admitted in events), events
    counts = Counter(label for label, _ in events)
    assert counts == {"primary":2, "mapping":1, "results":1, "empty":1, "denied":1, "bad-gateway":1, "gzip":1}
    if mode == "native":
        assert fallbacks.is_file() and not fallbacks.read_text().strip()
        proxy_status, proxy = models("proxy")
        relay_status, relay = models("relay")
        assert proxy_status == relay_status == 200
        assert proxy["warning"] == "Grok CLI model discovery failed: fetch failed"
        assert relay["warning"] == "Grok CLI model discovery failed: configured proxy is unavailable."
        assert len(events) == sum(counts.values())
        assert models("expired-refresh")[1]["warning"].endswith("The provider account is unavailable.")
        assert models("status-refresh")[1]["warning"].endswith("The provider account is unavailable.")
        assert events[-1] == ("status-refresh", True)
        status, data = models("echo")
        assert status == 200 and data["models"] == [{"id":"fixture-grok-scrubbed","name":"[redacted]",
                                                    "nested":{"note":"public metadata [redacted]"}}]
        status, response = models("error-echo")
        assert status == 200 and response["warning"].endswith("[redacted] [redacted] [redacted]")
        assert models("expired")[1]["warning"] == "Grok CLI access token has expired."
        assert models("invalid-json")[1]["warning"].endswith("invalid response.")
        assert models("oversize")[1]["warning"].endswith("invalid or oversized response.")
        assert models("unexpected-redirect")[1]["warning"].endswith("(307)")
        assert all(admitted for _, admitted in events)
        assert Counter(label for label, _ in events) == counts + Counter({
            "status-refresh":1, "echo":1, "error-echo":1, "invalid-json":1, "oversize":1, "unexpected-redirect":1,
        })
        assert not fallbacks.read_text().strip()
    return observed


with tempfile.TemporaryDirectory(prefix="9router-grok-models-") as directory:
    scratch = Path(directory)
    home = scratch / "home"
    home.mkdir()
    data = scratch / "data"
    seed(data)
    empty_path = scratch / "bin"
    empty_path.mkdir()
    fallbacks = scratch / "fallback"
    upstream = ThreadingHTTPServer(("127.0.0.1", 52989), Handler)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env = {"PATH":str(empty_path), "HOME":str(home), "PORT":str(port), "DATA_DIR":str(data),
               "HOSTNAME":"127.0.0.1", "NINEROUTER_MIGRATION_FIXTURE":"1",
               "MODEL_CATALOG_SYNC":"off", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1",
               "RUST_ANYTHING_FALLBACK_LOG":str(fallbacks)}
        command = [str(Path(args[0]).resolve()), "--no-browser"] if mode == "native" else [
            shutil.which("node"), "--import", str(repo / "rust/reference/mock-grok-cli-models.mjs"),
            str(repo / ".next/standalone/custom-server.js"),
        ]
        process = subprocess.Popen(command, cwd=repo, env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 9
            while process.poll() is None and time.monotonic() < deadline:
                try:
                    with socket.create_connection(("127.0.0.1", port), .2):
                        break
                except OSError:
                    time.sleep(.025)
            else:
                raise RuntimeError("Grok models fixture did not start")
            print(json.dumps(scenario(port, fallbacks), sort_keys=True))
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=7)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    finally:
        upstream.shutdown()
        upstream.server_close()
