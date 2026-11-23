import base64
import concurrent.futures
import datetime
import hashlib
import http.server
import json
import os
import pathlib
import select
import sqlite3
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE in {"native", "strict"}:
    executable, case = ARGS
    command = [executable]
else:
    case, = ARGS
    command = ["node", "--no-warnings", str(ROOT / "original.mjs")]

REQUESTS = []
POLL = 0
CLAUDE_PENDING = None
CODEX_PENDING = None
REFRESH_STARTED = threading.Event()
REFRESH_RELEASED = threading.Event()
CODE = "fixture-device-code"
CLAUDE_ACCESS = "fixture-claude-token"
GOOGLE_ACCESS = "fixture-google-token"
GOOGLE_ROTATED = "fixture-google-new-token"
GITHUB_ACCESS = "fixture-github-token"
COPILOT = "fixture-copilot-token"
CONCURRENT_COPILOT = "fixture-other-copilot-token"
REFRESH = "fixture-refresh"
GOOGLE_SECRET = "fixture-google-secret"
CODEX_ACCESS = "fixture-codex-token"
OLD_REFRESH = "fixture-prior-refresh"
ROTATED_REFRESH = "fixture-rotated-refresh"
PRIOR_ACCESS = "fixture-prior-access"
ROTATED_ACCESS = "fixture-rotated-access"
CONCURRENT_ACCESS = "fixture-other-writer-access"
CONCURRENT_REFRESH = "fixture-other-writer-refresh"
CLINE_ACCESS = "eyJfixture.profile.mock"
CLINE_ROTATED = "eyJfixture.rotated.mock"
CLINE_UNKNOWN = "eyJfixture.unknown.mock"
CLINE_EMAIL = "verified-cline@example.invalid"
REFRESH_ID = "fixture-refresh-account"
REFRESH_NUM = 0

def token_claims(payload):
    encoded = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
    return "eyJhbGciOiJub25lIn0." + encoded + ".fixture-signature"

