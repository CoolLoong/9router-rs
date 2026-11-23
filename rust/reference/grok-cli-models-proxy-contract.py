from contextlib import ExitStack
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from http.client import HTTPConnection
import json
from pathlib import Path
import runpy
import socket
import sqlite3
import tempfile
from threading import Event
from urllib.parse import parse_qs
from uuid import UUID


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
host = "cli-chat-proxy.grok.com"
issuer_host = "auth.x.ai"
token_host = "login.x.ai"
token = "offline-grok-proxy-access"
refresh = "offline-grok-proxy-refresh"
renewed = "offline-grok-proxy-renewed"
rotated = "offline-grok-proxy-rotated"
email = "offline-grok-proxy@fixture.invalid"
principal = "offline-grok-principal"
secrets = (token, refresh, renewed, rotated, email, principal)
url = "/api/providers/fixture-grok-proxy/models"
api = {"Authorization": "Bearer offline-chat-key"}
live = "gcli/fixture-grok-tunnel"
calls = []
oauth = []
chats = []
users = []
expired = "2000-01-01T00:00:00Z"


class Catalog(BaseHTTPRequestHandler):
    status = 200
    discovery = "trusted"
    grant = "rotate"
    hold = None
    user_status = 200

    def log_message(self, *_args):
        pass

    def do_GET(self):
        direct = self.server.server_port == 52989
        if self.path.endswith("/.well-known/openid-configuration"):
            return self.discover(direct)
        try:
            if self.path in ("/grok-cli/v1/user", "/v1/user"):
                assert self.path == ("/grok-cli/v1/user" if direct else "/v1/user")
                assert direct or self.headers["Host"] == host
                authorization = self.headers.get_all("Authorization")
                assert authorization in (["Bearer " + token], ["Bearer " + renewed])
                assert self.headers["User-Agent"].startswith("9Router/")
                assert self.headers["X-Xai-Token-Auth"] == "xai-grok-cli"
                assert self.headers["X-Grok-Client-Version"] is None
                assert self.headers["Accept"] == "application/json" and self.headers["Cookie"] is None
                status = 200 if authorization == ["Bearer " + renewed] else self.user_status
                users.append((direct, status))
                return self.reply(status, {"error": token})
            assert self.path == ("/grok-cli/v1/models" if direct else "/v1/models")
            assert direct or self.headers["Host"] == host
            authorization = self.headers.get_all("Authorization")
            assert authorization in (["Bearer " + token], ["Bearer " + renewed])
            assert self.headers["X-Email"] == email and self.headers["X-Userid"] == principal
            assert self.headers["Accept"] == "application/json"
            assert self.headers["User-Agent"].startswith("9Router/")
            assert self.headers["X-Grok-Client-Mode"] == "headless"
            assert self.headers["X-Xai-Token-Auth"] == "xai-grok-cli"
            for header in ("Proxy-Authorization", "X-Relay-Target", "Cookie", "X-9r-Cli-Token"):
                assert self.headers[header] is None
            status = 200 if authorization == ["Bearer " + renewed] else self.status
            calls.append((direct, status))
            if self.hold and status == 401:
                arrived, release = self.hold
                arrived.set()
                assert release.wait(5)
            payload = {"data": [{"id": "fixture-grok-tunnel", "name": "Live fixture",
                                 "capabilities": {"tools": True}},
                                {"id": authorization[0].removeprefix("Bearer ")}]}
            self.reply(status, payload if status < 400 else {"error": token + " " + email})
        except AssertionError as error:
            shared["ERRORS"].append((self.path, repr(error)))
            self.send_error(422)

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if status == 307:
            self.send_header("Location", "https://" + host + "/no-follow")
        self.end_headers()
        self.wfile.write(body)

    def oauth_headers(self):
        assert self.headers["Accept"] == "application/json"
        assert all(self.headers[name] is None for name in (
            "Authorization", "X-Email", "X-Userid", "Cookie", "Proxy-Authorization", "X-Relay-Target",
        ))

    def discover(self, direct):
        try:
            prefix = f"/xai-oauth/{issuer_host}" if direct else ""
            assert self.path == prefix + "/.well-known/openid-configuration"
            assert direct or self.headers["Host"] == issuer_host
            self.oauth_headers()
            oauth.append(("discovery", direct, issuer_host))
            site = token_host if self.discovery != "untrusted" else token_host + ".fixture.invalid"
            self.reply(307 if self.discovery == "redirect" else 200, {
                "issuer": "https://" + issuer_host,
                "authorization_endpoint": "https://" + issuer_host + "/oauth2/authorize",
                "token_endpoint": "https://" + site + "/oauth2/fixture-token?region=global",
            })
        except AssertionError as error:
            shared["ERRORS"].append((self.path, repr(error)))
            self.send_error(422)

    def do_POST(self):
        direct = self.server.server_port == 52989
        try:
            if self.path in ("/grok-cli/v1/responses", "/v1/responses"):
                return self.respond_chat(direct)
            site = issuer_host if self.path.endswith("/oauth2/token") else token_host
            prefix = f"/xai-oauth/{site}" if direct else ""
            route = "/oauth2/token" if site == issuer_host else "/oauth2/fixture-token?region=global"
            assert self.path == prefix + route and (direct or self.headers["Host"] == site)
            self.oauth_headers()
            assert self.headers.get_content_type() == "application/x-www-form-urlencoded"
            fields = parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode(), strict_parsing=True)
            assert fields == {"grant_type": ["refresh_token"], "refresh_token": [refresh],
                              "client_id": ["b1a00492-073a-47ea-816f-4c329264a828"]}
            oauth.append(("grant", direct, site))
            if self.grant == "disconnect":
                self.connection.shutdown(socket.SHUT_RDWR)
                self.close_connection = True
                return
            self.reply(200, {"access_token": renewed, "refresh_token": rotated, "expires_in": 3600})
        except (AssertionError, ValueError) as error:
            shared["ERRORS"].append((self.path, repr(error)))
            self.send_error(422)

    def respond_chat(self, direct):
        assert self.path == ("/grok-cli/v1/responses" if direct else "/v1/responses")
        assert direct or self.headers["Host"] == host
        authorization = self.headers.get_all("Authorization")
        assert authorization in (["Bearer " + token], ["Bearer " + renewed])
        assert self.headers["X-Email"] == email and self.headers["X-Userid"] == principal
        assert self.headers["User-Agent"].startswith("9Router/")
        assert self.headers["X-Xai-Token-Auth"] == "xai-grok-cli"
        assert self.headers["X-Grok-Client-Mode"] == "headless"
        assert self.headers["Accept"] == "text/event-stream" and self.headers["Content-Type"] == "application/json"
        for name in ("X-Grok-Client-Version", "X-Grok-Client-Identifier", "X-Relay-Target", "Proxy-Authorization", "Cookie"):
            assert self.headers[name] is None
        session = self.headers["X-Grok-Session-Id"]
        assert session == self.headers["X-Grok-Conv-Id"]
        UUID(session)
        request = self.headers["X-Grok-Req-Id"]
        UUID(request)
        turn = int(self.headers["X-Grok-Turn-Idx"])
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["stream"] is True and body["store"] is False
        assert body["model"] == self.headers["X-Grok-Model-Override"]
        assert body["model"] in ("grok-build", "grok-4.5")
        assert "reasoning.encrypted_content" in body["include"]
        assert not {"messages", "previous_response_id", "reasoning_effort"}.intersection(body)
        status = 200 if authorization == ["Bearer " + renewed] else self.status
        chats.append((direct, status, session, request, turn, body))
        if status == 503:
            type(self).status = 200
        if status != 200:
            return self.reply(status, {"error": f"fixture rejected {token} for {email}"})
        text = "native Grok fixture"
        item = {"id": "msg_grok_fixture", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]}
        result = {"id": "resp_grok_fixture", "object": "response", "created_at": 1795046400, "model": body["model"],
                  "status": "completed", "output": [item],
                  "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}}
        events = [
            {"type": "response.created", "response": {**result, "status": "in_progress", "output": [], "usage": None}},
            {"type": "response.output_text.delta", "output_index": 0, "item_id": item["id"], "delta": text},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": result},
        ]
        data = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events)
        wire = (data + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)


