from base64 import urlsafe_b64encode
from concurrent.futures import ThreadPoolExecutor
from hashlib import sha256
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event, Thread
import time
from urllib.parse import parse_qs
from uuid import UUID

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
secret = "offline-minimax-code-session"
grant = "urn:ietf:params:oauth:grant-type:device_code"
regions = {"minimax-code": "cn", "minimax-code-global": "io"}
auth = {"client_id": ["mcode-public"], "scope": ["agent.default"], "audience": ["agent-backend"]}
failures, history, challenges = [], [], {}
poll_started, poll_released = Event(), Event()


def encode(value):
    return urlsafe_b64encode(value).rstrip(b"=").decode()


def session(label):
    claims = {"authenticated": True, "iat": int(time.time()), "exp": int(time.time()) + 180, "oidcName": label}
    signed = encode(b'{"alg":"HS256"}') + "." + encode(json.dumps(claims).encode())
    signature = encode(hmac.digest(secret.encode(), signed.encode(), sha256))
    return {"Cookie": f"auth_token={signed}.{signature}"}


def verification(provider):
    return f"https://account.minimax.{regions[provider]}/device"


def access(provider):
    return f"offline-{provider}-access"


def refresh(provider):
    return f"offline-{provider}-refresh"


def tokens(provider, rotation=False):
    result = {"access_token": access(provider), "refresh_token": refresh(provider),
              "token_type": "bearer", "scope": "agent.default", "expires_in": 3600}
    if rotation:
        result["access_token"] = "new." + encode(b'{"scp":["agent.default"]}') + ".offline"
        result.pop("scope")
        result["expires_in"] = 7200
        if provider == "minimax-code":
            result["refresh_token"] += "-rotated"
        else:
            result.pop("refresh_token")
    return result


class Issuer(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def deliver(self, status, payload, extra=None):
        raw = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for key, value in (extra or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        try:
            self.run_request()
        except (AssertionError, KeyError, ValueError) as error:
            failures.append(str(error) or "Unexpected issuer request")
            self.deliver(500, {})

    def run_request(self):
        _, prefix, provider, endpoint = self.path.split("/", 3)
        assert prefix == "minimax-code-oauth" and provider in regions
        assert self.headers["Content-Type"] == "application/x-www-form-urlencoded"
        assert self.headers["Accept"] == "application/json"
        assert all(not self.headers.get(name) for name in ("Authorization", "X-User-Pre", "Cookie", "X-Mavis-Agent-Id"))
        fields = parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
        by_user = provider == "minimax-code"
        if endpoint == "oauth2/device/code":
            history.append((provider, "begin"))
            challenge = fields.pop("code_challenge")[0]
            assert len(challenge) == 43 and fields == {**auth, "code_challenge_method": ["S256"]}
            challenges[provider] = challenge
            base = verification(provider)
            if scenario == "unsafe":
                base = "https://account.minimax.io/device" if by_user else "https://account.minimax.io.evil.invalid/device"
            if scenario == "expire":
                return self.deliver(200, {"user_code": "OIN1", "device_code": "provider-private-code",
                                          "verification_uri": base, "expires_in": 1, "interval": 1})
            if by_user:
                self.deliver(200, {"user_code": "OIN1", "verification_url": base,
                                   "expired_in": 60, "interval": 1000})
            else:
                self.deliver(200, {"user_code": "OIN2", "device_code": "provider-private-code",
                                   "verification_uri": base, "verification_uri_complete": base + "?code=OIN2",
                                   "expires_in": 60, "interval": 1})
            return
        assert endpoint == "oauth2/token"
        if fields.get("grant_type") == ["refresh_token"]:
            history.append((provider, "refresh"))
            assert fields == {**auth, "grant_type": ["refresh_token"], "refresh_token": [refresh(provider)]}
            if not reference:
                row = account(provider)
                assert row["refreshToken"] == refresh(provider) and row["minimaxCodeRefreshAttempt"] == encode(sha256(refresh(provider).encode()).digest())
            if scenario == "refresh-invalid":
                return self.deliver(400, {"error": "invalid_grant", "error_description": refresh(provider)})
            return self.deliver(200, {"status": "success", **tokens(provider, True)})
        history.append((provider, "poll"))
        verifier = fields.pop("code_verifier")[0]
        assert len(verifier) == 43 and encode(sha256(verifier.encode()).digest()) == challenges[provider]
        assert fields == {"grant_type": [grant], "client_id": ["mcode-public"],
                          **({"user_code": ["OIN1"]} if by_user else {"device_code": ["provider-private-code"]})}
        if not reference:
            assert verifier != local_proofs[provider]
        if scenario == "redirect":
            return self.deliver(307, {}, {"Location": "http://127.0.0.1:52989/forbidden"})
        if scenario == "concurrent":
            poll_started.set()
            assert poll_released.wait(5)
        if scenario == "denied":
            return self.deliver(403, {"error": "access_denied", "error_description": verifier})
        if scenario == "wrong-scope":
            return self.deliver(200, {**tokens(provider), "scope": "openid"})
        if scenario == "pending" and history.count((provider, "poll")) == 1:
            return self.deliver(200, {"status": "pending"}) if by_user else self.deliver(400, {"error": "authorization_pending"})
        if scenario == "slow-down":
            return self.deliver(200, {"status": "slow_down"})
        self.deliver(200, {**tokens(provider), **({} if reference else {"status": "success"})})


def reset(name):
    global scenario
    scenario = name
    history.clear()
    challenges.clear()
    failures.clear()


def call(path, data=None, headers=None):
    request = http.client.HTTPConnection("127.0.0.1", port, timeout=7)
    try:
        extra = owner.copy() if headers is None else headers.copy()
        if data is not None:
            extra["Content-Type"] = "application/json"
        request.request("POST" if data is not None else "GET", path, None if data is None else json.dumps(data), extra)
        response = request.getresponse()
        raw = response.read()
        assert all(value.encode() not in raw for provider in regions for value in (access(provider), refresh(provider)))
        assert b"provider-private-code" not in raw
        result = json.loads(raw) if response.headers.get_content_type() == "application/json" else raw.decode()
        return response.status, result
    finally:
        request.close()


def action(provider, step):
    return f"/api/oauth/{provider}/{step}"


def begin(provider, headers=None):
    status, result = call(action(provider, "device-code"), headers=headers)
    assert status == 200, (status, result)
    proof = result["codeVerifier"]
    local_proofs[provider] = proof
    assert len(proof) == 43 and UUID(result["device_code"]).version == 4
    return result, {"deviceCode": result["device_code"], "codeVerifier": proof,
                    "extraData": {"_minimaxPollByUser": provider != "minimax-code"}}


def poll(provider, payload, headers=None):
    status, answer = call(action(provider, "poll"), payload, headers)
    assert status == 200, (status, answer)
    return answer


def account(provider, ident=None):
    with sqlite3.connect(database, timeout=3) as db:
        rows = db.execute("SELECT id,authType,data FROM providerConnections WHERE provider=? ORDER BY createdAt DESC",
                          (provider,)).fetchall()
    if ident:
        rows = [row for row in rows if row[0] == ident]
    assert rows and rows[0][1] == "oauth"
    return json.loads(rows[0][2])


def original(provider, *args):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/minimax-code-oauth-reference.mjs"), provider, *args,
    ], cwd=root))