def workspace_token(workspace):
    payload = {"email": "codex@example.invalid", "https://api.openai.com/auth": {"chatgpt_plan_type": "team"}}
    if workspace is not None:
        payload["https://api.openai.com/auth"]["chatgpt_account_id"] = workspace
    return token_claims(payload)

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
REDIRECT = "http://localhost:20128/callback"


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass

    def do_POST(self):
        global POLL
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0"))).decode()
        if self.path in {"/cline/token", "/clinepass/token"}:
            body = json.loads(raw)
            valid = self.headers.get("Accept") == "application/json" and (
                self.headers.get("Content-Type") == "application/json") and body == {
                "code": "fixture-code", "grant_type": "authorization_code",
                "client_type": "extension", "redirect_uri": REDIRECT,
            }
            REQUESTS.append({"path": self.path, "valid": valid})
            if case == "cline-token-rejected":
                return self.send(403, {"accessToken": CLINE_ACCESS, "refreshToken": REFRESH})
            tokens = {"accessToken": CLINE_ACCESS, "refreshToken": REFRESH}
            if case == "clinepass-opaque-flat":
                return self.send(200, tokens)
            return self.send(200, {"success": True, "data": {**tokens, "userInfo": {"email": CLINE_EMAIL}}})
        if self.path in {"/cline/refresh", "/clinepass/refresh"}:
            body = json.loads(raw)
            valid = self.headers.get("Accept") == "application/json" and body == {
                "refreshToken": REFRESH, "grantType": "refresh_token", "clientType": "extension",
            }
            REQUESTS.append({"path": self.path, "valid": valid})
            return self.send(200, {"success": True, "data": {
                "accessToken": CLINE_ROTATED, "refreshToken": ROTATED_REFRESH,
            }})
        if case.startswith("refresh-") and self.path in {"/claude/token", "/codex/token", "/google/token", "/github/poll"}:
            return self.refresh_response(raw)
        if self.path == "/codex/token":
            body = dict(urllib.parse.parse_qsl(raw))
            state, verifier, redirect = CODEX_PENDING
            valid = body == {
                "code": body.get("code"), "grant_type": "authorization_code", "client_id": "fixture-codex-client",
                "redirect_uri": redirect, "code_verifier": verifier,
            } and body["code"] in {"fixture-code", "fixture-code-rotated"}
            REQUESTS.append({"path": self.path, "valid": valid})
            workspace = None if case == "codex-no-workspace" else (
                "workspace-b" if case == "codex-workspaces" and body["code"] == "fixture-code-rotated" else "workspace-a"
            )
            return self.send(200, {"access_token": CODEX_ACCESS, "refresh_token": REFRESH,
                                   "id_token": workspace_token(workspace), "expires_in": 3600})
        if self.path == "/claude/token":
            body = json.loads(raw)
            state, verifier = CLAUDE_PENDING
            valid = body == {
                "code": "fixture-code", "state": state, "grant_type": "authorization_code",
                "client_id": "fixture-claude-client", "redirect_uri": REDIRECT, "code_verifier": verifier,
            }
            REQUESTS.append({"path": self.path, "valid": valid})
            return self.send(200, {"access_token": CLAUDE_ACCESS, "refresh_token": REFRESH, "expires_in": 3600, "scope": "user:profile"})
        if self.path == "/google/project":
            body = json.loads(raw)
            REQUESTS.append({"path": self.path, "valid": self.authorized(GOOGLE_ACCESS, GOOGLE_ROTATED), "shape": body})
            if case == "gemini-no-project":
                return self.send(503, {"unavailable": True})
            return self.send(200, {"cloudaicompanionProject": {"id": "fixture-project"}})
        body = dict(urllib.parse.parse_qsl(raw))
        if self.path == "/google/token":
            valid = body == {
                "grant_type": "authorization_code", "client_id": "fixture-google-client", "client_secret": GOOGLE_SECRET,
                "redirect_uri": REDIRECT, "code": body.get("code", ""),
            } and body["code"] in {"fixture-code", "fixture-code-rotated"}
            REQUESTS.append({"path": self.path, "valid": valid})
            if case == "gemini-rejected":
                return self.send(403, {"error": GOOGLE_SECRET})
            token = GOOGLE_ROTATED if body["code"] == "fixture-code-rotated" else GOOGLE_ACCESS
            return self.send(200, {"access_token": token, "refresh_token": REFRESH, "expires_in": 3600, "scope": "email profile"})
        if self.path == "/github/device":
            REQUESTS.append({"path": self.path, "valid": body == {"client_id": "fixture-github-client", "scope": "read:user"}})
            return self.send(200, {"device_code": CODE, "user_code": "TEST-USER", "verification_uri": "https://example.invalid/device", "expires_in": 900, "interval": 5})
        if self.path == "/github/poll":
            valid = body == {
                "client_id": "fixture-github-client", "device_code": CODE,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }
            REQUESTS.append({"path": self.path, "valid": valid})
            POLL += 1
            if case == "github-concurrent":
                time.sleep(0.25)
            if case == "github-success" and POLL == 1:
                return self.send(200, {"error": "authorization_pending", "error_description": "Verification pending"})
            if case == "github-slow":
                return self.send(200, {"error": "slow_down", "error_description": "Wait for the next interval"})
            if case == "github-denied":
                return self.send(200, {"error": "access_denied", "error_description": "Denied"})
            return self.send(200, {"access_token": GITHUB_ACCESS, "scope": "read:user", "token_type": "bearer"})
        return self.send(404, {})

    def refresh_response(self, raw):
        global REFRESH_NUM
        provider = self.path.split("/")[1]
        config = {"grant_type": "refresh_token", "client_id": f"fixture-{provider}-client", "refresh_token": OLD_REFRESH}
        if provider == "google":
            config["client_secret"] = GOOGLE_SECRET
        json_body = provider in {"claude", "codex"}
        body = json.loads(raw) if json_body else dict(urllib.parse.parse_qsl(raw))
        media = self.headers.get("Content-Type", "")
        content_valid = media.startswith("application/json" if json_body else "application/x-www-form-urlencoded")
        valid = body == config and content_valid and self.headers.get("Accept") == "application/json"
        REQUESTS.append({"path": self.path, "valid": valid, "encoding": "json" if json_body else "form"})
        REFRESH_NUM += 1
        REFRESH_STARTED.set()
        if case in {"refresh-concurrent", "refresh-concurrent-invalid", "refresh-cancel"}:
            time.sleep(0.2)
        if case in {"refresh-other-writer", "refresh-deleted", "refresh-disabled", "refresh-metadata", "refresh-github-other-copilot", "refresh-copilot-raced"}:
            assert REFRESH_RELEASED.wait(5)
        if case in {"refresh-codex-invalid", "refresh-concurrent-invalid"}:
            return self.send(401, {"error": {"code": "refresh_token_reused"}, "message": OLD_REFRESH})
        data = {"access_token": ROTATED_ACCESS, "expires_in": 3600}
        if provider != "google":
            data["refresh_token"] = ROTATED_REFRESH
        if provider == "codex":
            data["id_token"] = workspace_token("workspace-a")
        return self.send(200, data)

    def do_GET(self):
        if self.path in {"/cline/user", "/clinepass/user"}:
            valid = self.authorized(
                "workos:" + CLINE_ACCESS, "workos:" + CLINE_ROTATED, "workos:" + CLINE_UNKNOWN,
            ) and self.headers.get("HTTP-Referer") == "https://cline.bot" and (
                self.headers.get("X-Client-Type") == "9router") and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path": self.path, "valid": valid})
            if self.authorized("workos:" + CLINE_UNKNOWN):
                return self.send(401, {"error": "invalid token", "mirrored": CLINE_UNKNOWN})
            unrelated = case == "cline-direct-mismatched" and self.authorized("workos:" + CLINE_ROTATED)
            return self.send(200, {"success": True, "data": {"user": {
                "id": "another-account" if unrelated else "fixture-user-id",
                "email": "different@example.invalid" if unrelated else CLINE_EMAIL,
                "firstName": "Verified", "lastName": "Profile",
            }}})
        if self.path == "/google/user?alt=json":
            REQUESTS.append({"path": self.path, "valid": self.authorized(GOOGLE_ACCESS, GOOGLE_ROTATED)})
            return self.send(200, {"email": "google@example.invalid"})
        if case.startswith("refresh-") and self.path == "/github/copilot":
            prior_probe = case == "refresh-github-rejected-rotated" and REFRESH_NUM == 0
            expected = PRIOR_ACCESS if prior_probe or case in {"refresh-copilot-only", "refresh-copilot-raced", "refresh-github-rejected"} else ROTATED_ACCESS
            valid = (self.headers.get("Authorization") == "token " + expected
                and self.headers.get("User-Agent") == "GitHubCopilotChat/0.38.0"
                and self.headers.get("X-GitHub-Api-Version") == "2025-04-01"
                and self.headers.get("Editor-Version") == "vscode/1.110.0"
                and self.headers.get("Editor-Plugin-Version") == "copilot-chat/0.38.0")
            REQUESTS.append({"path": self.path, "valid": valid})
            if case == "refresh-copilot-raced":
                REFRESH_STARTED.set()
                assert REFRESH_RELEASED.wait(5)
            if prior_probe:
                return self.send(401, {"error":"test OAuth access was rejected"})
            return self.send(200, {"token": COPILOT, "expires_at": 1900000000})
        if self.path in {"/github/copilot", "/github/user"}:
            valid = self.authorized(GITHUB_ACCESS) and self.headers.get("X-GitHub-Api-Version") == "2022-11-28" and self.headers.get("User-Agent") == "GitHubCopilotChat/0.26.7"
            REQUESTS.append({"path": self.path, "valid": valid})
            if self.path == "/github/copilot":
                return self.send(200, {"token": COPILOT, "expires_at": 1900000000})
            return self.send(200, {"login": "fixture-login", "name": "Fixture GitHub", "email": "github@example.invalid", "id": 123})
        return self.send(404, {})

    def authorized(self, *tokens):
        return any(self.headers.get("Authorization") == "Bearer " + token for token in tokens)


