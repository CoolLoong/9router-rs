from contextlib import ExitStack, contextmanager
import http.client
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = runpy.run_path(str(ROOT / "rust/reference/zed-cline-models-proxy-contract.py"))
ADMIN = FIXTURE["ADMIN"]
HOST = "api.cline.bot"
CHAT, REFRESH_PATH = "/api/v1/chat/completions", "/api/v1/auth/refresh"
OAUTH = "WorkOS:eyJfixture.translator.saved"
FRESH = "workos:eyJfixture.translator.renewed"
KEY = "clp_fixture_translator"
REFRESH = "fixture-translator-refresh-secret"
MODELS = {"cline": "anthropic/claude-sonnet-4.6", "clinepass": "cline-pass/glm-5.2"}
AUTH = {MODELS["cline"]: OAUTH, MODELS["clinepass"]: KEY}
CALLS, REFRESHES, ERRORS = [], [], []
OLD_SECRETS = (OAUTH, FRESH, KEY, REFRESH)


class Vendor(FIXTURE["Vendor"]):
    grant_status = 200
    def do_POST(self):
        try:
            direct = self.server.server_port == 52989
            path = self.path.removeprefix("/cline") if direct else self.path
            if not direct:
                assert self.headers["Host"] == HOST
            for name in ("Proxy-Authorization", "X-Relay-Target", "X-Relay-Path", "X-Relay-Auth", "X-Client-Probe"):
                assert self.headers[name] is None
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            auth = self.headers["Authorization"]
            if path == REFRESH_PATH:
                assert auth is None and data == {
                    "clientType": "extension", "grantType": "refresh_token", "refreshToken": REFRESH,
                }
                REFRESHES.append(data)
                return self.reply(self.grant_status, {"data": {"accessToken": FRESH,"refreshToken": "fixture-rotated",
                                                  "expiresAt": "2099-12-31T00:00:00Z"}})
            assert path == CHAT and self.headers["X-CLIENT-TYPE"] == "9router"
            assert self.headers["X-Title"] == "Cline" and self.headers["HTTP-Referer"] == "https://cline.bot"
            model = data["model"]
            prompt = data["messages"][0]["content"]
            streamed = data["stream"]
            token = AUTH[model]
            assert auth == "Bearer " + token or (model == MODELS["cline"] and auth == "Bearer " + FRESH)
            expected = body(model, prompt, streamed)
            if not streamed:
                expected.pop("stream_options")
            if model == MODELS["cline"]:
                expected.pop("temperature")
            assert data == expected, data
            if streamed:
                assert self.headers["Accept"] == "text/event-stream"
            CALLS.append((direct, prompt, auth))
            if prompt in ("renew", "renew stream") and auth == "Bearer " + OAUTH:
                return self.reply(401, {"error": "renew credential"})
            if prompt == "echo error":
                return self.reply(403, {"error": {"message": f"echoed {auth} and {REFRESH}"}})
            if prompt == "redirect":
                return self.reply(307, {"error":"wrong origin"}, "https://cloud.zed.dev/elsewhere")
            if not streamed:
                return self.reply(200, {"success": True, "data": {"model":model, "choices": [{"message":{
                    "role":"assistant", "content":"Raw JSON 世界",
                }}]}})
            event = {"model":model, "choices":[{"delta":{"content": "Raw SSE 世界"}}]}
            if prompt in ("echo stream", "renew stream"):
                event["details"] = auth
            wire = ("data: " + json.dumps(event, ensure_ascii=False) + "\r\n\r\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            middle = wire.index(auth.encode()) + 9 if prompt in ("echo stream", "renew stream") else len(wire)//2
            self.wfile.write(wire[:middle])
            self.wfile.flush()
            self.wfile.write(wire[middle:])
        except (AssertionError, KeyError, ValueError) as error:
            ERRORS.append(repr(error))
            self.reply(422, {"error": "Unexpected fixture"})


def body(model, prompt, streamed):
    return {"model": model, "messages": [{"role":"user","content":prompt}],
            "stream":streamed, "stream_options":{"include_usage":True}, "temperature":.7}


def send(port, provider, prompt="ordinary", stream=False, *, payload=None, headers=ADMIN):
    model = MODELS[provider]
    if payload is None:
        payload = body(model,prompt,stream)
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request("POST", "/api/translator/send", json.dumps({"provider":provider,"model":model,
                     "body":payload,"url":"https://cloud.zed.dev/ignored","headers":{"Authorization":"ignored"}}),
                     {"Content-Type":"application/json","X-Client-Probe":"ignored", **headers})
        response = conn.getresponse()
        return response.status, response.headers["Content-Type"].split(";")[0], response.read()
    finally:
        conn.close()


def save(db, provider, *, proxy=None, pool=None, **updates):
    details = {"connectionProxyEnabled":True,"connectionProxyUrl":proxy} if proxy else {}
    if pool:
        details["proxyPoolId"] = pool
    initial = {"accessToken": OAUTH,"refreshToken":REFRESH,"expiresAt":"2099-12-31T00:00:00Z"}
    if provider == "clinepass":
        initial = {"apiKey":KEY}
    FIXTURE["save"](db,provider,details,**(initial | updates))


def raw(port):
    values = []
    for provider in MODELS:
        status, mime, response = send(port,provider)
        assert (status, mime) == (200,"text/event-stream"), (status, response, ERRORS)
        assert json.loads(response)["success"] is True
        values.append(response)
        status, mime, response = send(port,provider,stream=True)
        assert (status, mime) == (200,"text/event-stream") and response.endswith(b"\r\n\r\ndata: [DONE]\n\n")
        values.append(response)
    return values


@contextmanager
def pinned(scratch):
    home = scratch / "pinned"
    home.mkdir()
    data = home / "data"
    subprocess.run([sys.executable,str(ROOT / "rust/reference/seed.py"),str(data)], check=True,
                   env={"PATH":os.environ["PATH"],"HOME":str(home)}, stderr=subprocess.DEVNULL)
    db = data / "db/data.sqlite"
    for provider in MODELS:
        save(db,provider)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    rewrite = home / "vendor-map.mjs"
    rewrite.write_text('''
import { createRequire } from "node:module";
const remap = request => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (url.origin !== "https://api.cline.bot") return request(source, options);
  const target = "http://127.0.0.1:52989/cline" + url.pathname + url.search;
  return request(source instanceof Request ? new Request(target, source) : target, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(REFERENCE_SERVER)("undici");
undici.fetch = remap(undici.fetch);
'''.replace("REFERENCE_SERVER", json.dumps(str(ROOT / ".next/standalone/server.js"))))
    env = {"DATA_DIR":str(data), "PORT":str(port), "HOSTNAME":"127.0.0.1", "HOME":str(home),
           "PATH":os.environ["PATH"], "ENABLE_TRANSLATOR":"true", "MODEL_CATALOG_SYNC":"off"}
    with subprocess.Popen(["node","--import",str(rewrite),".next/standalone/custom-server.js"],cwd=ROOT,env=env,
                          stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) as process:
        try:
            deadline = time.monotonic()+10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if FIXTURE["get"](port,"/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("Pinned Cline Translator did not start")
            yield port, db
        finally:
            process.terminate()
            process.wait(timeout=7)


def selected(db, provider):
    with sqlite3.connect(db) as sql:
        value, = sql.execute("SELECT data FROM providerConnections WHERE id=?",(provider,)).fetchone()
    return json.loads(value)


def protected(port, db, proxy, address, wrong_host):
    started = len(CALLS)
    assert send(port,"cline",headers={})[0] == 401
    assert send(port,"cline",payload={"input":"wrong format"})[0] == 400
    assert len(CALLS) == started
    Vendor.grant_status = 403
    status,_,response = send(port,"cline","echo error")
    assert status == 403 and json.loads(json.loads(response)["details"]) == {
        "error":{"message":"echoed Bearer [redacted] and [redacted]"},
    }
    Vendor.grant_status = 200
    status,_,response = send(port,"cline","echo stream",True)
    assert status == 200 and b"Bearer [redacted]" in response
    save(db,"cline",proxy=address)
    refreshed = len(REFRESHES)
    status,_,_ = send(port,"cline","renew")
    assert status == 200 and len(REFRESHES) == refreshed+1 and selected(db,"cline")["refreshToken"] == "fixture-rotated"
    assert CALLS[-2:] == [(False,"renew","Bearer " + token) for token in (OAUTH,FRESH)]
    save(db,"cline",proxy=address)
    status,_,response = send(port,"cline","renew stream",True)
    assert status == 200 and b"Bearer [redacted]" in response and FRESH.encode() not in response
    assert len(REFRESHES) == refreshed+2
    status,_,response = send(port,"clinepass","redirect")
    assert status == 502 and b"redirected the request" in response
    save(db,"cline",proxy=address,expiresAt="2000-12-31T00:00:00Z")
    assert send(port,"cline")[0] == 200 and len(REFRESHES) == refreshed+3
    assert CALLS[-1][0] is False and CALLS[-1][2] == "Bearer " + FRESH
    save(db,"cline",proxy=address)
    before = (len(CALLS),len(FIXTURE["TUNNELS"]))
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            SELECT 'direct-backup',provider,authType,name,2,1,json_remove(data,'$.providerSpecificData'),createdAt,updatedAt
            FROM providerConnections WHERE id='cline'""")
    proxy.reject = True
    assert send(port,"cline")[0] == 502
    assert (len(CALLS),len(FIXTURE["TUNNELS"])) == (before[0],before[1]+1)
    proxy.reject, proxy.target = False, wrong_host
    assert send(port,"cline")[0] == 502 and len(CALLS) == before[0]
    before_tunnels = len(FIXTURE["TUNNELS"])
    for pool in ("missing-pool","fixture-relay"):
        save(db,"cline",pool=pool)
        assert send(port,"cline")[0] == 502
    assert len(CALLS) == before[0] and len(FIXTURE["TUNNELS"]) == before_tunnels


with tempfile.TemporaryDirectory(prefix="9router-cline-translator-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    root_ca, upstream, wrong_host = FIXTURE["tls_services"](stack,scratch,Vendor)
    stack.enter_context(FIXTURE["serving"](ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    with pinned(scratch) as (port,db):
        reference = raw(port)
        renewal = send(port,"cline","renew")
        assert renewal[0] == 200 and selected(db,"cline")["accessToken"] == FRESH, (renewal, ERRORS)
    with FIXTURE["gateway"](scratch,"native-direct",None,[]) as (port,db):
        for provider in MODELS:
            save(db,provider)
        assert raw(port) == reference
        assert send(port,"cline","renew") == renewal and selected(db,"cline")["accessToken"] == FRESH
    assert len(REFRESHES) == 2
    for mode in ("connect","socks5h"):
        proxy = FIXTURE["Proxy"](("127.0.0.1",0),FIXTURE["Tunnel"])
        proxy.mode,scheme = ("connect","http") if mode == "connect" else ("socks","socks5h")
        proxy.reject,proxy.target,proxy.secrets = False,upstream,OLD_SECRETS
        stack.enter_context(FIXTURE["serving"](proxy))
        address = f"{scheme}://127.0.0.1:{proxy.server_address[1]}"
        before = len(CALLS)
        with FIXTURE["gateway"](scratch,mode,root_ca,[("fixture-relay","cloudflare",address)]) as (port,db):
            for provider in MODELS:
                save(db,provider,proxy=address)
            assert raw(port) == reference
            protected(port,db,proxy,address,wrong_host)
        assert all(not direct for direct,_,_ in CALLS[before:])
    assert all(host == HOST for host in FIXTURE["HELLOS"])
    assert not ERRORS and not FIXTURE["ERRORS"], (ERRORS,FIXTURE["ERRORS"])
print("Cline Translator Send preserved raw envelopes and SSE through direct, CONNECT and SOCKS5h, including renewal.")
