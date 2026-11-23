import base64
import concurrent.futures
import datetime as dt
import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
native_auth = "https://api.openai.com/auth"
profile = "https://api.openai.com/profile"
single_path = "/api/oauth/codex/import-token"
bulk_path = "/api/oauth/codex/bulk-import"


def claims(payload, standard=False):
    encode = base64.b64encode if standard else base64.urlsafe_b64encode
    return "fixture." + encode(json.dumps(payload, ensure_ascii=False).encode()).decode().rstrip("=") + ".synthetic"


def request(port, path, method="POST", payload=None, headers=cli, raw=None):
    headers = headers.copy()
    if payload is not None or raw is not None:
        headers["content-type"] = "application/json"
    body = raw if raw is not None else json.dumps(payload).encode() if payload is not None else None
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, path, body=body, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        client.close()


with tempfile.TemporaryDirectory(prefix="9router-codex-import-contract-") as folder:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallbacks = Path(folder) / "fallbacks.txt"
    env = {
        **os.environ, "PORT": str(port), "DATA_DIR": folder, "TMPDIR": folder,
        "NINEROUTER_MIGRATION_DATA_DIR": folder, "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(name, None)
    process = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("Codex import fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        def single(token, **fields):
            status, body = request(port, single_path, payload={"accessToken": token, **fields})
            assert status == 200 and body["success"] is True and token not in json.dumps(body)
            return body["connection"]

        def bulk(payload):
            status, body = request(port, bulk_path, payload=payload)
            assert status == 200
            return body

        for route in [single_path, bulk_path]:
            assert request(port, route, payload={}, headers={}) == (401, {"error":"Unauthorized"})
        assert request(port, single_path, payload={"accessToken": 33}) == (400, {"error":"Access token is required"})
        assert request(port, bulk_path, payload={"accounts":[]}) == (400, {"error":"No accounts provided"})
        status, error = request(port, bulk_path, raw=b'{"accessToken":"offline-unclosed-secret"')
        assert status == 400 and "offline-unclosed-secret" not in json.dumps(error)

        token = claims({
            profile: {"email":"profile-fixture@example.invalid"}, "email":"ignored@example.invalid",
            native_auth: {"chatgpt_account_id":"fixture-single-workspace","chatgpt_plan_type":"team"},
            "exp":2300090100,
        }, standard=True)
        summary = single("  " + token + "\t")
        assert summary["email"] == summary["name"] == "profile-fixture@example.invalid"
        assert summary["workspace"] == "fixture-single-workspace" and summary["plan"] == "team"
        duplicate = single(token, name="Second single entry")
        assert duplicate["id"] != summary["id"]
        fallback_token = claims({"account_id":"plain-workspace","plan_type":"free"})
        fallback = single(fallback_token)
        assert fallback["workspace"] == "plain-workspace" and fallback["plan"] == "free"
        assert fallback["email"] is None and fallback["name"] == "ChatGPT Access Token"
        opaque = single("opaque-single-fixture", name="Raw retained")
        redacted = single("opaque-summary-fixture", name="Label opaque-summary-fixture")
        assert redacted["name"] == "Label [redacted]"

        admin_email = "fixture-oauth@example.invalid"
        jwt_access = claims({"email":"ignored-when-id-present@example.invalid","account_id":"ignored"})
        id_token = claims({
            "email": admin_email,
            native_auth: {"chatgpt_account_id":"ws-1","chatgpt_plan_type":"plus"},
        })
        cases = {"accounts":[
            {"accessToken":jwt_access, "idToken":id_token, "refreshToken":"old-refresh-fixture",
             "id":"pretend-id", "provider":"pretend-provider", "authType":"apikey",
             "createdAt":"pretend-created", "updatedAt":"pretend-updated",
             "name":"Initial OAuth", "priority":41, "isActive":False, "expiresIn":3600,
             "lastError":"fixture previous error","errorCode":432},
            False,
            {"accessToken":""},
            {"accessToken":"bad-shape", "providerSpecificData":"bad fixture data"},
            {"accessToken":"new-access-fixture", "email":admin_email,
             "providerSpecificData":{"chatgptAccountId":"ws-1","chatgptPlanType":"pro"}},
            {"accessToken":"blocked-overwrite", "email":admin_email,
             "providerSpecificData":{"chatgptAccountId":"ws-1"}, "allowOverwrite":False},
            {"accessToken":"different-workspace", "email":admin_email,
             "providerSpecificData":{"chatgptAccountId":"ws-2"}},
            {"accessToken":"bare-fixture-1","email":admin_email},
            {"accessToken":"bare-fixture-2","email":admin_email},
            {"accessToken":"other-email", "email":"fixture-other@example.invalid",
             "providerSpecificData":{"chatgptAccountId":"ws-1"}},
        ]}
        result = bulk(cases)
        assert result["success"] == 6 and result["failed"] == 4
        items = result["results"]
        assert [entry["index"] for entry in items] == list(range(10))
        assert items[0]["id"] == items[4]["id"]
        assert [items[i]["error"] for i in range(1,4)] == [
            "Item is not an object", "Missing accessToken", "Invalid providerSpecificData",
        ]
        assert items[5]["ok"] is False and "Initial OAuth" in items[5]["error"]
        assert len({items[i]["id"] for i in [0,6,7,8,9]}) == 5
        assert not any(secret in json.dumps(result) for secret in [jwt_access,id_token,"old-refresh-fixture","new-access-fixture"])

        raw_duplicate = bulk([
            {"accessToken":"malformed-name-transaction","name":{"not":"scalar"},
             "email":admin_email,"providerSpecificData":{"chatgptAccountId":"ws-1"}},
            {"accessToken":"successful-suffix","email":"fixture-next@example.invalid"},
        ])
        assert raw_duplicate["failed"] == 1 and raw_duplicate["results"][1]["ok"]

        def simultaneous(index):
            return bulk({"accessToken": f"concurrent-fixture-{index}", "email":"parallel-fixture@example.invalid",
                         "providerSpecificData":{"chatgptAccountId":"parallel-ws"}})["results"][0]

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as callers:
            uploads = list(callers.map(simultaneous, range(8)))
        assert all(item["ok"] for item in uploads) and len({item["id"] for item in uploads}) == 1

        with sqlite3.connect(Path(folder) / "db" / "data.sqlite") as db:
            db.row_factory = sqlite3.Row
            def row(id):
                return dict(db.execute("SELECT * FROM providerConnections WHERE id=?", (id,)).fetchone())

            single_row = row(summary["id"])
            assert single_row["provider"] == "codex" and single_row["authType"] == "access_token"
            parsed = json.loads(single_row["data"])
            assert parsed["accessToken"] == token and parsed["testStatus"] == "active"
            assert parsed["providerSpecificData"] == {
                "authMethod":"access_token",
                "chatgptAccountId":"fixture-single-workspace","chatgptPlanType":"team","jwtExp":2300090100,
            }
            assert json.loads(row(opaque["id"])["data"])["accessToken"] == "opaque-single-fixture"

            oauth = row(items[0]["id"])
            fields = json.loads(oauth["data"])
            assert oauth["provider"] == "codex" and oauth["authType"] == "oauth"
            assert oauth["id"] != "pretend-id" and oauth["createdAt"] != "pretend-created" and oauth["updatedAt"] != "pretend-updated"
            assert oauth["isActive"] == 1 and oauth["name"] == "Initial OAuth" and oauth["priority"] == 41
            assert fields["accessToken"] == "new-access-fixture"
            assert fields["refreshToken"] == "old-refresh-fixture" and fields["idToken"] == id_token
            assert fields["testStatus"] == "active" and fields["lastError"] is None and fields["errorCode"] is None
            assert fields["providerSpecificData"] == {"chatgptAccountId":"ws-1","chatgptPlanType":"pro"}
            expires = dt.datetime.fromisoformat(fields["expiresAt"])
            assert 3500 < (expires - dt.datetime.now(dt.timezone.utc)).total_seconds() <= 3600
            assert row(items[6]["id"])["priority"] == 42
            assert db.execute("SELECT count(*) FROM providerConnections WHERE email='parallel-fixture@example.invalid'").fetchone()[0] == 1
        assert not fallbacks.exists() or not fallbacks.read_text().strip()
        print("Codex token claims, distinct direct imports, OAuth workspace dedup, rollback and concurrent commits passed")
    finally:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
