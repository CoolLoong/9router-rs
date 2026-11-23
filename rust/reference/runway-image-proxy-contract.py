from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import quote


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("runway_route_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "api.dev.runwayml.com"
ORIGIN = "https://" + HOST
MODEL, VIDEO = "runway/gen4_image", "runway/gen4_turbo"
KEY, BACKUP = "fixture-runway-proxy-key", "fixture-runway-proxy-backup"
RELAY = "https://runway-fixture.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/runway-fixture.offline-lane.workers.dev"
RELAY_KEY = "R" * 43
TASK = "0d83f61c-249a-4fca-841a-87cbda251160"
ASSET = "https://runway-fixture-assets.com/media/result.mp4?signature=offline"
IMAGE = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII="
AUTH = {"Authorization":"Bearer offline-chat-key", "Content-Type":"application/json", "X-Gateway-Probe":"not forwarded"}
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.runway-fixture.dev"
shared.SECRETS = (KEY, BACKUP, RELAY_KEY)
CALLS, ERRORS = [], []
MODE = "normal"


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, data, location=None):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def exchange(self):
        try:
            path = self.path
            if path == RELAY_PATH:
                route = "relay"
                assert self.headers["x-relay-auth"] == RELAY_KEY and self.headers["x-relay-target"] == ORIGIN
                path = self.headers["x-relay-path"]
            elif self.server.server_port == 52989:
                route = "direct"
                assert path.startswith("/runway/v1/")
                path = path.removeprefix("/runway")
            else:
                route = "tunnel"
                assert self.headers["Host"] == HOST
            if route != "relay":
                assert all(self.headers[name] is None for name in ("x-relay-auth", "x-relay-target", "x-relay-path"))
            assert all(self.headers[name] is None for name in ("Proxy-Authorization", "Cookie", "X-Gateway-Probe"))
            assert self.headers.get_all("Authorization") == ["Bearer " + KEY]
            assert self.headers["x-runway-version"] == "2024-11-06" and self.headers["Accept"] == "application/json"
            assert all(secret not in path for secret in shared.SECRETS)
            data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            payload = json.loads(data) if data else None
            CALLS.append((route, self.command, path, payload))
            if self.command == "POST":
                if path == "/v1/text_to_image":
                    assert payload == {"model":"gen4_image", "promptText":"queue-image", "ratio":"1920:1080",
                                       "referenceImages":[{"uri":IMAGE}]}
                else:
                    assert path == "/v1/image_to_video"
                    assert payload == {"model":"gen4_turbo", "promptText":"queue-video", "ratio":"720:1280",
                                       "promptImage":IMAGE, "duration":5}
                if MODE == "change-route":
                    configure(DATABASE,{"proxyPoolId":"managed-relay"})
                    with sqlite3.connect(DATABASE) as sql:
                        sql.execute("UPDATE proxyPools SET isActive=0 WHERE id='fixture-socks'")
                if MODE == "fail-poll-tunnel":
                    CONNECT.reject = True
                if MODE == "submit-redirect":
                    return self.reply(307, {}, "https://cloud.zed.dev/capture")
                if MODE == "reject-submit":
                    return self.reply(429, {"error":{"message":f"refused {KEY} and {RELAY_KEY}"}})
                if MODE == "accepted-error":
                    return self.reply(422, {"id":TASK,"message":"Task accepted with validation error"})
                if MODE == "invalid-id":
                    return self.reply(200, {"id": "../../elsewhere"})
                return self.reply(200, {"id":TASK, "pollUrl":"https://cloud.zed.dev/capture"})
            assert self.command == "GET" and path == f"/v1/tasks/{TASK}" and payload is None
            if MODE == "poll-redirect":
                return self.reply(307, {}, "https://cloud.zed.dev/capture")
            if MODE == "poll-error":
                return self.reply(200, {"id":TASK, "status":"FAILED", "failure":f"echoed {quote(KEY)} and {RELAY_KEY}"})
            if MODE == "asset-secret":
                return self.reply(200, {"id":TASK,"status":"SUCCEEDED","output":[ASSET+"&secret="+RELAY_KEY]})
            if len(CALLS) >= 2 and CALLS[-2][1] == "POST":
                return self.reply(200, {"id":TASK,"status":"RUNNING"})
            self.reply(200,{"id":TASK,"status":"SUCCEEDED","output":[ASSET]})
        except (AssertionError, ValueError, KeyError, TypeError) as error:
            ERRORS.append((self.path,repr(error)))
            self.reply(422,{"error":"Fixture request mismatch"})

    do_POST = do_GET = exchange


def configure(db, details, backup=False):
    key, index = (BACKUP, 2) if backup else (KEY, 1)
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'runwayml','apikey','Offline Runway',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            (f"runway-{index}", index, json.dumps({"apiKey":key,"providerSpecificData":details})))


