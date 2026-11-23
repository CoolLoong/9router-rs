from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import socket
import sqlite3
import sys
import tempfile


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("assemblyai_route_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "api.assemblyai.com"
ORIGIN = "https://" + HOST
KEY, BACKUP = "fixture-assembly-primary", "fixture-assembly-backup"
RELAY = "https://assemblyai-fixture.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/assemblyai-fixture.offline-lane.workers.dev"
RELAY_KEY = "A" * 43
UPLOAD, TRANSCRIPT = "/v2/upload", "/v2/transcript"
TASK = "0d83f61c-249a-4fca-841a-87cbda251160"
POLL = TRANSCRIPT + "/" + TASK
CDN = "https://cdn.assemblyai.com/upload/f756988d-47e2-4ca3-96ce-04bb168f8f2a"
CUSTOM = "http://127.0.0.1:52989/custom/v2/transcript"
AUDIO = b"RIFF\x00\xff\x80\r\nopaque synthetic speech bytes\x00"
OUT = {"text":"AssemblyAI 本地 fixture transcript"}
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.assemblyai-fixture.dev"
shared.SECRETS = (KEY, BACKUP, RELAY_KEY)
REQUESTS, ERRORS = [], []
MODE = "normal"
REPLACED_UPLOAD = ""


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
                lane = "relay"
                assert self.headers["x-relay-auth"] == RELAY_KEY and self.headers["x-relay-target"] == ORIGIN
                path = self.headers["x-relay-path"]
            elif self.server.server_port == 52989:
                lane = "local"
                assert path.startswith(("/assemblyai/v2/", "/custom/v2/"))
                path = path.removeprefix("/assemblyai")
            else:
                lane = "tunnel"
                assert self.headers["Host"] == HOST
            if lane != "relay":
                assert all(self.headers[name] is None for name in ("x-relay-auth", "x-relay-target", "x-relay-path"))
            assert all(self.headers[name] is None for name in ("Proxy-Authorization", "Cookie", "X-Internal-Probe"))
            assert all(secret not in path for secret in shared.SECRETS)
            authorization, = self.headers.get_all("Authorization")
            token = authorization.removeprefix("Bearer ")
            assert token in (KEY, BACKUP)
            route = "direct" if authorization.startswith("Bearer ") else ("bypass" if lane == "local" else lane)
            assert route == "direct" and lane == "local" or authorization == KEY
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            REQUESTS.append((route, self.command, path, token))
            if path == UPLOAD:
                assert self.command == "POST" and raw == AUDIO
                assert self.headers["Content-Type"] == "application/octet-stream"
                if MODE == "change-route":
                    configure(DATABASE,{"proxyPoolId":"managed-relay"})
                    with sqlite3.connect(DATABASE) as sql:
                        sql.execute("UPDATE proxyPools SET isActive=0 WHERE id='fixture-socks'")
                return self.reply(200,{"upload_url":REPLACED_UPLOAD if MODE == "invalid-upload" else CDN})
            if path in (TRANSCRIPT, "/custom" + TRANSCRIPT):
                assert self.command == "POST" and json.loads(raw) == {
                    "audio_url":CDN,"speech_models":["universal-3-pro"],"language_detection":True,
                }
                if MODE == "drop-submit":
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                if MODE == "reject-submit" and token == KEY:
                    return self.reply(401,{"error":"rejected "+KEY})
                if MODE == "accepted-error":
                    return self.reply(422,{"id":TASK,"error":"accepted "+KEY})
                if MODE == "submit-redirect":
                    return self.reply(307,{},"https://cloud.zed.dev/capture")
                if MODE == "invalid-id":
                    return self.reply(200,{"id":"%2e%2e/elsewhere"})
                if MODE == "drop-poll-tunnel":
                    CONNECT.reject = True
                return self.reply(200,{"id":TASK,"status":"queued","pollUrl":"https://cloud.zed.dev/capture"})
            assert self.command == "GET" and path in (POLL, "/custom" + POLL) and not raw
            if MODE == "different-id":
                return self.reply(200,{"id":"different","status":"completed",**OUT})
            if MODE == "poll-error":
                return self.reply(200,{"id":TASK,"status":"error","error":f"echoed {KEY} {RELAY_KEY}"})
            if MODE == "poll-redirect":
                return self.reply(307,{},"https://cloud.zed.dev/capture")
            if MODE == "poll-wait":
                return self.reply(200,{"id":TASK,"status":"processing"})
            self.reply(200,{"id":TASK,"status":"completed",**OUT})
        except (AssertionError, ValueError, KeyError, TypeError) as error:
            ERRORS.append((self.path,repr(error)))
            self.reply(422,{"error":"Fixture request mismatch"})

    do_POST = do_GET = exchange


def configure(db, details, backup=False):
    index, key = (2,BACKUP) if backup else (1,KEY)
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'assemblyai','apikey','Offline AssemblyAI',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            (f"assembly-{index}",index,json.dumps({"apiKey":key,"providerSpecificData":details})))


