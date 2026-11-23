import base64
import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from contextlib import closing
from email.parser import BytesParser
from email.policy import default
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread
from urllib.parse import urlsplit

root = Path(__file__).resolve().parents[2]
reference = sys.argv[1] == "reference"
binary = Path(sys.argv[2]).resolve() if not reference else None
api = "/v1/images/generations"
gateway = {"Authorization":"Bearer offline-chat-key"}
asset_base = "http://93.184.216.34:52991"
photo = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII="
png = base64.b64decode(photo)
uri = "data:image/png;base64," + photo
core_key = "offline-stability-image"
hf_key = "offline-hf-hosted"
custom_key = "offline-hf-self"
ledger = []
failures = []
assets = []
captures = []
jobs = {}


class ImageServer(BaseHTTPRequestHandler):
    def log_message(self,*_args):
        pass

    def reply(self,status,body,content_type="application/json",headers=None):
        raw = json.dumps(body).encode() if content_type == "application/json" else body
        self.send_response(status)
        self.send_header("Content-Type",content_type)
        self.send_header("Content-Length",str(len(raw)))
        for name,value in (headers or {}).items():
            self.send_header(name,value)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        try:
            if self.path == "/capture":
                captures.append(self.headers.get("Authorization"))
                return self.reply(200,{"caught":True})
            if self.path in ("/input.png","/generated.png") and self.server.server_port == 52991:
                assert self.headers.get("Authorization") is None
                assets.append(self.path)
                return self.reply(200,png,"image/png")
            url = urlsplit(self.path)
            assert not reference and url.query == "_subdomain=queue"
            assert self.headers.get_all("Authorization") == ["Bearer "+hf_key]
            label = "/hf-hosted/fal-ai/fal-ai/flux-kontext/requests/"
            assert url.path.startswith(label),url.path
            job, _, update = url.path.removeprefix(label).partition("/")
            assert job in jobs
            ledger.append(("queue",job,update))
            if update == "status":
                jobs[job] += 1
                return self.reply(200,{"status":"IN_PROGRESS"} if jobs[job] == 1 else {
                    "status":"COMPLETED","request_id":job,
                })
            assert not update and jobs[job] == 2
            return self.reply(200,{"images":[{"url":asset_base+"/generated.png"}]})
        except (AssertionError, KeyError) as error:
            failures.append((self.path,str(error) or "unexpected GET"))
            self.reply(500,{})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        try:
            if self.path == "/capture":
                captures.append(self.headers.get("Authorization"))
                return self.reply(200,{"caught":True})
            url = urlsplit(self.path)
            if url.path.startswith("/stability/"):
                branch = url.path.rsplit("/",1)[-1]
                assert self.headers.get_all("Authorization") == ["Bearer "+core_key]
                assert self.headers.get("Accept") == "application/json"
                if reference:
                    assert self.headers["Content-Type"] == "application/json"
                    values = json.loads(raw)
                else:
                    content_type = self.headers["Content-Type"]
                    assert content_type.startswith("multipart/form-data; boundary=")
                    parts = BytesParser(policy=default).parsebytes(
                        ("Content-Type: "+content_type+"\r\n\r\n").encode() + raw
                    )
                    assert parts.is_multipart()
                    values = {part.get_param("name",header="content-disposition"):
                              part.get_payload(decode=True).decode() for part in parts.iter_parts()}
                prompt = values["prompt"]
                expected = {
                    "core fixture":("core",{"prompt":prompt,"output_format":"png","aspect_ratio":"3:2","style_preset":"photographic"}),
                    "ultra fixture":("ultra",{"prompt":prompt,"output_format":"png","aspect_ratio":"1:1"}),
                    "large turbo fixture":("sd3",{"prompt":prompt,"output_format":"png",
                                   "aspect_ratio":"9:16","model":"sd3.5-large-turbo"}),
                    "redirect fixture":("core",{"prompt":prompt,"output_format":"png"}),
                    "redaction fixture":("ultra",{"prompt":prompt,"output_format":"png"}),
                }
                assert (branch,values) == expected[prompt],(branch,values)
                ledger.append(("stability",branch,prompt))
                if prompt == "redirect fixture":
                    return self.reply(307,{},headers={"Location":"/capture"})
                if prompt == "redaction fixture":
                    return self.reply(403,{"errors":["echo "+core_key]})
                return self.reply(200,{"image":photo})
            if url.path.startswith("/hf-self-hosted/"):
                assert self.headers.get_all("Authorization") == ["Bearer "+custom_key]
                payload = json.loads(raw)
                expected = {
                    "/hf-self-hosted/image/v2/namespace/model": {"inputs":"self-hosted text fixture"},
                    "/hf-self-hosted/image/v2/black-forest-labs/FLUX.1-Kontext-dev":{
                        "inputs":photo,"parameters":{"prompt":"self-hosted edit fixture"},
                    },
                }
                assert payload == expected[url.path],(url.path,payload)
                ledger.append(("hf-self-hosted",url.path))
                return self.reply(200,png,"image/png")
            assert url.path.startswith("/hf-hosted/fal-ai/fal-ai/")
            assert self.headers.get_all("Authorization") == ["Bearer "+hf_key]
            payload = json.loads(raw)
            if url.path.endswith("/flux/dev"):
                name = "inputs" if reference else "prompt"
                assert payload == {name:"hosted text fixture"} and not url.query
                ledger.append(("hf-hosted","text"))
                if reference:
                    return self.reply(200,png,"image/png")
                return self.reply(200,{"images":[{"url":asset_base+"/generated.png"}]})
            assert url.path.endswith("/flux-kontext/dev"),url.path
            if reference:
                assert payload["inputs"] == photo and not url.query
                assert payload == {"inputs":photo,"parameters":{"prompt":payload["parameters"]["prompt"]}}
                ledger.append(("hf-hosted","edit"))
                return self.reply(200,png,"image/png")
            assert url.query == "_subdomain=queue" and payload == {
                "prompt":payload["prompt"],"image_url":uri,"image_urls":[uri],
            }
            job = "fixture-image-" + str(len(jobs))
            jobs[job] = 0
            ledger.append(("hf-hosted","edit"))
            callback = "https://queue.fal.run/fal-ai/flux-kontext/requests/" + job
            if payload["prompt"] == "foreign queue fixture":
                callback = "http://127.0.0.1:52989/capture"
            return self.reply(200,{"status":"IN_QUEUE","request_id":job,"response_url":callback})
        except (AssertionError, KeyError, ValueError) as error:
            failures.append((self.path,str(error) or "unexpected POST"))
            self.reply(500,{})