def request(port, provider, action, query=None, data=None, auth=True):
    path = "/fixture/refresh" if provider == "fixture" else f"/api/oauth/{provider}/{action}"
    url = f"http://127.0.0.1:{port}{path}"
    if query:
        url += "?" + urllib.parse.urlencode(query)
    headers = {"content-type": "application/json"}
    if auth:
        headers["x-replay-admin"] = "fixture"
    encoded = json.dumps(data).encode() if data is not None else None
    try:
        result = OPENER.open(urllib.request.Request(url, data=encoded, headers=headers), timeout=6)
    except urllib.error.HTTPError as error:
        result = error
    with result:
        raw = result.read()
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            payload = {}
        return result.status, payload


def status(port, provider, action, expect=200, **kwargs):
    actual, data = request(port, provider, action, **kwargs)
    assert actual == expect, (action, actual, expect)
    return data


def authorization(port, provider):
    global CLAUDE_PENDING, CODEX_PENDING
    redirect = "http://localhost:1455/auth/callback" if provider == "codex" else REDIRECT
    data = status(port, provider, "authorize", query={"redirect_uri": redirect})
    assert len(data["state"]) == len(data["codeVerifier"]) == 43
    assert base64.urlsafe_b64encode(hashlib.sha256(data["codeVerifier"].encode()).digest()).decode().rstrip("=") == data["codeChallenge"]
    assert data["callbackPath"] == ("/auth/callback" if provider == "codex" else "/callback")
    assert data["redirectUri"] == redirect
    if provider == "codex":
        CODEX_PENDING = (data["state"], data["codeVerifier"], redirect)
        assert data["fixedPort"] == 1455
        parsed = urllib.parse.urlsplit(data["authUrl"])
        query = dict(urllib.parse.parse_qsl(parsed.query))
        assert query["code_challenge"] == data["codeChallenge"] and query["code_challenge_method"] == "S256"
        assert query["id_token_add_organizations"] == query["codex_cli_simplified_flow"] == "true"
        assert query["originator"] == "codex_cli_rs" and "%20" in parsed.query
    if provider == "claude":
        CLAUDE_PENDING = (data["state"], data["codeVerifier"])
        query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(data["authUrl"]).query))
        assert query["code_challenge"] == data["codeChallenge"] and query["code_challenge_method"] == "S256"
    return data


