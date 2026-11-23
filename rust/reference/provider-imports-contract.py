import base64
import concurrent.futures
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit


binary = str(Path(sys.argv[1]).resolve())
root = Path(__file__).resolve().parents[2]
requests = []
social_requests = []
iflow_grants = []
kilo_requests = []


class Gitlab(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path == "/gitlab-oauth/api/v4/user":
            assert self.headers["Authorization"] == "Bearer synthetic-gitlab-oauth-token"
            requests.append(self.path)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"email":"fixture-gitlab-oauth@invalid.example","username":"oauth-fixture","name":"OAuth fixture"}')
            return
        if self.path == "/mimo/v1/models":
            assert self.headers["X-Mimo-Source"] == "mimocode-cli"
            requests.append(self.path)
            if self.headers["Authorization"] != "Bearer sk-fixture-mimo":
                self.send_response(401)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"data":[{"id":"mimo-fixture-model"}]}')
            return
        if self.path == "/first/api/v4/user" and self.headers.get("Authorization") is not None:
            assert self.headers["Authorization"] == "Bearer fixture-renewed" and "Private-Token" not in self.headers
            token = "fixture-renewed"
        else:
            token = self.headers["Private-Token"]
        requests.append(self.path)
        if self.path.startswith("/redirect/"):
            self.send_response(307)
            self.send_header("Location", "/unintended")
            self.end_headers()
            return
        if token == "fixture-rejected":
            self.send_response(401)
            self.end_headers()
            self.wfile.write(f"Rejected {token}".encode())
            return
        assert token in {"fixture-first", "fixture-renewed", "fixture-other"}
        assert self.path in {"/first/api/v4/user", "/other/api/v4/user"}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"public_email":"fixture@invalid.example","username":"one","name":"Fixture"}')

    def do_POST(self):
        if self.path == "/first/api/v4/chat/completions":
            assert self.headers["Authorization"] == "Bearer fixture-renewed"
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert body["model"] == "fixture-gitlab-model" and "Cookie" not in self.headers
            requests.append(self.path)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"id":"fixture-gitlab-chat","model":"fixture-gitlab-model","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"tenant fixture"},"finish_reason":"stop"}]}')
            return
        assert self.path == "/gitlab-oauth/oauth/token"
        assert self.headers["Content-Type"] == "application/x-www-form-urlencoded"
        body = self.rfile.read(int(self.headers["Content-Length"])).decode()
        fields = parse_qs(body)
        assert fields["client_id"] == ["fixture-gitlab-client"] and fields["client_secret"] == ["synthetic-gitlab-client-secret"]
        assert fields["grant_type"] == ["authorization_code"] and fields["code"] == ["synthetic-gitlab-grant"]
        assert len(fields["code_verifier"][0]) == 43
        requests.append(self.path)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"access_token":"synthetic-gitlab-oauth-token","refresh_token":"synthetic-gitlab-oauth-refresh","expires_in":3600,"scope":"api read_user"}')


