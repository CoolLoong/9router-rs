import contextlib
import http.client
import http.server
import json
import os
import pathlib
import sqlite3
import socket
import subprocess
import sys
import tempfile
import threading
import time

SCRIPT=pathlib.Path(__file__).resolve()
if sys.argv[1]!="--isolated":
    os.execvp("unshare",["unshare","-Urn","--","sh","-c",'ip link set lo up && exec "$@"',
                         "sh",sys.executable,str(SCRIPT),"--isolated",*sys.argv[1:]])
MODE,*ARGV=sys.argv[2:]
ROOT=SCRIPT.parent
if MODE in {"native","strict"}:
    binary,CASE=ARGV
    binary=pathlib.Path(binary).resolve()
else:
    CASE,=ARGV
    COMMAND=["node","--no-warnings",str(ROOT/"original.mjs")]

ACCESS="fixture-saved-github-access"
COPILOT="fixture-saved-copilot-access"
FRESH="fixture-refreshed-copilot-access"
API="fixture-saved-api-key"
REFRESH="fixture-saved-github-refresh"
CALLS=[]
FAULTS=[]
ITEM={
    "mode":"management" if CASE.startswith("management") else "live",
    "account":{"id":"fixture","provider":"github","authType":"oauth","name":"Fixture","priority":1,"isActive":True,
        "createdAt":"2026-11-19T00:00:00.000Z","updatedAt":"2026-11-19T00:00:00.000Z",
        "accessToken":ACCESS,"apiKey":API,"refreshToken":REFRESH,
        "providerSpecificData":{"copilotToken":COPILOT,"copilotTokenExpiresAt":4102444800,"other":"unchanged"}},
}
if CASE in {"management-key","management-no-token"}:
    ITEM["account"].pop("accessToken")
    ITEM["account"]["providerSpecificData"].pop("copilotToken")
if CASE in {"management-no-token","live-no-token"}:
    ITEM["account"].pop("apiKey")
    ITEM["account"].pop("accessToken",None)
    ITEM["account"]["providerSpecificData"].pop("copilotToken",None)
if CASE=="live-access":
    ITEM["account"]["providerSpecificData"].pop("copilotToken")
if CASE in {"live-cache","live-force","live-empty","live-empty-policy","live-reauth"}:
    ITEM["turns"]=2
if CASE=="live-force":
    ITEM["force"]=True
if CASE=="live-proactive":
    ITEM["account"]["providerSpecificData"]["copilotTokenExpiresAt"]=1
if CASE=="live-proxy":
    ITEM["account"]["providerSpecificData"]["proxyPoolId"]="fixture-proxy"

RAW={"data":[
    {"id":"fixture-gpt","name":"Fixture GPT","capabilities":{"type":"chat","supports":{"tool_calls":True}},"model_picker_enabled":True,"version":"2"},
    {"id":"fixture-strict-chat","name":"", "capabilities":{"type":"chat"},"policy":{"state":"enabled"}},
    {"id":"fixture-preview","name":"Preview","capabilities":{"type":"chat"},"policy":{"state":"preview"}},
    {"id":"fixture-locked","name":"Disabled","capabilities":{"type":"chat"},"policy":{"state":"disabled"}},
    {"id":"fixture-embed","name":"Embeddings","capabilities":{"type":"embeddings"}},
    {"id":"fixture-gpt","name":"Duplicate","capabilities":{"type":"chat"}},
    {"id":"fixture-missing-type"},
]}

