from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("key_access_picker_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

KEY_ID = "fixture-picker-id"
MODEL = "openai/gpt-5.4"
COMBO = "picker-fixture"
ALIAS = "picker-model-alias"
DISABLED = "disabled-fixture-not-offered"
MODEL_KEY = "offline-picker-provider-key"
INFERENCE_KEY = "offline-picker-inference-key"
CALLS = []


class DiscoveryTrap(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        CALLS.append(self.path)
        self.send_error(503)


def call(port, method, path, body=None, headers=None):
    headers = dict(headers or {})
    if body is not None:
        headers["Content-Type"] = "application/json"
    with closing(HTTPConnection("127.0.0.1", port, timeout=8)) as request:
        request.request(method, path, json.dumps(body) if body is not None else None, headers)
        response = request.getresponse()
        raw = response.read()
        return response.status, response.headers, json.loads(raw)


def seed(db):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('picker-openai','openai','apikey','Fixture OpenAI',1,1,?,
                    '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (json.dumps({"apiKey": MODEL_KEY}),))
        sql.execute("UPDATE providerConnections SET isActive=1 WHERE id='fixture-live-connection'")
        sql.execute("""INSERT INTO apiKeys
            (id,key,name,machineId,isActive,createdAt,accessRestricted,accessAllow)
            VALUES (?,?,'Fixture picker key','fixture',1,'2026-11-20T00:00:00Z',0,'[]')""",
                    (KEY_ID, INFERENCE_KEY))
        sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES ('fixture-picker-combo',?,'llm',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (COMBO, json.dumps([MODEL])))
        sql.execute("INSERT INTO kv (scope,key,value) VALUES ('modelAliases',?,?)", (ALIAS, json.dumps(MODEL)))


with tempfile.TemporaryDirectory(prefix="9router-key-access-picker-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), DiscoveryTrap)))
    with shared.gateway(scratch, "native", None, []) as (port, db):
        seed(db)
        forwarded = {"X-Forwarded-For": "192.0.2.40"}
        assert call(port, "GET", "/api/models/access-options", headers=forwarded)[0] == 401
        assert call(port, "GET", "/api/models/access-options", headers={
            **forwarded, "Authorization": "Bearer " + INFERENCE_KEY,
        })[0] == 401
        status, headers, _ = call(port, "POST", "/api/auth/login", {"password": "123456"})
        assert status == 200
        cookie = headers["Set-Cookie"].split(";", 1)[0]
        remote = {**forwarded, "Cookie": cookie}
        assert call(port, "GET", "/v1/models", headers=remote)[0] == 401
        assert call(port, "POST", "/api/models/custom", {
            "providerAlias": "openai", "id": DISABLED, "name": "Hidden fixture", "type": "llm",
        }, remote)[0] in (200, 201)
        assert call(port, "POST", "/api/models/disabled", {"providerAlias": "openai", "ids": [DISABLED]}, remote)[0] == 200
        status, _, catalog = call(port, "GET", "/api/models/access-options", headers=remote)
        assert status == 200 and not CALLS, (status, CALLS)
        assert all(set(entry) == {"id", "provider"} for entry in catalog["models"])
        assert MODEL in {entry["id"] for entry in catalog["models"]}
        assert not any(secret in json.dumps(catalog) for secret in (MODEL_KEY, INFERENCE_KEY, DISABLED))
        subprocess.run(["node", str(shared.ROOT / "rust/reference/key-access-picker-contract.mjs"),
                        f"http://127.0.0.1:{port}", KEY_ID], cwd=shared.ROOT,
                       env={"PATH": os.environ["PATH"], "HOME": str(scratch),
                            "NINEROUTER_FIXTURE_COOKIE": cookie}, check=True)
        with sqlite3.connect(db) as sql:
            active, restricted, allow = sql.execute(
                "SELECT isActive,accessRestricted,accessAllow FROM apiKeys WHERE id=?", (KEY_ID,)
            ).fetchone()
        assert active and restricted and json.loads(allow) == [MODEL, COMBO, "openai/fixture-manual-id"]
        assert not CALLS, CALLS
print("Native endpoint key picker passed remote dashboard authentication, stored-only discovery and persisted edits.")
