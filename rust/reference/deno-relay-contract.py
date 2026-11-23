import http.client
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Lock, Thread

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
prefix = "/deno-management/v2"
route = "/api/proxy-pools/deno-deploy"
pools = "/api/proxy-pools"
backup = "/api/settings/database"
session = {"x-9r-cli-token": "68977b6fd9422fbb"}
api_token = "ddo_offline-deno-manager"
runtime = (root / "rust/app/src/deno_relay_runtime.mjs").read_text()
handler = (root / "rust/app/src/cloudflare_relay_worker.mjs").read_text()
pinned = (root / "rust/app/src/pinned_relay_https.mjs").read_text()
existing_id = "88a2bd5e-6e1a-4fe4-a799-aa6d9a6ab26e"
apps = {"existing": {"id":existing_id,"published":True}}
revisions = {}
ledger = []
failures = []
redirects = []
probes = []
guard = Lock()


class DenoApi(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(raw)))
        for name,value in (headers or {}).items():
            self.send_header(name,value)
        self.end_headers()
        self.wfile.write(raw)

    def handle_api(self):
        size = int(self.headers.get("Content-Length","0"))
        body = json.loads(self.rfile.read(size)) if size else {}
        try:
            if self.path == "/capture":
                redirects.append(self.headers.get("Authorization"))
                return self.reply(200,{})
            if self.path.startswith("/deno-relay/"):
                hosts = {
                    f"/deno-relay/{name}.fixture-org.deno.net": app
                    for name,app in apps.items() if app.get("secret") and app.get("published")
                }
                app = hosts[self.path]
                assert self.command == "GET"
                assert self.headers.get_all("x-relay-auth") == [app["secret"]]
                assert self.headers.get("Authorization") is None
                assert self.headers["x-relay-target"] == "https://httpbin.org"
                assert self.headers["x-relay-path"] == "/get"
                probes.append(self.path)
                return self.reply(200,{"ok":True})
            assert self.headers.get_all("Authorization") == ["Bearer "+api_token]
            ledger.append((self.command,self.path))
            if self.path == prefix + "/apps" and self.command == "POST":
                slug = body["slug"]
                assert body["labels"] == {"custom.kind":"9router-relay"}
                assert body["config"] == {"runtime":{"type":"dynamic","entrypoint":"main.mjs"}}
                secret, url = body["env_vars"]
                assert secret["key"] == "RELAY_AUTH" and secret["secret"] is True
                assert secret["contexts"] == ["production"]
                assert re.fullmatch(r"[A-Za-z0-9_-]{43}",secret["value"])
                assert url["key"] == "RELAY_URL" and url["secret"] is False
                assert url["contexts"] == ["production"]
                assert re.fullmatch(rf"https://{slug}\.[a-z-]+\.deno\.net",url["value"])
                if slug == "api-redirect":
                    return self.reply(307,{},{"Location":"/capture"})
                if slug == "token-rejected":
                    return self.reply(403,{"error":{"message":"echo "+api_token}})
                with guard:
                    if slug in apps:
                        return self.reply(409,{"error":{"message":"already exists"}})
                    app_id = f"{len(apps):08x}-0000-4000-8000-000000000000"
                    apps[slug] = {"id":app_id,"secret":secret["value"],"published":False,"url":url["value"]}
                secret.pop("value")
                if slug == "bad-mask":
                    secret["secret"] = False
                return self.reply(200,{"id":app_id,"slug":slug,"env_vars":[secret,url]})
            if self.command == "POST" and self.path.endswith("/deploy"):
                app_id = self.path.removeprefix(prefix+"/apps/").removesuffix("/deploy")
                name, app = next((key,row) for key,row in apps.items() if row["id"] == app_id)
                assert body == {"production":True,"preview":False,"assets":{
                    "main.mjs":{"kind":"file","encoding":"utf-8","content":runtime},
                    "relay.mjs":{"kind":"file","encoding":"utf-8","content":handler},
                    "pinned-https.mjs":{"kind":"file","encoding":"utf-8","content":pinned},
                }}
                if name == "upload-denied":
                    return self.reply(403,{"error":{"message":"echo "+app["secret"]}})
                revision = f"r{len(revisions)+1000}offline"
                revisions[revision] = {"name":name,"polls":0}
                return self.reply(202,{"id":revision,"status":"queued","timelines":[]})
            if self.command == "GET" and self.path.startswith(prefix+"/revisions/"):
                revision = self.path.removeprefix(prefix+"/revisions/")
                entry = revisions[revision]
                entry["polls"] += 1
                name = entry["name"]
                if name == "revision-moved":
                    return self.reply(200,{"id":"another-valid-id","status":"succeeded","timelines":[]})
                if entry["polls"] == 1:
                    return self.reply(200,{"id":revision,"status":"building","timelines":[]})
                if name == "revision-failed":
                    return self.reply(200,{"id":revision,"status":"failed","timelines":[]})
                apps[name]["published"] = True
                return self.reply(200,{"id":revision,"status":"succeeded","timelines":[
                    {"name":"Production","context":"Production","hostnames":[name+".fixture-org.deno.net"]},
                ]})
            raise AssertionError((self.command,self.path))
        except (AssertionError, KeyError, StopIteration) as error:
            failures.append((self.command,self.path,str(error) or "unexpected wire"))
            self.reply(500,{})

    do_GET = do_POST = do_DELETE = handle_api