def save(db, detail=None, expires="2099-12-31T00:00:00Z", authorization=False):
    account = {"accessToken": token, "expiresAt": expires,
               "providerSpecificData": {"email": email, "principalId": principal, **(detail or {})}}
    if authorization:
        account["refreshToken"] = refresh
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES ('fixture-grok-proxy','grok-cli','oauth','Offline Grok',1,1,?,
                  '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (json.dumps(account),))


def stored(db):
    with sqlite3.connect(db) as sql:
        raw, = sql.execute("SELECT data FROM providerConnections WHERE id='fixture-grok-proxy'").fetchone()
    return json.loads(raw)


def get(port, path, headers):
    status, raw = shared["get"](port, path, headers)
    assert status == 200 and all(secret.encode() not in raw for secret in secrets), (status, raw)
    return json.loads(raw)


def admin(port):
    return get(port, url, shared["ADMIN"])


def catalog(port):
    return {row["id"]: row for row in get(port, "/v1/models", api)["data"] if row["owned_by"] == "gcli"}


def invoke(port, path, body, session="grok-fixture-thread", headers=api):
    connection = HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        connection.request("POST", path, json.dumps(body), headers={**headers, "Content-Type": "application/json",
                                                                   "X-Session-Id": session})
        response = connection.getresponse()
        raw = response.read()
        assert all(secret.encode() not in raw for secret in secrets), raw
        content = response.getheader("Content-Type", "")
        return response.status, raw.decode() if "text/event-stream" in content else json.loads(raw)
    finally:
        connection.close()


def chat_cases(port, db, http, pool, connect):
    chat_path = "/v1/chat/completions"
    responses_path = "/v1/responses"
    message = "native Grok fixture"
    raw = {"model": "gcli/grok-4.5-low", "input": "Hello", "stream": False, "store": True,
           "previous_response_id": "resp_old", "reasoning_effort": "max"}
    chat_body = {"model": "gcli/grok-build", "messages": [{"role": "user", "content": "Hello"}]}
    save(db)
    status, body = invoke(port, responses_path, raw)
    assert status == 200 and body["output"][0]["content"][0]["text"] == message, (status, body)
    initial = chats[-1]
    assert initial[:2] == (True, 200) and initial[4] == 1
    assert initial[5]["reasoning"] == {"summary": "concise", "effort": "xhigh"}
    status, body = invoke(port, responses_path, {**raw, "stream": True})
    assert status == 200 and "response.output_text.delta" in body and message in body
    later = chats[-1]
    assert later[2] == initial[2] and later[3] != initial[3] and later[4] == 2
    status, body = invoke(port, chat_path, chat_body, "other-grok-session")
    assert status == 200 and body["choices"][0]["message"]["content"] == message, (status, body)
    assert chats[-1][2] != initial[2] and chats[-1][4] == 1
    assert "effort" not in chats[-1][5]["reasoning"]
    status, body = invoke(port, "/v1/messages", {**chat_body, "max_tokens": 100}, "messages-grok-session")
    assert status == 200 and body["content"][0]["text"] == message, (status, body)

    before = len(oauth), len(shared["TUNNELS"]), len(chats)
    Catalog.status = 401
    save(db, http, authorization=True)
    status, body = invoke(port, responses_path, raw)
    assert status == 200 and body["output"][0]["content"][0]["text"] == message, (status, body)
    assert oauth[before[0]:] == [("discovery", False, issuer_host), ("grant", False, token_host)]
    assert shared["TUNNELS"][before[1]:] == [("connect", name, 443) for name in (host, issuer_host, token_host, host)]
    first, second = chats[before[2]:]
    assert first[:2] == (False, 401) and second[:2] == (False, 200) and first[2:] == second[2:]
    assert stored(db)["refreshToken"] == rotated

    before = len(oauth), len(shared["TUNNELS"]), len(chats)
    save(db, pool, expired, authorization=True)
    assert invoke(port, chat_path, chat_body)[0] == 200
    assert len(chats) == before[2] + 1 and chats[-1][:2] == (False, 200)
    assert oauth[before[0]:] == [("discovery", False, issuer_host), ("grant", False, token_host)]
    assert shared["TUNNELS"][before[1]:] == [("socks", name, 443) for name in (issuer_host, token_host, host)]

    Catalog.status = 402
    save(db, http)
    status, body = invoke(port, responses_path, raw)
    assert status == 402 and "[redacted]" in body["error"]["message"], (status, body)
    Catalog.status = 503
    before_retry = len(chats)
    assert invoke(port, responses_path, raw)[0] == 200
    first, second = chats[before_retry:]
    assert first[:2] == (False, 503) and second[:2] == (False, 200) and first[2:] == second[2:]
    save(db, http)
    before = len(chats)
    connect.reject = True
    assert invoke(port, responses_path, raw)[0] == 502
    assert len(chats) == before
    connect.reject = False
    save(db, {"proxyPoolId": "grok-relay"})
    assert invoke(port, responses_path, raw)[0] == 503 and len(chats) == before


def user_tests(port, db, http, pool):
    route = "/api/providers/fixture-grok-proxy/test"
    admin = shared["ADMIN"]
    before_chats = len(chats)
    save(db)
    assert invoke(port, route, {}, headers=admin) == (200,{"valid":True,"error":None,"refreshed":False})
    assert users == [(True, 200)]

    Catalog.user_status = 402
    save(db, http)
    status, result = invoke(port, route, {}, headers=admin)
    assert status == 200 and result["valid"] is True and "credits are exhausted" in result["error"]
    row = stored(db)
    assert row["testStatus"] == "active" and row["lastError"] == result["error"] and row["lastErrorAt"]
    status, batch = invoke(port, "/api/providers/test-batch", {"mode":"provider","providerId":"grok-cli"}, headers=admin)
    assert status == 200 and batch["summary"] == {"total":1,"failed":0,"passed":1}
    assert batch["results"][0]["error"] == result["error"] and users[-2:] == [(False, 402)] * 2

    Catalog.user_status = 401
    save(db, http, authorization=True)
    start = len(oauth), len(shared["TUNNELS"])
    assert invoke(port, route, {}, headers=admin) == (200,{"valid":True,"error":None,"refreshed":True})
    assert users[-2:] == [(False, 401), (False, 200)]
    assert oauth[start[0]:] == [("discovery", False, issuer_host), ("grant", False, token_host)]
    assert shared["TUNNELS"][start[1]:] == [("connect", name, 443) for name in (host, issuer_host, token_host, host)]
    assert stored(db)["refreshToken"] == rotated and stored(db)["lastError"] is None

    save(db, pool, expired, authorization=True)
    start = len(users), len(shared["TUNNELS"])
    assert invoke(port, route, {}, headers=admin) == (200,{"valid":True,"error":None,"refreshed":True})
    assert users[start[0]:] == [(False, 200)]
    assert shared["TUNNELS"][start[1]:] == [("socks", name, 443) for name in (issuer_host, token_host, host)]
    assert len(chats) == before_chats


with tempfile.TemporaryDirectory(prefix="9router-grok-cli-models-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    hosts = (host, issuer_host, token_host)
    ca, target, _ = shared["tls_services"](stack, scratch, Catalog, hosts)
    stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1", 52989), Catalog)))
    connect, socks, relay = [shared["Proxy"](("127.0.0.1", 0), shared["Tunnel"]) for _ in range(3)]
    for proxy, mode in ((connect, "connect"), (socks, "socks"), (relay, "connect")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        proxy.hosts, proxy.secrets = hosts, secrets
        stack.enter_context(shared["serving"](proxy))
    address = f"http://127.0.0.1:{connect.server_address[1]}"
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": address}
    pool = {"proxyPoolId": "grok-socks"}
    pools = [("grok-socks", "socks5", f"socks5h://127.0.0.1:{socks.server_address[1]}"),
             ("grok-relay", "cloudflare", f"http://127.0.0.1:{relay.server_address[1]}")]
    with shared["gateway"](scratch, "local-tls", ca, pools) as (port, db):
        save(db)
        assert shared["get"](port, url)[0] == 401 and not calls
        baseline = admin(port)
        public = catalog(port)
        assert len(baseline["models"]) == 1 and public[live]["capabilities"] == {"tools": True}
        assert calls == [(True, 200)] * 2
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO providerConnections
              (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
              VALUES ('fixture-unmigrated','qoder','apikey','Offline Qoder',1,1,'{}',
                      '2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""")
        for detail in (http, pool):
            save(db, detail)
            assert admin(port) == baseline and catalog(port) == public
        assert calls[2:] == [(False, 200)] * 4
        assert shared["TUNNELS"] == [("connect", host, 443)] * 2 + [("socks", host, 443)] * 2

        save(db, http | {"connectionNoProxy": ".grok.com"})
        assert admin(port) == baseline and catalog(port) == public
        assert calls[-2:] == [(True, 200)] * 2 and len(calls) == 8
        for detail in ({"proxyPoolId": "grok-relay"}, {"proxyPoolId": "missing"},
                       http | {"connectionNoProxy": ".grok.com", "strictProxy": True}):
            save(db, detail)
            assert "warning" in admin(port) and live not in catalog(port)
        save(db, http, expired)
        assert "access token has expired" in admin(port)["warning"] and live not in catalog(port)
        assert len(calls) == 8 and len(shared["TUNNELS"]) == 4

        save(db, http)
        Catalog.status = 307
        assert "redirect refused" in admin(port)["warning"]
        Catalog.status = 401
        assert "[redacted] [redacted]" in admin(port)["warning"]
        assert stored(db)["accessToken"] == token and not stored(db).get("grokCliRefreshAttempt")
        connect.reject = True
        assert "fetch failed" in admin(port)["warning"]
        assert calls[-2:] == [(False, 307), (False, 401)] and len(calls) == 10
        connect.reject = False
        Catalog.status = 200

        for detail, lookup, grant, model_is_direct in (
            (http, ("discovery", False, issuer_host), ("grant", False, token_host), False),
            (http | {"connectionNoProxy": f"{host},{issuer_host}"},
             ("discovery", True, issuer_host), ("grant", False, token_host), True),
            (http | {"connectionNoProxy": token_host},
             ("discovery", False, issuer_host), ("grant", True, token_host), False),
        ):
            before = len(calls), len(oauth)
            save(db, detail, expired, authorization=True)
            assert admin(port) == baseline
            assert calls[before[0]:] == [(model_is_direct, 200)] and oauth[before[1]:] == [lookup, grant]
            assert stored(db)["accessToken"] == renewed and stored(db)["refreshToken"] == rotated
            assert stored(db)["grokCliRefreshAttempt"] is None

        Catalog.status = 401
        before = len(oauth), len(shared["TUNNELS"])
        save(db, pool, authorization=True)
        with ThreadPoolExecutor(max_workers=4) as workers:
            assert all(response == public for response in workers.map(lambda _: catalog(port), range(4)))
        assert oauth[before[0]:] == [("discovery", False, issuer_host), ("grant", False, token_host)]
        assert all(row[0] == "socks" for row in shared["TUNNELS"][before[1]:])
        assert stored(db)["refreshToken"] == rotated

        save(db, authorization=True)
        shared["enable_global_proxy"](db, address)
        arrived, release = Catalog.hold = Event(), Event()
        before = len(calls), len(oauth)
        with ThreadPoolExecutor() as workers:
            pending = workers.submit(admin, port)
            try:
                assert arrived.wait(5)
                with sqlite3.connect(db) as sql:
                    sql.execute("UPDATE settings SET data='{}' WHERE id=1")
            finally:
                release.set()
            assert pending.result()["warning"].endswith("configured proxy is unavailable.")
        Catalog.hold = None
        assert calls[before[0]:] == [(False, 401)] and len(oauth) == before[1]
        assert not stored(db).get("grokCliRefreshAttempt")

        Catalog.status = 200
        Catalog.discovery = "untrusted"
        save(db, http, expired, authorization=True)
        assert admin(port) == baseline
        assert oauth[-2:] == [("discovery", False, issuer_host), ("grant", False, issuer_host)]

        Catalog.discovery = "redirect"
        save(db, http, expired, authorization=True)
        before = len(calls), len(oauth)
        assert "warning" in admin(port) and not stored(db).get("grokCliRefreshAttempt")
        assert len(calls) == before[0] and oauth[before[1]:] == [("discovery", False, issuer_host)]

        Catalog.discovery = "trusted"
        Catalog.grant = "disconnect"
        save(db, http, expired, authorization=True)
        before = len(calls), len(oauth)
        first = admin(port)
        assert first == admin(port) and "sign in again" in first["warning"]
        assert len(calls) == before[0] and oauth[before[1]:] == [
            ("discovery", False, issuer_host), ("grant", False, token_host)]
        assert stored(db)["refreshToken"] == refresh and stored(db)["grokCliRefreshAttempt"]
        Catalog.grant = "rotate"

    with shared["gateway"](scratch, "untrusted-tls", None, pools) as (port, db):
        save(db, http)
        before = len(calls), len(oauth)
        assert "fetch failed" in admin(port)["warning"]
        assert shared["HELLOS"][-1] == host
        save(db, http, expired, authorization=True)
        assert "warning" in admin(port) and not stored(db).get("grokCliRefreshAttempt")
        assert (len(calls), len(oauth)) == before and shared["HELLOS"][-1] == issuer_host
    with shared["gateway"](scratch, "grok-inference", ca, pools) as (port, db):
        chat_cases(port, db, http, pool, connect)
    with shared["gateway"](scratch, "grok-user-test", ca, pools) as (port, db):
        user_tests(port, db, http, pool)
    assert not shared["ERRORS"], shared["ERRORS"]
print("Grok CLI catalog, OAuth Test, renewal and inference contracts passed direct, CONNECT and SOCKS5h checks.")
