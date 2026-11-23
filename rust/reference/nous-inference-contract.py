from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Thread
import time
from urllib.parse import parse_qs

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode in {"reference", "management-reference"}
model = "poolside/laguna-xs-2.1:free"
api_key = "offline-nous-fixture-api"
agent_key = "offline-nous-fixture-agent"
access = "offline-nous-fixture-access"
refresh = "offline-nous-fixture-refresh"
new_access = "offline-nous-fixture-access-after"
new_refresh = "offline-nous-fixture-refresh-after"
new_key = "offline-nous-fixture-agent-after"
alive = "2100-01-01T00:00:00Z"
dead = "2000-01-01T00:00:00Z"
details = {"inferenceBaseUrl": "https://inference-api.nousresearch.com/v1"}
kept = {"fixtureAccountMetadata": "retained"}
message = "Nous local response"
scenario = ""
ledger = []
errors = []
expired = {"authType": "oauth", "apiKey": agent_key, "accessToken": access, "refreshToken": refresh,
           "expiresAt": dead, "providerSpecificData": dict(details, agentKeyExpiresAt=dead, **kept)}
live = dict(expired, expiresAt=alive, providerSpecificData=dict(details, agentKeyExpiresAt=alive, **kept))


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            if self.path == "/nous-device/token":
                self.renew(raw)
            elif self.path == "/nous-device/agent-key":
                self.mint(raw)
            elif self.path == "/static-chat/nous":
                self.generate(raw)
            elif self.path == "/nous-management/chat/completions":
                self.probe(raw)
            else:
                raise AssertionError("Unexpected destination")
        except AssertionError as error:
            errors.append(str(error) or self.path)
            self.reply(500, {})

    def renew(self, raw):
        ledger.append("renew")
        assert self.headers.get("Accept") == "application/json"
        assert self.headers.get("Content-Type") == "application/x-www-form-urlencoded"
        assert self.headers.get("Authorization") is None
        assert self.headers.get_all("x-nous-refresh-token") == [refresh]
        assert parse_qs(raw.decode()) == {"client_id": ["hermes-cli"], "refresh_token": [refresh],
                                          "grant_type": ["refresh_token"]}
        if not reference:
            assert stored()["nousRefreshAttempt"] and stored()["accessToken"] == access
        if scenario == "disconnect":
            self.close_connection = True
            return
        if scenario == "redirect":
            return self.reply(307, {}, extra={"Location": "http://127.0.0.1:52989/must-not-receive"})
        if scenario == "throttled" and ledger.count("renew") == 1:
            return self.reply(429, {})
        if scenario == "concurrent":
            time.sleep(.15)
        if scenario == "management-refresh-failed":
            return self.reply(401, {"error": "invalid_grant", "error_description": refresh})
        self.reply(200, {"access_token": new_access, "refresh_token": new_refresh, "expires_in": 3600})

    def mint(self, raw):
        ledger.append("mint")
        assert self.headers.get_all("Authorization") == ["Bearer " + new_access]
        assert self.headers.get("Content-Type") == "application/json"
        assert json.loads(raw) == {"min_ttl_seconds": 7200}
        if scenario == "mint-fail":
            return self.reply(403, {"error": new_access})
        self.reply(200, {"api_key": new_key, "key_id": "offline-new-agent", "expires_at": alive})

    def generate(self, raw):
        key = self.headers.get("Authorization", "").removeprefix("Bearer ")
        ledger.append("infer:" + key)
        assert self.headers.get_all("Authorization") == ["Bearer " + key]
        assert self.headers.get("Content-Type") == "application/json"
        assert key in {api_key, agent_key, access, new_key, new_access}
        payload = json.loads(raw)
        assert payload["model"] == model and payload["messages"] == [{"role": "user", "content": "Fixture prompt"}]
        if scenario == "api-key":
            assert payload["tags"] == ["product=user-supplied-fixture"] and key == api_key
        else:
            assert "tags" not in payload
        if (scenario == "rejected" and key == agent_key) or scenario == "redacted":
            return self.reply(401, {"error": {"message": "Synthetic refusal " + key + " " + access}})
        if payload["stream"]:
            assert self.headers.get("Accept") == "text/event-stream"
            def chunk(delta, reason):
                return "data: " + json.dumps({"id": "nous-local", "object": "chat.completion.chunk", "created": 1,
                    "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}) + "\n\n"
            wire = chunk({"role": "assistant", "content": message}, None) + chunk({}, "stop") + "data: [DONE]\n\n"
            return self.reply(200, wire.encode(), "text/event-stream")
        self.reply(200, {"id": "nous-local", "object": "chat.completion", "created": 1, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": message}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})

    def probe(self, raw):
        key = self.headers.get("Authorization", "").removeprefix("Bearer ")
        ledger.append("probe:" + key)
        assert self.headers.get_all("Authorization") == ["Bearer " + key]
        assert self.headers.get("Content-Type") == "application/json"
        assert json.loads(raw) == {
            "model": model, "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1, "stream": False,
        }
        assert key in {api_key, agent_key, access, new_key, new_access}
        if scenario == "management-forbidden":
            return self.reply(403, {"error": key})
        if scenario == "management-paid":
            return self.reply(402, {"error": key})
        if scenario == "management-retry-rejected" or scenario == "management-rejected" and key == agent_key:
            return self.reply(401, {"error": key})
        return self.reply(200, {})

    def reply(self, status, body, content="application/json", extra=None):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", content)
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)


def configure(name, account):
    global scenario
    scenario = name
    ledger.clear()
    errors.clear()
    if not reference:
        with sqlite3.connect(database) as db:
            db.execute("DELETE FROM providerConnections WHERE provider='nous'")
            db.execute("""INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES ('nous_fixture','nous',?,'Nous Fixture',1,1,?,'2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""",
                (account["authType"], json.dumps(account)))


def stored():
    with sqlite3.connect(database) as db:
        row, = db.execute("SELECT data FROM providerConnections WHERE id='nous_fixture'").fetchone()
    return json.loads(row)


def call(path, payload=None, token="offline-chat-key", headers=None):
    client = HTTPConnection("127.0.0.1", port, timeout=8)
    if headers is None:
        headers = {"Authorization": "Bearer " + token}
    if payload is not None:
        headers = dict(headers, **{"Content-Type": "application/json"})
    try:
        client.request("GET" if payload is None else "POST", path, None if payload is None else json.dumps(payload), headers)
        response = client.getresponse()
        raw = response.read().decode()
        assert all(secret not in raw for secret in (api_key, agent_key, access, refresh, new_access, new_refresh, new_key)), raw
        return response.status, raw
    finally:
        client.close()


def payload(stream=False):
    body = {"model": model, "messages": [{"role": "user", "content": "Fixture prompt"}], "stream": stream}
    if scenario == "api-key":
        body["tags"] = ["product=user-supplied-fixture"]
    return body


def send(stream=False):
    body = payload(stream)
    body["model"] = "nous/" + model
    return call("/v1/chat/completions", body)


def unpack(status, raw, stream=False):
    assert status == 200, (status, raw, errors, ledger)
    if stream:
        frames = [json.loads(row[6:]) for row in raw.splitlines() if row.startswith("data: {")]
        assert "data: [DONE]" in raw
        return "".join(choice.get("delta", {}).get("content", "") for frame in frames for choice in frame["choices"])
    return json.loads(raw)["choices"][0]["message"]["content"]


def boot():
    child = subprocess.Popen([binary, "--no-browser"], cwd=root, env=environment,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and child.poll() is None:
        try:
            if call("/api/health")[0] == 200:
                return child
        except OSError:
            pass
        time.sleep(.025)
    stop(child)
    raise RuntimeError("Native gateway did not start")


def stop(child):
    if child.poll() is None:
        child.terminate()
        child.wait(timeout=8)


def compare():
    observations = {}
    for name, account, stream, renewed, expected_key in [
        ("api-key", {"authType": "apikey", "apiKey": api_key}, False, False, api_key),
        ("minted-stream", live, True, False, agent_key),
        ("fresh-rotation", expired, False, True, new_key),
        ("mint-fail", expired, False, True, new_access),
    ]:
        configure(name, account)
        if reference:
            argument = json.dumps({"credentials": account, "body": payload(stream), "refresh": renewed})
            observation = json.loads(subprocess.check_output([
                "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
                str(root / "rust/reference/nous-inference-reference.mjs"), argument,
            ], cwd=root))
            assert observation["status"] == 200
            completion = observation["content"]
            rotated = observation["rotated"]
        else:
            status, raw = send(stream)
            completion = unpack(status, raw, stream)
            rotated = stored() if renewed else None
        expected = (["renew", "mint"] if renewed else []) + ["infer:" + expected_key]
        assert not errors and ledger == expected, (errors, ledger)
        assert completion == message
        if renewed:
            assert rotated["accessToken"] == new_access and rotated["refreshToken"] == new_refresh
            assert rotated["apiKey"] == expected_key and rotated["expiresIn"] == 3600
            assert rotated["providerSpecificData"].get("agentKeyExpiresAt") == (alive if name == "fresh-rotation" else None)
            if not reference:
                assert rotated["providerSpecificData"]["fixtureAccountMetadata"] == "retained"
                assert not rotated.get("nousRefreshAttempt")
        observations[name] = {"content": completion, "stream": stream, "rotated": renewed}
    print(json.dumps(observations, sort_keys=True))


def strict(child):
    configure("using-live-access", dict(live, providerSpecificData=dict(details, agentKeyExpiresAt=dead)))
    assert unpack(*send()) == message and ledger == ["infer:" + access]
    configure("mint-live-access-expired", dict(live, expiresAt=dead))
    assert unpack(*send()) == message and ledger == ["infer:" + agent_key]

    configure("rejected", live)
    assert unpack(*send()) == message
    assert ledger == ["infer:" + agent_key, "renew", "mint", "infer:" + new_key]

    configure("concurrent", expired)
    with ThreadPoolExecutor(2) as pool:
        first, second = list(pool.map(lambda _: send(), range(2)))
    assert unpack(*first) == unpack(*second) == message
    assert ledger == ["renew", "mint", "infer:" + new_key, "infer:" + new_key]

    configure("throttled", expired)
    assert send()[0] in {401, 503} and not stored().get("nousRefreshAttempt")
    assert unpack(*send()) == message
    assert ledger == ["renew", "renew", "mint", "infer:" + new_key]

    configure("redacted", dict(live, refreshToken=None))
    status, raw = send()
    assert status == 503 and "[401]" in raw and "redacted" in raw, (status, raw, ledger, errors)
    assert ledger == ["infer:" + agent_key]

    configure("redirect", expired)
    assert send()[0] in {401, 503} and stored()["nousRefreshAttempt"]
    assert send()[0] in {401, 503} and ledger == ["renew"]

    configure("disconnect", expired)
    assert send()[0] in {401, 503} and stored()["nousRefreshAttempt"]
    assert send()[0] in {401, 503} and ledger == ["renew"]
    assert fallback_log.read_bytes() == b""
    stop(child)
    replacement = boot()
    try:
        assert send()[0] in {401, 503} and ledger == ["renew"]
        status, public = call("/api/providers", headers={"x-9r-cli-token": "68977b6fd9422fbb"})
        assert status == 200 and "nous_fixture" in public and "nousRefreshAttempt" not in public
    finally:
        stop(replacement)
    assert not errors, errors
    assert fallback_log.read_bytes() == b""
    print("Nous native chat verified: credential selection, streaming, rejection and concurrent renewal, ambiguous refresh persistence, zero fallbacks")


def management():
    admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
    route = "/api/providers/nous_fixture/test"
    observations = {}
    replay = mode in {"management-reference", "management-native"}

    def test(account):
        if reference:
            argument = json.dumps({"credentials": account, "management": True})
            return json.loads(subprocess.check_output([
                "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
                str(root / "rust/reference/nous-inference-reference.mjs"), argument,
            ], cwd=root))
        status, raw = call(route, {}, headers=admin)
        assert status == 200, (status, raw)
        return json.loads(raw)

    for name, account, error, did_refresh, sequence, rotated in [
        ("management-live", live, None, False, ["probe:" + agent_key], False),
        ("management-live-access", dict(live, providerSpecificData=dict(details, agentKeyExpiresAt=dead)),
         None, False, ["probe:" + access], False),
        ("management-forbidden", live, "Access denied", False, ["probe:" + agent_key], False),
        ("management-paid", live, "API returned 402", False, ["probe:" + agent_key], False),
        ("management-no-access", dict(live, accessToken=None), "No access token", False, [], False),
        ("management-expired", expired, None, True, ["renew", "mint", "probe:" + new_key], True),
        ("management-rejected", live, None, True, ["probe:" + agent_key, "renew", "mint", "probe:" + new_key], True),
        ("management-refresh-failed", expired, "Token expired and refresh failed", False, ["renew"], False),
        ("management-retry-rejected", live, "Token invalid or revoked", False,
         ["probe:" + agent_key, "renew", "mint", "probe:" + new_key], True),
    ]:
        if replay and name == "management-live-access":
            continue
        configure(name, account)
        result = test(account)
        assert result == {"valid": error is None, "error": error, "refreshed": did_refresh}, (name, result)
        assert ledger == sequence and not errors, (name, ledger, errors)
        observations[name] = result
        if reference:
            continue
        row = stored()
        assert row["testStatus"] == ("active" if error is None else "error")
        assert row["lastError"] == error
        assert row.get("accessToken") == (new_access if rotated else account["accessToken"])
        if rotated:
            assert row["refreshToken"] == new_refresh and row["apiKey"] == new_key

    if replay:
        print(json.dumps(observations, sort_keys=True))
        return

    configure("management-batch", expired)
    with sqlite3.connect(database) as db:
        db.execute("""INSERT INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('nous_api_key','nous','apikey','Nous API key',2,1,?,'2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""",
            (json.dumps({"apiKey": api_key}),))
    status, raw = call("/api/providers/test-batch", {"mode": "provider", "providerId": "nous"}, headers=admin)
    batch = json.loads(raw)
    assert status == 200 and batch["summary"] == {"total": 2, "passed": 2, "failed": 0}, batch
    results = {row["connectionId"]: row for row in batch["results"]}
    assert {name: row["authType"] for name, row in results.items()} == {"nous_fixture": "oauth", "nous_api_key": "apikey"}
    assert all(row["valid"] and row["error"] is None and "refreshed" not in row for row in results.values())
    assert ledger == ["renew", "mint", "probe:" + new_key, "probe:" + api_key] and not errors, (ledger, errors)
    assert stored()["testStatus"] == "active" and stored()["apiKey"] == new_key
    assert fallback_log.read_bytes() == b""
    print("Nous management passed: direct OAuth tests, rotation flags and persistence, mixed native batch")


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as server, tempfile.TemporaryDirectory(prefix="9router-nous-inference-") as directory:
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    child = None
    try:
        if not reference:
            binary = str(Path(sys.argv[2]).resolve())
            data = Path(directory) / "data"
            database = data / "db/data.sqlite"
            subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)],
                           stderr=subprocess.DEVNULL, check=True)
            home = Path(directory) / "home"
            home.mkdir()
            fallback_log = Path(directory) / "fallback.log"
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            environment = {"PATH": "", "HOME": str(home), "DATA_DIR": str(data),
                           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
                           "NINEROUTER_MIGRATION_DATA_DIR": str(data), "PORT": str(port), "HOSTNAME": "127.0.0.1"}
            child = boot()
        if mode == "strict":
            strict(child)
        elif mode.startswith("management"):
            management()
        else:
            compare()
    finally:
        if child is not None:
            stop(child)
        server.shutdown()
        thread.join()
