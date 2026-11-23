import hashlib
import http.client
import io
import json
import re
import socket
import subprocess
import sys
import tempfile
import time
import zipfile
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Lock, Thread
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
path = "/.netlify/functions/relay"
remote = "/netlify-management/api/v1"
api_token = "offline-netlify-personal-token"
account = "fixture-team"
auth = {"x-9r-cli-token":"68977b6fd9422fbb"}
pools = "/api/proxy-pools"
deploy = pools + "/netlify-deploy"
backup = "/api/settings/database"
sources = {
    "relay.js":"netlify_relay_function.cjs",
    "relay.mjs":"cloudflare_relay_worker.mjs",
    "pinned-https.mjs":"pinned_relay_https.mjs",
}
code = {name:(root/"rust/app/src"/source).read_bytes() for name,source in sources.items()}
reference_zip = subprocess.check_output(["node","--input-type=module","-e", """
    import { readFileSync } from "node:fs";
    import { buildStoredZip } from "./src/lib/network/netlifyRelay.js";
    const files = JSON.parse(process.argv[1]);
    process.stdout.write(buildStoredZip(Object.entries(files).map(([name, path]) => ({
      name, data: readFileSync("rust/app/src/" + path),
    }))));
""",json.dumps(sources)],cwd=root,stderr=subprocess.DEVNULL)
digest = hashlib.sha256(reference_zip).hexdigest()
ledger = []
probes = []
captures = []
failures = []
sites = {"existing":{"id":"88a2bd5e-6e1a-4fe4-a799-aa6d9a6ab26e"}}
batches = {}
guard = Lock()


