from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import parse_qs, quote_plus, urlsplit

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "vertex_chat_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "aiplatform.googleapis.com"
ORIGIN = "https://" + HOST
MODEL, PARTNER = "gemini-2.5-flash", "deepseek-ai/deepseek-v3.2-maas"
SINGLE, PUBLISHER = "vertex/" + MODEL, "vertex-partner/" + PARTNER
ROOT = f"publishers/google/models/{MODEL}"
PLAIN, STREAM = f"/v1/{ROOT}:generateContent", f"/v1/{ROOT}:streamGenerateContent"
SCOPED = f"/v1/projects/fixture-project/locations/eu/{ROOT}:generateContent"
COMPAT = "/v1/projects/fixture-project/locations/global/endpoints/openapi/chat/completions"
RAW, TOKEN = "offline-vertex-protected-+/=key", "offline-vertex-protected-token"
BACKUP = "offline-vertex-backup-must-not-send"
RELAY_HOST = "vertex-fixture.offline-lane.workers.dev"
RELAY = "https://" + RELAY_HOST
RELAY_KEY = "J" * 43
SA_SECRET, ADC_SECRET = "never-send-private-key", "never-send-adc-secret"
USAGE = {"promptTokenCount": 8, "candidatesTokenCount": 3, "totalTokenCount": 11}
CHAT_USAGE = {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
MODE, EXPECT = "normal", (PLAIN, RAW, "query")
CALLS, ERRORS = [], []
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.vertex-fixture.invalid"
shared.SECRETS = (RAW, quote_plus(RAW), TOKEN, BACKUP, RELAY_KEY, SA_SECRET, ADC_SECRET)


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw, mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        path = urlsplit(self.path)
        try:
            assert not any(self.headers[field] for field in
                           ("Cookie", "x-fixture-internal", "Proxy-Authorization", "x-api-key"))
            direct = self.server.server_port == 52989
            lane = "direct" if direct else "tunnel"
            if path.path == "/cloudflare-relay/" + RELAY_HOST:
                lane = "relay"
                assert self.headers.get_all("x-relay-target") == [ORIGIN] and not path.query
                assert self.headers.get_all("x-relay-auth") == [RELAY_KEY]
                path = urlsplit(self.headers["x-relay-path"])
            else:
                assert not any(self.headers[field] for field in ("x-relay-auth", "x-relay-target", "x-relay-path"))
                if direct:
                    path = urlsplit(self.path.removeprefix("/vertex"))
                else:
                    assert self.headers["Host"] == HOST
            expected, credential, channel = EXPECT
            assert path.path == expected
            stream = expected == STREAM
            expect_query = {"alt": ["sse"]} if stream else {}
            if channel == "query":
                expect_query["key"] = [credential]
            assert parse_qs(path.query) == expect_query
            assert self.headers.get_all("Authorization") == (["Bearer " + credential] if channel == "bearer" else None)
            assert self.headers.get_all("x-goog-api-key") == ([credential] if channel == "header" else None)
            assert (self.headers["Accept"] == "text/event-stream") == stream
            assert self.headers["Content-Type"] == "application/json"
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if expected == COMPAT:
                assert body["model"] == PARTNER and "contents" not in body
                assert body["messages"] == [{"role": "user", "content": "fixture prompt"}]
            else:
                assert body["model"] == MODEL and "messages" not in body and len(body["safetySettings"]) == 5
                assert body["contents"][0]["parts"] == [{"text": "fixture prompt"}]
            CALLS.append((lane, expected, body))
            if MODE == "redirect":
                return self.reply(307, b"{}", location="https://untrusted.invalid/credential-sink")
            if MODE == "echo":
                error = f"vendor rejected {credential} {quote_plus(credential)}"
                if lane == "relay":
                    error += " and " + RELAY_KEY
                return self.reply(422, json.dumps({"error": {"message": error}}).encode())
            if MODE == "401":
                return self.reply(401, b'{"error":{"message":"unauthorized"}}')
            if expected == COMPAT:
                result = {"id": "fixture-vertex-partner", "created": 1795046400, "model": PARTNER, "object": "chat.completion",
                          "choices": [{"index": 0, "message": {"role": "assistant", "content": "fixture answer"}, "finish_reason": "stop"}],
                          "usage": CHAT_USAGE}
                return self.reply(200, json.dumps(result).encode())
            base = {"responseId": "fixture-vertex", "modelVersion": MODEL, "createTime": "2026-11-19T00:00:00Z"}
            content = {"role": "model", "parts": [{"text": "fixture answer"}]}
            if stream:
                frames = [
                    {"response": {**base, "candidates": [{"content": content}]}},
                    {**base, "candidates": [{"finishReason": "STOP", "content": {"parts": []}}], "usageMetadata": USAGE},
                ]
                return self.reply(200, b"".join(b"data: " + json.dumps(frame).encode() + b"\n\n" for frame in frames), "text/event-stream")
            result = {"response": {**base, "candidates": [{"content": content, "finishReason": "STOP"}], "usageMetadata": USAGE}}
            self.reply(200, json.dumps(result).encode())
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((path.path, repr(error)))
            self.reply(422, b"{}")


def save(db, provider="vertex", details=None, credentials=None, backup=False):
    if credentials is None:
        credentials = {"apiKey": BACKUP if backup else RAW}
    state = {**credentials, "providerSpecificData": {"baseUrl": "https://untrusted.invalid", **(details or {})}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Synthetic Vertex',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if backup else ""), provider, 2 if backup else 1, json.dumps(state)))


def ask(port, model=SINGLE, stream=False, status=200, cli=False):
    headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json",
               "Cookie": "must not forward", "x-fixture-internal": "must not forward"}
    if cli:
        headers["x-app"] = "cli"
    body = {"model": model, "messages": [{"role": "user", "content": "fixture prompt"}], "stream": stream}
    with closing(HTTPConnection("127.0.0.1", port, timeout=6)) as app:
        app.request("POST", "/v1/chat/completions", json.dumps(body).encode(), headers)
        response = app.getresponse()
        mime, raw = response.getheader("Content-Type"), response.read()
        assert not any(secret.encode() in raw for secret in shared.SECRETS)
        assert response.status == status, (response.status, raw, ERRORS)
        if status != 200:
            return json.loads(raw)["error"]["message"]
        if stream:
            assert mime == "text/event-stream"
            data = [json.loads(line[6:]) for line in raw.decode().splitlines()
                    if line.startswith("data: ") and line != "data: [DONE]"]
            choices = [choice for value in data for choice in value["choices"]]
            assert "".join(choice.get("delta", {}).get("content", "") or "" for choice in choices) == "fixture answer"
            assert any(choice.get("finish_reason") == "stop" for choice in choices)
        else:
            value = json.loads(raw)
            assert value["choices"][0]["message"]["content"] == "fixture answer"
            assert value["usage"]["total_tokens"] == 2011
        return raw


def cases(port, db, connect, http, socks_address, wrong_tls):
    global MODE, EXPECT
    save(db)
    save(db, backup=True)
    save(db, "vertex-partner", {"projectId": "fixture-project"})
    ask(port)
    plain = CALLS[-1][2]
    assert CALLS[-1][:2] == ("direct", PLAIN)
    shared.enable_global_proxy(db, http["connectionProxyUrl"])
    ask(port)
    assert CALLS[-1] == ("tunnel", PLAIN, plain)
    assert shared.TUNNELS == [("connect", HOST, 443)] and shared.HELLOS == [HOST]
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")

    save(db, details={"proxyPoolId": "socks"})
    EXPECT = (STREAM, RAW, "query")
    ask(port, stream=True)
    assert shared.TUNNELS[-1] == ("socks", HOST, 443)
    save(db, details=http | {"projectId": "fixture-project", "location": "eu"},
         credentials={"accessToken": TOKEN, "expiresAt": "2099-01-01T00:00:00Z", "apiKey": BACKUP})
    EXPECT = (SCOPED, TOKEN, "bearer")
    ask(port)
    assert CALLS[-1][:2] == ("tunnel", SCOPED)
    EXPECT = (COMPAT, RAW, "query")
    save(db, "vertex-partner", http | {"projectId": "fixture-project"})
    ask(port, PUBLISHER)
    EXPECT = (COMPAT, TOKEN, "bearer")
    save(db, "vertex-partner", {"proxyPoolId": "socks", "projectId": "fixture-project"}, {"accessToken": TOKEN})
    ask(port, PUBLISHER)
    assert CALLS[-1][:2] == ("tunnel", COMPAT) and shared.TUNNELS[-1] == ("socks", HOST, 443)

    save(db, details=http | {"connectionNoProxy": ".googleapis.com"})
    EXPECT = (PLAIN, RAW, "query")
    count = len(shared.TUNNELS)
    ask(port)
    assert CALLS[-1] == ("direct", PLAIN, plain) and len(shared.TUNNELS) == count
    shared.enable_global_proxy(db, socks_address)
    ask(port)
    assert CALLS[-1][0] == "tunnel" and shared.TUNNELS[-1] == ("socks", HOST, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data=? WHERE id=1", (json.dumps({"providerOverrides": {"vertex": {"headers": {
            "x-goog-api-key": BACKUP, "Proxy-Authorization": BACKUP, "x-relay-auth": BACKUP, "accept": "broken",
        }}}}),))

    save(db, details={"proxyPoolId": "managed"})
    EXPECT = (PLAIN, RAW, "header")
    ask(port)
    assert CALLS[-1] == ("relay", PLAIN, plain)
    EXPECT = (STREAM, RAW, "header")
    ask(port, stream=True)
    MODE = "echo"
    assert "[redacted]" in ask(port, stream=True, status=422)
    MODE = "normal"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")

    before = (len(CALLS), len(shared.TUNNELS))
    managed = {"proxyPoolId": "managed"}
    sa = json.dumps({"type": "service_account", "project_id": "fixture-project", "client_email": "fixture@example.invalid",
                     "private_key": SA_SECRET})
    adc = json.dumps({"type": "authorized_user", "quota_project_id": "fixture-project", "client_id": "fixture",
                      "client_secret": ADC_SECRET, "refresh_token": TOKEN})
    for provider, model, details, credentials in (
        ("vertex", SINGLE, http, {"apiKey": sa, "accessToken": TOKEN}),
        ("vertex", SINGLE, http, {"apiKey": adc}),
        ("vertex", SINGLE, http | {"projectId": "fixture-project"}, {"accessToken": TOKEN, "expiresAt": 0}),
        ("vertex", SINGLE, http | {"projectId": "fixture-project"}, {"accessToken": TOKEN, "refreshToken": ADC_SECRET}),
        ("vertex", SINGLE, managed | {"projectId": "fixture-project"}, None),
        ("vertex", SINGLE, managed | {"projectId": "fixture-project"}, {"accessToken": TOKEN}),
        ("vertex", SINGLE, {"proxyPoolId": "unmanaged"}, None),
        ("vertex", SINGLE, {"vercelRelayUrl": RELAY}, None),
        ("vertex", SINGLE, {"proxyPoolId": "missing"}, None),
        ("vertex", SINGLE, http | {"strictProxy": True, "connectionNoProxy": ".googleapis.com"}, None),
        ("vertex-partner", PUBLISHER, http, None),
        ("vertex-partner", PUBLISHER, managed | {"projectId": "fixture-project"}, None),
    ):
        save(db, provider, details, credentials)
        ask(port, model, status=503)
        assert (len(CALLS), len(shared.TUNNELS)) == before
    save(db, details=http)
    ask(port, status=503, cli=True)
    assert (len(CALLS), len(shared.TUNNELS)) == before
    EXPECT = (PLAIN, RAW, "query")
    for mode, status, expected in (("redirect", 502, "redirect refused"), ("401", 401, "unauthorized"),
                                    ("echo", 422, "[redacted]")):
        MODE = mode
        count = (len(CALLS), len(shared.TUNNELS))
        assert expected in ask(port, status=status)
        assert (len(CALLS), len(shared.TUNNELS)) == (count[0] + 1, count[1] + 1)
    MODE = "normal"
    count = (len(CALLS), len(shared.TUNNELS))
    normal_tls = connect.target
    connect.target = wrong_tls
    ask(port, status=502)
    assert len(CALLS) == count[0] and len(shared.TUNNELS) == count[1] + 1
    assert shared.HELLOS[-1] == HOST
    connect.target = normal_tls
    connect.reject = True
    save(db, "vertex-partner", {"projectId": "fixture-project"})
    for combo, members in (("vertex-proxy-inner", [SINGLE, PUBLISHER]),
                           ("vertex-proxy-outer", ["vertex-proxy-inner", PUBLISHER])):
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                VALUES (?,?,'chat',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        (combo, combo, json.dumps(members)))
        count = (len(CALLS), len(shared.TUNNELS))
        ask(port, combo, status=502)
        assert (len(CALLS), len(shared.TUNNELS)) == (count[0], count[1] + 1)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)


with tempfile.TemporaryDirectory(prefix="9router-vertex-chat-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, tls, wrong_tls = shared.tls_services(stack, scratch, Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, tls
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_address = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("socks", "socks5", socks_address), ("managed", "cloudflare", RELAY), ("unmanaged", "cloudflare", RELAY)]
    with shared.gateway(scratch, "vertex-chat", ca, pools) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed'",
                        (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
        cases(port, db, connect, http, socks_address, wrong_tls)

print("Vertex and Vertex Partner native chat preserve pinned JSON/SSE wire and protected proxy boundaries.")
