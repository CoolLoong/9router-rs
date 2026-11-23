import base64
import hashlib
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
from threading import Thread
import time

binary = Path(sys.argv[1]).resolve()
SECRET = b"offline-windows-preview-dashboard"
TARGET = "x86_64-pc-windows-gnu"


class SignedFeed(BaseHTTPRequestHandler):
    response = b""
    paths = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.paths.append(self.path)
        if self.path != "/manifest":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(self.response)))
        self.end_headers()
        self.wfile.write(self.response)


def b64(value):
    return base64.urlsafe_b64encode(value).decode().rstrip("=")


def windows(path):
    return "Z:" + str(path).replace("/", "\\")


def disposable_key(directory):
    key = directory / "synthetic-signer.pem"
    subprocess.run(["openssl","genpkey","-algorithm","ED25519","-out",key],check=True)
    der = subprocess.check_output(["openssl","pkey","-in",key,"-pubout","-outform","DER"])
    assert len(der) == 44
    return key,b64(der[-32:])


def manifest(workspace,key,scenario):
    payload = workspace / "payload.json"
    root = {
        "schema":"9router-native-update/v1","channel":"preview","target":TARGET,
        "version":"90.0.1","expiresAt":int(time.time())+600,"artifact":"/9router.exe",
        "size":1024,"sha256":"01"*32,
    }
    if scenario == "wrong-target":
        root["target"] = "x86_64-unknown-linux-gnu"
    if scenario == "external-artifact":
        root["artifact"] = "https://fixture.invalid/9router.exe"
    payload.write_text(json.dumps(root,separators=(",",":")))
    signature = subprocess.check_output(["openssl","pkeyutl","-sign","-rawin","-inkey",key,"-in",payload])
    raw = payload.read_bytes()
    if scenario == "invalid-signature":
        raw += b" "
    return json.dumps({"payload":b64(raw),"signature":b64(signature)}).encode()


def shutdown_headers():
    header = b64(b'{"alg":"HS256"}')
    claims = b64(json.dumps({"authenticated":True,"sub":"offline-windows",
                            "iat":int(time.time()),"exp":int(time.time())+120}).encode())
    part = f"{header}.{claims}"
    signature = b64(hmac.new(SECRET,part.encode(),hashlib.sha256).digest())
    return {"Cookie":f"auth_token={part}.{signature}"}


def call(port,path,method="GET",headers=None):
    conn = http.client.HTTPConnection("127.0.0.1",port,timeout=5)
    try:
        conn.request(method,path,headers=headers or {})
        reply = conn.getresponse()
        return reply.status,json.loads(reply.read())
    finally:
        conn.close()


def run(workspace,env,scenario):
    data = workspace / scenario
    data.mkdir()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0))
        port = sock.getsockname()[1]
    args = ["--no-browser","--host","127.0.0.1","--port",str(port)]
    if scenario == "skip":
        args.append("--skip-update")
    before = len(SignedFeed.paths)
    with (data/"terminal.log").open("w") as log:
        child = subprocess.Popen(["wine",windows(binary),*args],env=env | {"DATA_DIR":windows(data)},
                                 stdin=subprocess.DEVNULL,stdout=log,stderr=log)
        try:
            until = time.monotonic() + 25
            while time.monotonic() < until and child.poll() is None:
                try:
                    if call(port,"/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError("Wine fixture gateway did not start")
            if scenario == "valid":
                while len(SignedFeed.paths) == before and time.monotonic() < until:
                    time.sleep(.03)
                assert len(SignedFeed.paths) == before + 1
            elif scenario == "skip":
                time.sleep(.15)
                assert len(SignedFeed.paths) == before
            code,version = call(port,"/api/version")
            latest = "90.0.1" if scenario in {"valid","skip"} else None
            assert code == 200 and version["latestVersion"] == latest and version["hasUpdate"] is bool(latest), version
            assert SignedFeed.paths[before:] == ["/manifest"], SignedFeed.paths[before:]
            assert call(port,"/api/version/shutdown","POST",shutdown_headers())[0] == 200
            assert child.wait(timeout=15) == 0
        except Exception:
            print((data/"terminal.log").read_text(errors="replace"),file=sys.stderr)
            raise


with tempfile.TemporaryDirectory(prefix="9router-windows-preview-discovery-") as temp:
    workspace = Path(temp)
    home = workspace/"home"
    home.mkdir()
    key, public = disposable_key(workspace)
    feed = ThreadingHTTPServer(("127.0.0.1",0),SignedFeed)
    Thread(target=feed.serve_forever,daemon=True).start()
    env = {
        "PATH":os.environ["PATH"],"HOME":str(home),"WINEPREFIX":str(workspace/"wine"),
        "WINEDLLOVERRIDES":"mscoree,mshtml=","WINEDEBUG":"-all","WINEARCH":"win64",
        "NINEROUTER_MIGRATION_FIXTURE":"1","MODEL_CATALOG_SYNC":"off","JWT_SECRET":SECRET.decode(),
        "NINEROUTER_NATIVE_UPDATE_MANIFEST":f"http://127.0.0.1:{feed.server_port}/manifest",
        "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY":public,
    }
    try:
        subprocess.run(["wineboot","-u"],env=env,check=True,timeout=60,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for scenario in ("valid","skip","invalid-signature","wrong-target","external-artifact"):
            SignedFeed.response = manifest(workspace,key,scenario)
            run(workspace,env,scenario)
        print("Windows signed discovery passed five isolated Wine scenarios")
    finally:
        subprocess.run(["wineserver","-k"],env=env,timeout=10,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver","-w"],env=env,timeout=10,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        feed.shutdown()
        feed.server_close()
