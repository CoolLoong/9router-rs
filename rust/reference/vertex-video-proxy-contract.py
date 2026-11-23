from base64 import urlsafe_b64encode
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
spec = importlib.util.spec_from_file_location("vertex_video_proxy_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "aiplatform.googleapis.com"
MODEL = "veo-3.1-generate-preview"
PROJECT = "fixture-video-bearer"
REGION = "europe-west1"
PRIMARY, SECONDARY = "offline-vertex-video-bearer", "offline-vertex-video-protected-secondary"
PREFIX = f"projects/{PROJECT}/locations/{REGION}/publishers/google/models/{MODEL}"
SUBMIT = f"/v1/{PREFIX}:predictLongRunning"
FETCH = f"/v1/{PREFIX}:fetchPredictOperation"
CREATE = "/v1/videos/generations"
API_KEY = {"Authorization": "Bearer offline-chat-key"}
BODY = {"model":"vertex/"+MODEL,"prompt":"Vertex video primary fixture","image":"data:image/png;base64,ZmFrZS1pbWFnZQ==",
        "video":{"gcsUri":"gs://example.invalid/input.mp4"},"n":"0x2","duration":"4","aspect_ratio":"16:9","resolution":"720p",
        "seed":"0009","negative_prompt":"no overlays","storage_uri":"gs://example.invalid/output","generate_audio":False}
CALLS, FAILURES = [], []
BEHAVIOR = "normal"
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.vertex-fixture.dev"
shared.SECRETS = (PRIMARY, SECONDARY, "offline-vertex-video-key-not-supported")


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

    def do_POST(self):
        try:
            direct = self.server.server_port == 52989
            if direct:
                assert self.path.startswith("/vertex/v1/")
                path = self.path.removeprefix("/vertex")
            else:
                assert self.headers["Host"] == HOST
                path = self.path
            assert path in (SUBMIT, FETCH) and "?" not in path
            auth = self.headers.get_all("Authorization")
            account = {"Bearer "+PRIMARY:"primary","Bearer "+SECONDARY:"secondary"}[auth[0]]
            assert len(auth) == 1 and self.headers["Content-Type"] == "application/json"
            assert not any(self.headers[field] for field in ("Cookie","Idempotency-Key","x-goog-api-key",
                                                            "Proxy-Authorization","x-relay-auth","x-relay-target"))
            body = self.rfile.read(int(self.headers["Content-Length"]))
            CALLS.append(("direct" if direct else "tunnel", path, account, json.loads(body)))
            if BEHAVIOR == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            if BEHAVIOR == "rejected":
                return self.reply(401, json.dumps({"error":"fixture rejected " + auth[0]}).encode())
            headers = {"Authorization":"Bearer "+PRIMARY,"Accept":self.headers["Accept"],
                       "Content-Type":self.headers["Content-Type"]}
            with closing(HTTPConnection("127.0.0.1",52988,timeout=3)) as upstream:
                upstream.request("POST","/vertex"+path,body,headers)
                response = upstream.getresponse()
                raw = response.read()
                assert response.status in (200,202,502), raw
                self.reply(response.status,raw,response.getheader("Content-Type","application/json"))
        except (AssertionError, KeyError, IndexError, TypeError, ValueError) as error:
            FAILURES.append((self.path,repr(error)))
            self.reply(422,b"{}")

    def do_GET(self):
        FAILURES.append(("unexpected upstream GET", self.path))
        self.reply(422,b"{}")


def save(db, account="primary", details=None, credentials=None, auth_type="oauth"):
    token = PRIMARY if account == "primary" else SECONDARY
    fields = {"accessToken":token} if credentials is None else credentials
    state = fields | {"projectId":PROJECT,
                      "providerSpecificData":{"location":REGION,"projectId":"wrong-lower-priority",**(details or {})}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'vertex',?,'Synthetic Vertex',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (account,auth_type,1 if account == "primary" else 2,json.dumps(state)))


def job(name):
    return urlsafe_b64encode(f"{PREFIX}/operations/{name}".encode()).decode().rstrip("=")


def request(port, path, payload=None, *, preferred=None, authorized=True):
    headers = dict(API_KEY) if authorized else {}
    if preferred:
        headers["x-connection-id"] = preferred
    if payload is not None:
        headers |= {"Content-Type":"application/json; charset=utf-8","Idempotency-Key":"never-forward","Cookie":"offline"}
    body = None if payload is None else json.dumps(payload).encode()
    with closing(HTTPConnection("127.0.0.1",port,timeout=6)) as client:
        client.request("GET" if body is None else "POST",path,body,headers)
        response = client.getresponse()
        raw = response.read()
        assert not any(secret.encode() in raw for secret in shared.SECRETS)
        return response.status,response.getheader("x-9router-connection-id"),json.loads(raw)


def cases(port, db, http, socks, connect):
    global BEHAVIOR
    save(db)
    save(db, "secondary")
    poll = "/v1/videos/" + job("job-empty")
    direct = request(port, CREATE, BODY)
    direct_poll = request(port,poll+"?provider=vertex")
    assert direct == (202,"primary",{"id":job("job-pending"),"request_id":job("job-pending"),"status":"pending"})
    assert direct_poll[:2] == (200,"primary") and direct_poll[2]["videos"] == []
    assert request(port,poll,preferred="primary",authorized=False)[0] == 401
    assert [row[:3] for row in CALLS] == [("direct",SUBMIT,"primary"),("direct",FETCH,"primary")]

    save(db, details=http | {"baseUrl":"http://127.0.0.1:52989/untrusted"})
    save(db,"secondary",{"proxyPoolId":"fixture-socks"})
    assert request(port,CREATE,BODY) == direct and request(port,poll,preferred="primary") == direct_poll
    selected = request(port,CREATE,BODY,preferred="secondary")
    assert selected[:2] == (202,"secondary")
    hinted = request(port,"/v1/videos/"+selected[2]["id"]+"?provider=openrouter",preferred=selected[1])
    assert hinted[:2] == (200,"secondary") and hinted[2]["status"] == "pending"
    assert [row[1:] for row in CALLS[2:4]] == [row[1:] for row in CALLS[:2]]
    assert CALLS[-1][3] == {"operationName":PREFIX+"/operations/job-pending"}
    assert shared.TUNNELS == [(protocol,HOST,443) for protocol in ("connect","connect","socks","socks")]
    assert shared.HELLOS == [HOST] * 4

    save(db, details=http | {"connectionNoProxy":".googleapis.com"})
    assert request(port,CREATE,BODY) == direct and request(port,poll,preferred="primary") == direct_poll
    assert [row[0] for row in CALLS[-2:]] == ["direct","direct"]
    shared.enable_global_proxy(db,socks)
    assert request(port,poll,preferred="primary") == direct_poll and shared.TUNNELS[-1] == ("socks",HOST,443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.googleapis.com') WHERE id='fixture-socks'")

    before = (len(CALLS),len(shared.TUNNELS))
    for config in ({"proxyPoolId":"fixture-socks"},{"proxyPoolId":"missing"},{"proxyPoolId":"relay"},
                   {"strictProxy":True},{"connectionNoProxy":"*","connectionProxyEnabled":True}):
        save(db,details=config)
        assert request(port,CREATE,BODY)[0] == 503
        assert request(port,poll,preferred="primary")[0] == 503
    service_key = subprocess.check_output(["openssl","genpkey","-algorithm","RSA","-pkeyopt","rsa_keygen_bits:2048"],
                                          stderr=subprocess.DEVNULL,text=True)
    service = json.dumps({"type":"service_account","project_id":"offline-vertex-service",
                          "client_email":"fake@example.invalid","private_key":service_key})
    save(db,details=http,credentials={"apiKey":service,"accessToken":PRIMARY},auth_type="apikey")
    for response in (request(port,CREATE,BODY),request(port,poll,preferred="primary")):
        assert response[0] == 400 and "stored OAuth" in response[2]["error"]["message"]
    save(db,details=http,credentials={"apiKey":"offline-vertex-video-key-not-supported"},auth_type="apikey")
    status, _, raw = request(port,CREATE,BODY)
    assert status == 400 and "raw API keys are not supported" in raw["error"]["message"]
    save(db,details=http)
    assert request(port,"/v1/videos/illegal-vertex-reference",preferred="primary")[0] == 400
    assert (len(CALLS),len(shared.TUNNELS)) == before

    save(db,"secondary")
    BEHAVIOR = "rejected"
    status, _, raw = request(port,CREATE,BODY)
    assert status == 401 and "[redacted]" in raw["error"]["message"] and len(CALLS) == before[0] + 1
    BEHAVIOR = "redirect"
    status, _, raw = request(port,poll,preferred="primary")
    assert status == 502 and "redirect refused" in raw["error"]["message"] and len(CALLS) == before[0] + 2
    BEHAVIOR = "normal"
    assert request(port,CREATE,BODY | {"prompt":"Vertex video ambiguous fixture"})[0] == 502
    assert len(CALLS) == before[0] + 3
    before = (len(CALLS),len(shared.TUNNELS))
    connect.reject = True
    assert request(port,CREATE,BODY)[0] == 502
    assert request(port,poll,preferred="primary")[0] == 502
    assert (len(CALLS),len(shared.TUNNELS)) == (before[0],before[1]+2)
    assert not FAILURES and not shared.ERRORS, (FAILURES,shared.ERRORS)
    print("Vertex video stored bearer create and translated poll kept fixed CONNECT/SOCKS authority and valid bypass, without mint or replay.")


with tempfile.TemporaryDirectory(prefix="9router-vertex-video-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect, socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((connect,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,target
        stack.enter_context(shared.serving(proxy))
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks","socks5",socks_url),("relay","cloudflare","https://fixture.offline-lane.workers.dev")]
    node = subprocess.Popen(["node","--input-type=module","-e","""
      import {createServer} from 'node:http';
      import {handleVertexVideoMock} from './rust/reference/mock-vertex-video.mjs';
      createServer(handleVertexVideoMock).listen(52988, '127.0.0.1', () => console.log('ready'));
    """],cwd=shared.ROOT,env={"PATH":os.environ["PATH"],"HOME":directory,"NODE_NO_WARNINGS":"1"},stdout=subprocess.PIPE,text=True)
    try:
        assert select.select([node.stdout],[],[],5)[0] and node.stdout.readline().strip()=="ready"
        with shared.gateway(scratch,"vertex-video",ca,pools) as (port,db):
            with sqlite3.connect(db) as sql:
                sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,'{}')")
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='relay'",
                            (json.dumps({"url":pools[1][2],"token":"V"*43}),))
            cases(port,db,http,socks_url,connect)
    finally:
        node.terminate()
        node.wait(timeout=4)