def request(port,path,method="GET",body=None,headers=session):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=12)) as client:
        if body is not None:
            headers = headers | {"Content-Type":"application/json"}
        client.request(method,path,body=None if body is None else json.dumps(body),headers=headers)
        answer = client.getresponse()
        return answer.status,json.loads(answer.read())


def public(payload):
    data = json.dumps(payload)
    assert api_token not in data and "_managedDeno" not in data
    assert not any(app.get("secret","unused-fixture-secret") in data for app in apps.values())


with tempfile.TemporaryDirectory(prefix="9router-deno-relay-") as folder:
    base = Path(folder)
    home, commands, data = (base / name for name in ("home","commands","data"))
    for directory in (home,commands,data):
        directory.mkdir()
    (data/"auth").mkdir()
    (data/"machine-id").write_text("migration-fixture-machine-id\n")
    (data/"auth/cli-secret").write_text("migration-fixture-cli-secret\n")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    remote = ThreadingHTTPServer(("127.0.0.1",52989),DenoApi)
    Thread(target=remote.serve_forever,daemon=True).start()
    proc = subprocess.Popen([str(binary),"--no-browser"],cwd=base,env={
        "HOME":str(home),"PATH":str(commands),"PORT":str(port),"HOST":"127.0.0.1",
        "DATA_DIR":str(data),"MODEL_CATALOG_SYNC":"off","NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(base/"fallbacks"),
    },stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic()+10
        while proc.poll() is None and time.monotonic() < deadline:
            try:
                if request(port,"/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(0.05)
        else:
            raise AssertionError("Fixture gateway did not start")
        deploy = lambda project,org="fixture-org.deno.net": request(port,route,"POST",{
            "denoToken":api_token,"orgDomain":org,"projectName":project,
        })
        assert request(port,route,"POST",{},headers={})[0] == 401
        assert request(port,route,"POST",{}) == (400,{"error":"Organization domain is required"})
        assert request(port,route,"POST",{"orgDomain":"fixture-org"}) == (400,{"error":"Deno Deploy API token is required"})
        assert deploy("../outside")[0] == 400 and deploy("valid","example.org")[0] == 400
        assert not ledger
        assert deploy("existing") == (409,{"error":'App "existing" already exists. Choose a different name.'})
        assert apps["existing"] == {"id":existing_id,"published":True}
        for name, status in (("api-redirect",502),("token-rejected",403),("bad-mask",502),
                             ("revision-moved",502),("revision-failed",502),("upload-denied",403)):
            result = deploy(name)
            assert result[0] == status,(name,result,failures)
            public(result[1])
        assert deploy("wrong-org","different-org")[0] == 502
        assert not redirects and request(port,pools)[1] == {"proxyPools":[]}

        ledger.clear()
        status, deployed = deploy("my-relay")
        assert status == 201,(status,deployed,failures)
        assert deployed["deployUrl"] == "https://my-relay.fixture-org.deno.net"
        assert deployed["proxyPool"]["type"] == "deno"
        public(deployed)
        pool = pools + "/" + deployed["proxyPool"]["id"]
        rev = next(r for r,entry in revisions.items() if entry["name"] == "my-relay")
        assert ledger == [
            ("POST",prefix+"/apps"),("POST",prefix+"/apps/"+apps["my-relay"]["id"]+"/deploy"),
            ("GET",prefix+"/revisions/"+rev),("GET",prefix+"/revisions/"+rev),
        ]
        assert request(port,pool,"PUT",{"proxyUrl":"https://other.fixture-org.deno.net"})[0] == 409
        public(request(port,pool,"PUT",{"name":"Deno Relay"})[1])
        public(request(port,pool)[1])
        public(request(port,pools)[1])
        assert request(port,pool+"/test","POST")[1]["ok"]
        assert probes == ["/deno-relay/my-relay.fixture-org.deno.net"]
        status, data = request(port,backup)
        assert status == 200 and api_token not in json.dumps(data)
        assert data["proxyPools"][0]["_managedDeno"] == {
            "url":deployed["deployUrl"],"token":apps["my-relay"]["secret"],
        }
        assert request(port,backup,"POST",data) == (200,{"success":True})
        assert request(port,pool+"/test","POST")[1]["ok"]
        public(request(port,pool)[1])
        with ThreadPoolExecutor(max_workers=2) as pair:
            assert sorted(pair.map(lambda _: deploy("race-deno", "fixture-org")[0],range(2))) == [201,409]
        assert deploy("")[0] == 201
        assert not any(method == "DELETE" for method,_ in ledger)
        assert not failures,failures
        log = base / "fallbacks"
        assert not log.exists() or log.read_text() == ""
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
                raise
        remote.shutdown()
        remote.server_close()

print("Deno create-only relay deployment, exact production binding, masked pool and authenticated probe passed")
