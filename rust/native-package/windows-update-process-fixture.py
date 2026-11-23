import hashlib
import base64
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sqlite3
import sys
import tempfile
from threading import Thread
import time

fixtures = Path(__file__).resolve().parent


def windows(path):
    return "Z:" + str(path).replace("/", "\\")


def compile_fixture(workspace):
    source = workspace / "build"
    (source / "src").mkdir(parents=True)
    (source / "Cargo.toml").write_text("""
[package]
name = "windows-update-process-fixture"
version = "0.1.0"
edition = "2024"
[features]
native-package = []
[dependencies]
windows-sys = {version = "=0.61.2", features = ["Wdk_Storage_FileSystem","Win32_Foundation","Win32_NetworkManagement_IpHelper","Win32_Security_Authorization","Win32_Security_Cryptography","Win32_Storage_FileSystem","Win32_System_IO","Win32_System_Memory","Win32_System_Threading"]}
[workspace]
""")
    shutil.copyfile(fixtures / "windows-update-process-fixture.rs", source / "src/main.rs")
    shutil.copyfile(fixtures / "windows-update-process.rs", source / "src/windows_process.rs")
    shutil.copyfile(fixtures / "windows-update-handoff.rs", source / "src/windows_handoff.rs")
    shutil.copyfile(fixtures / "windows-update-acl.rs", source / "src/windows_acl.rs")
    shutil.copyfile(fixtures / "windows-update-gateway.rs", source / "src/windows_gateway.rs")
    compilation = dict(os.environ,RUSTC_WRAPPER="",CARGO_TARGET_DIR=str(source/"target"),
                       CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="x86_64-w64-mingw32-gcc")
    result = subprocess.run(["rustup","run","stable","cargo","build","--offline","--target",
                             "x86_64-pc-windows-gnu"],cwd=source,env=compilation,capture_output=True)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.decode(errors="replace"))
    return (source/"target/x86_64-pc-windows-gnu/debug/windows-update-process-fixture.exe").read_bytes()


def query(port,request):
    with socket.create_connection(("127.0.0.1",port),timeout=4) as connection:
        connection.sendall(request)
        return connection.makefile("rb").read()


def scenario(workspace,env,pe,kind,marker,expected):
    directory = workspace / ("phase " + kind)
    directory.mkdir()
    install = directory/"9router.exe"
    helper = directory/"helper.exe"
    replacement = directory/"incoming.exe"
    report = directory/"report.txt"
    install.write_bytes(pe+b"1")
    helper.write_bytes(pe+b"1")
    replacement.write_bytes(pe+marker)
    reserved = socket.socket()
    reserved.bind(("127.0.0.1",0))
    port = reserved.getsockname()[1]
    occupied = kind == "unowned-listener"
    decoy_probes = []
    if occupied:
        def decoy():
            while True:
                try:
                    connection,_ = reserved.accept()
                except OSError:
                    return
                with connection:
                    decoy_probes.append(connection.recv(16))
                    connection.sendall(b"2")
        reserved.listen()
        Thread(target=decoy,daemon=True).start()
    else:
        reserved.close()
    allow = directory/"allow.txt"
    log = directory/"launcher.log"
    with log.open("w") as output:
        proc = subprocess.Popen(["wine",windows(install),"parent",windows(replacement),
                                 windows(report),f"127.0.0.1:{port}",
                                 *([windows(allow)] if occupied else [])],env=env,stdout=output,stderr=output)
        assert proc.wait(timeout=20) == 0, (kind,log.read_text(),report.read_text() if report.exists() else "")
    last = "verified-replacement" if expected == b"2" else "verified-rollback"
    end = time.monotonic()+15
    while time.monotonic() < end and last not in report.read_text():
        if occupied and "candidate-rejected" in report.read_text() and not allow.exists():
            reserved.shutdown(socket.SHUT_RDWR)
            reserved.close()
            assert decoy_probes == []
            allow.write_text("continue\n")
        time.sleep(.03)
    assert report.read_text().splitlines() == [
        "parent-lease-held","parent-exiting","parent-exit-observed",
        *(["candidate-rejected"] if expected == b"1" else []),last,
    ], (kind,report.read_text(),log.read_text())
    assert query(port,b"VERSION") == expected
    assert hashlib.sha256(install.read_bytes()).digest() == hashlib.sha256(pe+expected).digest()
    assert not (directory/"previous.exe").exists()
    contender = ["wine",windows(install),"contender",windows(directory/"lease")]
    rejected = subprocess.run(contender,env=env,capture_output=True,timeout=10)
    assert rejected.returncode == 17, (kind,rejected)
    assert query(port,b"STOP") == b""
    end = time.monotonic() + 6
    while time.monotonic() < end:
        confirmation = subprocess.run(contender,env=env,capture_output=True,timeout=10)
        if confirmation.returncode == 0:
            break
        assert confirmation.returncode == 17, (kind,confirmation)
        time.sleep(.035)
    else:
        raise RuntimeError("fixture candidate did not release the installation lease")
    print(f"{kind}: passed")


