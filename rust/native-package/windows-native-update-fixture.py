import base64
import hashlib
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import runpy
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Thread
import time

OLD, NEW = [Path(arg).resolve() for arg in sys.argv[1:3]]
CURRENT, NEXT = "0.5.119", "0.5.120"
SECRET = b"offline-windows-native-update"
fixture = runpy.run_path(str(Path(__file__).with_name("windows-update-process-fixture.py")))
windows = fixture["windows"]
compile_fixture = fixture["compile_fixture"]


class Feed(BaseHTTPRequestHandler):
    envelope = b""
    artifact = None
    events = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.events.append(self.path)
        if self.path == "/manifest":
            self.send_response(200)
            self.send_header("Content-Length",str(len(self.envelope)))
            self.end_headers()
            self.wfile.write(self.envelope)
        elif self.path == "/9router.exe":
            self.send_response(200)
            self.send_header("Content-Length",str(self.artifact.stat().st_size))
            self.end_headers()
            with self.artifact.open("rb") as file:
                try:
                    shutil.copyfileobj(file,self.wfile,length=1024*1024)
                except (BrokenPipeError,ConnectionResetError):
                    pass
        else:
            self.send_error(404)


def b64(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file,"sha256").hexdigest()


def session():
    header=b64(b'{"alg":"HS256"}')
    claim=b64(json.dumps({"authenticated":True,"exp":int(time.time())+600,"iat":int(time.time())}).encode())
    proof=b64(hmac.new(SECRET,f"{header}.{claim}".encode(),hashlib.sha256).digest())
    return f"auth_token={header}.{claim}.{proof}"


def call(port,path,method="GET",authenticated=True,origin=None):
    headers={"Cookie":session()} if authenticated else {}
    if origin:
        headers["Origin"]=origin
    connection=http.client.HTTPConnection("127.0.0.1",port,timeout=15)
    try:
        connection.request(method,path,headers=headers)
        result=connection.getresponse()
        return result.status, json.loads(result.read())
    finally:
        connection.close()


def when(predicate,description,timeout=28):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        try:
            result=predicate()
            if result:
                return result
        except (OSError,ValueError,http.client.HTTPException):
            pass
        time.sleep(.065)
    raise RuntimeError(description)


def ready(port,version):
    def check_running():
        return (call(port,"/api/health")==(200,{"ok":True})
                and call(port,"/api/version")[1].get("currentVersion")==version
                and call(port,"/api/settings")[0]==200)
    when(check_running,f"Windows gateway v{version} never became ready")


def sign(key,fixture_dir,artifact):
    payload=json.dumps({
        "schema":"9router-native-update/v1","channel":"preview",
        "target":"x86_64-pc-windows-gnu","version":NEXT,"expiresAt":int(time.time())+600,
        "artifact":"/9router.exe","size":artifact.stat().st_size,"sha256":digest(artifact),
    },separators=(",",":")).encode()
    file=fixture_dir/"release.json"
    file.write_bytes(payload)
    signature=subprocess.check_output(["openssl","pkeyutl","-sign","-rawin","-inkey",key,"-in",file])
    Feed.artifact=artifact
    Feed.envelope=json.dumps({"payload":b64(payload),"signature":b64(signature)}).encode()


def write_sentinel(db,value):
    with sqlite3.connect(db,timeout=5) as connection:
        connection.execute("""INSERT INTO settings(id,data) VALUES(1,json_object('windowsUpdateFixture',?))
                              ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.windowsUpdateFixture',?)""",
                           (value,value))


def read_sentinel(db):
    with sqlite3.connect(db,timeout=5) as connection:
        return connection.execute("SELECT json_extract(data,'$.windowsUpdateFixture') FROM settings WHERE id=1").fetchone()


