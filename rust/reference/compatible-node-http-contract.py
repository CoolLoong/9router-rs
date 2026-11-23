from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("compatible_node_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
ROOT = shared.ROOT
KEY = "offline-compatible-probe"
CALLS, ERRORS = [], []
VALIDATE = "/api/provider-nodes/validate"


class Models(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, code, value, **headers):
        raw = value if isinstance(value,bytes) else json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, val in headers.items():
            self.send_header(name, val)
        self.end_headers()
        try:
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def respond(self):
        try:
            assert self.headers.get_all("Authorization") == ["Bearer "+KEY]
            assert all(self.headers[name] is None for name in ("Cookie", "x-9r-cli-token", "Proxy-Authorization"))
            anthropic = self.path.startswith("/anthropic/")
            assert self.headers["x-api-key"] == (KEY if anthropic else None)
            assert self.headers["anthropic-version"] == ("2023-06-01" if anthropic else None)
            CALLS.append((self.command,self.path))
            if self.command == "GET":
                assert self.path.endswith("/models")
                if self.path.startswith("/redirect/"):
                    return self.reply(307,{},Location=f"http://127.0.0.1:{self.server.server_port}/capture")
                if self.path.startswith("/unauthorized/"):
                    return self.reply(401,{"message":KEY})
                if self.path.startswith(("/missing/","/invalid-model/","/anthropic/missing/")):
                    return self.reply(404,{})
                return self.reply(200,{"data":[]})
            assert self.command == "POST"
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path.endswith("/embeddings"):
                assert data == {"model":"fixture-embedding","input":"ping"}
                if self.path.startswith("/embeds-auth/"):
                    return self.reply(401,{"message":KEY})
                if self.path.startswith("/embeds-empty/"):
                    return self.reply(200,{"data":[{"embedding":[]}]})
                if self.path.startswith("/embeds-missing/"):
                    return self.reply(200,{"data":[{"embedding":"not a vector"}]})
                if self.path.startswith("/embeds-malformed/"):
                    return self.reply(200,b"invalid JSON")
                if self.path.startswith("/embeds-lg-success/"):
                    return self.reply(200,b" "*(2*1024*1024+1))
                if self.path.startswith("/embeds-error/"):
                    return self.reply(503,("temporary error " + "v"*260).encode())
                if self.path.startswith("/embeds-lg-error/"):
                    return self.reply(503,(KEY + "a"*65536).encode())
                if self.path.startswith("/embeds-echo/"):
                    return self.reply(422,(f'{{"message":"literal {KEY}, encoded %6f{KEY[1:]}, escaped \\u006f{KEY[1:]}"}}').encode())
                if self.path.startswith("/embeds-redirect/"):
                    return self.reply(307,{},Location=f"http://127.0.0.1:{self.server.server_port}/capture")
                return self.reply(200,{"data":[{"embedding":[.1,.2,.3]}]})
            if self.path.endswith("/responses"):
                assert data == {"model":"fixture-model","input":"ping","max_output_tokens":16}
                return self.reply(200,{})
            assert self.path.endswith("/chat/completions") and data == {
                "model":"fixture-model","messages":[{"role":"user","content":"ping"}],"max_tokens":1,
            }
            if self.path.startswith("/invalid-model/"):
                return self.reply(400,{"key":KEY})
            self.reply(200,{})
        except (AssertionError, ValueError, KeyError) as error:
            ERRORS.append((self.path,repr(error)))
            self.reply(418,{"error":"Unexpected fixture request"})

    do_POST = do_GET = respond


def request(port, method, path, data=None, headers=None):
    headers = dict(headers or {})
    if data is not None:
        headers["Content-Type"] = "application/json"
    with closing(HTTPConnection("127.0.0.1",port,timeout=10)) as client:
        client.request(method,path,json.dumps(data) if data is not None else None,headers)
        response=client.getresponse()
        raw=response.read()
        return response.status,response.headers,(json.loads(raw) if response.headers.get("Content-Type","").startswith("application/json") else raw)


def login(port):
    status,headers,result=request(port,"POST","/api/auth/login",{"password":"123456"})
    assert status==200 and result["success"]
    return headers["Set-Cookie"].split(";",1)[0]


def parity(port,cookie,source):
    table = [
        ("/v1", {}, {"valid":True}),
        ("/unauthorized/v1", {"modelId":"fixture-model"}, {"valid":False,"error":"API key unauthorized"}),
        ("/missing/v1", {}, {"valid":False,"error":"/models endpoint not found - try chat validation with model ID"}),
        ("/missing/v1", {"modelId":"fixture-model"}, {"valid":True,"method":"chat"}),
        ("/invalid-model/v1", {"modelId":"fixture-model"}, {"valid":False,"method":"chat","error":"Invalid model or bad request"}),
        ("/anthropic/v1/messages", {"type":"anthropic-compatible"}, {"valid":True}),
        ("/anthropic/missing/v1/messages", {"type":"anthropic-compatible","modelId":"fixture-model"}, {"valid":True,"method":"chat"}),
    ]
    headers={"Cookie":cookie}
    observed=[]
    before=len(CALLS)
    for path,options,expected in table:
        status,_,reply=request(port,"POST",VALIDATE,{"baseUrl":source+path,"apiKey":KEY,**options},headers)
        assert status==200 and reply==expected,(status,reply,ERRORS)
        observed.append(reply)
    assert len(CALLS)==before+10
    return observed,CALLS[before:]


def embedding_parity(port,cookie,source):
    cases = [
        ("/embeds/v1", None, {"valid":False,"error":"Model ID required for embedding validation"}),
        ("/embeds/v1", "  fixture-embedding  ", {"valid":True,"method":"embeddings","dimensions":3}),
        ("/embeds-empty/v1", "fixture-embedding", {"valid":True,"method":"embeddings","dimensions":0}),
        ("/embeds-missing/v1", "fixture-embedding", {"valid":True,"method":"embeddings","dimensions":None}),
        ("/embeds-malformed/v1", "fixture-embedding", {"valid":True,"method":"embeddings","dimensions":None}),
        ("/embeds-auth/v1", "fixture-embedding", {"valid":False,"error":"API key unauthorized"}),
        ("/embeds-error/v1", "fixture-embedding", {"valid":False,"method":"embeddings",
            "error":"Embeddings request failed (503): "+("temporary error "+"v"*260)[:200]}),
    ]
    headers={"Cookie":cookie}
    observed=[]
    before=len(CALLS)
    for path, model, expected in cases:
        payload={"baseUrl":source+path,"apiKey":KEY,"type":"custom-embedding"}
        if model is not None:
            payload["modelId"]=model
        status,_,reply=request(port,"POST",VALIDATE,payload,headers)
        assert status==200 and reply==expected,(status,reply,ERRORS)
        observed.append(reply)
    assert CALLS[before:]==[("POST",path+"/embeddings") for path, model, _ in cases if model is not None]
    return observed,CALLS[before:]


@contextmanager
def reference(home):
    data=home/"data"
    subprocess.run([sys.executable,str(ROOT/"rust/reference/seed.py"),str(data)],check=True,cwd=ROOT,
                   env={"PATH":os.environ["PATH"],"HOME":str(home)},stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port=listener.getsockname()[1]
    env={"DATA_DIR":str(data),"HOSTNAME":"127.0.0.1","PORT":str(port),"HOME":str(home),"PATH":os.environ["PATH"],
         "MODEL_CATALOG_SYNC":"off","DISABLE_BACKGROUND_TOKEN_REFRESH":"1"}
    with subprocess.Popen(["node",".next/standalone/custom-server.js"],cwd=ROOT,env=env,
                          stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) as process:
        try:
            deadline=time.monotonic()+10
            while process.poll() is None and time.monotonic()<deadline:
                try:
                    if request(port,"GET","/api/health")[0]==200:
                        break
                except OSError:
                    pass
                time.sleep(.04)
            else:
                raise RuntimeError("Pinned provider validation did not become ready")
            yield port
        finally:
            process.terminate()
            process.wait(timeout=7)


with tempfile.TemporaryDirectory(prefix="9router-compatible-node-") as temporary, ExitStack() as stack:
    scratch=Path(temporary)
    models=stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",0),Models)))
    source=f"http://127.0.0.1:{models.server_port}"
    with reference(scratch) as port:
        cookie=login(port)
        expected=parity(port,cookie,source)
        expected_embedding=embedding_parity(port,cookie,source)
    with shared.gateway(scratch,"compatible",None,[]) as (port,_db):
        assert request(port,"GET","/dashboard/providers/nodes/new")[0]==307
        assert request(port,"POST",VALIDATE,{"baseUrl":source,"apiKey":KEY})[0]==401
        cookie=login(port)
        assert parity(port,cookie,source)==expected
        assert embedding_parity(port,cookie,source)==expected_embedding
        headers={"Cookie":cookie}
        before=len(CALLS)
        for base,extra in ((source,{"X-Forwarded-For":"192.0.2.20"}),
                           (source.replace("http:","https:"),{"X-Forwarded-For":"192.0.2.20"}),
                           ("https://localhost",{"X-Forwarded-For":"192.0.2.20"}),
                           (source.replace("://","://username:password@"),{}),(source+"?debug=yes",{})):
            status,_,reply=request(port,"POST",VALIDATE,{"baseUrl":base,"apiKey":KEY},headers|extra)
            assert status==400 and reply=={"error":"URL not allowed"},(status,reply)
        assert len(CALLS)==before
        status,_,reply=request(port,"POST",VALIDATE,{"baseUrl":source+"/redirect/v1","apiKey":KEY,"modelId":"fixture-model"},headers)
        assert status==200 and reply=={"valid":False,"error":"Provider redirect refused"}
        assert CALLS[before:]==[("GET","/redirect/v1/models")]
        assert request(port,"PATCH","/api/settings",{"outboundProxyEnabled":True,"outboundProxyUrl":"http://127.0.0.1:1"},headers)[0]==200
        before=len(CALLS)
        statu,_,reply=request(port,"POST",VALIDATE,{"baseUrl":source+"/v1","apiKey":KEY},headers)
        assert statu==200 and reply=={"valid":False,"error":"Endpoint checks require a direct route"}
        assert len(CALLS)==before
        assert request(port,"PATCH","/api/settings",{"outboundNoProxy":"127.0.0.1"},headers)[0]==200
        assert request(port,"POST",VALIDATE,{"baseUrl":source+"/v1","apiKey":KEY},headers)[2]=={"valid":True}
        assert request(port,"PATCH","/api/settings",{"outboundProxyEnabled":False},headers)[0]==200
        for path, expected in (
            ("/embeds-echo/v1", {"valid":False,"method":"embeddings",
              "error":'Embeddings request failed (422): {"message":"literal [redacted], encoded [redacted], escaped [redacted]"}'}),
            ("/embeds-lg-success/v1", {"valid":True,"method":"embeddings","dimensions":None}),
            ("/embeds-lg-error/v1", {"valid":False,"method":"embeddings","error":"Embeddings request failed (503)"}),
            ("/embeds-redirect/v1", {"valid":False,"method":"embeddings","error":"Provider redirect refused"}),
        ):
            start=len(CALLS)
            status,_,reply=request(port,"POST",VALIDATE,{"baseUrl":source+path,"apiKey":KEY,"modelId":"fixture-embedding",
                                                       "type":"custom-embedding"},headers)
            assert status==200 and reply==expected,(status,reply)
            assert CALLS[start:]==[("POST",path+"/embeddings")]
        subprocess.run(["node",str(ROOT/"rust/reference/compatible-node-ui-contract.mjs"),f"http://127.0.0.1:{port}",cookie,source],
                       check=True,cwd=ROOT,env={"PATH":os.environ["PATH"],"HOME":str(scratch)})
    assert not ERRORS, ERRORS
print("OpenAI, Anthropic and custom embedding validators matched fourteen pinned outputs and requests; bounded errors, secrets and redirect guards passed.")
