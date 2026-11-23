from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("bfl_proxy_helpers",Path(__file__).with_name("zed-cline-models-proxy-contract.py"))
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
SUBMIT, POLL = "api.bfl.ai", "api.us1.bfl.ai"
KEY = "offline-bfl-proxy-key"
RESULT = "https://assets.bfl.ai/offline-image.png"
MODEL = "black-forest-labs/flux-pro-1.1"
ledger = []
errors = []
behavior = "normal"
shared.ZED_HOST, shared.CLINE_HOST = SUBMIT, POLL
shared.SECRETS = (KEY,)


class Vendor(BaseHTTPRequestHandler):
    def log_message(self,*_):
        pass

    def respond(self,status,body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def handle_api(self):
        try:
            mode,host,path = "tls",self.headers["Host"],self.path
            if path.startswith("/bfl/"):
                _, _, host, suffix = path.split("/",3)
                mode,path = "direct","/" + suffix
            assert self.headers["X-Key"] == KEY and self.headers.get("Authorization") is None
            assert not self.headers.get("Cookie") and not self.headers.get("X-Injected")
            assert KEY not in path
            ledger.append((mode,self.command,host,path))
            if self.command == "POST":
                assert host==SUBMIT and path=="/v1/flux-pro-1.1"
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert payload == {"prompt":"Fixture BFL image","width":1024,"height":768}
                if behavior == "reject-poll":
                    connect.reject = True
                if behavior == "edit-route":
                    configure(current_db,{"proxyPoolId":"missing-after-submission"})
                job = KEY if behavior == "credential-id" else "offline-job"
                self.respond(200,{"id":job,"polling_url":f"https://{POLL}/v1/get_result?id={job}"})
            else:
                assert self.command=="GET" and host==POLL and path=="/v1/get_result?id=offline-job"
                self.respond(200,{"status":"Ready","result":{"sample":RESULT}})
        except (AssertionError,TypeError,ValueError) as error:
            errors.append(str(error) or self.path)
            self.respond(422,{})

    do_GET = do_POST = handle_api


def configure(db,details,priority=1):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES (?,'black-forest-labs','apikey','Offline FLUX',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')
                       ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (f"bfl-proxy-{priority}",priority,json.dumps({"apiKey":KEY if priority==1 else "offline-bfl-never-selected",
                     "providerSpecificData":details})))


def call(port):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request("POST","/v1/images/generations",json.dumps({"model":MODEL,"prompt":"Fixture BFL image","size":"1024x768"}),
                       {"Content-Type":"application/json","Authorization":"Bearer offline-active-key","X-Injected":"test"})
        response = client.getresponse()
        code,data = response.status,json.loads(response.read())
        if code == 200:
            assert isinstance(data.pop("created"),int)
        return code,data


with tempfile.TemporaryDirectory(prefix="9router-bfl-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca,tls,_ = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect,socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((connect,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,tls
        stack.enter_context(shared.serving(proxy))
    http_proxy = f"http://127.0.0.1:{connect.server_address[1]}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    with shared.gateway(scratch,"native",ca,[("socks","http",socks_url)]) as (port,current_db):
        configure(current_db,{})
        configure(current_db,{},2)
        expected = (200,{"data":[{"url":RESULT}]})
        assert call(port)==expected
        chosen = {"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy}
        configure(current_db,chosen)
        behavior = "edit-route"
        assert call(port)==expected
        behavior = "normal"
        configure(current_db,{"proxyPoolId":"socks"})
        assert call(port)==expected
        assert shared.TUNNELS == [("connect",SUBMIT,443),("connect",POLL,443),("socks",SUBMIT,443),("socks",POLL,443)]
        assert shared.HELLOS == [SUBMIT,POLL,SUBMIT,POLL]
        configure(current_db,chosen|{"connectionNoProxy":SUBMIT})
        assert call(port)==expected and [row[0] for row in ledger[-2:]]==["direct","direct"]
        relay = "https://bfl-fixture.vercel.app/api/relay"
        with sqlite3.connect(current_db) as sql:
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                         VALUES ('managed',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        (json.dumps({"type":"vercel","proxyUrl":relay,"_managedVercel":{"url":relay,"token":"r"*43}}),))
        configure(current_db,{"proxyPoolId":"managed"})
        before = (len(ledger),len(shared.TUNNELS))
        code,problem = call(port)
        assert code==502 and "regional polling requires a tunnel" in problem["error"]["message"]
        assert (len(ledger),len(shared.TUNNELS))==before
        configure(current_db,chosen)
        behavior = "credential-id"
        before = (len(ledger),len(shared.TUNNELS))
        code,problem = call(port)
        assert code==502 and "contains a credential" in problem["error"]["message"] and KEY not in json.dumps(problem)
        assert (len(ledger),len(shared.TUNNELS))==(before[0]+1,before[1]+1)
        behavior = "reject-poll"
        before = (len(ledger),len(shared.TUNNELS))
        assert call(port)[0]==502
        assert (len(ledger),len(shared.TUNNELS))==(before[0]+1,before[1]+2)
        assert not errors and not shared.ERRORS,(errors,shared.ERRORS)
        print("FLUX submission and regional polling matched across direct, CONNECT, SOCKS5h and explicit bypass without resubmission.")
