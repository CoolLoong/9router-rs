from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("cline_probe_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
ROOT = shared.ROOT
MODE = sys.argv[-1]
HOST = "api.cline.bot"
RELAY = "https://cline-probe-fixture.offline-lane.workers.dev"
RELAY_KEY = "P" * 43
OAUTH = "workos:eyJfixture.probe.original"
RENEWED = "workos:eyJfixture.probe.renewed"
REFRESH = "fixture-cline-probe-refresh"
PASS = "clp_fixture_probes"
TEST = "/api/models/test"
shared.ZED_HOST,shared.CLINE_HOST=HOST,"unused.probe-fixture.com"
shared.SECRETS=(OAUTH,RENEWED,REFRESH,PASS,RELAY_KEY)
CATALOG = json.loads((ROOT/"rust/app/src/openai-catalog-data.json").read_text())["models"]
AUTH, ATTEMPTS, ERRORS = [], [], []
BATCH = None
REPEAT_ERRORS = False


def choice(content="ok", reason="stop", **message):
    return {"choices":[{"message":{"role":"assistant","content":content,**message},"finish_reason":reason}]}


SEMANTIC = {
    "fixture-completion": (200, {"success":True,"data":choice()}, True, None),
    "fixture-status": (200, {"success":True,"data":{"status":731,"msg":"semantic status",**choice()}},
                       False, "Provider status 731: semantic status"),
    "fixture-zero": (200, {"success":True,"data":{"status":0,"msg":"fine",**choice()}}, True,None),
    "fixture-embedded-error": (200, {"success":False,"error":{"message":"embedded rejection"}},
                               False, "embedded rejection"),
    "fixture-empty": (200, {"success":True,"data":{"choices":[]}},False,
                      "Provider returned no completion choices for this model"),
    "fixture-reasoning": (200, {"success":True,"data":choice("\ufeff ","length",thinking_content="reasoned")},True,None),
    "fixture-nonempty": (200, {"success":True,"data":choice([{"type":"text","text":"ok"}],"length",reasoning="r")},True,None),
    "fixture-truncate": (200, {"status":"oops","message":"转录"*180},False,"Provider status oops: "+"转录"*120),
    "fixture-http-error": (400, {"error":{"message":"probe rejected"}},False,
                           'HTTP 400: [400]: {"error":{"message":"probe rejected"}}'),
}


class Vendor(shared.Vendor):
    def do_POST(self):
        direct = self.server.server_port == 52989
        path = self.path.removeprefix("/cline") if direct else self.path
        try:
            if not direct:
                assert self.headers["Host"] == HOST
            assert all(self.headers[name] is None for name in ("x-9r-cli-token","Proxy-Authorization","X-Relay-Auth","Cookie"))
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            payload = json.loads(raw)
            auth = self.headers["Authorization"]
            if path == "/api/v1/auth/refresh":
                assert auth is None and payload == {"refreshToken":REFRESH,"grantType":"refresh_token","clientType":"extension"}
                AUTH.append("refresh")
                return self.reply(200,{"data":{"accessToken":RENEWED,"refreshToken":"fixture-rotated", "expiresAt":"2099-11-20T00:00:00Z"}})
            assert path == "/api/v1/chat/completions" and self.headers["X-CLIENT-TYPE"] == "9router"
            model = payload["model"]
            assert payload == {"model":model,"max_tokens":1024,"stream":False,
                              "messages":[{"role":"user","content":"hi"}]}, payload
            if model.startswith("cline-pass/"):
                assert auth == "Bearer "+PASS
            else:
                assert auth in ("Bearer "+OAUTH,"Bearer "+RENEWED)
            ATTEMPTS.append((model,direct,auth))
            if model in SEMANTIC:
                status,content,_,_=SEMANTIC[model]
                if model == "fixture-http-error":
                    raw = json.dumps(content,separators=(",",":")).encode()
                    self.send_response(status)
                    self.send_header("Content-Type","application/json")
                    self.send_header("Content-Length",str(len(raw)))
                    self.end_headers()
                    self.wfile.write(raw)
                    return
                return self.reply(status,content)
            assert BATCH and model in BATCH["ids"], model
            first = BATCH["ids"][0]
            if model == first:
                if BATCH["renew"] and auth == "Bearer "+OAUTH:
                    return self.reply(401,{"error":"reject saved access"})
                time.sleep(.04)
                BATCH["warm"].set()
                return self.reply(200,{"success":True,"data":choice()})
            assert BATCH["warm"].is_set()
            if BATCH["renew"]:
                assert auth == "Bearer "+RENEWED
            BATCH["parallel"].wait(timeout=6)
            time.sleep(.006*(len(BATCH["ids"])-BATCH["ids"].index(model)))
            if REPEAT_ERRORS and model == BATCH["ids"][1]:
                return self.reply(200,{"error":{"message":f"echoed {OAUTH} and {RENEWED}"}})
            return self.reply(200,{"success":True,"data":choice()})
        except (KeyError, AssertionError, ValueError, threading.BrokenBarrierError) as error:
            ERRORS.append((self.path,repr(error)))
            self.reply(422,{"message":"Fixture upstream mismatch"})

    def do_GET(self):
        ERRORS.append((self.path,"static batch must not discover models"))
        self.reply(422,{"error":"unexpected discovery"})


def save(db, provider, details=None, renew=False, **overrides):
    values = {"accessToken":OAUTH,"expiresAt":"2099-01-01T00:00:00Z"} if provider == "cline" else {"apiKey":PASS}
    if renew:
        values["refreshToken"]=REFRESH
    shared.save(db,provider,details,**(values | overrides))
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          SELECT id||'-catalog',provider,authType,name,9,0,'{}',createdAt,updatedAt FROM providerConnections WHERE id=?""",(provider,))


def post(port,path,value=None,headers=shared.ADMIN):
    with closing(HTTPConnection("127.0.0.1",port,timeout=20)) as connection:
        connection.request("POST",path,json.dumps(value) if value is not None else None,
                           {"Content-Type":"application/json",**headers})
        response=connection.getresponse()
        return response.status,json.loads(response.read())


def stable(value):
    assert isinstance(value["latencyMs"],int) and value["latencyMs"]>=0, value
    return {name:item for name,item in value.items() if name != "latencyMs"}


def batch(port, provider, renew=False):
    global BATCH
    alias = "cl" if provider == "cline" else "clinepass"
    rows = CATALOG[alias]
    ids = [row["id"] for row in rows]
    BATCH = {"ids":ids,"renew":renew,"warm":threading.Event(),"parallel":threading.Barrier(len(ids)-1)}
    before=len(ATTEMPTS)
    code,value=post(port,f"/api/providers/{provider}-catalog/test-models")
    BATCH=None
    assert code==200 and value["connectionId"]==provider+"-catalog" and value["provider"]==provider,(code,value,ERRORS)
    assert [row["modelId"] for row in value["results"]]==ids
    assert [row["name"] for row in value["results"]]==[row["name"] for row in rows]
    observed=[model for model,_,_ in ATTEMPTS[before:]]
    count=2 if renew else 1
    assert observed[:count]==[ids[0]]*count and set(observed[count:])==set(ids[1:]),observed
    return [stable(row) for row in value["results"]]


def compare(port,db,native):
    global REPEAT_ERRORS
    save(db,"cline")
    save(db,"clinepass")
    assert post(port,TEST,{"model":"cl/fixture-completion"},headers={})[0]==401
    assert post(port,TEST,{})==(400,{"error":"Model required"})
    assert post(port,"/api/providers/none/test-models")== (404,{"error":"Connection not found"})
    summary={}
    for model,(http,_,ok,error) in SEMANTIC.items():
        status,result=post(port,TEST,{"model":"cl/"+model,"kind":"llm"})
        expected={"ok":ok,"error":error,"status":http}
        if model=="fixture-reasoning":
            expected["note"]="reasoning-only response (length-limited)"
        assert status==200 and stable(result)==expected,(model,status,result,ERRORS)
        summary[model]=stable(result)
    save(db,"cline",renew=True)
    before=len(AUTH)
    summary["cline"]=batch(port,"cline",renew=True)
    summary["clinepass"]=batch(port,"clinepass")
    assert len(AUTH)==before+1
    assert all(row["ok"] for provider in ("cline","clinepass") for row in summary[provider])
    if native:
        save(db,"cline",renew=True)
        REPEAT_ERRORS=True
        rows=batch(port,"cline",renew=True)
        assert rows[1]["error"]=="echoed [redacted] and [redacted]",rows[1]
    print(json.dumps(summary,ensure_ascii=False,separators=(",",":"),sort_keys=True))


@contextmanager
def reference(scratch):
    data=scratch/"data"
    subprocess.run([sys.executable,str(ROOT/"rust/reference/seed.py"),str(data)],check=True,
                   env={"PATH":os.environ["PATH"],"HOME":str(scratch)},stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port=listener.getsockname()[1]
    module=scratch/"vendor.mjs"
    module.write_text('''
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
'''.replace("REFERENCE_SERVER",json.dumps(str(ROOT/".next/standalone/server.js"))))
    env={"DATA_DIR":str(data),"PORT":str(port),"HOSTNAME":"127.0.0.1","HOME":str(scratch),
         "PATH":os.environ["PATH"],"MODEL_CATALOG_SYNC":"off","DISABLE_BACKGROUND_TOKEN_REFRESH":"1"}
    with subprocess.Popen(["node","--import",str(module),".next/standalone/custom-server.js"],cwd=ROOT,env=env,
                          stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) as process:
        try:
            deadline=time.monotonic()+10
            while process.poll() is None and time.monotonic()<deadline:
                try:
                    if shared.get(port,"/api/health")[0]==200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("Pinned model probes server did not become ready")
            yield port,data/"db/data.sqlite"
        finally:
            process.terminate()
            process.wait(timeout=7)


def protected(port,db,http,socks,connect,wrong_host):
    save(db,"cline",http)
    save(db,"clinepass",{"proxyPoolId":"probe-socks"})
    status,row=post(port,TEST,{"model":"cl/fixture-reasoning"})
    assert status==200 and row["ok"] and row["note"]=="reasoning-only response (length-limited)"
    assert all(row["ok"] for row in batch(port,"cline"))
    assert all(row["ok"] for row in batch(port,"clinepass"))
    assert {route for route,_,_ in shared.TUNNELS}=={"connect","socks"}
    assert shared.HELLOS==[HOST for _,_,_ in shared.TUNNELS]
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                SELECT 'cline-direct','cline','oauth','direct',2,1,json_remove(data,'$.providerSpecificData'),createdAt,updatedAt
                FROM providerConnections WHERE id='cline'""")
    for details in ({"proxyPoolId":"probe-relay"},{"proxyPoolId":"missing-pool"},{"strictProxy":True}):
        save(db,"cline",details)
        before=(len(ATTEMPTS),len(shared.TUNNELS))
        status,row=post(port,TEST,{"model":"cl/fixture-completion"})
        assert status==200 and not row["ok"] and row["status"]==503,(status,row)
        assert (len(ATTEMPTS),len(shared.TUNNELS))==before
    save(db,"cline",http,renew=True,expiresAt="2000-01-01T00:00:00Z")
    before=(len(ATTEMPTS),len(shared.TUNNELS))
    status,row=post(port,TEST,{"model":"cl/fixture-completion"})
    assert status==200 and row["ok"],(status,row)
    assert (len(ATTEMPTS),len(shared.TUNNELS))==(before[0]+1,before[1]+2)
    assert AUTH == ["refresh"] and ATTEMPTS[-1] == ("fixture-completion",False,"Bearer "+RENEWED)
    save(db,"cline",http)
    for reject in (True,False):
        connect.reject=reject
        if not reject:
            connect.target=wrong_host
        before=(len(ATTEMPTS),len(shared.TUNNELS))
        status,row=post(port,TEST,{"model":"cl/fixture-completion"})
        assert status==200 and not row["ok"] and row["status"]==502,(status,row)
        assert len(ATTEMPTS)==before[0] and len(shared.TUNNELS)==before[1]+1
        assert all(secret not in json.dumps(row) for secret in shared.SECRETS)
    assert all(not direct for _,direct,_ in ATTEMPTS)
    print("Cline probes used the fixed TLS route for renewal and stopped after proxy failures.")


