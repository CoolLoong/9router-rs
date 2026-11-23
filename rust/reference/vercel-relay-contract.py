import base64
import http.client
import json
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
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
prefix = "/vercel-management"
endpoint = "/api/proxy-pools/vercel-deploy"
pools = "/api/proxy-pools"
backup = "/api/settings/database"
cli = {"x-9r-cli-token":"68977b6fd9422fbb"}
pat = "offline-vercel-personal-token"
team = "team_fixture"
control = []
access = []
failures = []
captures = []
projects = {"existing":{"id":"prj_preexisting"}}
deploys = {}
guard = Lock()
files = {
    "api/relay.mjs":"vercel_relay_function.mjs",
    "lib/relay.mjs":"cloudflare_relay_worker.mjs",
    "lib/pinned-https.mjs":"pinned_relay_https.mjs",
}


def domain(name):
    return name + "-assigned.vercel.app"


class Vercel(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def respond(self,status,body,headers=None):
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
        url = urlsplit(self.path)
        try:
            if url.path == "/capture":
                captures.append("Authorization" in self.headers or "x-relay-auth" in self.headers)
                return self.respond(200,{})
            if url.path.startswith("/vercel-relay/"):
                host = url.path.removeprefix("/vercel-relay/")
                name = host.removesuffix("-assigned.vercel.app")
                row = projects[name]
                assert self.command == "GET" and host == domain(name)
                assert self.headers.get("Authorization") is None
                credential = self.headers.get("x-relay-auth")
                target = self.headers.get("x-relay-target")
                access.append((name,credential is not None,target is not None))
                if name == "platform-locked":
                    assert credential is None
                    return self.respond(401,{"error":"Vercel Authentication"})
                if name == "open-runtime":
                    assert credential is None
                    return self.respond(200,{"open":True})
                if credential is None:
                    assert target is None
                    return self.respond(403,{"error":"Relay authorization required"})
                assert self.headers.get_all("x-relay-auth") == [row["secret"]]
                if name == "runtime-redirect":
                    return self.respond(307,{},{"Location":"/capture"})
                if not target:
                    return self.respond(400,{"error":"A public HTTPS destination is required"})
                assert target == "https://httpbin.org" and self.headers["x-relay-path"] == "/get"
                return self.respond(200,{"probe":"ready"})
            assert self.headers.get_all("Authorization") == ["Bearer "+pat]
            assert url.path.startswith(prefix)
            path = url.path.removeprefix(prefix)
            query = parse_qs(url.query)
            control.append((self.command,path,query))
            if self.command == "POST" and path == "/v11/projects":
                name = body["name"]
                assert body == {"name":name,"framework":None}
                if name == "api-redirect":
                    return self.respond(307,{},{"Location":"/capture"})
                if name == "invalid-pat":
                    return self.respond(403,{"error":{"message":"echo "+pat}})
                with guard:
                    if name in projects:
                        return self.respond(409,{"error":{"message":"already exists"}})
                    project = "prj_"+str(len(projects))
                    projects[name] = {"id":project,"team":query.get("teamId"),"system":name!="enable-system"}
                return self.respond(201,{"id":project,"name":name,
                    "autoExposeSystemEnvs":projects[name]["system"]})
            parts = path.split("/")
            if len(parts) >= 4 and parts[2] == "projects":
                project = parts[3]
                name,row = next((name,row) for name,row in projects.items() if row["id"] == project)
                assert query.get("teamId") == row["team"]
                if self.command == "PATCH" and len(parts) == 4:
                    assert name == "enable-system" and body == {"autoExposeSystemEnvs":True}
                    row["system"] = True
                    return self.respond(200,{"id":project,"autoExposeSystemEnvs":True})
                if self.command == "POST" and parts[-1] == "env":
                    assert row["system"] is True and set(body) == {"key","value","type","target"}
                    assert body["key"] == "RELAY_AUTH" and body["type"] == "sensitive"
                    assert body["target"] == ["production"] and re.fullmatch(r"[A-Za-z0-9_-]{43}",body["value"])
                    row["secret"] = body["value"]
                    return self.respond(201,{"created":{"key":"RELAY_AUTH","target":["production"],
                        "type":"plain" if name == "unprotected-secret" else "sensitive"},"failed":[]})
                if self.command == "GET" and parts[-1] == "domains":
                    assert query | {"teamId":None} == {
                        "production":["true"],"verified":["true"],"redirects":["false"],"teamId":None,
                    }
                    return self.respond(200,{"domains":[
                        {"name":domain(name),"verified":True},
                        {"name":"wrong.example.org","verified":True},
                        {"name":"other-preview.vercel.app","verified":True,"gitBranch":"preview"},
                    ]})
            if self.command == "POST" and path == "/v13/deployments":
                name = body["name"]
                project = projects[name]
                assert project["secret"] and query.get("teamId") == project["team"]
                assert body["project"] == project["id"] and body["target"] == "production"
                assert body["projectSettings"] == {"framework":None}
                assets = {entry["file"]:base64.b64decode(entry["data"],validate=True)
                          for entry in body["files"] if entry["encoding"] == "base64"}
                assert set(assets) == set(files) | {"package.json"}
                for destination,source in files.items():
                    assert assets[destination] == (root/"rust/app/src"/source).read_bytes()
                assert json.loads(assets["package.json"]) == {
                    "name":name,"version":"1.0.0","type":"module","private":True,
                }
                assert all(project["secret"].encode() not in data and pat.encode() not in data for data in assets.values())
                with guard:
                    deploy_id = "dpl_"+str(len(deploys))
                    deploys[deploy_id] = {"name":name,"polls":0}
                    project["deployment"] = deploy_id
                return self.respond(200,{"id":deploy_id})
            if self.command == "GET" and path.startswith("/v13/deployments/"):
                deploy_id = parts[-1]
                deployment = deploys[deploy_id]
                name = deployment["name"]
                assert query.get("teamId") == projects[name]["team"]
                deployment["polls"] += 1
                if name == "deploy-failed":
                    return self.respond(200,{"id":deploy_id,"target":"production","readyState":"ERROR"})
                return self.respond(200,{"id":deploy_id,"target":"production",
                    "readyState":"BUILDING" if deployment["polls"] == 1 else "READY",
                    "aliasAssigned":deployment["polls"] >= 3,
                    "alias":[domain(name),"wrong.example.org","other-preview.vercel.app"]})
            if self.command == "GET" and path.startswith("/v4/aliases/"):
                host = parts[-1]
                name = host.removesuffix("-assigned.vercel.app")
                project = projects[name]
                assert host == domain(name) and query["projectId"] == [project["id"]]
                assert query.get("teamId") == project["team"]
                return self.respond(200,{"alias":host,"projectId":project["id"],
                    "deploymentId":"dpl_another" if name=="misbound" else project["deployment"]})
            raise AssertionError((self.command,path))
        except (AssertionError,KeyError,StopIteration,ValueError) as error:
            failures.append((self.command,url.path,str(error) or "unexpected wire"))
            self.respond(500,{})

    do_GET = do_POST = do_PATCH = do_DELETE = handle_api


def request(port,path,method="GET",body=None,headers=cli):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=12)) as connection:
        if body is not None:
            headers = headers | {"Content-Type":"application/json"}
        connection.request(method,path,body=None if body is None else json.dumps(body),headers=headers)
        response = connection.getresponse()
        return response.status,json.loads(response.read())