class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self,*_args): pass

    def send(self,status,data):
        raw=json.dumps(data).encode() if isinstance(data,dict) else data
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(raw)))
        self.end_headers(); self.wfile.write(raw)

    def do_GET(self):
        try:
            self.get()
        except (AssertionError, KeyError) as fault:
            FAULTS.append(str(fault) or f"Unexpected Copilot request to {self.path}")
            self.send(500,{"error":"Invalid model discovery fixture request"})

    def get(self):
        auth=self.headers.get("Authorization")
        native=MODE in {"native","strict"}
        refresh_endpoint="/github/copilot_internal/v2/token" if native else "/copilot"
        catalog_endpoint="/github/models" if native else "/models"
        if self.path==refresh_endpoint:
            assert auth=="token "+ACCESS
            assert self.headers["User-Agent"]=="GitHubCopilotChat/0.38.0"
            CALLS.append({"path":"/copilot","credential":"github-access"})
            if CASE=="live-refresh-fail":
                return self.send(403,{"error":"No mock token"})
            return self.send(200,{"token":FRESH,"expires_at":4102444800})
        assert self.path==catalog_endpoint
        assert self.headers["Copilot-Integration-Id"]=="vscode-chat"
        assert self.headers["Content-Type"]=="application/json"
        keys={"Bearer "+COPILOT:"copilot","Bearer "+ACCESS:"github-access","Bearer "+API:"api-key","Bearer "+FRESH:"rotated"}
        if ITEM["mode"]=="management":
            assert self.headers["Editor-Version"]=="vscode/1.107.1"
            assert self.headers["Editor-Plugin-Version"]=="copilot-chat/0.26.7"
            assert self.headers["User-Agent"]=="GitHubCopilotChat/0.26.7"
            assert self.headers.get("X-GitHub-Api-Version") is None
        else:
            assert self.headers["Editor-Version"]=="vscode/1.110.0"
            assert self.headers["Editor-Plugin-Version"]=="copilot-chat/0.38.0"
            assert self.headers["User-Agent"]=="GitHubCopilotChat/0.38.0"
            assert self.headers["X-GitHub-Api-Version"]=="2025-04-01"
        CALLS.append({"path":"/models","credential":keys[auth]})
        if CASE=="management-status":
            return self.send(429,{"error":"Fixture request limit"})
        if CASE=="management-unauthorized":
            return self.send(401,{"error":"Fixture authorization failed"})
        if CASE in {"live-reauth","live-retry-fail","live-refresh-fail"} and (auth!="Bearer "+FRESH or CASE=="live-retry-fail"):
            return self.send(403,{"error":"Token unavailable"})
        if CASE in {"live-failed","live-no-policy"}:
            if CASE=="live-failed": return self.send(503,{"error":"Model catalog unavailable"})
        if CASE in {"live-empty","management-empty"}:
            return self.send(200,{"data":[]})
        if CASE=="live-empty-policy":
            return self.send(200,{"data":[{"id":"bad","policy":{"state":"pending"},"capabilities":{"type":"chat"}}]})
        if CASE in {"management-malformed","live-malformed"}:
            return self.send(200,{"data":{"unexpected":True}})
        if CASE in {"management-invalid-json","live-invalid-json"}:
            return self.send(200,b"not JSON")
        return self.send(200,RAW)

    def do_POST(self):
        CALLS.append({"path":self.path,"unexpected":True})
        self.send(404,{"error":"No model discovery fixture accepts writes"})

def request(port,path,body=None,cookie=None):
    with contextlib.closing(http.client.HTTPConnection("127.0.0.1",port,timeout=12)) as conn:
        headers={"Cookie":cookie} if cookie else {}
        if body is not None:
            headers["Content-Type"]="application/json"
        conn.request("GET" if body is None else "POST",path,
                     body=None if body is None else json.dumps(body),headers=headers)
        response=conn.getresponse()
        return response.status,json.loads(response.read()),response.headers