with tempfile.TemporaryDirectory(prefix="9router-cline-model-probes-") as folder, ExitStack() as stack:
    scratch=Path(folder)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    if MODE == "reference":
        with reference(scratch) as (port,db):
            compare(port,db,False)
    elif MODE == "native":
        with shared.gateway(scratch,"native",None,[]) as (port,db):
            compare(port,db,True)
    else:
        assert MODE == "protected"
        ca,target,wrong_host=shared.tls_services(stack,scratch,Vendor)
        connect,socks=[shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
        for proxy,mode in ((connect,"connect"),(socks,"socks")):
            proxy.mode,proxy.reject,proxy.target=mode,False,target
            stack.enter_context(shared.serving(proxy))
        http={"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{connect.server_address[1]}"}
        socks=f"socks5h://127.0.0.1:{socks.server_address[1]}"
        pools=[("probe-socks","socks5",socks),("probe-relay","cloudflare",RELAY)]
        with shared.gateway(scratch,"protected",ca,pools) as (port,db):
            with sqlite3.connect(db) as sql:
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='probe-relay'",
                            (json.dumps({"url":RELAY,"token":RELAY_KEY}),))
            protected(port,db,http,socks,connect,wrong_host)
    assert not ERRORS and not shared.ERRORS,(ERRORS,shared.ERRORS)