def b64(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


secret = b"offline-windows-gateway-handoff"


def gateway_request(port,path,method="GET"):
    header = b64(b'{"alg":"HS256"}')
    claim = b64(json.dumps({"authenticated":True,"exp":int(time.time())+90,"iat":int(time.time())}).encode())
    credentials = f"{header}.{claim}"
    token = f"{credentials}.{b64(hmac.new(secret,credentials.encode(),hashlib.sha256).digest())}"
    client = http.client.HTTPConnection("127.0.0.1",port,timeout=4)
    try:
        client.request(method,path,headers={"Cookie":"auth_token="+token})
        response = client.getresponse()
        assert response.status == 200, (path,response.status,response.read())
        return json.loads(response.read())
    finally:
        client.close()


class Feed(BaseHTTPRequestHandler):
    response = b""

    def log_message(self,*_):
        pass

    def do_GET(self):
        assert self.path == "/manifest", self.path
        self.send_response(200)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(self.response)))
        self.end_headers()
        self.wfile.write(self.response)


def gateway_scenarios(workspace,env,probe,old,new):
    previous,expected = ("0.5.119","0.5.120")
    signer = workspace/"gateway-fixture-key.pem"
    subprocess.run(["openssl","genpkey","-algorithm","ED25519","-out",signer],check=True)
    der = subprocess.check_output(["openssl","pkey","-in",signer,"-pubout","-outform","DER"])
    payload_file = workspace/"manifest-payload"
    feed = ThreadingHTTPServer(("127.0.0.1",0),Feed)
    Thread(target=feed.serve_forever,daemon=True).start()
    try:
        for kind,artifact,version in [("upgrade",new,expected),("rollback",old,previous),("interrupt",new,previous)]:
            root = f"C:\\gateway-phase-{kind}"
            data = Path(env["WINEPREFIX"])/"drive_c"/f"gateway-phase-{kind}"
            signature_payload = json.dumps({
                "schema":"9router-native-update/v1","channel":"preview",
                "target":"x86_64-pc-windows-gnu","version":expected,"expiresAt":int(time.time())+120,
                "artifact":"/9router.exe","size":artifact.stat().st_size,
                "sha256":hashlib.sha256(artifact.read_bytes()).hexdigest(),
            },separators=(",",":")).encode()
            payload_file.write_bytes(signature_payload)
            signature = subprocess.check_output(["openssl","pkeyutl","-sign","-rawin","-inkey",signer,"-in",payload_file])
            Feed.response = json.dumps({"payload":b64(signature_payload),"signature":b64(signature)}).encode()
            process_env=env | {
                "DATA_DIR":root+"\\store","NINEROUTER_MIGRATION_FIXTURE":"1","MODEL_CATALOG_SYNC":"off",
                "JWT_SECRET":secret.decode(),
                "NINEROUTER_NATIVE_UPDATE_MANIFEST":f"http://127.0.0.1:{feed.server_port}/manifest",
                "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY":b64(der[-32:]),
            }
            subprocess.run(["wine",windows(probe),"gateway-init",root,windows(old),windows(artifact)],
                           env=process_env,check=True,timeout=15)
            with socket.socket() as sock:
                sock.bind(("127.0.0.1",0))
                port = sock.getsockname()[1]
            report = data/"gateway.txt"
            log = data/"launcher.txt"
            with log.open("w") as output:
                fault = {"rollback":["exercise-dirty-store"],"interrupt":["interrupt-after-swap"]}.get(kind,[])
                child = subprocess.Popen(["wine",windows(probe),"gateway-update",root,f"127.0.0.1:{port}",
                                          previous,expected,*fault],
                                         env=process_env,stdout=output,stderr=output)
                def await_mark(mark):
                    until=time.monotonic()+20
                    while time.monotonic()<until:
                        if report.exists() and mark in report.read_text():
                            return
                        if child.poll() is not None:
                            break
                        time.sleep(.04)
                    raise RuntimeError(f"{kind} {mark}: {log.read_text(errors='replace')}")
                await_mark("old-signed-feed-verified")
                db = data/"store/db/data.sqlite"
                with sqlite3.connect(db,timeout=4) as connection:
                    connection.execute("""INSERT INTO settings(id,data) VALUES(1,'{"windowsUpdateFixture":"kept"}')
                                       ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.windowsUpdateFixture','kept')""")
                gateway_request(port,"/api/version/shutdown","POST")
                if kind == "rollback":
                    await_mark("bad-candidate-awaiting-dirty-store")
                    with sqlite3.connect(db,timeout=4) as connection:
                        connection.execute("UPDATE settings SET data=json_set(data,'$.windowsUpdateFixture','dirty') WHERE id=1")
                    (data/"restore.txt").write_text("continue\n")
                if kind == "interrupt":
                    await_mark("recovery-snapshot-ready")
                    assert child.wait(timeout=10) == 23, log.read_text(errors="replace")
                    assert (data/"install/stage/previous.exe").read_bytes() == old.read_bytes()
                    assert (data/"install/9router.exe").read_bytes() == new.read_bytes()
                    with sqlite3.connect(db,timeout=4) as connection:
                        connection.execute("UPDATE settings SET data=json_set(data,'$.windowsUpdateFixture','dirty') WHERE id=1")
                    child = subprocess.Popen(["wine",windows(probe),"gateway-recover",root,f"127.0.0.1:{port}",previous],
                                             env=process_env,stdout=output,stderr=output)
                final = {"upgrade":"actual-replacement-verified","rollback":"actual-rollback-verified",
                         "interrupt":"actual-manual-recovery-verified"}[kind]
                await_mark(final)
                assert gateway_request(port,"/api/version")["currentVersion"] == version
                with sqlite3.connect(db,timeout=4) as connection:
                    assert connection.execute("SELECT json_extract(data,'$.windowsUpdateFixture') FROM settings WHERE id=1").fetchone() == ("kept",)
                gateway_request(port,"/api/version/shutdown","POST")
                assert child.wait(timeout=10)==0,log.read_text(errors="replace")
            expected_image = artifact if kind=="upgrade" else old
            assert hashlib.sha256((data/"install/9router.exe").read_bytes()).digest() == hashlib.sha256(expected_image.read_bytes()).digest()
            print(f"actual signed gateway {kind}: passed")
    finally:
        feed.shutdown()
        feed.server_close()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="9router-windows-update-process-") as temp:
        workspace=Path(temp)
        pe=compile_fixture(workspace)
        home=workspace/"home"
        home.mkdir()
        runtime=workspace/"runtime"
        runtime.mkdir(mode=0o700)
        env={
            "PATH":os.environ["PATH"],"HOME":str(home),"WINEPREFIX":str(workspace/"fresh-wine"),
            "WINEDLLOVERRIDES":"mscoree,mshtml=","WINEDEBUG":"-all","WINEARCH":"win64",
            "XDG_RUNTIME_DIR":str(runtime),
        }
        try:
            subprocess.run(["wineboot","-u"],env=env,check=True,timeout=60,
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            acl_probe = workspace / "acl-proof.exe"
            acl_probe.write_bytes(pe)
            result = subprocess.run(["wine",windows(acl_probe),"acl",r"C:\9router-fixture-acl-scope"],
                                    cwd=workspace,env=env,capture_output=True,timeout=15)
            assert result.returncode == 0, result.stderr.decode(errors="replace")
            print(result.stdout.decode().strip())
            for kind,marker,expected in (
                ("upgrade",b"2",b"2"),("early-exit",b"!",b"1"),("wrong-version",b"3",b"1"),
                ("unowned-listener",b"2",b"1"),
            ):
                scenario(workspace,env,pe,kind,marker,expected)
            if len(sys.argv) == 3:
                gateway_scenarios(workspace,env,acl_probe,Path(sys.argv[1]).resolve(),Path(sys.argv[2]).resolve())
        finally:
            subprocess.run(["wineserver","-k"],env=env,timeout=10,
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            subprocess.run(["wineserver","-w"],env=env,timeout=10,
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
