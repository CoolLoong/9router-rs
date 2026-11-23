from contextlib import ExitStack, contextmanager
from concurrent.futures import ThreadPoolExecutor
import http.client
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import select
import sqlite3
import subprocess
import sys
import tempfile
from threading import Event


ROOT = Path(__file__).resolve().parents[2]
PROXY = runpy.run_path(str(ROOT / "rust/reference/zed-cline-models-proxy-contract.py"))
CASES = ROOT / "rust/cline-transport-replay/rust/app-cases"
TARGET = "api.cline.bot"
CHAT = "/api/v1/chat/completions"
GRANT = "/api/v1/auth/refresh"
INFERENCE = "/v1/chat/completions"
ACCESS = "WorkOS:eyJfixture.bearer.mock"
BARE_ACCESS = "eyJfixture.raw.mock"
PASS_ACCESS = "eyJfixture.passrenewed.mock"
PASS_KEY = "clp_fixture_only"
REFRESH = "fixture-rejected-refresh"
RENEWED = "workos:eyJfixture.proxy-renewed.signature"
ROTATED = "fixture-proxy-rotated"
HEADERS = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json"}
MODELS = {"cline": "cl/anthropic/claude-sonnet-4.6", "clinepass": "clinepass/cline-pass/glm-5.2"}
CALLS, DIRECT, ERRORS = [], [], []
GRANTS = []


@contextmanager
def pinned_service(scratch):
    (scratch / "cline-transport-implementation").write_text("rust")
    code = '''
import http from "node:http";
const { handleClineMock } = await import(process.argv[1]);
const server = http.createServer((request, response) => {
  handleClineMock(request, response).catch(() => response.writeHead(500).end());
});
server.listen(0, "127.0.0.1", () => console.log(server.address().port));
'''
    with subprocess.Popen(["node", "--input-type=module", "-e", code,
                           (ROOT / "rust/reference/mock-cline.mjs").as_uri()], cwd=scratch,
                          env={"DATA_DIR": str(scratch), "HOME": str(scratch), "PATH": os.environ["PATH"]},
                          stdout=subprocess.PIPE, text=True) as node:
        try:
            assert select.select([node.stdout], [], [], 5)[0]
            yield int(node.stdout.readline())
        finally:
            node.terminate()
            node.wait(timeout=5)


class Vendor(PROXY["Vendor"]):
    hold = None
    grant_status = 200

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        payload = json.loads(raw)
        auth = self.headers["Authorization"]
        if self.server.server_port == 52989:
            DIRECT.append((self.path, auth))
            return self.reply(422, {"error": "Request escaped the proxy"})
        try:
            if self.path == GRANT:
                assert self.headers["Host"] == TARGET and auth is None
                assert payload == {"refreshToken":REFRESH,"grantType":"refresh_token","clientType":"extension"}
                GRANTS.append(self.path)
                return self.reply(self.grant_status,{"data":{"accessToken":RENEWED,"refreshToken":ROTATED,
                                                 "expiresAt":"2099-12-31T00:00:00Z"}})
            assert self.path == CHAT and self.headers["Host"] == TARGET
            assert all(self.headers[name] is None for name in (
                "Proxy-Authorization", "X-Relay-Target", "X-Relay-Path", "X-Relay-Auth",
            ))
            assert self.headers["X-Fixture-Cline"] == "preserved"
            scenario = payload["messages"][0]["content"]
            CALLS.append((scenario, auth))
            if scenario == "tunneled-denial":
                assert auth == "Bearer " + ACCESS
                return self.reply(401, {"error": {"message": f"rejected {auth} and {REFRESH}"}})
            if scenario in ("tunneled-renew", "tunneled-expired", "tunneled-pass-renew", "tunneled-route-change", "tunneled-renew-denied"):
                if auth in ("Bearer " + ACCESS, "Bearer workos:" + PASS_ACCESS):
                    assert scenario != "tunneled-expired"
                    if scenario == "tunneled-route-change":
                        arrived, release = self.hold
                        arrived.set()
                        assert release.wait(5)
                    return self.reply(401, {"error":"Renew credential"})
                assert auth == "Bearer " + RENEWED
                if scenario == "tunneled-renew-denied":
                    return self.reply(403,{"error":f"rejected {auth} after {ACCESS} and {ROTATED}"})
                return self.reply(200,{"choices":[{"message":{"role":"assistant","content":"Renewed Cline"},
                                                  "finish_reason":"stop"}]})
            if scenario == "tunneled-redirect":
                assert auth == "Bearer " + PASS_KEY
                return self.reply(307, {"error": "Wrong destination"}, "https://cloud.zed.dev/elsewhere")
            connection = http.client.HTTPConnection("127.0.0.1", self.pinned_port, timeout=5)
            try:
                headers = {key: value for key, value in self.headers.items() if key.lower() != "host"}
                connection.request("POST", "/cline" + CHAT, raw, headers)
                response = connection.getresponse()
                result = response.read()
            finally:
                connection.close()
            self.send_response(response.status)
            self.send_header("Content-Type", response.headers["Content-Type"])
            self.send_header("Content-Length", str(len(result)))
            self.end_headers()
            self.wfile.write(result)
        except (KeyError, ValueError, AssertionError) as error:
            ERRORS.append((self.path, repr(error)))
            self.reply(422, {"error": "Unexpected fixture request"})


