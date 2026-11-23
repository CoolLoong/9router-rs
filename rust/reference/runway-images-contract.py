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
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread
from urllib.parse import quote
from uuid import UUID

root = Path(__file__).resolve().parents[2]
reference = sys.argv[1] == "reference"
executable = Path(sys.argv[2]).resolve() if not reference else None
api = "/v1/images/generations"
gateway = {"Authorization":"Bearer offline-chat-key"}
primary = "offline-runway-primary"
backup = "offline-runway-backup"
key = lambda headers: headers["Authorization"].removeprefix("Bearer ")
prefix = "/runway/v1/"
image_model = "runway/gen4_image"
turbo_image = "runway/gen4_image_turbo"
turbo_video = "runway/gen4_turbo"
uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJGQAAAAASUVORK5CYII="
uploaded = "runway://fixture/uploads/photo.png"
asset = "https://runway-fixture-assets.com/media/"
jobs = {}
ledger = []
captures = []
faults = []


class Runway(BaseHTTPRequestHandler):
    def log_message(self,*_args):
        pass

    def reply(self,status,body,headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(raw)))
        for field,value in (headers or {}).items():
            self.send_header(field,value)
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        try:
            if self.path == "/capture":
                captures.append(self.headers.get("Authorization"))
                return self.reply(200,{})
            assert self.headers["X-Runway-Version"] == "2024-11-06"
            account = key(self.headers)
            assert account in (primary,backup)
            assert self.headers["Content-Type"] == "application/json"
            assert self.path in [prefix+"text_to_image",prefix+"image_to_video"]
            prompt = body["promptText"]
            ledger.append(("submit",prompt,account))
            if prompt == "landscape":
                assert body == {"model":"gen4_image","promptText":prompt,"ratio":"16:9" if reference else "1920:1080"}
            elif prompt == "portrait reference":
                assert body == {"model":"gen4_image_turbo","promptText":prompt,
                    "ratio":"2:3" if reference else "1080:1440","referenceImages":[{"uri":uri}]}
            elif prompt == "portrait video":
                assert self.path == prefix+"image_to_video"
                assert body == {"model":"gen4_turbo","promptText":prompt,
                    "ratio":"9:16" if reference else "720:1280","duration":5,"promptImage":uploaded}
            elif prompt == "pixel size":
                assert body == {"model":"gen4_image","promptText":prompt,
                    "ratio":"1:1" if reference else "1280:720"}
            elif prompt == "pre-submit rejected" and account == primary:
                return self.reply(401,{"error":"Auth rejected "+account})
            elif prompt == "ambiguous submit":
                return self.reply(500,{"error":"Ambiguous "+account})
            elif prompt == "accepted error":
                return self.reply(400,{"id":str(UUID(int=9998)),"error":"Reported task "+account})
            elif prompt == "submit redirect":
                return self.reply(307,{},{"Location":"/capture"})
            elif prompt == "invalid task":
                return self.reply(200,{"id":"../invalid?next=/capture"})
            job = str(UUID(int=len(jobs)+1))
            jobs[job] = {"prompt":prompt,"account":account,"polls":0}
            return self.reply(200,{"id":job})
        except (AssertionError,KeyError) as error:
            faults.append((self.path,str(error) or "unexpected submit"))
            return self.reply(500,{})

    def do_GET(self):
        try:
            if self.path == "/capture":
                captures.append(self.headers.get("Authorization"))
                return self.reply(200,{})
            assert self.path.startswith(prefix+"tasks/")
            assert self.headers["X-Runway-Version"] == "2024-11-06"
            job = self.path.removeprefix(prefix+"tasks/")
            state = jobs[job]
            assert state["account"] == key(self.headers)
            prompt = state["prompt"]
            state["polls"] += 1
            ledger.append(("poll",prompt,state["account"]))
            response = {"id":job}
            if prompt == "task timeout":
                return self.reply(200,response | {"status":"THROTTLED"})
            if prompt == "different task":
                return self.reply(200,response | {"id":str(UUID(int=9999)),"status":"SUCCEEDED","output":[asset+"x.png"]})
            if prompt == "poll redirect":
                return self.reply(307,{},{"Location":"/capture"})
            if prompt == "failed task":
                encoded = quote(state["account"],safe="").replace("-", "%2D")
                return self.reply(200,response | {"status":"FAILED","failure":"failed "+encoded})
            if prompt == "private output":
                return self.reply(200,response | {"status":"SUCCEEDED","output":["https://127.0.0.1/capture"]})
            if prompt == "credential output":
                encoded = quote(state["account"],safe="").replace("-", "%2D")
                return self.reply(200,response | {"status":"SUCCEEDED","output":[asset+"private.png?q="+encoded]})
            if prompt == "landscape":
                statuses = ["PENDING"] if reference else ["PENDING","THROTTLED","RUNNING"]
                if state["polls"] <= len(statuses):
                    return self.reply(200,response | {"status":statuses[state["polls"]-1]})
            outputs = {
                "landscape":[asset+"first.png?signature=offline",asset+"second.png"],
                "portrait video":[asset+"generated.mp4"],
            }.get(prompt,[asset+"one.png"])
            return self.reply(200,response | {"status":"SUCCEEDED","output":outputs})
        except (AssertionError,KeyError) as error:
            faults.append((self.path,str(error) or "unexpected poll"))
            return self.reply(500,{})