def generate(port, model=MODEL, **fields):
    video = model == VIDEO
    payload = {"model":model,"prompt":"queue-video" if video else "queue-image",
               "size":"1024x1792" if video else "1792x1024", "image":IMAGE} | fields
    with closing(HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request("POST","/v1/images/generations",json.dumps(payload),AUTH)
        response = client.getresponse()
        return response.status,json.loads(response.read())


def success(port, route, model=MODEL):
    before = len(CALLS)
    status,result = generate(port,model)
    assert status == 200 and result["data"] == [{"url":ASSET}], (status,result,ERRORS)
    assert [row[:2] for row in CALLS[before:]] == [(route,method) for method in ("POST","GET","GET")]


def cases(port, db, http, socks):
    global MODE
    configure(db,{})
    configure(db,{},backup=True)
    success(port,"direct")
    configure(db,http)
    success(port,"tunnel",VIDEO)
    assert shared.TUNNELS == [("connect",HOST,443)] * 3
    configure(db,{"proxyPoolId":"fixture-socks"})
    MODE = "change-route"
    success(port,"tunnel")
    assert shared.TUNNELS[-3:] == [("socks",HOST,443)] * 3
    MODE = "normal"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET isActive=1 WHERE id='fixture-socks'")
    configure(db,{"proxyPoolId":"managed-relay"})
    success(port,"relay")

    before = len(shared.TUNNELS)
    configure(db,http | {"connectionNoProxy":".runwayml.com"})
    success(port,"direct")
    configure(db,{})
    shared.enable_global_proxy(db,socks)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data=json_set(data,'$.outboundNoProxy','.runwayml.com') WHERE id=1")
    success(port,"direct")
    assert len(shared.TUNNELS) == before
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.runwayml.com') WHERE id='fixture-socks'")
        combos = {"protected-runway-combo":[MODEL,VIDEO], "protected-runway-nested":["protected-runway-combo"]}
        sql.executemany("""INSERT INTO combos(id,name,kind,models,createdAt,updatedAt)
           VALUES (?,?,'image',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            [(name,name,json.dumps(members)) for name,members in combos.items()])

    before = len(CALLS), len(shared.TUNNELS)
    configure(db,http)
    assert generate(port,"protected-runway-nested")[0] == 503
    assert generate(port,image=f"https://asset.example.net/{quote(KEY)}")[0] == 400
    for details in ({"proxyPoolId":"fixture-socks"},{"proxyPoolId":"missing"},{"proxyPoolId":"unmanaged-relay"},
                    {"strictProxy":True},{"vercelRelayUrl":RELAY}):
        configure(db,details)
        assert generate(port,"protected-runway-combo")[0] == 502
    assert (len(CALLS),len(shared.TUNNELS)) == before

    configure(db,http)
    MODE = "fail-poll-tunnel"
    start = len(CALLS)
    status,result = generate(port,"protected-runway-combo")
    CONNECT.reject = False
    assert status == 502 and "Runway polling request failed" in result["error"]["message"]
    assert [row[:2] for row in CALLS[start:]] == [("tunnel","POST")]

    for scenario, status, stages, reason in (
        ("reject-submit",429,["POST"],"[redacted]"),
        ("accepted-error",422,["POST"],"Task accepted"),
        ("submit-redirect",502,["POST"],"redirect refused"),
        ("invalid-id",502,["POST"],"invalid task ID"),
        ("poll-redirect",502,["POST","GET"],"redirect refused"),
        ("poll-error",502,["POST","GET"],"[redacted]"),
        ("asset-secret",502,["POST","GET"],"invalid media URL"),
    ):
        MODE = scenario
        configure(db,{"proxyPoolId":"managed-relay"})
        start = len(CALLS)
        code,result = generate(port,"protected-runway-combo")
        assert code == status and reason in result["error"]["message"], (code,result)
        assert all(secret not in json.dumps(result) for secret in shared.SECRETS)
        assert [row[:2] for row in CALLS[start:]] == [("relay",method) for method in stages]
    MODE = "normal"


with tempfile.TemporaryDirectory(prefix="9router-runway-proxy-") as folder, ExitStack() as stack:
    scratch = Path(folder)
    ca, target, wrong_host = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    CONNECT, socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((CONNECT,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,target
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{CONNECT.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks","socks5",socks_url),("managed-relay","cloudflare",RELAY),("unmanaged-relay","cloudflare",RELAY)]
    with shared.gateway(scratch,"runway",ca,pools) as (port,DATABASE):
        with sqlite3.connect(DATABASE) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed-relay'",
                        (json.dumps({"url":RELAY,"token":RELAY_KEY}),))
        cases(port,DATABASE,http,socks_url)
        before = len(CALLS)
        CONNECT.target = wrong_host
        configure(DATABASE,http)
        assert generate(port)[0] == 502 and len(CALLS) == before
    assert all(host == HOST for host in shared.HELLOS)
    assert not ERRORS and not shared.ERRORS,(ERRORS,shared.ERRORS)
print("Runway submit and task polling shared one CONNECT/SOCKS5h or managed Relay, with valid NO_PROXY and no protected resubmit.")