def sanitized(fields):
    return {k: fields[k] for k in ("accessToken", "refreshToken", "expiresIn")}


def compare():
    for provider in regions:
        reset("pending")
        if reference:
            result = original(provider)
        else:
            code, payload = begin(provider)
            assert poll(provider, payload)["error"] == "authorization_pending"
            assert history == [(provider, "begin")]
            time.sleep(1.10)
            pending = poll(provider, payload)
            time.sleep(1.10)
            success = poll(provider, payload)
            assert success["success"] and set(success["connection"]) == {"id", "provider"}
            assert success["connection"]["provider"] == provider
            ident = success["connection"]["id"]
            fields = account(provider, ident)
            result = {"verification": code["verification_uri_complete"] or code["verification_uri"],
                      "interval": int(code["interval"]), "expiresIn": int(code["expires_in"]), "userCode": code["user_code"],
                      "byUser": code["_minimaxPollByUser"], "pending": pending["error"], "grant": sanitized(fields)}
            assert poll(provider, payload)["error"] == "expired_token"
        assert history == [(provider, step) for step in ("begin", "poll", "poll")] and not failures
        print(json.dumps(result, sort_keys=True))


def strict():
    cn, overseas = regions
    for provider in regions:
        assert call(action(provider, "device-code"), headers={}) == (401, {"error": "Unauthorized"})
        assert call(action(provider, "poll"), {}, headers={}) == (401, {"error": "Unauthorized"})
        assert call(action(provider, "poll"), {}) == (400, {"error": "Missing device code"})
        page, html = call(f"/dashboard/providers/{provider}")
        assert page == 200 and f'data-provider="{provider}"' in html and "/__9router-ui/provider-account.js" in html
    reset("concurrent")
    _, payload = begin(cn)
    assert poll(cn, payload, session("other"))["error"] == "expired_token"
    assert poll(overseas, payload)["error"] == "expired_token"
    assert poll(cn, {**payload, "codeVerifier": "fake-proof"})["error"] == "expired_token"
    time.sleep(1.10)
    with ThreadPoolExecutor(max_workers=1) as tasks:
        first = tasks.submit(poll, cn, payload)
        try:
            assert poll_started.wait(4)
            assert poll(cn, payload) == {"success": False, "error": "authorization_pending", "pending": True}
        finally:
            poll_released.set()
        assert first.result()["success"]
    assert history == [(cn, "begin"), (cn, "poll")] and not failures
    for name, expected in (("redirect", "poll_failed"), ("denied", "access_denied"),
                           ("wrong-scope", "invalid_token_response"), ("expire", "expired_token")):
        reset(name)
        _, payload = begin(overseas)
        time.sleep(1.10)
        assert poll(overseas, payload) == {"success": False, "error": expected, "pending": False}
        assert history == [(overseas, "begin")] + ([] if name == "expire" else [(overseas, "poll")])
        assert not failures
    reset("unsafe")
    for provider in regions:
        assert call(action(provider, "device-code")) == (502, {"error": "Could not start MiniMax Code device authorization"})
    assert history == [(p, "begin") for p in regions] and not failures
    reset("slow-down")
    _, payload = begin(cn)
    time.sleep(1.10)
    assert poll(cn, payload)["error"] == "slow_down"
    time.sleep(1.10)
    assert poll(cn, payload)["error"] == "authorization_pending"
    assert history == [(cn, "begin"), (cn, "poll")] and not failures


