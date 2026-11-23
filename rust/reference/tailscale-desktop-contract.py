import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
mode, executable = sys.argv[1:]
binary = Path(executable).resolve()
fixture = {"policy":"unsigned", "path":None, "requests":[]}
cli = {"x-9r-cli-token":"68977b6fd9422fbb"}


class PackageServer(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        fixture["requests"].append(self.path)
        assert self.path == fixture["path"]
        assert not any(self.headers.get(key) for key in ("cookie", "authorization", "x-9r-cli-token"))
        if fixture["policy"] == "hold":
            assert release.wait(8)
        content = b"xar!fixture package only" if mode == "macos" else b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1unsigned MSI fixture"
        status, extra = 200, {}
        if fixture["policy"] == "bad-format":
            content = b"wrong header"
        elif fixture["policy"] == "redirect":
            status, extra = 307, {"Location":"http://127.0.0.1:52989/wrong"}
        elif fixture["policy"] == "oversized":
            content, extra = b"", {"Content-Length":str(161 * 1024 * 1024)}
        self.send_response(status)
        extra.setdefault("Content-Length",str(len(content)))
        for name, value in extra.items():
            self.send_header(name,value)
        self.end_headers()
        try:
            self.wfile.write(content)
        except (BrokenPipeError, ConnectionResetError):
            pass


def macos(root):
    fixture["path"] = "/tailscale-desktop/Tailscale-latest-macos.pkg"
    ledger, brew = root / "brew-log", root / "fake brew"
    brew.write_text(f"#!{sys.executable}\n" + f"""import json, os, sys
from pathlib import Path
assert os.environ["HOMEBREW_NO_AUTO_UPDATE"] == "1"
Path({str(ledger)!r}).write_text(json.dumps(sys.argv[1:]))
""")
    brew.chmod(0o700)
    stage = root / "stage"
    stage.mkdir()
    env = dict(os.environ, TMPDIR=str(stage), NINEROUTER_MIGRATION_FIXTURE="1")

    def launch(tool):
        return subprocess.run([str(binary),tool,str(brew)],env=env,check=True,timeout=12,
                              capture_output=True,text=True).stdout.splitlines()

    assert launch("brew") == [] and json.loads(ledger.read_text()) == ["install","tailscale"]
    assert fixture["requests"] == []
    for policy, result in [
        ("signed",["pkgutil","spctl","trusted"]),("unsigned",["pkgutil","rejected"]),
        ("wrong-team",["pkgutil","rejected"]),("gatekeeper-denied",["pkgutil","spctl","rejected"]),
    ]:
        env["TAILSCALE_SIGNATURE_FIXTURE"] = policy
        assert launch("package") == result
        assert not any(stage.iterdir())
    assert fixture["requests"] == [fixture["path"]] * 4
    print("macOS Homebrew command, PKG publisher and Gatekeeper checks verified with synthetic commands and package.")


def endpoint(port, path, method="GET", headers=None):
    client = http.client.HTTPConnection("127.0.0.1",port,timeout=15)
    try:
        client.request(method,path,headers=headers or {})
        reply = client.getresponse()
        payload = reply.read().decode()
        if reply.headers.get_content_type() == "text/event-stream":
            events = []
            for block in payload.replace("\r\n","\n").split("\n\n"):
                event = next((line[7:] for line in block.splitlines() if line.startswith("event: ")),None)
                data = next((line[6:] for line in block.splitlines() if line.startswith("data: ")),None)
                if event and data:
                    events.append((event,json.loads(data)))
            return reply.status, events
        return reply.status, json.loads(payload)
    finally:
        client.close()


def windows(root):
    fixture["path"] = "/tailscale-desktop/tailscale-setup-latest-amd64.msi"
    data, stage = root / "data", root / "stage"
    stage.mkdir()
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"),str(data)],check=True,
                   stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    local = lambda path: "Z:" + str(path).replace("/","\\")
    env = {
        "PATH":os.environ["PATH"],"LANG":"C.UTF-8","WINEPREFIX":str(root / "prefix"),"WINEDEBUG":"-all",
        "WINEDLLOVERRIDES":"mscoree,mshtml=","DATA_DIR":local(data),"TEMP":local(stage),"TMP":local(stage),
        "NINEROUTER_MIGRATION_FIXTURE":"1","BROWSER":"/usr/bin/false",
    }
    with socket.socket() as listener:
        listener.bind(("127.0.0.1",0))
        port = listener.getsockname()[1]
    app = subprocess.Popen(["wine",str(binary),"--port",str(port),"--no-browser"],env=env,
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    route = "/api/tunnel/tailscale-install"
    try:
        deadline = time.monotonic() + 55
        while app.poll() is None and time.monotonic() < deadline:
            try:
                if endpoint(port,"/api/health")[0] == 200:
                    break
            except OSError:
                time.sleep(.06)
        else:
            raise RuntimeError("Wine gateway did not start")
        assert endpoint(port,route,"POST")[0] == 403 and not fixture["requests"]
        status, check = endpoint(port,"/api/tunnel/tailscale-check",headers=cli)
        assert status == 200 and check["platform"] == "win32" and not check["installed"]
        for policy, expected in [
            ("bad-format","Tailscale package format is invalid"),("redirect","unexpected redirect"),
            ("oversized","exceeded the allowed download size"),("unsigned","installer signature could not be verified"),
        ]:
            fixture["policy"] = policy
            code, jobs = endpoint(port,route,"POST",cli)
            assert code == 200 and jobs[0][0] == "progress" and jobs[-1][0] == "error", jobs
            assert expected in jobs[-1][1]["error"], jobs
            assert not list(stage.glob("9router-tailscale-*"))
        fixture["policy"] = "hold"
        with ThreadPoolExecutor(max_workers=1) as worker:
            in_progress = worker.submit(endpoint, port, route, "POST", cli)
            try:
                deadline = time.monotonic() + 4
                while len(fixture["requests"]) < 5 and time.monotonic() < deadline:
                    time.sleep(.02)
                assert len(fixture["requests"]) == 5
                assert endpoint(port, route, "POST", cli)[0] == 409
            finally:
                release.set()
            assert in_progress.result(timeout=6)[1][-1][0] == "error"
        assert not list(stage.glob("9router-tailscale-*"))
        assert fixture["requests"] == [fixture["path"]] * 5
        assert endpoint(port,"/api/health")[0] == 200
        print("Wine desktop POST authorizes locally, accepts no password, enforces download bounds, and rejects unsigned MSI before UAC.")
    finally:
        if app.poll() is None:
            app.terminate()
            try:
                app.wait(timeout=8)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait()
        subprocess.run(["wineserver","-k"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver","-w"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


with tempfile.TemporaryDirectory(prefix="9router-tailscale-desktop-") as directory:
    release = threading.Event()
    upstream = ThreadingHTTPServer(("127.0.0.1",52989),PackageServer)
    threading.Thread(target=upstream.serve_forever,daemon=True).start()
    try:
        {"macos":macos, "windows":windows}[mode](Path(directory))
    finally:
        release.set()
        upstream.shutdown()
        upstream.server_close()