class Netlify(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self,status,body=None,headers=None):
        raw = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(raw)))
        for name,value in (headers or {}).items():
            self.send_header(name,value)
        self.end_headers()
        self.wfile.write(raw)

    def wire(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length","0")))
        url = urlsplit(self.path)
        try:
            if url.path == "/capture":
                captures.append(self.headers.get("Authorization"))
                return self.reply(200,{})
            if url.path.startswith("/netlify-relay/"):
                host = url.path.removeprefix("/netlify-relay/")
                name = host.removesuffix(".netlify.app")
                assert host == name+".netlify.app" and self.command == "GET"
                assert sites[name]["published"]
                assert self.headers.get_all("x-relay-auth") == [sites[name]["secret"]]
                assert self.headers["x-relay-target"] == "https://httpbin.org"
                assert self.headers["x-relay-path"] == "/get"
                assert self.headers.get("Authorization") is None
                probes.append(name)
                return self.reply(200,{"status":"ok"})
            assert url.path.startswith(remote) and self.headers.get_all("Authorization") == ["Bearer "+api_token]
            assert self.headers["User-Agent"] == "9Router"
            endpoint = url.path.removeprefix(remote)
            ledger.append((self.command,endpoint))
            if self.command == "POST" and endpoint == "/sites":
                name = json.loads(raw)["name"]
                if name == "redirect":
                    return self.reply(307,{},{"Location":"/capture"})
                if name == "unauthorized-pat":
                    return self.reply(403,{"message":"echo "+api_token})
                with guard:
                    if name in sites:
                        return self.reply(422,{"message":"taken"})
                    site_id = f"{len(sites):08x}-0000-4000-8000-000000000000"
                    sites[name] = {"id":site_id,"published":None}
                return self.reply(201,{"id":site_id,"account_id":account,"name":name,
                    "ssl_url":"https://"+name+".netlify.app/"})
            if self.command == "POST" and endpoint == "/accounts/"+account+"/env":
                site_id, = parse_qs(url.query)["site_id"]
                name, site = next((name,row) for name,row in sites.items() if row["id"] == site_id)
                secret, public = json.loads(raw)
                assert secret["key"] == "RELAY_AUTH" and secret["is_secret"] is True
                assert "scopes" not in secret and len(secret["values"]) == 1
                variable, = secret["values"]
                assert variable["context"] == "production" and re.fullmatch(r"[A-Za-z0-9_-]{43}",variable["value"])
                assert public == {"key":"RELAY_URL","is_secret":False,"values":[
                    {"context":"production","value":"https://"+name+".netlify.app"+path},
                ]}
                if name == "no-secrets-support":
                    return self.reply(403,{"message":"echo "+variable["value"]})
                site["secret"] = variable.pop("value")
                if name == "plaintext-ack":
                    secret["is_secret"] = False
                return self.reply(201,[secret,public])
            if self.command == "POST" and endpoint.startswith("/sites/") and endpoint.endswith("/deploys"):
                site_id = endpoint.split("/")[2]
                name, site = next((name,row) for name,row in sites.items() if row["id"] == site_id)
                assert site["secret"] and parse_qs(url.query) == {"production":["true"]}
                body = json.loads(raw)
                assert body["functions"] == {"relay":digest}, "Rust and pinned ZIP manifests differ"
                assert re.fullmatch(r"[a-f0-9]{40}",body["files"]["/index.html"])
                deployment = f"offline-deploy-{len(batches)}"
                batches[deployment] = {
                    "site":site_id,"name":name,"file":body["files"]["/index.html"],"uploads":[],"polls":0,
                }
                return self.reply(200,{
                    "id":deployment,"site_id":site_id,"state":"uploading",
                    "required":[] if name == "deduped" else [body["files"]["/index.html"]],
                    "required_functions":[] if name == "deduped" else (["unregistered-digest"] if name == "bad-digest" else [digest]),
                })
            if self.command == "PUT" and endpoint.startswith("/deploys/"):
                deployment = endpoint.split("/")[2]
                row = batches[deployment]
                assert self.headers["Content-Type"] == "application/octet-stream"
                if endpoint.endswith("/files/index.html"):
                    assert hashlib.sha1(raw).hexdigest() == row["file"], "Wrong SHA1 for index"
                    row["uploads"].append("file")
                    return self.reply(204)
                assert endpoint.endswith("/functions/relay") and parse_qs(url.query) == {"runtime":["js"]}
                assert raw == reference_zip and hashlib.sha256(raw).hexdigest() == digest, "Wrong function archive"
                with zipfile.ZipFile(io.BytesIO(raw)) as pack:
                    assert pack.namelist() == list(sources)
                    assert {name:pack.read(name) for name in pack.namelist()} == code
                    assert all(not key.get("secret", "unused-fixture-secret").encode() in raw for key in sites.values())
                row["uploads"].append("function")
                return self.reply(204)
            if self.command == "GET" and endpoint.count("/") == 4:
                site_id, deployment = endpoint.split("/")[2::2]
                row = batches[deployment]
                assert site_id == row["site"]
                assert row["uploads"] == ([] if row["name"] == "deduped" else ["file","function"])
                row["polls"] += 1
                if row["polls"] == 1:
                    return self.reply(200,{"id":deployment,"site_id":site_id,"state":"processing"})
                sites[row["name"]]["published"] = "other-deploy" if row["name"] == "misbound" else deployment
                return self.reply(200,{"id":deployment,"site_id":site_id,"state":"ready"})
            if self.command == "GET" and endpoint.startswith("/sites/"):
                site_id = endpoint.removeprefix("/sites/")
                name, site = next((name,row) for name,row in sites.items() if row["id"] == site_id)
                return self.reply(200,{"id":site_id,"name":name,"ssl_url":"https://"+name+".netlify.app",
                    "published_deploy":{"id":site["published"]}})
            raise AssertionError((self.command,endpoint))
        except (AssertionError,KeyError,StopIteration) as error:
            failures.append((self.command,url.path,str(error) or "unexpected wire"))
            self.reply(500,{})

    do_GET = do_POST = do_PUT = do_DELETE = wire


def request(port, url, method="GET", payload=None, headers=auth):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=12)) as client:
        if payload is not None:
            headers = headers | {"Content-Type":"application/json"}
        client.request(method,url,body=None if payload is None else json.dumps(payload),headers=headers)
        response = client.getresponse()
        return response.status,json.loads(response.read())