def public(value):
    data = json.dumps(value)
    assert pat not in data and "_managedVercel" not in data
    assert all(project.get("secret","unused-fixture-secret") not in data for project in projects.values())


with tempfile.TemporaryDirectory(prefix="9router-vercel-relay-") as directory:
    base = Path(directory)
    data, home, commands = (base / name for name in ("data","home","bin"))
    for folder in (data,home,commands):
        folder.mkdir()
    (data/"auth").mkdir()
    (data/"machine-id").write_text("migration-fixture-machine-id\n")
    (data/"auth/cli-secret").write_text("migration-fixture-cli-secret\n")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    mock = ThreadingHTTPServer(("127.0.0.1",52989),Vercel)
    Thread(target=mock.serve_forever,daemon=True).start()
    proc = subprocess.Popen([str(binary),"--no-browser"],cwd=base,env={
        "HOME":str(home),"PATH":str(commands),"PORT":str(port),"HOST":"127.0.0.1",
        "DATA_DIR":str(data),"NINEROUTER_MIGRATION_FIXTURE":"1","MODEL_CATALOG_SYNC":"off",
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
        create = lambda name, *scope: request(port,endpoint,"POST",
            {"projectName":name,"vercelToken":pat} | ({"teamId":scope[0]} if scope else {}))
        assert request(port,endpoint,"POST",{},headers={})[0] == 401
        assert request(port,endpoint,"POST",{}) == (400,{"error":"Vercel API token is required"})
        assert create("../outside")[0] == 400 and create("valid","bad/team")[0] == 400
        assert not control
        assert create("existing")[0] == 409 and projects["existing"] == {"id":"prj_preexisting"}
        for name,status in (("api-redirect",502),("invalid-pat",403),("unprotected-secret",502),
                            ("deploy-failed",502),("misbound",502),("platform-locked",502),
                            ("open-runtime",502),("runtime-redirect",502)):
            result = create(name)
            assert result[0] == status,(name,result,failures)
            public(result[1])
        assert "deployment" not in projects["unprotected-secret"] and not captures
        assert access == [
            ("platform-locked",False,False),("open-runtime",False,False),
            ("runtime-redirect",False,False),("runtime-redirect",True,False),
        ]
        assert request(port,pools)[1] == {"proxyPools":[]}

        control.clear()
        access.clear()
        status, deployed = create("managed-relay",team)
        assert status == 201,(status,deployed,failures)
        assert deployed["deployUrl"] == "https://"+domain("managed-relay")+"/api/relay"
        assert deployed["proxyPool"]["type"] == "vercel"
        public(deployed)
        pool = pools + "/" + deployed["proxyPool"]["id"]
        assert all(query.get("teamId") == [team] for _,_,query in control)
        assert access == [("managed-relay",False,False),("managed-relay",True,False)]
        assert request(port,pool+"/test","POST")[1]["ok"] and access[-1] == ("managed-relay",True,True)
        assert request(port,pool,"PUT",{"proxyUrl":deployed["deployUrl"]+"/elsewhere"})[0] == 409
        public(request(port,pool,"PUT",{"name":"Vercel protected relay"})[1])
        public(request(port,pools)[1])
        public(request(port,pool)[1])
        status,saved = request(port,backup)
        assert status == 200 and pat not in json.dumps(saved)
        record, = saved["proxyPools"]
        assert record["_managedVercel"] == {
            "url":deployed["deployUrl"],"token":projects["managed-relay"]["secret"],
        }
        assert request(port,backup,"POST",saved) == (200,{"success":True})
        assert request(port,pool+"/test","POST")[1]["ok"]
        assert create("enable-system")[0] == 201
        assert [row for row in control if row[0] == "PATCH"] == [
            ("PATCH","/v9/projects/"+projects["enable-system"]["id"],{}),
        ]
        with ThreadPoolExecutor(max_workers=2) as repeat:
            assert sorted(repeat.map(lambda _:create("one-name")[0],range(2))) == [201,409]
        assert not any(method == "DELETE" for method,_,_ in control)
        assert not captures and not failures,failures
        assert not (base/"fallbacks").exists() or (base/"fallbacks").read_text() == ""
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
                raise
        mock.shutdown()
        mock.server_close()

print("Vercel create-only project, sensitive production binding, closed verified route and portable backup passed")
