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
from threading import Thread
import time

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
route = "/api/providers/test-batch"
ledger = []
mock_secrets = ("offline-batch-apikey", "offline-batch-badkey", "offline-legacy-password")
cursor_token = "offline-cursor-token"
checked_at = "2026-11-19T00:00:00.000Z"
expected_errors = {
    "openai-first":None,
    "cursor-oauth":None,
    "openai-second":"Invalid API key or base URL",
    "anthropic-tied":None,
    "kiro-oauth":"No access token",
    "kiro-fallback-auth":"No access token",
    "codex-raw":"No access token",
    "ollama-key":None,
    "compat-fallback-auth":"Provider test not supported",
    "embedding-fallback-auth":"Provider test not supported",
}


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def result(self, status):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"{}")

    def do_GET(self):
        ledger.append(self.path)
        if self.path == "/compatible/models":
            time.sleep(.03)
            self.result(200 if self.headers.get("Authorization") == f"Bearer {mock_secrets[0]}" else 500)
        elif self.path == "/broken/models":
            self.result(401 if self.headers.get("Authorization") == f"Bearer {mock_secrets[1]}" else 500)
        elif self.path == "/ollama/api/tags":
            self.result(200)
        else:
            self.result(404)

    def do_POST(self):
        ledger.append(self.path)
        data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        valid = (self.path == "/claude/v1/messages" and self.headers.get("x-api-key") == mock_secrets[0]
                 and self.headers.get("anthropic-version") == "2023-06-01"
                 and data == {"model":"claude-3-haiku-20240307","max_tokens":1,
                              "messages":[{"role":"user","content":"test"}]})
        self.result(400 if valid else 401)


def request(port, path, method="POST", body=None, cookie=None):
    headers = {"Content-Type":"application/json"}
    if cookie:
        headers["Cookie"] = cookie
    if body is not None and not isinstance(body, bytes):
        body = json.dumps(body)
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, path, body=body, headers=headers)
        reply = client.getresponse()
        return reply.status, json.loads(reply.read()), reply.headers
    finally:
        client.close()


def add(db, identifier, provider, auth, priority, name, email=None, active=True, **data):
    db.execute("""INSERT INTO providerConnections
        (id,provider,authType,priority,isActive,name,email,data,createdAt,updatedAt)
        VALUES (?,?,?,?,?,?,?,?,?,?)""", (identifier, provider, auth, priority, int(active),
                                           name, email, json.dumps(data), checked_at, checked_at))


def configure(db_path, target):
    with sqlite3.connect(db_path) as db:
        db.execute("DELETE FROM providerConnections")
        add(db, "openai-first", "openai-compatible-batch", "apikey", 1, "OpenAI fixture",
            providerSpecificData={"baseUrl":target + "/compatible/"}, apiKey=mock_secrets[0])
        add(db, "cursor-oauth", "cursor", "oauth", 2, "", email="fixture@example.invalid",
            accessToken=cursor_token)
        add(db, "openai-second", "openai-compatible-batch", "apikey", 3, "",
            providerSpecificData={"baseUrl":target + "/broken"}, apiKey=mock_secrets[1])
        add(db, "anthropic-tied", "anthropic-compatible-batch", "apikey", 3, "Anthropic fixture",
            providerSpecificData={"baseUrl":target + "/claude/messages"}, apiKey=mock_secrets[0])
        add(db, "kiro-oauth", "kiro", "oauth", 4, "Kiro fixture")
        add(db, "kiro-fallback-auth", "kiro", "", 5, "Kiro old profile")
        add(db, "codex-raw", "codex", "access_token", 6, "Codex raw fixture")
        add(db, "ollama-key", "ollama-local", "apikey", 0, "Ollama fixture",
            providerSpecificData={"baseUrl":target + "/ollama/"})
        add(db, "compat-fallback-auth", "openai-compatible-empty-auth", "", 8, "Old compatible profile")
        add(db, "embedding-fallback-auth", "custom-embedding-batch", "", 9, "Old embedding profile")
        add(db, "inactive", "openai-compatible-batch", "apikey", -1, "Inactive fixture", active=False,
            providerSpecificData={"baseUrl":target + "/inactive"})