class OAuthIssuer(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        if self.path == "/kilocode-device/api/device-auth/codes":
            assert self.headers["Content-Type"] == "application/json" and "Authorization" not in self.headers
            kilo_requests.append("initiate")
            self.write_json({"code":"KILO-FIXTURE","verificationUrl":"https://app.kilo.ai/device","expiresIn":90})
            return
        if self.path == "/kilocode-device/api/openrouter/chat/completions":
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert self.headers["Authorization"] == "Bearer synthetic-kilocode-token"
            assert self.headers["X-Kilocode-OrganizationID"] == "fixture-kilocode-org"
            assert body["model"] == "openai/gpt-4.1" and body["messages"] == [{"role":"user","content":"fixture"}]
            kilo_requests.append("chat")
            self.write_json({"id":"fixture-kilo-chat","object":"chat.completion","model":body["model"],
                             "choices":[{"index":0,"message":{"role":"assistant","content":"kilo reply"},"finish_reason":"stop"}]})
            return
        if self.path == "/iflow-fixture/oauth/token":
            auth = self.headers["Authorization"]
            assert auth.startswith("Basic ")
            client, secret = base64.b64decode(auth.split(" ", 1)[1]).decode().split(":", 1)
            fields = parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
            assert fields["client_id"] == [client] and fields["client_secret"] == [secret] and secret
            grant, = fields["grant_type"]
            iflow_grants.append(grant)
            if grant == "authorization_code":
                assert fields["code"] == ["synthetic-iflow-oauth-code"]
                assert fields["redirect_uri"][0].endswith("/callback")
                access, refresh = "synthetic-iflow-oauth-access", "synthetic-iflow-oauth-refresh"
            else:
                assert grant == "refresh_token" and fields["refresh_token"] == ["synthetic-iflow-oauth-refresh"]
                access, refresh = "synthetic-iflow-renewed-access", "synthetic-iflow-renewed-refresh"
            self.write_json({"access_token":access,"refresh_token":refresh,"expires_in":259200})
            return
        if self.path == "/iflow-fixture/v1/chat/completions":
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert self.headers["Authorization"] == "Bearer sk-synthetic-iflow-oauth-key"
            assert self.headers["x-iflow-signature"] and body["model"] == "qwen3-coder-plus"
            self.write_json({"id":"fixture-iflow-oauth-chat","model":"qwen3-coder-plus","object":"chat.completion",
                             "choices":[{"index":0,"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}]})
            return
        assert self.path == "/kiro-social/oauth/token"
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["redirect_uri"] == "kiro://kiro.kiroAgent/authenticate-success"
        assert len(body["code_verifier"]) == 43
        social_requests.append(body["code"])
        if body["code"] == "synthetic-kiro-failure":
            self.send_response(403)
            self.end_headers()
            self.wfile.write(f"Rejected {body['code']} {body['code_verifier']}".encode())
            return
        assert body["code"] == "synthetic-kiro-code"
        claims = base64.urlsafe_b64encode(b'{"email":"kiro-social@invalid.example"}').decode().rstrip("=")
        result = {"accessToken":"e30."+claims+".fixture", "refreshToken":"synthetic-kiro-social-refresh",
                  "profileArn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture", "expiresIn":3600}
        self.write_json(result)

    def do_GET(self):
        uri = urlsplit(self.path)
        if uri.path == "/kilocode-device/api/device-auth/codes/KILO-FIXTURE":
            assert "Authorization" not in self.headers and not uri.query
            kilo_requests.append("poll")
            if kilo_requests.count("poll") == 1:
                self.send_response(202)
                self.end_headers()
            else:
                self.write_json({"status":"approved","token":"synthetic-kilocode-token","userEmail":"kilo@invalid.example"})
            return
        if uri.path == "/kilocode-device/api/profile":
            assert self.headers["Authorization"] == "Bearer synthetic-kilocode-token" and not uri.query
            kilo_requests.append("profile")
            self.write_json({"organizations":[{"id":"fixture-kilocode-org"}]})
            return
        assert uri.path == "/iflow-fixture/api/oauth/getUserInfo"
        assert parse_qs(uri.query)["accessToken"] == ["synthetic-iflow-oauth-access"]
        self.write_json({"success":True,"data":{"phone":"synthetic-iflow-identity",
                        "apiKey":"sk-synthetic-iflow-oauth-key","nickname":"Fixture user"}})

    def write_json(self, result):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(result).encode())


def request(port, method, path, data=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = dict(headers or {})
        if data is not None:
            headers["content-type"] = "application/json"
        connection.request(method, path, json.dumps(data) if data is not None else None, headers)
        response = connection.getresponse()
        return response.status, response.headers, json.loads(response.read())
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-imports-") as directory:
    home = Path(directory)
    path = home / "empty-path"
    path.mkdir()
    store = home / "data"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Gitlab)
    server = threading.Thread(target=upstream.serve_forever, daemon=True)
    server.start()
    social = ThreadingHTTPServer(("127.0.0.1", 52989), OAuthIssuer)
    threading.Thread(target=social.serve_forever, daemon=True).start()
    env = {"PATH":str(path),"HOME":str(home),"PORT":str(port),"DATA_DIR":str(store),
           "RUST_ANYTHING_FALLBACK_LOG":str(home / "fallbacks"),
           "NINEROUTER_MIGRATION_FIXTURE":"1"}
    process = subprocess.Popen([binary, "--no-browser"], cwd=root, env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                if request(port, "GET", "/api/health")[0] == 200:
                    break
            except OSError:
                time.sleep(.035)
        else:
            raise AssertionError("Rust gateway not ready")
        status, headers, _ = request(port, "POST", "/api/auth/login", {"password":"123456"})
        assert status == 200
        session = {"cookie":headers["set-cookie"].split(";", 1)[0]}
        base = f"http://127.0.0.1:{upstream.server_port}"
        gitlab = "/api/oauth/gitlab/pat"
        grok = "/api/oauth/grok-cli/bulk-import"
        mimo = "/api/oauth/xiaomi-mimo/api-key"
        commandcode = "/api/oauth/commandcode/auto-import"
        kiro = "/api/oauth/kiro/auto-import"
        authorize = "/api/oauth/kiro/social-authorize?provider="
        exchange = "/api/oauth/kiro/social-exchange"
        assert request(port, "GET", authorize + "google")[0] == 401
        assert request(port, "GET", authorize + "other", headers=session)[0] == 400
        status, _, login = request(port, "GET", authorize + "google", headers=session)
        assert status == 200
        url = urlsplit(login["authUrl"])
        assert (url.scheme,url.hostname,url.path) == ("https","prod.us-east-1.auth.desktop.kiro.dev","/login")
        query = parse_qs(url.query)
        challenge = base64.urlsafe_b64encode(hashlib.sha256(login["codeVerifier"].encode()).digest()).decode().rstrip("=")
        assert query["code_challenge"] == [challenge] and query["code_challenge_method"] == ["S256"]
        assert query["state"] == [login["state"]] and query["idp"] == ["Google"]
        payload = {"code":"synthetic-kiro-code","codeVerifier":login["codeVerifier"],
                   "state":login["state"],"provider":"google"}
        assert request(port, "POST", exchange, {**payload,"state":"invalid"}, session)[0] == 400
        assert request(port, "POST", exchange, {**payload,"provider":"github"}, session)[0] == 400
        assert social_requests == []
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as callers:
            results = list(callers.map(lambda _: request(port, "POST", exchange, payload, session), range(2)))
        assert sorted(status for status, _, _ in results) == [200, 400]
        saved = next(body for status, _, body in results if status == 200)
        assert saved["connection"]["email"] == "kiro-social@invalid.example"
        assert social_requests == ["synthetic-kiro-code"]
        status, _, failure_login = request(port, "GET", authorize + "github", headers=session)
        assert status == 200 and parse_qs(urlsplit(failure_login["authUrl"]).query)["idp"] == ["Github"]
        failure = {"code":"synthetic-kiro-failure","codeVerifier":failure_login["codeVerifier"],"provider":"github"}
        status, _, rejected = request(port, "POST", exchange, failure, session)
        assert status == 500 and rejected["error"] == "Token exchange failed: Rejected [redacted] [redacted]"
        iflow_redirect = f"http://127.0.0.1:{port}/callback"
        status, _, iflow = request(port, "GET", "/api/oauth/iflow/authorize?"
                                  + urlencode({"redirect_uri":iflow_redirect}), headers=session)
        assert status == 200
        auth_link = urlsplit(iflow["authUrl"])
        assert auth_link.hostname == "iflow.cn" and parse_qs(auth_link.query)["state"] == [iflow["state"]]
        grant = {"code":"synthetic-iflow-oauth-code","redirectUri":iflow_redirect,
                 "state":iflow["state"],"codeVerifier":iflow["codeVerifier"]}
        assert request(port, "POST", "/api/oauth/iflow/exchange", {**grant,"state":"bad"}, session)[0] == 400
        status, _, iflow_saved = request(port, "POST", "/api/oauth/iflow/exchange", grant, session)
        assert status == 200 and iflow_saved["connection"]["email"] == "synthetic-iflow-identity"
        iflow_id = iflow_saved["connection"]["id"]
        with sqlite3.connect(store / "db/data.sqlite") as db:
            raw, = db.execute("SELECT data FROM providerConnections WHERE id=?", (iflow_id,)).fetchone()
            current = json.loads(raw)
            current["expiresAt"] = "2000-01-01T00:00:00.000Z"
            db.execute("UPDATE providerConnections SET data=? WHERE id=?", (json.dumps(current),iflow_id))
        assert request(port, "PATCH", "/api/settings", {"requireApiKey":False}, session)[0] == 200
        status, _, completion = request(port, "POST", "/v1/chat/completions", {
            "model":"if/qwen3-coder-plus","messages":[{"role":"user","content":"fixture"}],"stream":False,
        })
        assert status == 200 and completion["choices"][0]["message"]["content"] == "fixture"
        assert iflow_grants == ["authorization_code","refresh_token"]
        with sqlite3.connect(store / "db/data.sqlite") as db:
            raw, = db.execute("SELECT data FROM providerConnections WHERE id=?", (iflow_id,)).fetchone()
        assert json.loads(raw)["refreshToken"] == "synthetic-iflow-renewed-refresh"
        kilo_device = "/api/oauth/kilocode/device-code"
        kilo_poll = "/api/oauth/kilocode/poll"
        assert request(port, "GET", kilo_device)[0] == 401
        status, headers, kilo = request(port, "GET", kilo_device, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert kilo["user_code"] == "KILO-FIXTURE" and kilo["device_code"] != kilo["user_code"]
        assert kilo["verification_uri_complete"] == "https://app.kilo.ai/device"
        assert kilo["interval"] == 3 and kilo["expires_in"] == 90
        kilo_grant = {"deviceCode":kilo["device_code"]}
        assert request(port, "POST", kilo_poll, kilo_grant)[0] == 401
        assert request(port, "POST", kilo_poll, {"deviceCode":kilo["user_code"]}, session)[2]["error"] == "expired_token"
        assert request(port, "POST", kilo_poll, kilo_grant, session)[2]["error"] == "authorization_pending"
        assert request(port, "POST", kilo_poll, kilo_grant, session)[2]["error"] == "slow_down"
        assert kilo_requests == ["initiate", "poll"]
        time.sleep(3.05)
        status, _, kilo_saved = request(port, "POST", kilo_poll, kilo_grant, session)
        assert status == 200 and kilo_saved["success"] and kilo_saved["connection"]["provider"] == "kilocode"
        assert "synthetic-kilocode-token" not in json.dumps(kilo_saved)
        assert request(port, "POST", kilo_poll, kilo_grant, session)[2]["error"] == "expired_token"
        with sqlite3.connect(store / "db/data.sqlite") as db:
            raw, = db.execute("SELECT data FROM providerConnections WHERE id=?", (kilo_saved["connection"]["id"],)).fetchone()
        assert json.loads(raw)["providerSpecificData"]["orgId"] == "fixture-kilocode-org"
        status, _, completion = request(port, "POST", "/v1/chat/completions", {
            "model":"kc/openai/gpt-4.1","messages":[{"role":"user","content":"fixture"}],"stream":False,
        })
        assert status == 200 and completion["choices"][0]["message"]["content"] == "kilo reply", completion
        assert kilo_requests == ["initiate", "poll", "poll", "profile", "chat"]
        cursor = "/api/oauth/cursor/auto-import"
        zed = "/api/oauth/zed/auto-import"
        mimo_local = "/api/oauth/xiaomi-mimo/auto-import"
        assert "secret-tool not found" in request(port, "GET", zed, headers=session)[2]["error"]
        secret_lookup = path / "secret-tool"
        secret_lookup.write_text(f"#!{sys.executable}\n"
            "import sys\n"
            "assert sys.argv[1:] == ['search','--all','url','https://zed-fixture.invalid']\n"
            "print('label = zed-github-account\\nsecret = synthetic-private-zed-token')\n"
            "print('attribute.username = 17', file=sys.stderr)\n")
        secret_lookup.chmod(0o700)
        zed_settings = home / ".config/zed/settings.json"
        zed_settings.parent.mkdir(parents=True)
        zed_settings.write_text('{"credentials_url":"https://zed-fixture.invalid/"}')
        zed_db = home / ".local/share/zed/db/0-preview/db.sqlite"
        zed_db.parent.mkdir(parents=True)
        with sqlite3.connect(zed_db) as db:
            db.execute("CREATE TABLE kv_store (key TEXT, value TEXT)")
            db.execute("INSERT INTO kv_store VALUES ('system_id', 'fixture-zed-system')")
        assert request(port, "GET", zed, headers={**session,"x-real-ip":"192.0.2.4"})[0] == 403
        status, headers, zed_auth = request(port, "GET", zed, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert zed_auth == {"found":True,"userId":"17","accessToken":"synthetic-private-zed-token",
                           "systemId":"fixture-zed-system","credentialsUrl":"https://zed-fixture.invalid"}
        secret_lookup.write_text(secret_lookup.read_text().replace("zed-github-account","unrelated-label"))
        assert request(port, "GET", zed, headers=session)[2]["found"] is False
        assert not request(port, "GET", mimo_local, headers=session)[2]["found"]
        mimo_file = home / ".local/share/mimocode/auth.json"
        mimo_file.parent.mkdir(parents=True)
        mimo_file.write_text('{"xiaomi":{"key":" sk-local-mimo-fixture ","metadata":{"uid":"local-fixture"}}}')
        assert request(port, "GET", mimo_local, headers={**session,"x-real-ip":"192.0.2.4"})[0] == 403
        status, headers, auto = request(port, "GET", mimo_local, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert auto == {"found":True,"apiKey":"sk-local-mimo-fixture","uid":"local-fixture",
                        "baseUrl":"https://api.xiaomimimo.com/v1","source":str(mimo_file),
                        "mimoPassToken":None,"mimoUserId":None,"mimoCUserId":None}
        assert not request(port, "GET", cursor, headers=session)[2]["found"]
        cursor_db = home / ".config/Cursor/User/globalStorage/state.vscdb"
        cursor_db.parent.mkdir(parents=True)
        with sqlite3.connect(cursor_db) as db:
            db.execute("CREATE TABLE itemTable (key TEXT UNIQUE, value BLOB)")
            db.executemany("INSERT INTO itemTable VALUES (?,?)", [
                ("cursorAuth/accessToken",json.dumps("synthetic-cursor-token")),
                ("cursorAuth/token","secondary-fixture-token"),
                ("storage.serviceMachineId",b"private-fixture-machine"),
                ("storage.machineId","fallback-fixture-machine"),
            ])
        assert "does not appear to be installed" in request(port, "GET", cursor, headers=session)[2]["error"]
        cursor_desktop = home / ".local/share/applications/cursor.desktop"
        cursor_desktop.parent.mkdir(parents=True)
        cursor_desktop.write_text("[Desktop Entry]\nName=Cursor fixture\n")
        cursor_bytes = cursor_db.read_bytes()
        assert request(port, "GET", cursor, headers={**session,"x-forwarded-for":"192.0.2.4"})[0] == 403
        status, headers, cursor_auth = request(port, "GET", cursor, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert cursor_auth == {"found":True,"accessToken":"synthetic-cursor-token","machineId":"private-fixture-machine"}
        assert cursor_db.read_bytes() == cursor_bytes
        cursor_import = "/api/oauth/cursor/import"
        assert request(port, "GET", cursor_import, headers=session)[2]["requiredFields"][0]["name"] == "accessToken"
        claims = base64.urlsafe_b64encode(b'{"email":"cursor-fixture@invalid.example","sub":"cursor-fixture-user"}').decode().rstrip("=")
        cursor_token = "eyJhbGciOiJub25lIn0." + claims + ".synthetic"
        payload = {"accessToken":cursor_token,"machineId":"11111111-2222-3333-4444-555555555555"}
        assert request(port, "POST", cursor_import, {**payload,"machineId":"not-a-machine-id"}, session)[0] == 500
        status, _, cursor_saved = request(port, "POST", cursor_import, payload, session)
        assert status == 200 and cursor_saved["connection"]["email"] == "cursor-fixture@invalid.example"
        assert cursor_token not in json.dumps(cursor_saved)
        assert request(port, "GET", kiro, headers=session)[2] == {
            "found":False,"error":"AWS SSO cache not found. Please login to Kiro IDE first.",
        }
        cache = home / ".aws/sso/cache"
        cache.mkdir(parents=True)
        (cache / "aaa.json").write_text('{"refreshToken":"aorAAAAAG-fixture-unrelated"}')
        chosen = cache / "kiro-auth-token.json"
        chosen.write_text(json.dumps({"refreshToken":"aorAAAAAG-fixture-kiro","clientIdHash":"fixture-client",
                                      "region":"eu-central-1","authMethod":"IdC"}))
        (cache / "fixture-client.json").write_text('{"clientId":"synthetic-client","clientSecret":"synthetic-secret"}')
        profile = home / ".config/Kiro/User/globalStorage/kiro.kiroagent/profile.json"
        profile.parent.mkdir(parents=True)
        profile.write_text('{"arn":"arn:aws:codewhisperer:eu-central-1:123456789012:profile/fixture"}')
        assert request(port, "GET", kiro, headers={**session,"x-real-ip":"192.0.2.4"})[0] == 403
        status, headers, imported = request(port, "GET", kiro, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert imported == {"found":True,"refreshToken":"aorAAAAAG-fixture-kiro","source":"kiro-auth-token.json",
                            "clientId":"synthetic-client","clientSecret":"synthetic-secret","region":"eu-central-1",
                            "authMethod":"IdC","profileArn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture"}
        (cache.parent / "foreign.json").write_text('{"clientId":"synthetic-unrelated","clientSecret":"fixture-remote-secret"}')
        chosen.write_text('{"refreshToken":"aorAAAAAG-fixture-kiro","clientIdHash":"../foreign"}')
        assert request(port, "GET", kiro, headers=session)[2]["clientSecret"] is None
        missing, _, absent = request(port, "GET", commandcode, headers=session)
        assert missing == 200 and not absent["found"] and ".commandcode/auth.json" in absent["error"]
        auth_file = home / ".commandcode/auth.json"
        auth_file.parent.mkdir()
        auth_file.write_text("{ malformed")
        assert request(port, "GET", commandcode, headers=session)[2]["error"].startswith("auth.json is not valid JSON")
        auth_file.write_text(json.dumps({"apiKey":" fixture-local-commandcode ","userName":"Fixture"}))
        assert request(port, "GET", commandcode)[0] == 403
        assert request(port, "GET", commandcode, headers={**session,"x-real-ip":"192.0.2.4"})[0] == 403
        status, headers, detected = request(port, "GET", commandcode, headers=session)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert detected == {"found":True,"apiKey":"fixture-local-commandcode","userId":None,
                           "userName":"Fixture","keyName":None,"authenticatedAt":None,"source":str(auth_file)}

        assert request(port, "POST", gitlab, {"token":"fixture-first","baseUrl":base+"/first"})[0] == 401
        assert requests == []
        for token, instance in [(" fixture-first ", "/first/"), ("fixture-renewed", "/first"),
                                ("fixture-other", "/other/")]:
            status, _, response = request(port, "POST", gitlab, {"token":token,"baseUrl":base+instance}, session)
            assert status == 200 and response == {"success":True}
        status, _, failed = request(port, "POST", gitlab, {"token":"fixture-rejected","baseUrl":base+"/first"}, session)
        assert status == 401 and "fixture-rejected" not in failed["error"] and "[redacted]" in failed["error"]
        assert request(port, "POST", gitlab, {"token":"fixture-other","baseUrl":base+"/redirect"}, session)[0] == 401
        assert "/unintended" not in requests

        instance = base + "/gitlab-oauth"
        redirect = f"http://127.0.0.1:{port}/callback"
        setup = "/api/oauth/gitlab/authorize?" + urlencode({"baseUrl":instance,"clientId":"fixture-gitlab-client",
                                                           "redirect_uri":redirect})
        saved_gitlab = "/api/oauth/gitlab/exchange"
        status, _, auth_code = request(port, "GET", setup, headers=session)
        assert status == 200 and auth_code["redirectUri"] == redirect and auth_code["flowType"] == "authorization_code_pkce"
        assert auth_code["authUrl"].startswith(instance + "/oauth/authorize?")
        query = parse_qs(urlsplit(auth_code["authUrl"]).query)
        assert query["code_challenge_method"] == ["S256"] and query["scope"] == ["api read_user"]
        grant = {"code":"synthetic-gitlab-grant","redirectUri":redirect,"codeVerifier":auth_code["codeVerifier"],
                 "state":auth_code["state"],"meta":{"baseUrl":instance,"clientId":"fixture-gitlab-client",
                                                  "clientSecret":"synthetic-gitlab-client-secret"}}
        assert request(port, "POST", saved_gitlab, {**grant,"meta":{**grant["meta"],"baseUrl":base+"/unintended"}}, session)[0] == 400
        assert "/gitlab-oauth/oauth/token" not in requests
        status, _, oauth_saved = request(port, "POST", saved_gitlab, grant, session)
        assert status == 200 and oauth_saved["connection"]["provider"] == "gitlab"
        assert request(port, "POST", saved_gitlab, grant, session)[0] == 400
        assert requests.count("/gitlab-oauth/oauth/token") == requests.count("/gitlab-oauth/api/v4/user") == 1
        status, _, gitlab_chat = request(port, "POST", "/v1/chat/completions", {
            "model":"gitlab/fixture-gitlab-model","messages":[{"role":"user","content":"fixture"}],"stream":False,
        })
        assert status == 200 and gitlab_chat["choices"][0]["message"]["content"] == "tenant fixture"
        assert requests.count("/first/api/v4/chat/completions") == 1
        with sqlite3.connect(store / "db/data.sqlite") as db:
            pat_id, = db.execute("SELECT id FROM providerConnections WHERE provider='gitlab' AND json_extract(data,'$.accessToken')=?",
                                 ("fixture-renewed",)).fetchone()
        expected_probe = {"valid":True,"error":None,"refreshed":False}
        for connection_id, path in [(pat_id,"/first/api/v4/user"),
                                    (oauth_saved["connection"]["id"],"/gitlab-oauth/api/v4/user")]:
            previous = len(requests)
            status, _, response = request(port, "POST", f"/api/providers/{connection_id}/test", headers=session)
            assert status == 200 and response == expected_probe and requests[previous:] == [path]

        payload = {"accessToken":"fixture-grok-access","refreshToken":"fixture-grok-refresh",
                   "email":"fixture-grok@invalid.example","providerSpecificData":{"username":"one"}}
        status, _, results = request(port, "POST", grok, {"accounts":[payload, None, {}]}, session)
        assert status == 200 and results["success"] == 1 and results["failed"] == 2
        imported = results["results"][0]["id"]
        payload["accessToken"] = "fixture-grok-rotated"
        status, _, results = request(port, "POST", grok, payload, session)
        assert status == 200 and results["results"][0]["id"] == imported

        mimo_payload = {"apiKey":" sk-fixture-mimo ","uid":"mimo-key-fixture","baseUrl":base+"/mimo/v1/"}
        status, _, first = request(port, "POST", mimo, mimo_payload, session)
        assert status == 200 and first["validated"] and first["modelCount"] == 1
        mimo_id = first["connection"]["id"]
        assert first["connection"]["email"] == "mimo-key-fixture@xiaomi"
        mimo_payload["apiKey"] = "sk-fixture-untested-mimo"
        status, _, rotated = request(port, "POST", mimo, mimo_payload, session)
        assert status == 200 and rotated["updated"] and not rotated["validated"] and rotated["modelCount"] == 0
        assert rotated["connection"]["id"] == mimo_id
        session_input = {"uid":"mimo-session-fixture","mimoPassToken":"fixture-mimo-cookie",
                         "mimoUserId":"mimo-session-id","baseUrl":base+"/mimo/v1"}
        status, _, imported_session = request(port, "POST", mimo, session_input, session)
        assert status == 200 and not imported_session["validated"]
        assert imported_session["connection"]["displayName"] == "Xiaomi mimo-session-fixture (Session)"
        assert requests.count("/mimo/v1/models") == 2

        with sqlite3.connect(store / "db/data.sqlite") as db:
            rows = list(db.execute("""SELECT provider,priority,data FROM providerConnections
                WHERE provider IN ('gitlab','grok-cli') ORDER BY provider,priority"""))
            xiaomi = list(db.execute("""SELECT authType,data FROM providerConnections
                WHERE provider='xiaomi-mimo' ORDER BY priority"""))
        assert [(provider, priority) for provider, priority, _ in rows] == [
            ("gitlab", 1), ("gitlab", 2), ("gitlab", 3), ("grok-cli", 1),
        ]
        assert [json.loads(row[2])["accessToken"] for row in rows] == [
            "fixture-renewed", "fixture-other", "synthetic-gitlab-oauth-token", "fixture-grok-rotated",
        ]
        assert json.loads(rows[2][2])["providerSpecificData"]["authKind"] == "oauth"
        assert [row[0] for row in xiaomi] == ["api_key", "oauth"]
        active_info = json.loads(xiaomi[0][1])
        assert active_info["accessToken"] == "sk-fixture-untested-mimo" and active_info["testStatus"] == "active"
        cookie_info = json.loads(xiaomi[1][1])
        assert cookie_info["providerSpecificData"]["mimoPassToken"] == "fixture-mimo-cookie"
        status, _, public = request(port, "GET", "/api/providers", headers=session)
        assert status == 200 and "fixture-renewed" not in json.dumps(public) and "fixture-grok-rotated" not in json.dumps(public)
        assert "sk-fixture-untested-mimo" not in json.dumps(public) and "fixture-mimo-cookie" not in json.dumps(public)
        assert (home / "fallbacks").read_text() == ""
        print("Verified local import containment, GitLab identity, Grok/Xiaomi rotation and zero fallback")
    finally:
        process.terminate()
        process.wait(timeout=10)
        upstream.shutdown()
        upstream.server_close()
        social.shutdown()
        social.server_close()