def rotate(expected):
    cn, overseas = regions
    reset("success")
    created = {}
    for provider in regions:
        _, payload = begin(provider)
        time.sleep(1.10)
        result = poll(provider, payload)
        assert result["success"], result
        created[provider] = result["connection"]["id"]
        row = account(provider, created[provider])
        assert row["accessToken"] == access(provider) and row["refreshToken"] == refresh(provider)
        assert row["expiresAt"] > "2000"
    with sqlite3.connect(database, timeout=3) as db:
        for ident in created.values():
            db.execute("UPDATE providerConnections SET data=json_set(data,'$.expiresAt',?) WHERE id=?",
                       ("2000-01-01T00:00:00Z", ident))
    reset(expected)
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        if len(history) == 2:
            time.sleep(.4)
            break
        time.sleep(.05)
    assert sorted(history) == sorted([(cn, "refresh"), (overseas, "refresh")]) and not failures
    for provider in regions:
        row = account(provider, created[provider])
        status, public = call("/api/providers/" + created[provider])
        assert status == 200 and "minimaxCodeRefreshAttempt" not in json.dumps(public)
        if expected == "refresh-invalid":
            assert row["accessToken"] == access(provider) and row["refreshToken"] == refresh(provider)
            assert row["minimaxCodeRefreshAttempt"]
        else:
            expected_refresh = refresh(provider) + ("-rotated" if provider == cn else "")
            replacement = tokens(provider, True)
            assert sanitized(row) == {"accessToken": replacement["access_token"], "refreshToken": expected_refresh, "expiresIn": 7200}
            assert row["expiresAt"] > "2000" and row["lastRefreshAt"] and not row.get("minimaxCodeRefreshAttempt")
            print(json.dumps(sanitized(row), sort_keys=True))


owner = session("owner")
local_proofs = {}
with ThreadingHTTPServer(("127.0.0.1", 52989), Issuer) as server, tempfile.TemporaryDirectory(prefix="minimax-code-oauth-") as scratch:
    watcher = Thread(target=server.serve_forever, daemon=True)
    watcher.start()
    gateway = None
    try:
        if not reference and mode != "reference-refresh":
            temp = Path(scratch)
            home, data, fallback = temp / "home", temp / "data", temp / "fallback"
            home.mkdir()
            data.mkdir()
            database = data / "db/data.sqlite"
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            environment = {"PATH": "", "HOME": str(home), "JWT_SECRET": secret, "DATA_DIR": str(data),
                           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
                           "NINEROUTER_MIGRATION_DATA_DIR": str(data), "PORT": str(port), "HOSTNAME": "127.0.0.1"}
            with (temp / "gateway.log").open("w") as log:
                gateway = subprocess.Popen([str(Path(sys.argv[2]).resolve()), "--no-browser", "--no-tray"],
                                           cwd=root, env=environment, stdout=log, stderr=log)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and gateway.poll() is None:
                try:
                    if call("/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError((temp / "gateway.log").read_text())
        if mode == "strict":
            strict()
        elif mode == "reference-refresh":
            reference = True
            reset("refresh-success")
            for provider in regions:
                print(json.dumps(sanitized(original(provider, refresh(provider))), sort_keys=True))
            assert len(history) == 2 and not failures
        elif mode in ("refresh-success", "refresh-invalid"):
            rotate(mode)
        else:
            compare()
        if gateway:
            assert not fallback.exists() or not fallback.read_bytes()
    finally:
        if gateway and gateway.poll() is None:
            gateway.terminate()
            gateway.wait(8)
        server.shutdown()
        watcher.join()