def summary_authorize(data):
    public = {"flow": data["flowType"], "matches_pkce": True}
    if data["authUrl"]:
        url = urllib.parse.urlparse(data["authUrl"])
        params = dict(urllib.parse.parse_qsl(url.query))
        if url.path.split("/")[1] in {"cline", "clinepass"}:
            if MODE in {"native", "strict"}:
                assert params.pop("state") == data["state"]
            assert params == {"client_type": "extension", "callback_url": REDIRECT, "redirect_uri": REDIRECT}
            public.update(path=url.path, parameters=sorted(params))
            return public
        assert params["state"] == data["state"]
        assert params["client_id"] == f"fixture-{url.path.split('/')[1]}-client"
        public.update(path=url.path, scope=params["scope"].split(" "), parameters=sorted(params))
    return public


def submit(port, auth, code="fixture-code", provider="gemini-cli", **kwargs):
    body = {"code": code, "state": auth["state"], "codeVerifier": auth["codeVerifier"], "redirectUri": auth["redirectUri"]}
    body.update(kwargs)
    return request(port, provider, "exchange", data=body)


def summarized_connection(body):
    assert body["success"]
    connection = body["connection"].copy()
    uuid.UUID(connection.pop("id"))
    return connection


def cline_pasted_code(access, refresh):
    payload = {
        "accessToken": access, "email": "asserted-only@example.invalid",
        "firstName": "Unverified", "lastName": "Claim", "expiresAt": "invalid",
    }
    if refresh:
        payload["refreshToken"] = REFRESH
    return base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")


def cline_state_case(port):
    provider = "clinepass" if case.startswith("clinepass") else "cline"
    auth = authorization(port, provider)
    summary_authorize(auth)
    if case.endswith("-state"):
        other = "cline" if provider == "clinepass" else "clinepass"
        assert request(port, provider, "exchange", data={"code": "unread"}, auth=False)[0] == 401
        assert submit(port, auth, provider=provider, state=None)[0] == 400
        assert submit(port, auth, provider=provider, codeVerifier="wrong")[0] == 400
        assert submit(port, auth, provider=provider, redirectUri="http://localhost:9/callback")[0] == 400
        assert submit(port, auth, provider=other)[0] == 400
        assert not REQUESTS
        actual, body = submit(port, auth, provider=provider)
        assert actual == 200 and summarized_connection(body)["email"] == CLINE_EMAIL
        assert submit(port, auth, provider=provider)[0] == 400
        assert len(REQUESTS) == 1
        return {"state_bound": True, "replay_rejected": True}
    if case == "cline-token-rejected":
        actual, body = submit(port, auth, provider=provider)
        assert actual == 500 and body == {"error": "Cline token exchange failed"}
        assert submit(port, auth, provider=provider)[0] == 400
        return {"rejected_and_redacted": True}
    access = CLINE_UNKNOWN if case == "cline-direct-unverified" else CLINE_ACCESS
    use_refresh = case != "clinepass-direct-access-only"
    actual, body = submit(port, auth, cline_pasted_code(access, use_refresh), provider=provider)
    assert submit(port, auth, provider=provider)[0] == 400
    if case in {"cline-direct-unverified", "cline-direct-mismatched"}:
        assert actual == 400 and body == {"error": "Could not verify the Cline OAuth credentials"}
        assert len(REQUESTS) == (1 if case == "cline-direct-unverified" else 3)
        return {"credential_rejected": True}
    assert actual == 200 and summarized_connection(body)["email"] == CLINE_EMAIL
    assert len(REQUESTS) == (3 if use_refresh else 1)
    return {"profile_verified": True, "refresh_verified": use_refresh}


