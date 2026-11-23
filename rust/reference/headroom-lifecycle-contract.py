import concurrent.futures
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(port, path, method="GET", payload=None, headers=cli):
    headers = headers.copy()
    if payload is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(payload).encode()
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=19)
    try:
        client.request(method, path, body=payload, headers=headers)
        response = client.getresponse()
        raw = response.read()
        return response.status, json.loads(raw) if raw else None, response.headers
    finally:
        client.close()


def finish(process):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def gone(pid):
    stat = Path(f"/proc/{pid}/stat")
    try:
        return stat.read_text().rsplit(") ", 1)[1].startswith(("Z ", "X "))
    except FileNotFoundError:
        return True


with tempfile.TemporaryDirectory(prefix="9router-headroom-lifecycle-") as directory:
    temp = Path(directory)
    home = temp / "home"
    scripts = home / ".local" / "bin"
    scripts.mkdir(parents=True)
    launcher = scripts / "headroom"
    launcher.write_text(f"#!{sys.executable}\n" + textwrap.dedent("""
        import json
        import signal
        import sys
        from http.server import BaseHTTPRequestHandler, HTTPServer

        class Health(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                assert self.path == "/health"
                body = b'{"ok":true}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        assert sys.argv[1:3] == ["proxy", "--port"]
        if "--code-aware" in sys.argv:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
        print(json.dumps(sys.argv[1:]), flush=True)
        HTTPServer(("127.0.0.1", int(sys.argv[3])), Health).serve_forever()
    """))
    launcher.chmod(0o700)
    metadata = scripts / "python3"
    metadata.write_text(f"#!{sys.executable}\n" + textwrap.dedent("""
        import json
        from pathlib import Path
        import sys
        import time

        selected = Path(__file__).with_name("fixture-installed-extra")
        args = sys.argv[1:]
        if args == ["--version"]:
            print("Python 3.13.1")
        elif args == ["-m", "pip", "list", "--format=json", "--disable-pip-version-check"]:
            packages = [{"name":"headroom-ai","version":"1.2.3"}]
            if selected.exists():
                packages.append({"name":"tree-sitter","version":"0.1"})
            print(json.dumps(packages))
        elif args == ["-m", "pip", "install", "--upgrade", "headroom-ai[proxy,code]"]:
            selected.write_text("installed")
            print("Installed fixture code extra")
        elif args == ["-m", "pip", "uninstall", "-y", "tree-sitter", "tree-sitter-language-pack"]:
            selected.unlink()
            print("Removed fixture code extra")
        elif args == ["-m", "pip", "install", "--upgrade", "headroom-ai[proxy,ml]"]:
            print("Started fixture ML operation", flush=True)
            time.sleep(0.45)
            sys.exit(7)
        else:
            raise AssertionError(args)
    """))
    metadata.chmod(0o700)
    gateway_port = port()
    compressor_port = port()
    storage = temp / "state"
    state = storage / "headroom"
    state.mkdir(parents=True)
    pidfile = state / "proxy.pid"
    ownerfile = state / "proxy.owner.json"
    logfile = state / "proxy.log"
    fallbacks = temp / "fallbacks.txt"
    env = {
        **os.environ, "PORT": str(gateway_port), "DATA_DIR": str(storage), "TMPDIR": directory,
        "HOME": str(home), "NINEROUTER_MIGRATION_DATA_DIR": str(storage),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "HEADROOM_URL"]:
        env.pop(name, None)

    def start_gateway():
        instance = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 15
        try:
            while True:
                if instance.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Headroom lifecycle fixture gateway did not start")
                try:
                    with socket.create_connection(("127.0.0.1", gateway_port), timeout=.2):
                        return instance
                except OSError:
                    time.sleep(.03)
        except BaseException:
            finish(instance)
            raise

    def call(path, action=None, payload=None, headers=cli):
        return request(gateway_port, path, action or "GET", payload, headers)

    def launch(name="start", headers=cli):
        return call(f"/api/headroom/{name}", "POST", headers=headers)

    gateway = None
    foreign = subprocess.Popen(["sleep", "90"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        gateway = start_gateway()
        assert launch("start", {})[0] == 403
        assert launch("stop", {})[0] == 403
        assert launch("restart", {})[0] == 403
        status, _, headers = call("/api/auth/login", "POST", {"password": "123456"}, {})
        assert status == 200
        cookie = headers["set-cookie"].split(";", 1)[0]
        extras = "/api/headroom/extras"
        status, details, _ = call(extras)
        assert status == 200 and details == {
            "available":["code","ml"], "installed":True, "version":"1.2.3", "extras":{"code":False,"ml":False},
        }
        assert call(extras, "POST", {"extras":["code"]}, {})[0] == 403
        assert call(extras + "?log=1", headers={"cookie":cookie,"x-forwarded-for":"192.0.2.50"})[0] == 403
        status, result, _ = call(extras, "POST", {"extras":["code","fixture-ignored"]})
        assert status == 200 and result["success"] and result["spec"] == "headroom-ai[proxy,code]"
        assert result["extras"] == {"code":True,"ml":False}
        assert call(extras + "?log=1")[1] == {"log":"Installed fixture code extra"}
        assert (state / "install.log").stat().st_mode & 0o077 == 0
        status, result, _ = call(extras, "DELETE", {"extras":["code"]})
        assert status == 200 and result["removed"] == ["tree-sitter","tree-sitter-language-pack"]
        assert result["extras"]["code"] is False
        assert call(extras, "DELETE", {"extras":["fixture-ignored"]})[1]["code"] == "INVALID_EXTRAS"
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as callers:
            install = callers.submit(call, extras, "POST", {"extras":["ml"]})
            deadline = time.monotonic() + 4
            while "Started fixture ML operation" not in call(extras + "?log=1")[1]["log"]:
                assert time.monotonic() < deadline
                time.sleep(0.01)
            assert call(extras, "DELETE", {"extras":["ml"]})[1]["code"] == "BUSY"
            status, result, _ = install.result(timeout=5)
            assert status == 500 and result["code"] == "INSTALL_FAILED" and "code=7" in result["error"]
        assert launch("start", {"cookie": cookie, "x-forwarded-for": "192.0.2.50"})[0] == 403
        assert launch("stop")[:2] == (409, {"stopped": False, "reason": "not_running"})

        assert call("/api/settings", "PATCH", {"headroomUrl": "https://example.invalid:2025"})[0] == 200
        for name in ["start", "restart"]:
            status, error, _ = launch(name)
            assert status == 400 and error["code"] == "EXTERNAL_PROXY"
        headroom_url = f"http://127.0.0.1:{compressor_port}"
        assert call("/api/settings", "PATCH", {
            "headroomUrl": headroom_url, "headroomCodeAware": True, "headroomKompress": False,
        })[0] == 200
        assert call("/api/headroom/status")[1]["path"] == str(launcher)

        pidfile.write_text(str(foreign.pid))
        for name in ["start", "stop", "restart"]:
            status, error, _ = launch(name)
            assert status == 409 and error["code"] == "UNVERIFIED_PROCESS"
        assert foreign.poll() is None
        assert call("/api/headroom/status")[1]["managedPid"] is None
        finish(foreign)
        assert launch("stop")[:2] == (409, {"stopped": False, "reason": "not_running"})
        assert not pidfile.exists()

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as callers:
            responses = list(callers.map(lambda _: launch("start"), range(2)))
        assert all(status == 200 and body["success"] for status, body, _ in responses)
        assert {body["alreadyRunning"] for _, body, _ in responses} == {True, False}
        procs = {body["pid"] for _, body, _ in responses}
        assert len(procs) == 1
        first = procs.pop()
        assert int(pidfile.read_text()) == first and ownerfile.stat().st_mode & 0o777 == 0o600
        assert request(compressor_port, "/health", headers={})[:2] == (200, {"ok": True})
        status, body, _ = call("/api/headroom/status")
        assert status == 200 and body["managedPid"] == first and body["running"] is True
        assert json.loads(logfile.read_text().splitlines()[0]) == [
            "proxy", "--port", str(compressor_port), "--code-aware", "--disable-kompress",
        ]

        real = ownerfile.read_bytes()
        changed = json.loads(real)
        changed["nonce"] = "00000000-0000-4000-8000-000000000000"
        ownerfile.write_text(json.dumps(changed))
        assert launch("stop")[1]["code"] == "UNVERIFIED_PROCESS"
        assert request(compressor_port, "/health", headers={})[0] == 200
        ownerfile.write_bytes(real)

        finish(gateway)
        assert not gone(first) and request(compressor_port, "/health", headers={})[0] == 200
        gateway = start_gateway()
        assert launch("start")[:2] == (200, {"success": True, "pid": first, "alreadyRunning": True})

        assert call("/api/settings", "PATCH", {"headroomCodeAware": False, "headroomKompress": True})[0] == 200
        code, restarted, _ = launch("restart")
        assert code == 200 and restarted["success"] and restarted["alreadyRunning"] is False
        second = restarted["pid"]
        assert first != second and gone(first)
        assert json.loads(logfile.read_text().splitlines()[1]) == ["proxy", "--port", str(compressor_port)]
        assert call("/api/headroom/status")[1]["managedPid"] == second
        assert launch("stop")[:2] == (200, {"stopped": True, "pid": second})
        assert gone(second) and not pidfile.exists() and not ownerfile.exists()

        launcher.write_text("#!/bin/sh\nexit 42\n")
        status, error, _ = launch("start")
        assert status == 500 and error["code"] == "EARLY_EXIT" and "code=42" in error["error"]
        assert not pidfile.exists() and not ownerfile.exists()
        assert not fallbacks.exists() or not fallbacks.read_text().strip()
        print("Headroom lifecycle passed in isolated PID/network namespaces, including ownership rejection and recovery")
    finally:
        finish(foreign)
        finish(gateway)
