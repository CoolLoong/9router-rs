from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("xai_video_proxy_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "api.x.ai"
OPENROUTER_HOST = "openrouter.ai"
BASE = "/v1/videos"
CREATE = BASE + "/generations"
POLL = BASE + "/fixture-job%2Fpart%3Fq"
PRIMARY, SECONDARY = "offline-xai-image", "offline-xai-video-secondary"
OR_PRIMARY, OR_SECONDARY = "offline-static-openrouter", "offline-openrouter-video-secondary"
OR_PATH, OR_FIXTURE_PATH = "/api/v1/videos", "/openrouter/api/v1/videos"
OR_POLL = BASE + "/fixture-openrouter-job"
RELAY_PATH = "/cloudflare-relay/fixture.offline-lane.workers.dev"
RELAY_TOKEN = "X" * 43
BEHAVIOR = "normal"
CALLS, FAILURES = [], []
openrouter = sys.argv[2:] == ["openrouter"]
shared.ZED_HOST, shared.CLINE_HOST = (OPENROUTER_HOST if openrouter else HOST), "unused.video-fixture.dev"
shared.SECRETS = (OR_PRIMARY, OR_SECONDARY, RELAY_TOKEN) if openrouter else (PRIMARY, SECONDARY)
MULTIPART = (b'--video-fixture\r\nContent-Disposition: form-data; name="image"; filename="fake.bin"\r\n\r\n'
             b"\x00\x7f\x80\xff\r\n--video-fixture--\r\n")


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw=b"", mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def exchange(self):
        try:
            if openrouter:
                return self.openrouter_exchange()
            direct = self.server.server_port == 52989
            if direct:
                assert self.path.startswith("/xai" + BASE + "/")
                path = self.path.removeprefix("/xai")
            else:
                assert self.headers["Host"] == HOST
                assert self.path.startswith(BASE + "/")
                path = self.path
            assert "?" not in path
            assert not any(self.headers[name] for name in ("Cookie","Proxy-Authorization","x-relay-auth","x-relay-target"))
            account = {"Bearer "+PRIMARY: "primary", "Bearer "+SECONDARY: "secondary"}[self.headers["Authorization"]]
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if self.command == "GET":
                assert not raw and not self.headers["Idempotency-Key"] and not self.headers["Content-Type"]
            CALLS.append(("direct" if direct else "tunnel", self.command, path, account, raw, self.headers["Idempotency-Key"]))
            if BEHAVIOR == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            headers = {"Authorization":self.headers["Authorization"], "Accept":self.headers["Accept"]}
            for name in ("Content-Type", "Idempotency-Key"):
                if self.headers[name] is not None:
                    headers[name] = self.headers[name]
            with closing(HTTPConnection("127.0.0.1", 52988, timeout=3)) as vendor:
                vendor.request(self.command, "/xai"+path, raw or None, headers)
                response = vendor.getresponse()
                result = response.read()
                assert response.status != 422 or b"sent" in result, result
                self.reply(response.status, result, response.getheader("Content-Type", "application/json"))
        except (AssertionError, KeyError, ValueError, TypeError) as error:
            FAILURES.append((self.path, repr(error)))
            self.reply(422, b"{}")

    def openrouter_exchange(self):
        if self.path == RELAY_PATH:
            lane = "relay"
            assert self.headers["x-relay-auth"] == RELAY_TOKEN
            assert self.headers["x-relay-target"] == "https://openrouter.ai"
            path = self.headers["x-relay-path"]
        elif self.server.server_port == 52989:
            lane = "direct"
            assert self.path.startswith(OR_FIXTURE_PATH)
            path = self.path.removeprefix("/openrouter")
        else:
            lane = "tunnel"
            path = self.path
            assert self.headers["Host"] == OPENROUTER_HOST
        assert "?" not in path and all(secret not in path for secret in shared.SECRETS)
        assert not any(self.headers[field] for field in ("Cookie", "Proxy-Authorization", "Idempotency-Key"))
        assert self.headers["HTTP-Referer"] == "https://endpoint-proxy.local" and self.headers["X-Title"] == "Endpoint Proxy"
        account = {"Bearer "+OR_PRIMARY:"primary", "Bearer "+OR_SECONDARY:"secondary"}[self.headers["Authorization"]]
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.command == "POST":
            assert path == OR_PATH and self.headers["Content-Type"] == "application/json"
            assert json.loads(raw) == {"model":"google/veo-3.1", "prompt":"openrouter fixture"}
        else:
            assert not raw and not self.headers["Content-Type"]
        CALLS.append((lane, self.command, path, account, raw, None))
        if BEHAVIOR == "redirect":
            return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
        if BEHAVIOR == "rejected":
            key = OR_PRIMARY if account == "primary" else OR_SECONDARY
            return self.reply(429, json.dumps({"error":f"fixture rejection {key} {RELAY_TOKEN}"}).encode())
        headers = {name: self.headers[name] for name in ("Accept", "HTTP-Referer", "X-Title", "Content-Type")
                   if self.headers[name] is not None}
        headers["Authorization"] = "Bearer " + OR_PRIMARY
        with closing(HTTPConnection("127.0.0.1", 52988, timeout=3)) as vendor:
            vendor.request(self.command, "/openrouter"+path, raw or None, headers)
            response = vendor.getresponse()
            result = response.read()
            assert response.status in (200, 201, 404), result
            self.reply(response.status, result, response.getheader("Content-Type", "application/json"))

    do_GET = do_POST = exchange


def save(db, account="primary", details=None):
    key = ((OR_PRIMARY, OR_SECONDARY) if openrouter else (PRIMARY, SECONDARY))[account == "secondary"]
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Synthetic video account',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (account, "openrouter" if openrouter else "xai", 1 if account == "primary" else 2,
                     json.dumps({"apiKey":key,"providerSpecificData":details or {}})))