def public(value):
    raw = json.dumps(value)
    assert api_token not in raw and "_managedNetlify" not in raw
    assert not any(site.get("secret","unused-fixture-secret") in raw for site in sites.values())


with tempfile.TemporaryDirectory(prefix="9router-netlify-relay-") as directory:
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
    test = ThreadingHTTPServer(("127.0.0.1",52989),Netlify)
    Thread(target=test.serve_forever,daemon=True).start()
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
        create = lambda name: request(port,deploy,"POST",{"siteName":name,"netlifyToken":api_token})
        assert request(port,deploy,"POST",{},headers={})[0] == 401
        assert request(port,deploy,"POST",{}) == (400,{"error":"Netlify API token is required"})
        assert not ledger
        assert create("existing")[0] == 409
        assert sites["existing"] == {"id":"88a2bd5e-6e1a-4fe4-a799-aa6d9a6ab26e"}
        for name,state in (("redirect",502),("unauthorized-pat",403),("no-secrets-support",403),
                           ("plaintext-ack",502),("bad-digest",502),("misbound",502)):
            result = create(name)
            assert result[0] == state,(name,result,failures)
            public(result[1])
        forbidden = {"plaintext-ack","no-secrets-support"}
        assert all(row["name"] not in forbidden for row in batches.values())
        assert not captures and request(port,pools)[1] == {"proxyPools":[]}

        ledger.clear()
        status, deployed = create("  Managed! Relay ")
        assert status == 201,(status,deployed,failures)
        label = "managed-relay"
        assert deployed["deployUrl"] == "https://"+label+".netlify.app"+path
        assert deployed["proxyPool"]["type"] == "netlify"
        public(deployed)
        pool = pools + "/" + deployed["proxyPool"]["id"]
        published = sites[label]["published"]
        site_id = sites[label]["id"]
        assert ledger == [
            ("POST","/sites"),("POST","/accounts/"+account+"/env"),
            ("POST","/sites/"+site_id+"/deploys"),
            ("PUT","/deploys/"+published+"/files/index.html"),("PUT","/deploys/"+published+"/functions/relay"),
            ("GET","/sites/"+site_id+"/deploys/"+published),("GET","/sites/"+site_id+"/deploys/"+published),
            ("GET","/sites/"+site_id),
        ]
        assert request(port,pool,"PUT",{"proxyUrl":deployed["deployUrl"]+"/elsewhere"})[0] == 409
        public(request(port,pool,"PUT",{"name":"Protected Netlify relay"})[1])
        public(request(port,pools)[1])
        public(request(port,pool)[1])
        assert request(port,pool+"/test","POST")[1]["ok"] and probes == [label]
        status,saved = request(port,backup)
        assert status == 200 and api_token not in json.dumps(saved)
        record, = saved["proxyPools"]
        assert record["_managedNetlify"] == {"url":deployed["deployUrl"],"token":sites[label]["secret"]}
        assert request(port,backup,"POST",saved) == (200,{"success":True})
        assert request(port,pool+"/test","POST")[1]["ok"] and probes == [label,label]
        record["proxyUrl"] += "/not-authorized"
        assert request(port,backup,"POST",saved)[0] == 200
        assert request(port,pool+"/test","POST")[1]["status"] == 400 and probes == [label,label]
        assert create("deduped")[0] == 201 and batches[sites["deduped"]["published"]]["uploads"] == []
        with ThreadPoolExecutor(max_workers=2) as race:
            assert sorted(race.map(lambda _:create("one-name")[0],range(2))) == [201,409]
        assert not any(method == "DELETE" for method,_ in ledger)
        assert not failures,failures
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
        test.shutdown()
        test.server_close()

print("Netlify create-only deploy, production function secret, pinned ZIP parity and portable relay passed")
