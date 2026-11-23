import http.client
import json
import os
import pathlib
import select
import shutil
import stat
import subprocess
import sys
import tempfile
import time

MODE,*ARGS=sys.argv[1:]
ROOT=pathlib.Path(__file__).resolve().parent
if MODE in {"native","strict"}:
    binary,CASE=ARGS
    COMMAND=[binary]
else:
    CASE,=ARGS
    COMMAND=[shutil.which("node"),"--no-warnings",str(ROOT/"original.mjs")]
SAVED="fixture-legacy-key"
CURRENT="fixture-selected-key"
OLD_ROUTER="fixture-previous-key"

def initial():
    return {"sticky":"original value","customModels":[
        {"id":"vendor-original","model":"private-model","index":8,"apiKey":SAVED},
        {"id":"custom:9Router-old","model":"previous gateway","index":9,"apiKey":OLD_ROUTER},
    ]}

def redact(value, home):
    if isinstance(value,list):
        return [redact(item,home) for item in value]
    if isinstance(value,str):
        return value.replace(str(home),"<home>")
    if not isinstance(value,dict):
        return value
    result={}
    for key,item in value.items():
        if key=="apiKey" and isinstance(item,str):
            result[key]={"legacy":item==SAVED,"selected":item==CURRENT,"previous":item==OLD_ROUTER,"placeholder":item=="your_api_key"}
            assert any(result[key].values())
        else:
            result[key]=redact(item,home)
    return result

def request(port,method,payload=None):
    connection=http.client.HTTPConnection("127.0.0.1",port,timeout=3)
    headers={"Content-Type":"application/json"}
    if CASE != "unauthorized": headers["x-replay-admin"]="fixture"
    try:
        encoded=json.dumps(payload) if isinstance(payload,dict) else payload
        connection.request(method,"/api/cli-tools/droid-settings",body=encoded,headers=headers)
        response=connection.getresponse()
        return {"status":response.status,"body":json.loads(response.read())}
    finally:
        connection.close()

with tempfile.TemporaryDirectory(prefix="9router-droid-settings-") as directory:
    home=pathlib.Path(directory)
    binary=home/"bin";binary.mkdir()
    file=home/".factory/settings.json"
    command=file
    if CASE=="installed-command":
        fake=binary/"droid";fake.write_text("#!/bin/sh\nexit 0\n");fake.chmod(0o700)
    if CASE in {"get-registered","get-jsonc","get-bad","apply-many","apply-no-default","apply-skip-invalid",
                "apply-bad-file","remove-keep","remove-empty","remove-malformed","symlink","unauthorized"}:
        file.parent.mkdir()
        if CASE=="symlink":
            command=home/"synthetic-configuration.json"
            file.symlink_to(command)
        if CASE in {"get-bad","remove-malformed","apply-bad-file"}:
            command.write_text("{ invalid settings")
        elif CASE=="get-jsonc":
            command.write_text('{"customModels":[{"id":"custom:9Router-test","model":"test",},],}')
        elif CASE=="remove-empty":
            command.write_text('{"theme":"plain","customModels":[{"id":"custom:9Router-2","model":"old"}]}')
        else:
            command.write_text(json.dumps(initial()))
        command.chmod(0o640)
    was_present=file.exists()
    environment=dict(os.environ,HOME=directory,DATA_DIR=directory,PATH=f"{binary}:/usr/bin:/bin")
    with subprocess.Popen(COMMAND,cwd=home,env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as process:
        try:
            timeout=time.monotonic()+5
            buf=bytearray();line=None
            while time.monotonic()<timeout:
                if not select.select([process.stdout],[],[],1)[0]: continue
                chunk=os.read(process.stdout.fileno(),4096)
                if not chunk: break
                buf.extend(chunk)
                line=next((line for line in buf.split(b"\n")[:-1] if line.startswith(b"ready ")),None)
                if line: break
            assert line,"fixture launcher did not become ready"
            port=int(line.split()[1])
            if CASE.startswith("apply-") or CASE in {"symlink","new-mode"}:
                payload={"baseUrl":"https://example.invalid/relay","model":"first"}
                if CASE=="apply-many":
                    payload.update(models=["first","second"],activeModel="second",apiKey=CURRENT)
                    payload["baseUrl"] += "/v1"
                if CASE=="apply-no-default":
                    payload.update(models=["first","second"],activeModel="",apiKey=CURRENT)
                if CASE=="apply-skip-invalid":
                    payload.update(models=["",200,"third"],activeModel="third")
                if CASE=="apply-missing":
                    payload={"baseUrl":"https://example.invalid","models":[]}
                if CASE=="apply-bad-body":
                    payload='{"broken"'
                reply=request(port,"POST",payload)
            elif CASE.startswith("remove-"):
                reply=request(port,"DELETE")
            else:
                reply=request(port,"GET")
            snapshot=None
            if file.exists():
                raw=file.read_text()
                try: snapshot=json.loads(raw)
                except json.JSONDecodeError: snapshot=raw
            mode=oct(stat.S_IMODE(file.stat().st_mode)) if was_present else None
            if CASE=="new-mode":
                assert file.stat().st_mode & 0o777 == 0o600
            print(json.dumps({"reply":redact(reply,home),"disk":redact(snapshot,home),
                              "mode":mode,"symlink":file.is_symlink()},sort_keys=True))
        finally:
            if process.poll() is None: process.terminate()
            try: process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill();process.communicate(timeout=3)