def transcribe(port):
    boundary = b"assemblyai-fixture"
    payload = (b"--"+boundary+b"\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nassemblyai/universal-3-pro\r\n"
               b"--"+boundary+b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.wav\"\r\n"
               b"Content-Type: audio/wav\r\n\r\n"+AUDIO+b"\r\n--"+boundary+b"--\r\n")
    headers = {"Authorization":"Bearer offline-chat-key","X-Internal-Probe":"ignored","Cookie":"probe=ignored",
               "Content-Type":"multipart/form-data; boundary="+boundary.decode()}
    with closing(HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request("POST","/v1/audio/transcriptions",payload,headers)
        response=client.getresponse()
        return response.status,json.loads(response.read())


def success(port, route, custom=False):
    start=len(REQUESTS)
    assert transcribe(port) == (200,OUT), ERRORS
    path = "/custom" + TRANSCRIPT if custom else TRANSCRIPT
    assert REQUESTS[start:] == [(route,"POST",UPLOAD,KEY),(route,"POST",path,KEY),(route,"GET",path+"/"+TASK,KEY)]


def cases(port, db, http, socks):
    global MODE, REPLACED_UPLOAD
    configure(db,{})
    configure(db,{},backup=True)
    success(port,"direct")
    configure(db,{"baseUrl":CUSTOM})
    success(port,"direct",custom=True)
    start=len(REQUESTS)
    configure(db,{})
    MODE = "reject-submit"
    assert transcribe(port) == (200,OUT)
    assert [(path,token) for _,_,path,token in REQUESTS[start:]] == [
        (UPLOAD,KEY),(TRANSCRIPT,KEY),(UPLOAD,BACKUP),(TRANSCRIPT,BACKUP),(POLL,BACKUP),
    ]
    for scenario, code, methods in (("drop-submit",502,["POST","POST"]),
                                    ("accepted-error",422,["POST","POST"]),
                                    ("poll-error",500,["POST","POST","GET"])):
        MODE = scenario
        configure(db,{})
        start=len(REQUESTS)
        status,result=transcribe(port)
        assert status == code and KEY not in json.dumps(result), (status,result)
        assert [(lane,method,token) for lane,method,_,token in REQUESTS[start:]] == [
            ("direct",method,KEY) for method in methods
        ]

    configure(db,http)
    MODE = "normal"
    success(port,"tunnel")
    assert shared.TUNNELS == [("connect",HOST,443)]*3
    configure(db,{"proxyPoolId":"fixture-socks"})
    MODE = "change-route"
    success(port,"tunnel")
    assert shared.TUNNELS[-3:] == [("socks",HOST,443)]*3
    MODE = "normal"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET isActive=1 WHERE id='fixture-socks'")
    configure(db,{"proxyPoolId":"managed-relay"})
    success(port,"relay")
    start=len(shared.TUNNELS)
    configure(db,http | {"connectionNoProxy":".assemblyai.com"})
    success(port,"bypass")
    configure(db,{})
    shared.enable_global_proxy(db,socks)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data=json_set(data,'$.outboundNoProxy','.assemblyai.com') WHERE id=1")
    success(port,"bypass")
    assert len(shared.TUNNELS) == start
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.assemblyai.com') WHERE id='fixture-socks'")
    start=len(REQUESTS),len(shared.TUNNELS)
    for details in ({"proxyPoolId":"fixture-socks"},{"proxyPoolId":"missing"},{"strictProxy":True},
                    {"proxyPoolId":"unmanaged-relay"},{"vercelRelayUrl":RELAY},{**http,"baseUrl":CUSTOM}):
        configure(db,details)
        assert transcribe(port)[0] == 503
    assert (len(REQUESTS),len(shared.TUNNELS)) == start

    configure(db,http)
    MODE = "invalid-upload"
    for url in ("https://cloud.zed.dev/capture", CDN+"?next=1", CDN+"/nested", CDN.replace("https","http"),
                CDN.replace("cdn.","key@cdn."), "https://cdn.assemblyai.com/upload/"+KEY):
        REPLACED_UPLOAD = url
        start=len(REQUESTS)
        status,result=transcribe(port)
        assert status == 502 and "Invalid AssemblyAI upload URL" in result["error"]["message"]
        assert REQUESTS[start:] == [("tunnel","POST",UPLOAD,KEY)]
    MODE = "drop-poll-tunnel"
    start=len(REQUESTS)
    status,result=transcribe(port)
    CONNECT.reject = False
    assert status == 502 and len(REQUESTS) == start+2
    assert REQUESTS[start:] == [("tunnel","POST",UPLOAD,KEY),("tunnel","POST",TRANSCRIPT,KEY)]

    for scenario, code, methods in (("reject-submit",401,["POST","POST"]),
                                    ("drop-submit",502,["POST","POST"]),
                                    ("accepted-error",422,["POST","POST"]),
                                    ("submit-redirect",502,["POST","POST"]),
                                    ("invalid-id",502,["POST","POST"]),
                                    ("different-id",502,["POST","POST","GET"]),
                                    ("poll-error",500,["POST","POST","GET"]),
                                    ("poll-redirect",502,["POST","POST","GET"])):
        MODE = scenario
        configure(db,{"proxyPoolId":"managed-relay"})
        start=len(REQUESTS)
        status,result=transcribe(port)
        assert status == code and all(secret not in json.dumps(result) for secret in shared.SECRETS),(status,result)
        assert [(route,method) for route,method,_,_ in REQUESTS[start:]] == [("relay",method) for method in methods]
    MODE = "poll-wait"
    start=len(REQUESTS)
    assert transcribe(port)[0] == 504
    assert [phase[2] for phase in REQUESTS[start:]].count(TRANSCRIPT) == 1
    MODE = "normal"


with tempfile.TemporaryDirectory(prefix="9router-assemblyai-proxy-") as folder, ExitStack() as stack:
    scratch = Path(folder)
    ca,target,wrong_host = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    CONNECT,socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((CONNECT,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,target
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{CONNECT.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks","socks5",socks_url),("managed-relay","cloudflare",RELAY),("unmanaged-relay","cloudflare",RELAY)]
    with shared.gateway(scratch,"assemblyai",ca,pools) as (port,DATABASE):
        with sqlite3.connect(DATABASE) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed-relay'",
                        (json.dumps({"url":RELAY,"token":RELAY_KEY}),))
        cases(port,DATABASE,http,socks_url)
        configure(DATABASE,http)
        start=len(REQUESTS)
        CONNECT.target = wrong_host
        assert transcribe(port)[0] == 502 and len(REQUESTS) == start
    assert all(name == HOST for name in shared.HELLOS)
    assert not ERRORS and not shared.ERRORS,(ERRORS,shared.ERRORS)
print("AssemblyAI upload, submit and poll bound to one exact route; raw auth, CDN validation and terminal submit verified.")
