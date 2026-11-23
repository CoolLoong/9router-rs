from http.client import HTTPConnection
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]


def request(port, method, path, headers, data=None):
    connection = HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, None if data is None else json.dumps(data), headers)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-codex-compact-") as work:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {**os.environ, "PORT":str(port),"DATA_DIR":work,"NINEROUTER_MIGRATION_DATA_DIR":work,
           "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(Path(work,"fallback"))}
    headers = {"content-type":"application/json","x-session-id":"fixture-codex-session",
               "user-agent":"codex-cli/fixture","authorization":"Bearer offline-chat-key"}
    with subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) as app:
        try:
            for _ in range(140):
                if app.poll() is not None:
                    raise RuntimeError("fixture process exited")
                try:
                    if request(port,"GET","/api/health",{})[0] == 200:
                        break
                except OSError:
                    time.sleep(.04)
            else:
                raise RuntimeError("fixture process did not become ready")
            assert request(port,"PUT","/api/providers/fixture-codex-primary",
                           {**headers,"x-9r-cli-token":"68977b6fd9422fbb"},{"isActive":True})[0] == 200
            values = []
            for path in ["/v1/responses/compact", "/v1/responses"]:
                status, raw = request(port,"POST",path,headers,{
                    "model":"cx/gpt-5.6-sol","input":"fixture Codex compact transport check","stream":False,
                })
                assert status==200, status
                values.append(json.loads(raw)["output"][0]["content"][0]["text"])
            if server == "original":
                assert values == ["regular endpoint","compact endpoint"], values
            else:
                assert values == ["compact endpoint","regular endpoint"], values
                assert "/v1/responses" not in Path(work,"fallback").read_text()
            print(f"{server}: {', '.join(values)}")
        finally:
            app.terminate()