def save(db, provider, proxy=None, pool=None, **overrides):
    details = {"connectionProxyEnabled": True, "connectionProxyUrl": proxy} if proxy else {}
    if pool:
        details["proxyPoolId"] = pool
    defaults = {"accessToken": ACCESS, "refreshToken": REFRESH, "expiresAt": "2099-12-31T00:00:00Z"}
    if provider == "clinepass":
        defaults = {"apiKey": PASS_KEY}
    PROXY["save"](db, provider, details, **(defaults | overrides))


def invoke(port, provider, label, stream=False, model=None, headers=HEADERS):
    body = {"model": model or MODELS[provider], "messages": [{"role": "user", "content": label}], "stream": stream}
    if stream:
        body["stream_options"] = {"include_usage": True}
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        connection.request("POST", INFERENCE, json.dumps(body), headers)
        response = connection.getresponse()
        return response.status, response.headers["Content-Type"].split(";")[0], response.read().decode()
    finally:
        connection.close()


def comparable(status, mime, raw):
    if mime == "application/json":
        return {"status": status, "mime": mime, "body": json.loads(raw)}
    assert mime == "text/event-stream"
    frames = [line[6:] for line in raw.splitlines() if line.startswith("data: ")]
    assert frames[-1] == "[DONE]"
    return {"status": status, "mime": mime, "frames": [json.loads(frame) for frame in frames[:-1]]}


def row(db, provider):
    with sqlite3.connect(db) as sql:
        value, = sql.execute("SELECT data FROM providerConnections WHERE id=?", (provider,)).fetchone()
    return json.loads(value)


def settings(db, **values):
    spec = {"providerOverrides": {provider: {"headers": {
        "X-Fixture-Cline": "preserved", "Proxy-Authorization": "local-test-only", "X-Relay-Auth": "local-relay-only",
    }} for provider in MODELS}} | values
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT INTO settings(id,data) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                    (json.dumps(spec),))


