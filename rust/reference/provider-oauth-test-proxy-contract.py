from contextlib import ExitStack
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import sqlite3
import tempfile
from urllib.parse import parse_qs, urlsplit


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
token = "offline-oauth-probe-token"
refresh = "offline-oauth-probe-refresh"
relay_token = "M" * 43
relay_host = "oauth-fixture.owner.workers.dev"
gitlab_host = "gitlab-oauth.invalid.org"
calls, failures = [], []
targets = {
    "github": ("https://api.github.com", "/user", 200),
    "iflow": ("https://iflow.cn", "/api/oauth/getUserInfo", 200),
    "kimchi": ("https://api.cast.ai", "/v1/llm/openai/supported-providers", 401),
    "qoder": ("https://openapi.qoder.sh", "/api/v1/userinfo", 403),
    "qoder-cn": ("https://openapi.qoder.com.cn", "/api/v1/userinfo", 503),
    "muse": ("https://api.meta.ai", "/v1/models", 200),
    "kilocode": ("https://api.kilo.ai", "/api/profile", 200),
}


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        try:
            assert self.headers["Cookie"] is None and self.headers["X-9r-Cli-Token"] is None
            if self.path == "/cloudflare-relay/" + relay_host:
                assert self.headers["X-Relay-Auth"] == relay_token
                dest = self.headers["X-Relay-Target"]
                path = urlsplit(self.headers["X-Relay-Path"])
                (provider, status), = ((name, code) for name, (host, route, code) in targets.items()
                                      if (host, route) == (dest, path.path))
                if provider == "iflow":
                    assert self.headers["Authorization"] is None
                    assert parse_qs(path.query) == {"accessToken":[token]}
                else:
                    assert self.headers["Authorization"] == "Bearer " + token and not path.query
                if provider == "github":
                    assert self.headers["User-Agent"] == "9Router"
                    assert self.headers["Accept"] == "application/vnd.github+json"
                if provider == "kimchi":
                    assert self.headers["User-Agent"] == "9Router" and self.headers["Accept"] == "application/json"
                if provider == "muse":
                    assert self.headers["X-Api-Version"] == "1.0.0"
                calls.append(("relay", provider))
            else:
                target = urlsplit(self.path)
                assert target.scheme == "http" and target.hostname == gitlab_host
                assert target.path == "/company/api/v4/user"
                assert self.headers["Authorization"] == "Bearer " + token
                calls.append(("proxy", "gitlab"))
                status = 200
            self.send_response(status)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"{}")
        except (AssertionError, ValueError) as error:
            failures.append(repr(error))
            self.send_error(422)


def save(db, provider, details, access=token, auth="oauth"):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES (?,?,?,'Synthetic OAuth fixture',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider, provider, auth, json.dumps({"accessToken":access,"refreshToken":refresh,
                                                         "providerSpecificData":details})))


def post(port, api, data=None, auth=shared["ADMIN"]):
    conn = HTTPConnection("127.0.0.1", port, timeout=7)
    try:
        conn.request("POST", api, json.dumps(data) if data is not None else None,
                     headers={**auth,"Content-Type":"application/json"})
        response = conn.getresponse()
        raw = response.read()
        assert all(value.encode() not in raw for value in (token,refresh,relay_token)), raw
        return response.status, json.loads(raw)
    finally:
        conn.close()


def check(port, provider, error=None):
    result = post(port, f"/api/providers/{provider}/test")
    assert result == (200,{"valid":error is None,"error":error,"refreshed":False}), result


with tempfile.TemporaryDirectory(prefix="9router-oauth-proxy-test-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    proxy = stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1", 0), Vendor)))
    address = f"http://127.0.0.1:{proxy.server_port}"
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":address}
    with shared["gateway"](scratch, "oauth-probes", None, []) as (port, db):
        with sqlite3.connect(db) as sql:
            url = "https://" + relay_host
            data = {"type":"cloudflare","proxyUrl":url,"strictProxy":True,
                    "_managedCloudflare":{"url":url,"token":relay_token}}
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                VALUES ('oauth-relay',1,'unknown',?,'2026-11-20','2026-11-20')""", (json.dumps(data),))
        relay = {"proxyPoolId":"oauth-relay"}
        for provider in targets:
            save(db, provider, relay)
        assert post(port, "/api/providers/github/test", auth={})[0] == 401 and not calls
        expected = {200:None, 401:"Token invalid or revoked", 403:"Access denied", 503:"API returned 503"}
        for provider, (_, _, code) in targets.items():
            check(port, provider, expected[code])
        assert calls == [("relay", provider) for provider in targets]

        for provider in ("cursor", "codebuddy-cn", "codebuddy-intl"):
            save(db, provider, {"proxyPoolId":"missing"})
            check(port, provider)
        assert len(calls) == len(targets)
        save(db, "cursor", http, access=None)
        check(port, "cursor", "No access token")

        details = {**http,"baseUrl":"http://" + gitlab_host + "/company"}
        for kind in ("oauth", "access_token"):
            save(db, "gitlab", details, auth=kind)
            check(port, "gitlab")
        assert calls[-2:] == [("proxy", "gitlab")] * 2
        save(db, "gitlab", {**details,"connectionProxyUrl":"ftp://127.0.0.1:52989"})
        check(port, "gitlab", "fetch failed")
        assert len(calls) == len(targets) + 2 and not failures, failures

        status, body = post(port, "/api/providers/test-batch", {"mode":"provider","providerId":"github"})
        assert status == 200 and body["summary"] == {"total":1,"passed":1,"failed":0}
        assert calls[-1] == ("relay", "github") and body["results"][0]["valid"] is True
        with sqlite3.connect(db) as sql:
            saved = dict((name, json.loads(value)) for name, value in sql.execute(
                "SELECT id,data FROM providerConnections"))
        assert saved["github"]["testStatus"] == "active" and saved["github"]["lastError"] is None
        assert saved["qoder"]["testStatus"] == "error" and saved["qoder"]["lastError"] == "Access denied"
print("OAuth connection Test verified managed Relay, custom GitLab proxy, status and local-token checks.")