@contextlib.contextmanager
def gateway(root):
    with socket.socket() as socket_port:
        socket_port.bind(("127.0.0.1",0))
        port=socket_port.getsockname()[1]
    commands=root/"commands"
    commands.mkdir(exist_ok=True)
    env={
        "HOME":str(root),"PATH":str(commands),"DATA_DIR":str(root),"PORT":str(port),
        "HOST":"127.0.0.1","HOSTNAME":"127.0.0.1","MODEL_CATALOG_SYNC":"off",
        "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(root/"fallbacks"),
    }
    log=root/"gateway.stderr"
    with log.open("wb") as stderr, subprocess.Popen([str(binary),"--no-browser"],cwd=root,env=env,
                          stdout=subprocess.DEVNULL,stderr=stderr) as process:
        try:
            deadline=time.monotonic()+8
            while process.poll() is None and time.monotonic()<deadline:
                try:
                    if request(port,"/api/health")[0]==200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise AssertionError(log.read_text(errors="replace")[-4096:] or "Standalone gateway did not start")
            yield port
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise AssertionError("Standalone gateway did not shut down")
            assert process.returncode in {0,-15},log.read_text(errors="replace")[-4096:]


def seed_account(root):
    fields=("id","provider","authType","name","priority","isActive","createdAt","updatedAt")
    account=ITEM["account"]
    data={key:value for key,value in account.items() if key not in fields}
    with sqlite3.connect(root/"db/data.sqlite") as db:
        db.execute("INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,createdAt,updatedAt,data) VALUES (?,?,?,?,?,?,?,?,?)",
                   (*[account[key] for key in fields],json.dumps(data)))


def native_turn(port,cookie=None):
    if ITEM["mode"]=="management":
        status,data,_=request(port,"/api/providers/fixture/models",cookie=cookie)
        return {"status":status,"models":data["models"]} if status==200 else {"status":status,"error":data["error"]}
    status,data,_=request(port,"/v1/models")
    assert status==200,data
    return [row for row in data["data"] if row["owned_by"]=="gh"]


def run_native(root):
    turns=ITEM.get("turns",1)
    with gateway(root) as port:
        seed_account(root)
        cookie=None
        if ITEM["mode"]=="management":
            status,_,headers=request(port,"/api/auth/login",body={"password":"123456"})
            assert status==200
            cookie=headers["Set-Cookie"].split(";",1)[0]
        result=[native_turn(port,cookie) for _ in range(1 if ITEM.get("force") else turns)]
    if ITEM.get("force"):
        with gateway(root) as port:
            result.append(native_turn(port))
    fallback=root/"fallbacks"
    assert not fallback.exists() or not fallback.read_text().strip()
    return result


with tempfile.TemporaryDirectory(prefix="9router-copilot-catalog-") as directory:
    root=pathlib.Path(directory)
    server=http.server.ThreadingHTTPServer(("127.0.0.1",52989 if MODE in {"native","strict"} else 0),Mock)
    threading.Thread(target=lambda:server.serve_forever(poll_interval=.02),daemon=True).start()
    base=f"http://127.0.0.1:{server.server_port}"
    env=dict(os.environ,HOME=directory,DATA_DIR=directory,NO_PROXY="localhost,127.0.0.1")
    try:
        if MODE in {"native","strict"}:
            actual=run_native(root)
        else:
            output=subprocess.run(COMMAND+[directory,base],cwd=root,env=env,text=True,input=json.dumps(ITEM),capture_output=True,timeout=15)
            assert output.returncode==0,output.stderr
            actual=json.loads(output.stdout.strip().splitlines()[-1])
    finally:
        server.shutdown();server.server_close()
    assert not FAULTS,FAULTS
    with sqlite3.connect(root/"db/data.sqlite") as sql:
        fields=json.loads(sql.execute("SELECT data FROM providerConnections WHERE id='fixture'").fetchone()[0])
    changed=fields["providerSpecificData"].get("copilotToken")==FRESH
    assert all(not run.get("unexpected") for run in CALLS)
    assert fields["providerSpecificData"]["other"]=="unchanged"
    if CASE=="live-proactive":
        assert [item["path"] for item in CALLS]==["/copilot","/models"] and changed
        assert [row["id"] for row in actual[0]]==["gh/fixture-gpt","gh/fixture-strict-chat"]
    if CASE=="live-proxy":
        assert CALLS==[] and actual[0] and all(not row["id"].startswith("gh/fixture-") for row in actual[0])
    print(json.dumps({"result":actual,"wire":CALLS,"persistedCopilot":changed},sort_keys=True))