def connect_ready(port, process):
    deadline = time.monotonic() + 16
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if request(port, "/api/health", "GET")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.03)
    raise RuntimeError("fixture service did not become ready")


with tempfile.TemporaryDirectory(prefix="9router-provider-test-batch-") as label:
    directory = Path(label)
    mock = ThreadingHTTPServer(("127.0.0.1", 0), Endpoint)
    Thread(target=mock.serve_forever, daemon=True).start()
    target = f"http://127.0.0.1:{mock.server_port}"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback_log = directory / "fallbacks.log"
    env = {"PATH":os.environ["PATH"], "HOME":str(directory), "DATA_DIR":str(directory),
           "PORT":str(port), "HOSTNAME":"127.0.0.1", "NEXT_TELEMETRY_DISABLED":"1",
           "NINEROUTER_MIGRATION_DATA_DIR":str(directory), "NINEROUTER_MIGRATION_FIXTURE":"1",
           "RUST_ANYTHING_FALLBACK_LOG":str(fallback_log)}
    if implementation == "standalone":
        empty = directory / "bin"
        empty.mkdir()
        env["PATH"] = str(empty)
    command = ["sh", "rust/reference/server.sh"] if implementation == "original" else [str(binary), "--no-browser"]
    process = subprocess.Popen(command, cwd=root, env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        connect_ready(port, process)
        unauthorized = request(port, route, body={"mode":"all"})
        assert unauthorized[:2] == (401, {"error":"Unauthorized"})
        status, _, headers = request(port, "/api/auth/login", body={"password":"123456"})
        assert status == 200
        cookie = headers["Set-Cookie"].split(";", 1)[0]
        db_path = directory / "db/data.sqlite"
        configure(db_path, target)
        snapshots = {}

        def snapshot(label, body, expected_status, ids=None, upstream=()):
            ledger.clear()
            status, data, _ = request(port, route, body=body, cookie=cookie)
            assert status == expected_status, (label, data)
            assert ledger == list(upstream), (label, ledger)
            if ids is not None:
                assert [row["connectionId"] for row in data["results"]] == ids, (label, data)
                assert data.pop("testedAt").endswith("Z")
                for row in data["results"]:
                    identifier = row["connectionId"]
                    assert row.pop("testedAt").endswith("Z")
                    elapsed = row.pop("latencyMs")
                    assert isinstance(elapsed, int) and elapsed >= 0
                    if identifier == "openai-first":
                        assert elapsed >= 20
                    assert row["statusCode"] is None
                    expected_error = expected_errors[identifier]
                    assert (row["valid"], row["error"], row["diagnosis"]) == (
                        expected_error is None, expected_error, None
                    ), (label, row)
                passed = sum(row["valid"] for row in data["results"])
                assert data["summary"] == {"total":len(ids),"passed":passed,"failed":len(ids)-passed}
            if all(secret not in json.dumps(data) for secret in (*mock_secrets, cursor_token)):
                snapshots[label] = data
            else:
                raise AssertionError("batch exposed a synthetic credential")
            return data

        assert snapshot("missing", {}, 400) == {"error":"mode is required"}
        assert snapshot("malformed", b"{", 500) == {"error":"Batch test failed"}
        assert snapshot("null", b"null", 500) == {"error":"Batch test failed"}
        assert snapshot("no-provider", {"mode":"provider"}, 400)["error"].startswith("Invalid mode.")
        assert snapshot("invalid-mode", {"mode":"wrong"}, 400)["error"].startswith("Invalid mode.")
        assert snapshot("no-matches", {"mode":"provider","providerId":"absent"}, 200, [])["providerId"] == "absent"
        picked = snapshot("provider", {"mode":"provider","providerId":"openai-compatible-batch"}, 200,
                          ["openai-first","openai-second"], ("/compatible/models","/broken/models"))
        assert picked["results"][0]["valid"] and not picked["results"][1]["valid"]
        assert picked["results"][1]["connectionName"] == "openai-compatible-batch"
        assert picked["results"][1]["error"] == "Invalid API key or base URL"
        snapshot("compatible", {"mode":"compatible"}, 200,
                 ["openai-first","openai-second","anthropic-tied","compat-fallback-auth"],
                 ("/compatible/models","/broken/models","/claude/v1/messages"))
        free = snapshot("free", {"mode":"free"}, 200, ["kiro-oauth","kiro-fallback-auth"])
        assert [row["authType"] for row in free["results"]] == ["oauth","free"]
        oauth = snapshot("oauth", {"mode":"oauth"}, 200, ["cursor-oauth"])
        assert oauth["results"][0]["connectionName"] == "fixture@example.invalid"
        snapshot("apikey", {"mode":"apikey"}, 200,
                 ["openai-first","openai-second","anthropic-tied","embedding-fallback-auth","ollama-key"],
                 ("/compatible/models","/broken/models","/claude/v1/messages","/ollama/api/tags"))
        all_rows = snapshot("all", {"mode":"all","providerId":"reported-only"}, 200,
                            ["openai-first","cursor-oauth","openai-second","anthropic-tied","kiro-oauth",
                             "kiro-fallback-auth","codex-raw","compat-fallback-auth",
                             "embedding-fallback-auth","ollama-key"],
                            ("/compatible/models","/broken/models","/claude/v1/messages","/ollama/api/tags"))
        assert all_rows["providerId"] == "reported-only"
        with sqlite3.connect(db_path) as db:
            saved = {id_:json.loads(data) for id_, data in db.execute("SELECT id,data FROM providerConnections")}
        assert saved["openai-first"]["testStatus"] == "active"
        assert saved["openai-second"]["testStatus"] == "error"
        assert saved["openai-second"]["lastError"] == "Invalid API key or base URL"
        assert saved["openai-second"]["lastErrorAt"]
        assert saved["cursor-oauth"]["testStatus"] == "active"
        assert saved["cursor-oauth"].get("lastError") is None
        assert saved["cursor-oauth"]["accessToken"] == cursor_token
        for identifier in ("kiro-oauth", "kiro-fallback-auth"):
            assert saved[identifier]["testStatus"] == "error"
            assert saved[identifier]["lastError"] == "No access token"
        for identifier in ("codex-raw", "compat-fallback-auth", "embedding-fallback-auth"):
            assert saved[identifier]["testStatus"] == "error"
            assert saved[identifier]["lastError"] == expected_errors[identifier]
        assert "testStatus" not in saved["inactive"]
        if implementation != "original":
            assert not fallback_log.exists() or not fallback_log.read_text().strip()
        if implementation == "rust":
            with sqlite3.connect(db_path) as db:
                add(db, "redaction", "ollama-local", "cookie", 1000, "Old private Ollama",
                    providerSpecificData={"baseUrl":target.replace("://", f"://offline-user:{mock_secrets[2]}@") + "/private"})
            private = request(port, route, body={"mode":"provider","providerId":"ollama-local"}, cookie=cookie)
            row = next(row for row in private[1]["results"] if row["connectionId"] == "redaction")
            assert not row["valid"] and mock_secrets[2] not in row["error"] and "[redacted]" in row["error"], row
            with sqlite3.connect(db_path) as db:
                stored = json.loads(db.execute("SELECT data FROM providerConnections WHERE id='redaction'").fetchone()[0])
            assert mock_secrets[2] not in stored["lastError"] and "[redacted]" in stored["lastError"]
        print(json.dumps(snapshots, sort_keys=True))
    finally:
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        mock.shutdown()
        mock.server_close()