def request(port, path, payload=None, preferred=None, *, auth=True, mime="application/json", once=None):
    headers = {"Authorization":"Bearer offline-chat-key"} if auth else {}
    if preferred:
        headers["x-connection-id"] = preferred
    if once:
        headers["idempotency-key"] = once
    if payload is not None:
        headers["Content-Type"] = mime
    body = json.dumps(payload).encode() if isinstance(payload, dict) else payload
    with closing(HTTPConnection("127.0.0.1", port, timeout=6)) as app:
        app.request("GET" if payload is None else "POST", path, body, headers)
        response = app.getresponse()
        raw = response.read()
        assert not any(secret.encode() in raw for secret in shared.SECRETS)
        return response.status, response.getheader("x-9router-connection-id"), response.getheader("Content-Type"), raw


def cases(port, db, http, socks, connect):
    global BEHAVIOR
    save(db)
    save(db, "secondary")
    payload = b'{"model":"grok-imagine-video","prompt":"text fixture","seconds":5}'
    direct = request(port, CREATE, payload, once="fixture-video-once")
    assert direct[:3] == (202, "primary", "application/vnd.fixture+json")
    assert CALLS == [("direct","POST",CREATE,"primary",payload,"fixture-video-once")]
    assert not shared.TUNNELS
    assert request(port, POLL, auth=False)[0] == 401

    save(db, details=http)
    save(db, "secondary", {"proxyPoolId":"fixture-socks"})
    assert request(port, CREATE, payload, once="fixture-video-once") == direct
    submitted = request(port, CREATE, {"model":"xai/fixture-video-affinity","prompt":"affinity fixture"}, preferred="secondary")
    assert submitted[:2] == (202, "secondary")
    pinned = request(port, POLL+"?provider=openrouter", preferred=submitted[1])
    assert pinned[:2] == (200, "secondary") and json.loads(pinned[-1])["account"] == "secondary"
    unpinned = request(port, POLL)
    assert unpinned[:2] == (200, "primary") and json.loads(unpinned[-1])["account"] == "primary"
    multipart = request(port, CREATE, MULTIPART, mime="multipart/form-data; boundary=video-fixture")
    assert multipart[:2] == (202, "primary") and CALLS[-1][4] == MULTIPART
    assert CALLS[2][4] == b'{"model":"fixture-video-affinity","prompt":"affinity fixture"}'
    assert shared.TUNNELS == [(protocol, HOST, 443) for protocol in ["connect","socks","socks","connect","connect"]]
    assert shared.HELLOS == [HOST] * 5 and all(lane == "tunnel" for lane, *_ in CALLS[1:])

    save(db, details=http | {"connectionNoProxy":".x.ai"})
    assert request(port, CREATE, payload, once="fixture-video-once") == direct and CALLS[-1][0] == "direct"
    assert request(port, POLL, preferred="primary")[:2] == (200, "primary") and CALLS[-1][0] == "direct"
    shared.enable_global_proxy(db, socks)
    assert request(port, CREATE, payload, once="fixture-video-once") == direct and shared.TUNNELS[-1] == ("socks", HOST, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.x.ai') WHERE id='fixture-socks'")

    before = (len(CALLS), len(shared.TUNNELS))
    for flags in ({"proxyPoolId":"fixture-socks"}, {"proxyPoolId":"missing"}, {"proxyPoolId":"relay"},
                  {"strictProxy":True}, {"vercelRelayUrl":"http://127.0.0.1:52989/unsafe"},
                  {"connectionProxyEnabled":True,"connectionNoProxy":"*"}):
        save(db, details=flags)
        assert request(port, CREATE, payload, once="fixture-video-once")[0] == 503
        assert request(port, POLL, preferred="primary")[0] == 503
    assert (len(CALLS), len(shared.TUNNELS)) == before

    save(db, details=http)
    save(db, "secondary")
    before = len(CALLS)
    failed = request(port, CREATE, {"model":"xai/fixture-video-rotate","prompt":"rotate fixture"})
    assert failed[:2] == (429, None) and len(CALLS) == before + 1
    assert CALLS[-1][:4] == ("tunnel","POST",CREATE,"primary")
    failed = request(port, CREATE, {"model":"xai/fixture-video-unavailable","prompt":"unavailable fixture"})
    assert failed[:2] == (502, None) and len(CALLS) == before + 2
    BEHAVIOR = "redirect"
    assert b"redirect refused" in request(port, POLL, preferred="primary")[-1]
    assert len(CALLS) == before + 3 and CALLS[-1][:4] == ("tunnel","GET",POLL,"primary")
    BEHAVIOR = "normal"

    before = (len(CALLS), len(shared.TUNNELS))
    connect.reject = True
    assert request(port, CREATE, payload, once="fixture-video-once")[0] == 502
    assert request(port, POLL, preferred="primary")[0] == 502
    assert len(CALLS) == before[0] and len(shared.TUNNELS) == before[1] + 2
    connect.reject = False
    assert request(port, POLL, preferred="primary")[:2] == (200, "primary")
    assert len(CALLS) == before[0] + 1 and CALLS[-1][:4] == ("tunnel","GET",POLL,"primary")
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("xAI video scoped create and poll kept canonical TLS, per-call account hints, binary/idempotency wire, and no proxy-failure replay.")


def openrouter_cases(port, db, http, socks, connect):
    global BEHAVIOR
    save(db)
    save(db, "secondary")
    payload = {"model":"openrouter/google/veo-3.1", "prompt":"openrouter fixture"}
    baseline = request(port, CREATE, payload, once="never-forward")
    assert baseline[:2] == (201, "primary") and CALLS[-1][:4] == ("direct", "POST", OR_PATH, "primary")
    baseline_poll = request(port, OR_POLL + "?provider=openrouter")
    assert baseline_poll[:2] == (200, "primary") and CALLS[-1][:4] == ("direct", "GET", OR_PATH + "/fixture-openrouter-job", "primary")
    assert request(port, OR_POLL, preferred="primary", auth=False)[0] == 401
    assert not shared.TUNNELS

    save(db, details=http)
    save(db, "secondary", {"proxyPoolId":"fixture-socks"})
    assert request(port, CREATE, payload, once="never-forward") == baseline
    assert request(port, OR_POLL + "?provider=openrouter") == baseline_poll
    secondary = request(port, CREATE, payload, preferred="secondary")
    assert secondary[:2] == (201, "secondary") and CALLS[-1][:4] == ("tunnel", "POST", OR_PATH, "secondary")
    assert request(port, OR_POLL + "?provider=xai", preferred=secondary[1])[:2] == (200, "secondary")
    assert CALLS[-1][:4] == ("tunnel", "GET", OR_PATH + "/fixture-openrouter-job", "secondary")
    assert shared.TUNNELS == [(kind, OPENROUTER_HOST, 443) for kind in ("connect","connect","socks","socks")]
    assert shared.HELLOS == [OPENROUTER_HOST] * 4

    save(db, details=http | {"connectionNoProxy":"openrouter.ai"})
    assert request(port, CREATE, payload) == baseline and CALLS[-1][0] == "direct"
    assert request(port, OR_POLL, preferred="primary") == baseline_poll and CALLS[-1][0] == "direct"
    shared.enable_global_proxy(db, socks)
    assert request(port, CREATE, payload) == baseline and shared.TUNNELS[-1] == ("socks", OPENROUTER_HOST, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','openrouter.ai') WHERE id='fixture-socks'")

    before = (len(CALLS), len(shared.TUNNELS))
    for flags in ({"proxyPoolId":"fixture-socks"}, {"proxyPoolId":"missing"}, {"strictProxy":True},
                  {"proxyPoolId":"unmanaged-relay"}, {"vercelRelayUrl":"http://127.0.0.1:52989/unsafe"}):
        save(db, details=flags)
        assert request(port, CREATE, payload)[0] == 503
        assert request(port, OR_POLL, preferred="primary")[0] == 503
    assert before == (len(CALLS), len(shared.TUNNELS))

    save(db, details={"proxyPoolId":"relay"})
    assert request(port, CREATE, payload) == baseline and CALLS[-1][0] == "relay"
    assert request(port, OR_POLL, preferred="primary") == baseline_poll and CALLS[-1][0] == "relay"
    assert request(port, BASE+"/another%2Fid%3Fvalue", preferred="primary")[0] == 404
    assert CALLS[-1][2] == OR_PATH + "/another%2Fid%3Fvalue"
    before = len(CALLS)
    assert request(port, BASE+"/"+RELAY_TOKEN, preferred="primary")[0] == 400
    assert len(CALLS) == before
    BEHAVIOR = "redirect"
    assert request(port, OR_POLL, preferred="primary")[0] == 502 and len(CALLS) == before + 1
    BEHAVIOR = "rejected"
    save(db, "secondary")
    status, _, _, raw = request(port, CREATE, payload)
    assert status == 429 and b"[redacted]" in raw and len(CALLS) == before + 2
    BEHAVIOR = "normal"
    save(db, details=http)
    connect.reject = True
    before = (len(CALLS),len(shared.TUNNELS))
    assert request(port, CREATE, payload)[0] == 502
    assert request(port, OR_POLL, preferred="primary")[0] == 502
    assert (len(CALLS),len(shared.TUNNELS)) == (before[0],before[1] + 2)
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("OpenRouter video uses the fixed create/poll origin via CONNECT/SOCKS, explicit bypass and managed Relay, without replay.")


with tempfile.TemporaryDirectory(prefix="9router-xai-video-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared.tls_services(stack, scratch, Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect, socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect,"connect"), (socks,"socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks","socks5",socks_url), ("relay","cloudflare","https://fixture.offline-lane.workers.dev"),
             ("unmanaged-relay","cloudflare","https://unmanaged.offline-lane.workers.dev")]
    node = subprocess.Popen(["node","--input-type=module","-e","""
      import {createServer} from 'node:http';
      import {handleVideoMock} from './rust/reference/mock-videos.mjs';
      createServer(handleVideoMock).listen(52988, '127.0.0.1', () => console.log('ready'));
    """], cwd=shared.ROOT, env={"PATH":os.environ["PATH"],"HOME":directory,"NODE_NO_WARNINGS":"1"},
                           stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([node.stdout], [], [], 5)[0] and node.stdout.readline().strip() == "ready"
        with shared.gateway(scratch,"xai",ca,pools) as (port,db):
            with sqlite3.connect(db) as sql:
                sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,'{}')")
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                            (json.dumps({"url":pools[1][2], "token":RELAY_TOKEN}),))
            (openrouter_cases if openrouter else cases)(port, db, http, socks_url, connect)
    finally:
        node.terminate()
        node.wait(timeout=4)