def scenario(workspace,env,probe,key,kind):
    destination=f"C:\\native-update-{kind}"
    disk=Path(env["WINEPREFIX"])/"drive_c"/f"native-update-{kind}"
    subprocess.run(["wine",windows(probe),"gateway-init",destination,windows(OLD),windows(NEW)],env=env,
                   check=True,timeout=25)
    installation=disk/"install/9router.exe"
    db=disk/"store/db/data.sqlite"
    artifacts=disk/"install/.9router-update"
    with socket.socket() as guard:
        guard.bind(("127.0.0.1",0))
        port=guard.getsockname()[1]
    runenv=env | {"DATA_DIR":destination+"\\store"}
    def start(output,fault=None):
        attempt=runenv.copy()
        if fault:
            attempt["NINEROUTER_WINDOWS_UPDATE_FIXTURE_INTERRUPT"]=fault
        return subprocess.Popen(["wine",windows(installation),"--port",str(port),"--no-browser"],
                                env=attempt,stdout=output,stderr=output,stdin=subprocess.DEVNULL)

    sign(key,workspace,OLD if kind=="wrong-version" else NEW)
    if kind=="bad-artifact":
        Feed.artifact=OLD
    with (disk/"server.log").open("w") as output:
        initial=start(output,"after-swap" if kind=="interrupted" else None)
        try:
            ready(port,CURRENT)
            write_sentinel(db,"kept")
            starting=len(Feed.events)
            assert call(port,"/api/version/update","POST",authenticated=False)[0]==401
            assert call(port,"/api/version/update","POST",origin="https://invalid.fixture.test")[0]==403
            assert len(Feed.events)==starting
            accepted=call(port,"/api/version/update","POST",origin=f"http://127.0.0.1:{port}")
            if kind=="bad-artifact":
                assert accepted[0]==409 and accepted[1]["success"] is False,accepted
                assert initial.poll() is None and digest(installation)==digest(OLD)
                assert not (artifacts/"journal.json").exists() and read_sentinel(db)==("kept",)
                assert call(port,"/api/version/shutdown","POST")[0]==200
                assert initial.wait(timeout=15)==0
                print("bad-artifact: signed digest mismatch rejected before shutdown")
                return
            assert accepted[0]==200 and accepted[1]["success"] is True,accepted
            assert initial.wait(timeout=20)==0
            assert "/9router.exe" in Feed.events[starting:]
            expected=NEXT if kind=="upgrade" else CURRENT
            if kind=="interrupted":
                when(lambda:(artifacts/"journal.json").is_file() and digest(installation)==digest(NEW),
                     "Interrupted helper did not leave a committed recovery snapshot")
                time.sleep(.4)
                assert not (artifacts/"committed").exists()
                write_sentinel(db,"dirty")
                retried=start(output,"rollback-after-pe")
                assert retried.wait(timeout=16)==0
                when(lambda:digest(installation)==digest(OLD),"Interrupted rollback did not restore the PE")
                time.sleep(.4)
                assert read_sentinel(db)==("dirty",)
                assert digest(artifacts/"previous.exe")==digest(OLD)
                assert not (artifacts/"committed").exists()
                db.unlink()
                restored_primary=start(output,"rollback-after-database")
                assert restored_primary.wait(timeout=16)==0
                when(lambda:digest(db)==digest(artifacts/"data.sqlite"),"Interrupted SQLite rollback did not restore the primary file")
                time.sleep(.4)
                assert not (artifacts/"committed").exists()
                recovery=start(output)
                assert recovery.wait(timeout=16)==0
            ready(port,expected)
            assert read_sentinel(db)==("kept",)
            assert digest(installation)==digest(NEW if kind=="upgrade" else OLD)
            assert (artifacts/"committed").is_file()
            assert call(port,"/api/version/shutdown","POST")[0]==200
            print(f"{kind}: native POST and store verified")
        except Exception:
            print((disk/"server.log").read_text(errors="replace"),file=sys.stderr)
            raise


with tempfile.TemporaryDirectory(prefix="9router-windows-actual-native-update-") as directory:
    workspace=Path(directory)
    home=workspace/"home"
    runtime=workspace/"runtime"
    home.mkdir()
    runtime.mkdir(mode=0o700)
    probe=workspace/"directory-probe.exe"
    probe.write_bytes(compile_fixture(workspace))
    key=workspace/"offline-signer.pem"
    subprocess.run(["openssl","genpkey","-algorithm","ED25519","-out",key],check=True)
    certificate=subprocess.check_output(["openssl","pkey","-in",key,"-pubout","-outform","DER"])
    feed=ThreadingHTTPServer(("127.0.0.1",0),Feed)
    Thread(target=feed.serve_forever,daemon=True).start()
    env={
        "HOME":str(home),"PATH":os.environ["PATH"],"WINEPREFIX":str(workspace/"fresh-wine"),
        "XDG_RUNTIME_DIR":str(runtime),"WINEDEBUG":"-all","WINEARCH":"win64","WINEDLLOVERRIDES":"mscoree,mshtml=",
        "NINEROUTER_MIGRATION_FIXTURE":"1","MODEL_CATALOG_SYNC":"off","JWT_SECRET":SECRET.decode(),
        "NINEROUTER_NATIVE_UPDATE_MANIFEST":f"http://127.0.0.1:{feed.server_port}/manifest",
        "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY":b64(certificate[-32:]),
    }
    try:
        subprocess.run(["wineboot","-u"],env=env,check=True,timeout=60,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for kind in sys.argv[3:] or ("upgrade","bad-artifact","wrong-version","interrupted"):
            scenario(workspace,env,probe,key,kind)
    finally:
        subprocess.run(["wineserver","-k"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver","-w"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        feed.shutdown()
        feed.server_close()