def state_case(port):
    if case == "codex-native-callback":
        auth = authorization(port, "codex")
        pending = {"app_port":port,"state":auth["state"],"code_verifier":auth["codeVerifier"],
                   "redirect_uri":auth["redirectUri"]}
        assert status(port, "codex", "start-proxy", expect=401, query=pending, auth=False)
        assert status(port, "codex", "start-proxy", expect=400, query={**pending,"code_verifier":"wrong"})
        with socket.socket() as unrelated:
            unrelated.bind(("127.0.0.1", 1455))
            unrelated.listen()
            assert status(port, "codex", "start-proxy", query=pending) == {
                "success":False,"reason":"port_busy","serverSide":False,
            }
        assert status(port, "codex", "start-proxy", query=pending) == {"success":True,"serverSide":True}
        assert status(port, "codex", "poll-status", query={"state":auth["state"]}) == {"status":"pending"}

        def callback(path, nonce, headers=None):
            values = urllib.parse.urlencode({"code":"fixture-code","state":nonce})
            url = f"http://127.0.0.1:1455{path}?{values}"
            try:
                result = OPENER.open(urllib.request.Request(url, headers=headers or {}), timeout=6)
            except urllib.error.HTTPError as error:
                result = error
            with result:
                return result.status, result.read().decode()

        assert callback("/auth/callback", auth["state"], {"Origin":"https://unrelated.example.invalid"})[0] == 403
        assert callback("/callback", auth["state"])[0] == 400
        assert callback("/auth/callback", "unissued-state")[0] == 400
        assert REQUESTS == []
        assert callback("/auth/callback", auth["state"]) == (200, "Authorization saved. You can close this tab.")
        result = status(port, "codex", "poll-status", query={"state":auth["state"]})
        assert result.get("status") == "done" and result.get("connectionId") and result.get("email") == "codex@example.invalid"
        assert status(port, "codex", "poll-status", query={"state":auth["state"]}) == {"status":"unknown"}
        assert len(REQUESTS) == 1 and REQUESTS[0]["valid"]
        return {"port_ownership":True,"callback_verified":True,"terminal_status_consumed":True}
    if case.startswith("cline"):
        return cline_state_case(port)
    if case == "gemini-rejected":
        code, error = submit(port, authorization(port, "gemini-cli"))
        assert code == 500 and "fixture-google" not in error.get("error", "")
        return {"redacted": True}
    if case == "github-unissued":
        assert status(port, "github", "poll", data={"deviceCode": CODE})["error"] == "expired_token"
        return {"unissued_rejected": True}
    if case == "unauthorized":
        assert request(port, "github", "device-code", auth=False)[0] == 401
        return {"unauthorized": True}
    if case == "codex-raw":
        assert request(port, "codex", "exchange", data={"code": workspace_token("workspace-import")})[0] == 501
        return {"raw_import_delegated": True}
    if case == "unhandled":
        assert request(port, "qwen", "authorize")[0] == 501
        return {"delegated": True}
    if case == "github-concurrent":
        status(port, "github", "device-code")
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: status(port, "github", "poll", data={"deviceCode": CODE}), range(2)))
        assert sum(bool(reply["success"]) for reply in results) == 1 and POLL == 1
        return {"one_grant": True}
    if case == "claude-mismatched-suffix":
        auth = authorization(port, "claude")
        assert submit(port, auth, "fixture-code#wrong", provider="claude")[0] == 400
        assert REQUESTS == []
        assert submit(port, auth, provider="claude")[0] == 200
        return {"code_state_mismatch_rejected": True}
    auth = authorization(port, "gemini-cli")
    if case == "cross-provider":
        assert submit(port, auth, provider="claude")[0] == 400
        assert REQUESTS == []
        assert submit(port, auth)[0] == 200
        return {"foreign_provider_rejected": True}
    if case == "gemini-concurrent":
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            assert sorted(pool.map(lambda _: submit(port, auth)[0], range(2))) == [200, 400]
        return {"one_grant": True}
    assert submit(port, auth, state="wrong-state")[0] == 400
    assert submit(port, auth, codeVerifier="wrong-verifier")[0] == 400
    assert submit(port, auth, redirectUri="http://localhost:9999/callback")[0] == 400
    assert REQUESTS == []
    assert submit(port, auth)[0] == 200
    assert submit(port, auth)[0] == 400
    return {"state_mismatches_rejected": True, "replay_rejected": True}


def render_time(value):
    return value.isoformat(timespec="milliseconds").replace("+00:00", "Z")

