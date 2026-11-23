import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from contextlib import closing
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve(strict=True)
admin = {"x-9r-cli-token":"68977b6fd9422fbb"}
gateway = {"Authorization":"Bearer offline-active-key", "Cookie":"synthetic=do-not-forward", "X-Gateway-Secret":"offline-internal"}
endpoint = "/v1/images/generations"
model = "fal-ai/fal-ai/flux/schnell"
backup = "fal-ai/fal-ai/flux/dev"


def call(port, path, method="GET", body=None, headers=None):
    headers = {} if headers is None else headers
    if body is not None:
        headers = {**headers,"Content-Type":"application/json"}
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request(method,path,None if body is None else json.dumps(body),headers)
        response = client.getresponse()
        return response.status,json.loads(response.read())


with tempfile.TemporaryDirectory(prefix="9router-fal-contract-") as directory:
    fallback = Path(directory) / "fallbacks"
    with socket.socket() as address:
        address.bind(("127.0.0.1",0))
        port = address.getsockname()[1]
    process = subprocess.Popen([sys.executable,str(root / "rust/reference/standalone-replay.py"),str(binary)],
                               stdin=subprocess.DEVNULL,env={**os.environ,"PORT":str(port),
                               "RUST_ANYTHING_FALLBACK_LOG":str(fallback)})
    try:
        deadline = time.monotonic() + 10
        while True:
            assert process.poll() is None and time.monotonic() < deadline
            try:
                if call(port,"/api/health")[0] == 200:
                    break
            except OSError:
                time.sleep(.05)
        for name in ["fixture-fal-image","fixture-fal-backup"]:
            assert call(port,f"/api/providers/{name}","PUT",{"isActive":True},admin)[0] == 200
        combo = "fixture-fal-queue-combo"
        code, response = call(port,"/api/combos","POST",{"name":combo,"kind":"image","models":[model,backup]},admin)
        assert code == 201, response

        def run(prompt, selected=model):
            length = len(call(52989,"/fal-image/ledger")[1])
            code, result = call(port,endpoint,"POST",{
                "model":selected,"prompt":prompt,"n":2,"size":"1024x1536",
            },gateway)
            return code,result,call(52989,"/fal-image/ledger")[1][length:]

        code, result, ledger = run("Fal foreign job",combo)
        assert code == 502 and "outside the submitted request" in result["error"]["message"], result
        assert [(record["method"],record["account"]) for record in ledger] == [("POST","primary")], ledger

        code, result, ledger = run("Fal status redirect",combo)
        assert code == 502 and "Fal status 307" in result["error"]["message"], result
        assert [(record["method"],record["account"]) for record in ledger] == [("POST","primary"),("GET","primary")], ledger

        code, result, ledger = run("Fal accepted empty",combo)
        assert code == 200 and result["data"] == [], result
        assert [record["method"] for record in ledger] == ["POST","GET","GET","GET"], ledger

        code, result, ledger = run("Fal reject before submit")
        assert code == 200 and result["data"] == [{"url":"https://image.example.invalid/fal-finished.png"}], result
        posts = [item for item in ledger if item["method"] == "POST"]
        assert [item["account"] for item in posts] == ["primary","backup"], ledger
        assert all(item["body"]["image_size"] == {"width":1024,"height":1536} for item in posts), posts
        assert all(item["account"] == "backup" for item in ledger if item["method"] == "GET"), ledger
        assert fallback.read_text() == ""
        print("Fal queue accepted once, rejected foreign and redirected URLs, and retried only the explicit credential rejection")
    finally:
        process.terminate()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