def cases(port, db, proxy, address, upstream, wrong_host):
    settings(db)
    for provider in MODELS:
        save(db, provider, address)
    for provider, prefix in (("cline", "oauth"), ("clinepass", "pass")):
        for streamed in (False, True):
            label = prefix + ("-stream" if streamed else "-json")
            expected = json.loads((CASES / (label + ".stdout")).read_text())
            assert comparable(*invoke(port, provider, label, streamed)) == expected

    save(db, "cline", address, accessToken=BARE_ACCESS)
    assert invoke(port, "cline", "oauth-normalized")[0] == 200
    assert CALLS[-1][1] == "Bearer workos:" + BARE_ACCESS
    save(db, "clinepass", address, auth_type="oauth", apiKey=None, accessToken=PASS_ACCESS,
         expiresAt="2099-12-31T00:00:00Z")
    assert invoke(port, "clinepass", "pass-oauth-refresh")[0] == 200
    assert CALLS[-1][1] == "Bearer workos:" + PASS_ACCESS

    save(db, "cline")
    settings(db, outboundProxyEnabled=True, outboundProxyUrl=address)
    assert invoke(port, "cline", "oauth-json")[0] == 200
    settings(db)
    for provider in MODELS:
        save(db, provider, address)
    before = len(CALLS)
    Vendor.grant_status = 403
    status, _, raw = invoke(port, "cline", "tunneled-denial")
    assert status == 401 and "[redacted]" in raw and ACCESS not in raw and REFRESH not in raw
    Vendor.grant_status = 200
    assert len(CALLS) == before + 1 and GRANTS == [GRANT]
    assert row(db, "cline")["accessToken"] == ACCESS
    status, _, raw = invoke(port, "clinepass", "tunneled-redirect")
    assert status == 502 and "redirected" in raw and len(CALLS) == before + 2

    save(db, "cline", address)
    status, _, raw = invoke(port,"cline","tunneled-renew")
    assert status == 200 and "Renewed Cline" in raw, (status,raw,ERRORS)
    assert CALLS[-2:] == [("tunneled-renew", "Bearer " + token) for token in (ACCESS, RENEWED)]
    assert GRANTS == [GRANT] * 2 and row(db, "cline")["refreshToken"] == ROTATED
    save(db, "cline", address, expiresAt="946684800")
    assert invoke(port,"cline","tunneled-expired")[0] == 200
    assert CALLS[-1] == ("tunneled-expired","Bearer " + RENEWED) and GRANTS == [GRANT] * 3
    save(db, "clinepass", address, auth_type="oauth", apiKey=None, accessToken=PASS_ACCESS, refreshToken=REFRESH)
    assert invoke(port,"clinepass","tunneled-pass-renew")[0] == 200
    assert CALLS[-2:] == [("tunneled-pass-renew", "Bearer workos:" + PASS_ACCESS),
                          ("tunneled-pass-renew", "Bearer " + RENEWED)]
    assert len(GRANTS) == 4 and row(db, "clinepass")["refreshToken"] == ROTATED
    save(db, "clinepass", address)
    save(db, "cline", address)
    status, _, raw = invoke(port,"cline","tunneled-renew-denied")
    assert status == 403 and "[redacted]" in raw and not any(secret in raw for secret in (RENEWED,ACCESS,ROTATED))
    assert len(GRANTS) == 5 and row(db, "cline")["refreshToken"] == ROTATED

    save(db, "cline")
    settings(db, outboundProxyEnabled=True, outboundProxyUrl=address)
    arrived, release = Vendor.hold = Event(), Event()
    with ThreadPoolExecutor() as workers:
        pending = workers.submit(invoke,port,"cline","tunneled-route-change")
        try:
            assert arrived.wait(5)
            settings(db)
        finally:
            release.set()
        assert pending.result()[0] == 401
    Vendor.hold = None
    assert len(GRANTS) == 5 and row(db, "cline")["accessToken"] == ACCESS

    tunnels, hellos = PROXY["TUNNELS"], PROXY["HELLOS"]
    assert all(entry[1:] == (TARGET, 443) for entry in tunnels)
    assert hellos == [TARGET] * len(tunnels)
    before = (len(CALLS), len(tunnels))
    save(db, "cline", address, refreshToken=None, expiresAt="2000-12-31T00:00:00Z")
    status, _, raw = invoke(port, "cline", "oauth-json")
    assert status == 401 and "expired" in raw and row(db, "cline")["accessToken"] == ACCESS
    save(db, "cline", address, apiKey=PASS_KEY)
    assert invoke(port, "cline", "oauth-json")[0] == 401
    save(db, "cline", address)
    assert invoke(port, "cline", "oauth-json", headers=HEADERS | {"X-App": "cli"})[0] == 503
    assert (len(CALLS), len(tunnels)) == before

    with sqlite3.connect(db) as sql:
        for provider in MODELS:
            sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                SELECT id||'-direct',provider,authType,name,2,1,json_remove(data,'$.providerSpecificData'),createdAt,updatedAt
                FROM providerConnections WHERE id=?""", (provider,))
        combos = {"cline-proxy-inner": [MODELS["cline"], MODELS["clinepass"]],
                  "cline-proxy-outer": ["cline-proxy-inner", MODELS["clinepass"]]}
        sql.executemany("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES (?,?,'llm',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            [(name, name, json.dumps(members)) for name, members in combos.items()])
    proxy.reject = True
    for provider in MODELS:
        assert invoke(port, provider, "oauth-json")[0] == 502
    for name in combos:
        assert invoke(port, "cline", "oauth-json", model=name)[0] == 502
    assert (len(CALLS), len(tunnels)) == (before[0], before[1] + 4)

    proxy.reject, proxy.target = False, wrong_host
    before_hello = len(hellos)
    assert invoke(port, "cline", "oauth-json")[0] == 502
    assert hellos[before_hello:] == [TARGET] and len(CALLS) == before[0]
    proxy.target = upstream
    before = len(tunnels)
    for pool in ("nonexistent-proxy", "fixture-relay"):
        for provider in MODELS:
            save(db, provider, pool=pool)
            assert invoke(port, provider, "oauth-json")[0] == 503
    assert len(tunnels) == before
    assert not DIRECT and not ERRORS and not PROXY["ERRORS"], (DIRECT, ERRORS, PROXY["ERRORS"])


with tempfile.TemporaryDirectory(prefix="9router-cline-chat-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    Vendor.pinned_port = stack.enter_context(pinned_service(scratch))
    anchor, upstream, wrong_host = PROXY["tls_services"](stack, scratch, Vendor)
    stack.enter_context(PROXY["serving"](ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    proxy = PROXY["Proxy"](("127.0.0.1", 0), PROXY["Tunnel"])
    mode = sys.argv[2]
    assert mode in ("connect", "socks5h")
    proxy.mode, scheme = ("connect", "http") if mode == "connect" else ("socks", "socks5h")
    proxy.reject, proxy.target, proxy.secrets = False, upstream, (ACCESS, PASS_KEY, REFRESH, BARE_ACCESS, PASS_ACCESS, RENEWED, ROTATED)
    stack.enter_context(PROXY["serving"](proxy))
    address = f"{scheme}://127.0.0.1:{proxy.server_address[1]}"
    with PROXY["gateway"](scratch, "cline-gateway", anchor, [("fixture-relay", "cloudflare", address)]) as (port, db):
        cases(port, db, proxy, address, upstream, wrong_host)
print(f"Cline and ClinePass pinned JSON/SSE matched through {mode}; relay, cert, auth, and nested fallback guards passed.")