def load_fixture_account(root):
    with sqlite3.connect(root / "db/data.sqlite") as db:
        db.row_factory = sqlite3.Row
        row = db.execute("SELECT * FROM providerConnections WHERE id=?", (REFRESH_ID,)).fetchone()
        if row is None:
            return None
        account = dict(row)
        fields = json.loads(account.pop("data"))
        account.update(fields)
        account["isActive"] = bool(account["isActive"])
        return account

def seed_refresh(root):
    provider = ("claude" if case == "refresh-claude" else "gemini-cli" if case in {"refresh-gemini", "refresh-fresh"}
                else "github" if case in {"refresh-github", "refresh-copilot-only", "refresh-github-other-copilot", "refresh-copilot-raced", "refresh-github-rejected", "refresh-github-rejected-rotated"} else "codex")
    now = datetime.datetime.now(datetime.timezone.utc)
    ttl = 3600 if provider == "claude" else (-3 if case not in {"refresh-fresh", "refresh-codex-stale", "refresh-codex-rejected", "refresh-copilot-only", "refresh-copilot-raced"} else 86400)
    if case.startswith("refresh-background"):
        ttl = 1860 if case == "refresh-background-fresh" else 1200
    last = now - datetime.timedelta(days=9) if case == "refresh-codex-stale" else now
    data = {
        "accessToken": PRIOR_ACCESS,
        "expiresAt": render_time(now + datetime.timedelta(seconds=ttl)),
        "lastRefreshAt": render_time(last),
        "scope": "fixture-scope",
    }
    if case not in {"refresh-copilot-only", "refresh-copilot-raced"}:
        data["refreshToken"] = OLD_REFRESH
    if provider == "github":
        data["providerSpecificData"] = {"githubLogin": "fixture-login", "copilotTokenExpiresAt": 1}
    elif provider == "codex":
        data["providerSpecificData"] = {"chatgptAccountId": "workspace-a", "retained": True}
        if case == "refresh-background-proxy":
            data["providerSpecificData"]["connectionProxyEnabled"] = True
    with sqlite3.connect(root / "db/data.sqlite") as db:
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES (?,?,'oauth','Fixture',1,1,?,?,?)""",
                   (REFRESH_ID, provider, json.dumps(data), render_time(now), render_time(now)))
    return load_fixture_account(root)

def refresh_case(port, root):
    expected = seed_refresh(root)
    def refresh(**fields):
        return status(port, "fixture", "refresh", data={"observed": expected, **fields})
    if case.startswith("refresh-background"):
        assert refresh()["account"]["accessToken"] == PRIOR_ACCESS and not REQUESTS
        observed = refresh(background=True)["account"]
        if case == "refresh-background-due":
            assert observed["accessToken"] == ROTATED_ACCESS
            assert load_fixture_account(root)["refreshToken"] == ROTATED_REFRESH
            assert len(REQUESTS) == 1 and REQUESTS[0]["valid"]
        else:
            assert observed["accessToken"] == PRIOR_ACCESS and not REQUESTS
        return {"background": case, "verified": True}
    if case == "refresh-cancel":
        assert refresh(cancel=True) == {"cancelled": True} and REFRESH_STARTED.is_set()
        for _ in range(100):
            if load_fixture_account(root)["accessToken"] == ROTATED_ACCESS:
                return {"rotation_survived_waiter_cancellation": True}
            time.sleep(0.03)
        assert False, "aborted request did not finish credential rotation"
    if case in {"refresh-concurrent", "refresh-concurrent-invalid"}:
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda _: refresh(rejected=True), range(2)))
        assert REFRESH_NUM == 1
        if case == "refresh-concurrent-invalid":
            assert all(result == {"error": "ReauthenticationRequired"} for result in results)
            return {"shared_failure": True}
        assert all(result["account"]["accessToken"] == ROTATED_ACCESS for result in results)
        return {"one_rotation": True}
    if case in {"refresh-other-writer", "refresh-deleted", "refresh-disabled", "refresh-metadata", "refresh-github-other-copilot", "refresh-copilot-raced"}:
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(refresh)
            assert REFRESH_STARTED.wait(5)
            with sqlite3.connect(root / "db/data.sqlite") as db:
                if case in {"refresh-other-writer", "refresh-copilot-raced"}:
                    db.execute("UPDATE providerConnections SET data=json_set(data,'$.accessToken',?,'$.refreshToken',?) WHERE id=?",
                               (CONCURRENT_ACCESS, CONCURRENT_REFRESH, REFRESH_ID))
                elif case == "refresh-deleted":
                    db.execute("DELETE FROM providerConnections WHERE id=?", (REFRESH_ID,))
                elif case == "refresh-disabled":
                    db.execute("UPDATE providerConnections SET isActive=0 WHERE id=?", (REFRESH_ID,))
                elif case == "refresh-github-other-copilot":
                    db.execute("UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData.copilotToken',?,'$.providerSpecificData.copilotTokenExpiresAt',?) WHERE id=?",
                               (CONCURRENT_COPILOT, 1900000000, REFRESH_ID))
                else:
                    db.execute("UPDATE providerConnections SET name='Renamed by another writer', data=json_set(data,'$.providerSpecificData.extra',9) WHERE id=?", (REFRESH_ID,))
            REFRESH_RELEASED.set()
            response = pending.result(timeout=5)
        if case == "refresh-other-writer":
            assert response["account"]["accessToken"] == CONCURRENT_ACCESS
            return {"newer_credentials_preserved": True}
        if case == "refresh-github-other-copilot":
            account = response["account"]
            assert account["accessToken"] == ROTATED_ACCESS and account["refreshToken"] == ROTATED_REFRESH
            assert account["providerSpecificData"]["copilotToken"] == CONCURRENT_COPILOT
            assert len(REQUESTS) == 1
            return {"independent_copilot_update_preserved": True}
        if case == "refresh-copilot-raced":
            account = response["account"]
            assert account["accessToken"] == CONCURRENT_ACCESS and "copilotToken" not in account["providerSpecificData"]
            return {"stale_copilot_discarded": True}
        if case == "refresh-metadata":
            assert response["account"]["accessToken"] == ROTATED_ACCESS
            assert response["account"]["name"] == "Renamed by another writer" and response["account"]["providerSpecificData"]["extra"] == 9
            return {"unrelated_changes_preserved": True}
        assert response == {"error": "AccountUnavailable"}
        return {"unavailable": True}
    response = refresh(rejected=case in {"refresh-codex-rejected", "refresh-github-rejected", "refresh-github-rejected-rotated"})
    if case == "refresh-codex-invalid":
        assert response == {"error": "ReauthenticationRequired"}
        return response
    account = response["account"]
    return {"access_rotated": account["accessToken"] == ROTATED_ACCESS,
            "refresh_rotated": account.get("refreshToken") == ROTATED_REFRESH,
            "copilot_refreshed": account.get("providerSpecificData", {}).get("copilotToken") == COPILOT}

def replay(port):
    if case.startswith("cline"):
        provider = "clinepass" if case.startswith("clinepass") else "cline"
        auth = authorization(port, provider)
        result = summary_authorize(auth)
        if not case.endswith("-authorize"):
            actual, body = submit(port, auth, provider=provider)
            assert actual == 200, actual
            result["account"] = summarized_connection(body)
        return result
    if case.startswith("codex"):
        auth = authorization(port, "codex")
        result = summary_authorize(auth)
        actual, body = submit(port, auth, provider="codex")
        assert actual == 200
        result["account"] = summarized_connection(body)
        if case != "codex-pkce":
            actual, second = submit(port, authorization(port, "codex"), "fixture-code-rotated", provider="codex")
            assert actual == 200
            result["same_account"] = second["connection"]["id"] == body["connection"]["id"]
        return result
    if case.startswith("claude"):
        auth = authorization(port, "claude")
        result = summary_authorize(auth)
        code = "fixture-code#" + auth["state"] if case == "claude-suffix" else "fixture-code"
        actual, body = submit(port, auth, code, provider="claude")
        assert actual == 200
        result["account"] = summarized_connection(body)
        return result
    if case.startswith("gemini"):
        auth = authorization(port, "gemini-cli")
        result = summary_authorize(auth)
        if case != "gemini-authorize":
            code, body = submit(port, auth)
            assert code == 200
            result["account"] = summarized_connection(body)
            if case == "gemini-relogin":
                code, second = submit(port, authorization(port, "gemini-cli"), "fixture-code-rotated")
                assert code == 200
                result["same_account"] = second["connection"]["id"] == body["connection"]["id"]
        return result

    auth = authorization(port, "github")
    result = summary_authorize(auth)
    device = status(port, "github", "device-code")
    assert device["device_code"] == CODE and len(device["codeVerifier"]) == 43
    result["device_fields"] = sorted(device)
    seen = []
    if case != "github-device":
        for _ in range(2 if case == "github-success" else 1):
            data = status(port, "github", "poll", data={"deviceCode": CODE})
            if data.get("success"):
                seen.append(summarized_connection(data))
            else:
                seen.append({"error": data["error"], "pending": data["pending"], "has_description": bool(data.get("errorDescription"))})
    result["poll"] = seen
    return result


def snapshot(root):
    file = root / "db/data.sqlite"
    if not file.exists():
        return []
    with sqlite3.connect(file) as db:
        rows = list(db.execute("SELECT provider, authType, name, email, priority, isActive, data, createdAt FROM providerConnections ORDER BY provider,priority"))
    results = []
    for provider, auth_type, name, email, priority, active, raw, created in rows:
        data = json.loads(raw)
        if "accessToken" in data:
            access = data.pop("accessToken")
            assert access in {CLAUDE_ACCESS, CODEX_ACCESS, GOOGLE_ACCESS, GOOGLE_ROTATED, GITHUB_ACCESS, PRIOR_ACCESS, ROTATED_ACCESS, CONCURRENT_ACCESS, CLINE_ACCESS, CLINE_ROTATED}
            data["hasAccess"] = True
            data["rotated"] = access in {GOOGLE_ROTATED, ROTATED_ACCESS, CLINE_ROTATED}
            if case.startswith("refresh-"):
                data["foreignAccess"] = access == CONCURRENT_ACCESS
        if "refreshToken" in data:
            refresh_token = data.pop("refreshToken")
            assert refresh_token in {REFRESH, OLD_REFRESH, ROTATED_REFRESH, CONCURRENT_REFRESH}
            data["hasRefresh"] = True
            if case.startswith("refresh-"):
                data["refreshRotated"] = refresh_token == ROTATED_REFRESH
                data["foreignRefresh"] = refresh_token == CONCURRENT_REFRESH
        if "copilotToken" in data.get("providerSpecificData", {}):
            copilot = data["providerSpecificData"].pop("copilotToken")
            assert copilot in {COPILOT, CONCURRENT_COPILOT}
            data["providerSpecificData"]["hasCopilotToken"] = True
            if case == "refresh-github-other-copilot":
                data["providerSpecificData"]["foreignCopilot"] = copilot == CONCURRENT_COPILOT
        if "idToken" in data:
            assert data.pop("idToken") in {workspace_token("workspace-a"), workspace_token("workspace-b"), workspace_token(None)}
            data["hasIdToken"] = True
        refreshed_at = None
        if "lastRefreshAt" in data:
            refreshed_at = datetime.datetime.fromisoformat(data.pop("lastRefreshAt"))
            data["refreshTimeSet"] = True
        if data.get("expiresAt"):
            parse = datetime.datetime.fromisoformat
            expires = (parse(data.pop("expiresAt")) - parse(created)).total_seconds()
            if case.startswith("refresh-"):
                if data.get("expiresIn") and refreshed_at is not None:
                    assert abs((parse(json.loads(raw)["expiresAt"]) - refreshed_at).total_seconds() - data["expiresIn"]) < 5
                    data["expiryTimeSeconds"] = data["expiresIn"]
                else:
                    data["expiryTimeSeconds"] = round(expires)
            else:
                assert abs(expires - 3600) < 5
                data["expiresComputed"] = True
        results.append({"provider": provider, "type": auth_type, "name": name, "email": email, "priority": priority, "active": bool(active), "data": data})
    return results


with tempfile.TemporaryDirectory(prefix="9router-oauth-fixture-") as directory:
    root = pathlib.Path(directory)
    simulator = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=lambda: simulator.serve_forever(poll_interval=0.02), daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{simulator.server_port}"
    env = dict(os.environ, HOME=directory, DATA_DIR=directory, NO_PROXY="localhost,127.0.0.1")
    try:
        with subprocess.Popen(command + [directory, base], cwd=root, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
            try:
                deadline = time.monotonic() + 5
                output = bytearray()
                readiness = None
                while time.monotonic() < deadline:
                    if not select.select([child.stdout], [], [], 1)[0]:
                        continue
                    chunk = os.read(child.stdout.fileno(), 4096)
                    if not chunk:
                        break
                    output.extend(chunk)
                    readiness = next((line for line in output.split(b"\n")[:-1] if line.startswith(b"ready ")), None)
                    if readiness:
                        break
                assert readiness, "fixture launcher did not announce its port"
                port = int(readiness.split()[1])
                if case.startswith("refresh-"):
                    result = refresh_case(port, root)
                else:
                    result = state_case(port) if MODE == "strict" else replay(port)
                rows = snapshot(root)
            finally:
                if child.poll() is None:
                    child.terminate()
                try:
                    child.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.communicate(timeout=3)
    finally:
        simulator.shutdown()
        simulator.server_close()
    assert all(operation["valid"] for operation in REQUESTS)
    print(json.dumps({"result": result, "requests": REQUESTS, "db": rows}, sort_keys=True))
