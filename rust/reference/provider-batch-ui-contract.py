from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("provider_batch_ui_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
ROOT = shared.ROOT
REQUESTS = []


class Endpoint(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        REQUESTS.append(self.path)
        assert self.path in ("/good/models", "/bad/models")
        auth = "offline-probe-key" if self.path.startswith("/good/") else "offline-bad-key"
        assert self.headers["Authorization"] == "Bearer " + auth
        time.sleep(.04)
        raw = json.dumps({"secret":auth}).encode()
        self.send_response(200 if self.path.startswith("/good/") else 401)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def request(port, method, path, data=None):
    with closing(HTTPConnection("127.0.0.1",port,timeout=8)) as connection:
        connection.request(method,path,json.dumps(data) if data is not None else None,
                           {"Content-Type":"application/json"} if data is not None else {})
        response=connection.getresponse()
        raw=response.read()
        return response.status,response.headers,raw


with tempfile.TemporaryDirectory(prefix="9router-provider-batch-ui-") as temporary, ExitStack() as stack:
    scratch=Path(temporary)
    upstream=stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",0),Endpoint)))
    target=f"http://127.0.0.1:{upstream.server_port}"
    with shared.gateway(scratch,"provider-batch",None,[]) as (port,db):
        assert request(port,"GET","/dashboard/providers")[0] == 307
        assert request(port,"POST","/api/providers/test-batch",{"mode":"apikey"})[0] == 401
        assert not REQUESTS
        status,headers,_=request(port,"POST","/api/auth/login",{"password":"123456"})
        assert status == 200
        cookie=headers["Set-Cookie"].split(";",1)[0]
        rows = [
            ("good","openai-compatible-ui","apikey","Healthy fixture",1,1,
             {"apiKey":"offline-probe-key","providerSpecificData":{"baseUrl":target+"/good"}}),
            ("bad","openai-compatible-ui","apikey","Failed fixture",2,1,
             {"apiKey":"offline-bad-key","providerSpecificData":{"baseUrl":target+"/bad"}}),
            ("free","kiro","oauth","Expired free fixture",3,1,{}),
            ("disabled","openai-compatible-ui","apikey","Disabled fixture",4,0,
             {"apiKey":"offline-probe-key","providerSpecificData":{"baseUrl":target+"/disabled"}}),
        ]
        with sqlite3.connect(db) as sql:
            sql.execute("DELETE FROM providerConnections")
            sql.executemany("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
              VALUES (?,?,?,?,?,?,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
              [(*row[:-1],json.dumps(row[-1])) for row in rows])
        subprocess.run(["node",str(ROOT/"rust/reference/provider-batch-ui-contract.mjs"),f"http://127.0.0.1:{port}"],
                       cwd=ROOT,env={"PATH":os.environ["PATH"],"HOME":str(scratch),"NINEROUTER_FIXTURE_COOKIE":cookie},check=True)
        assert REQUESTS==["/good/models","/bad/models"], REQUESTS
        with sqlite3.connect(db) as sql:
            saved={id:json.loads(value) for id,value in sql.execute("SELECT id,data FROM providerConnections")}
        assert [saved[id]["testStatus"] for id in ("good","bad","free")]==["active","error","error"]
        assert "testStatus" not in saved["disabled"]
print("Native Provider overview passed authenticated raw HTTP and persisted group test health.")