def request(port,path,body=None):
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=16)) as conn:
        headers = gateway if body is None else gateway | {"Content-Type":"application/json"}
        conn.request("GET" if body is None else "POST",path,
                     headers=headers,body=None if body is None else json.dumps(body))
        response = conn.getresponse()
        return response.status,json.loads(response.read())


def prepare(data):
    subprocess.run([sys.executable,"rust/reference/seed.py",str(data)],cwd=root,check=True,
                   stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    with sqlite3.connect(data/"db/data.sqlite") as db:
        db.execute("UPDATE providerConnections SET isActive=0")
        for number,secret in enumerate([primary,backup],1):
            db.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'runwayml','apikey','Runway fixture',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                (f"fixture-runway-{number}",number,json.dumps({"apiKey":secret})))
        db.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES ('runway-queued-stop','runway-queued-stop','image',?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            (json.dumps([image_model,turbo_image]),))


with tempfile.TemporaryDirectory(prefix="9router-runway-images-") as directory:
    parent = Path(directory)
    home, data, bin_dir = (parent/name for name in ("home","data","commands"))
    home.mkdir()
    bin_dir.mkdir()
    prepare(data)
    upstream = ThreadingHTTPServer(("127.0.0.1",52989),Runway)
    Thread(target=upstream.serve_forever,daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    command = ["node","--import","./rust/reference/mock-runway-images.mjs",".next/standalone/custom-server.js"] if reference else [
        str(executable),"--no-browser",
    ]
    process = subprocess.Popen(command,cwd=root,env={
        "PATH":os.environ["PATH"] if reference else str(bin_dir),"HOME":str(home),
        "DATA_DIR":str(data),"PORT":str(port),"HOST":"127.0.0.1","HOSTNAME":"127.0.0.1",
        "MODEL_CATALOG_SYNC":"off","NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(parent/"fallbacks"),
    },stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic()+10
        while process.poll() is None and time.monotonic() < deadline:
            try:
                if request(port,"/api/health")[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(0.05)
        else:
            raise AssertionError("Fixture gateway did not start")

        def generate(model,prompt,**options):
            body = {"model":model,"prompt":prompt} | options
            return request(port,api,body)

        positive = [
            (image_model,"landscape",{"size":"1792x1024"},[asset+"first.png?signature=offline",asset+"second.png"]),
            (turbo_image,"portrait reference",{"size":"1024x1536","image":uri},[asset+"one.png"]),
            (turbo_video,"portrait video",{"size":"1024x1792","image":uploaded},[asset+"generated.mp4"]),
            (image_model,"pixel size",{"size":"1280:720"},[asset+"one.png"]),
        ]
        summary = {}
        for model,prompt,inputs,outputs in positive:
            status,result = generate(model,prompt,**inputs)
            assert status == 200 and isinstance(result["created"],int),(status,result,faults)
            assert result["data"] == [{"url":url} for url in outputs]
            summary[prompt] = result["data"]
        if not reference:
            before = list(ledger)
            assert generate(turbo_image,"missing photo")[0] == 400
            assert generate(turbo_video,"missing start")[0] == 400
            assert generate("runway/gen3a_turbo","expired model",image=uri)[0] == 400
            assert generate(image_model,"private source",image="https://127.0.0.1/capture")[0] == 400
            assert generate(image_model,"unsupported URI",image="file:///tmp/offline-image.png")[0] == 400
            assert ledger == before
            for prompt,expected,text in [
                ("invalid task",502,"invalid task ID"),
                ("different task",502,"different task"),
                ("submit redirect",502,"redirect refused"),
                ("poll redirect",502,"redirect refused"),
                ("failed task",502,"[redacted]"),
                ("private output",502,"invalid media URL"),
                ("credential output",502,"invalid media URL"),
                ("task timeout",504,"polling timeout"),
            ]:
                start = time.monotonic()
                status,result = generate(image_model,prompt)
                raw = json.dumps(result)
                assert status == expected and text in raw and primary not in raw and backup not in raw, (status,raw)
                assert [account for stage,p,account in ledger if p == prompt and stage == "submit"] == [primary]
                if prompt == "task timeout":
                    assert time.monotonic()-start < 3
            status,result = generate("runway-queued-stop","ambiguous submit",image=uri)
            raw = json.dumps(result)
            assert status == 500 and "[redacted]" in raw and primary not in raw and backup not in raw, (status,raw)
            assert [account for stage,p,account in ledger if p == "ambiguous submit" and stage == "submit"] == [primary]
            status,result = generate("runway-queued-stop","accepted error",image=uri)
            raw = json.dumps(result)
            assert status == 400 and "[redacted]" in raw and primary not in raw and backup not in raw,(status,raw)
            assert [account for stage,p,account in ledger if p == "accepted error" and stage == "submit"] == [primary]
            status,result = generate(image_model,"pre-submit rejected")
            assert status == 200 and result["data"] == [{"url":asset+"one.png"}],(status,result)
            assert [account for stage,p,account in ledger if p == "pre-submit rejected" and stage == "submit"] == [primary,backup]
            log = parent/"fallbacks"
            assert not log.exists() or log.read_text() == ""
        assert not captures and not faults,(captures,faults)
        print(json.dumps(summary,sort_keys=True))
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise
        upstream.shutdown()
        upstream.server_close()