def req(port,path,body=None,headers=gateway):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=10)) as client:
        client.request("POST" if body is not None else "GET",path,
            body=None if body is None else json.dumps(body),
            headers=headers | ({"Content-Type":"application/json"} if body is not None else {}))
        response = client.getresponse()
        return response.status,response.getheader("Content-Type","").split(";")[0],response.read()


def seed(folder):
    subprocess.run([sys.executable,"rust/reference/seed.py",str(folder)],cwd=root,check=True,
                   stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    with sqlite3.connect(folder / "db/data.sqlite") as db:
        db.execute("UPDATE providerConnections SET isActive=0")
        for identifier,provider,key,details,priority in [
            ("stability-fixture","stability-ai",core_key,{},1),
            ("hf-hosted-fixture","huggingface",hf_key,{},1),
            ("hf-self-fixture","huggingface",custom_key,{"baseUrl":"http://local-hf.fixture:8080/image/v2/"},2),
        ]:
            db.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,?,'apikey','Image fixture',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                (identifier,provider,priority,json.dumps({"apiKey":key,"providerSpecificData":details})))


with tempfile.TemporaryDirectory(prefix="9router-hosted-images-") as directory:
    folder = Path(directory)
    data, home, bin_dir = (folder / name for name in ("data","home","bin"))
    home.mkdir()
    bin_dir.mkdir()
    seed(data)
    vendor = ThreadingHTTPServer(("127.0.0.1",52989),ImageServer)
    source = ThreadingHTTPServer(("93.184.216.34",52991),ImageServer)
    for server in (vendor,source):
        Thread(target=server.serve_forever,daemon=True).start()
    with socket.socket() as available:
        available.bind(("127.0.0.1",0))
        port = available.getsockname()[1]
    args = ["node","--import","./rust/reference/mock-hosted-images.mjs",".next/standalone/custom-server.js"] if reference else [
        str(binary),"--no-browser",
    ]
    proc = subprocess.Popen(args,cwd=root,env={
        "PATH":os.environ["PATH"] if reference else str(bin_dir),"HOME":str(home),"DATA_DIR":str(data),
        "PORT":str(port),"HOST":"127.0.0.1","HOSTNAME":"127.0.0.1","MODEL_CATALOG_SYNC":"off",
        "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(folder/"fallbacks"),
    },stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        until = time.monotonic()+10
        while proc.poll() is None and time.monotonic() < until:
            try:
                if req(port,"/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(0.05)
        else:
            raise AssertionError("Fixture gateway did not start")

        def image(model,prompt,**options):
            custom = options.pop("custom",False)
            binary = options.pop("binary",False)
            headers = gateway | ({"x-connection-id":"hf-self-fixture"} if custom else {})
            path = api + ("?response_format=binary" if binary else "")
            return req(port,path,{"model":model,"prompt":prompt,**options},headers)

        def result(reply):
            status,mime,raw = reply
            assert status == 200 and mime == "application/json",(status,mime,raw,failures)
            content = json.loads(raw)
            assert content["data"] == [{"b64_json":photo}] and isinstance(content["created"],int)
            return content["data"]

        core = "stability/stable-image-core"
        ultra = "stability/stable-image-ultra"
        turbo = "stability/sd3.5-large-turbo"
        text = "hf/black-forest-labs/FLUX.1-dev"
        edit = "hf/black-forest-labs/FLUX.1-Kontext-dev"
        snap = {}
        snap["core"] = result(image(core,"core fixture",size="1536x1024",style="photographic"))
        snap["ultra"] = result(image(ultra,"ultra fixture",size="odd",output_format="PNG"))
        status,mime,raw = image(turbo,"large turbo fixture",size="1024x1792",binary=True)
        assert (status,mime,raw) == (200,"image/png",png),(status,mime,raw,failures)
        snap["sd35 binary"] = [mime,base64.b64encode(raw).decode()]
        snap["hosted text"] = result(image(text,"hosted text fixture"))
        snap["hosted data-uri edit"] = result(image(edit,"inline edit fixture",image=uri))
        snap["hosted remote edit"] = result(image(edit,"remote edit fixture",image=asset_base+"/input.png"))
        snap["self-hosted text"] = result(image("hf/namespace/model","self-hosted text fixture",custom=True))
        snap["self-hosted edit"] = result(image(edit,"self-hosted edit fixture",custom=True,images=[photo]))
        assert assets.count("/input.png") == 1 and not failures,failures
        if not reference:
            assert assets.count("/generated.png") == 3
            assert sum(call[:1] == ("queue",) for call in ledger) == 6
            before = list(ledger)
            assert image(edit,"rejected source fixture",image="http://127.0.0.1:52989/capture")[0] == 400
            assert image("hf/org/%2e%2e/model","traversal fixture",custom=True)[0] == 400
            with sqlite3.connect(data/"db/data.sqlite") as db:
                db.execute("""UPDATE providerConnections SET
                    data=json_set(data,'$.providerSpecificData.baseUrl','http://ignored@local-hf.fixture:8080/image/v2')
                    WHERE id='hf-self-fixture'""")
            assert image("hf/namespace/other","unapproved host fixture",custom=True)[0] == 400
            assert ledger == before and not captures
            with sqlite3.connect(data/"db/data.sqlite") as db:
                db.execute("UPDATE providerConnections SET isActive=0 WHERE id='hf-self-fixture'")
            status,_,raw = image(edit,"foreign queue fixture",image=uri)
            assert status == 502 and b"Invalid HuggingFace image job" in raw and not captures,(status,raw)
            status,_,raw = image(core,"redirect fixture")
            assert status == 502 and b"redirect" in raw and not captures
            status,_,raw = image(ultra,"redaction fixture")
            assert status == 403 and core_key.encode() not in raw and b"[redacted]" in raw
            log = folder/"fallbacks"
            assert not log.exists() or log.read_text() == ""
        assert not failures,failures
        print(json.dumps(snap,sort_keys=True))
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
                raise
        for server in (vendor,source):
            server.shutdown()
            server.server_close()
