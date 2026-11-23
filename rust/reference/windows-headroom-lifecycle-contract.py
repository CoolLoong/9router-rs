from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve(strict=True)
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def call(port, path, method="GET", body=None, headers=None):
    headers = cli.copy() if headers is None else headers.copy()
    if body is not None:
        headers["Content-Type"] = "application/json"
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=23)) as client:
        client.request(method, path, None if body is None else json.dumps(body), headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())


def ready(check, timeout=18):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            observed = check()
            if observed:
                return observed
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(.05)
    raise AssertionError("Timed out waiting for isolated Headroom response")


def command(*args):
    return subprocess.run(args, env=env, check=True, timeout=60, stdout=output, stderr=output)


def wine(*args):
    return command("wine", *args)


def app_child(*args):
    return subprocess.Popen(["wine", *args], env=env, cwd=workspace,
                            stdin=subprocess.DEVNULL, stdout=output, stderr=output)


def winpath(path):
    return "Z:" + str(path).replace("/", "\\")


def halt_gateway(gateway):
    assert call(gateway_port, "/api/version/shutdown", "POST")[0] == 200
    assert gateway.wait(12) == 0


def start_gateway():
    gateway = app_child(r"C:\Gateway\9router.exe", "--port", str(gateway_port),
                        "--no-browser", "--no-tray", "--skip-update")
    ready(lambda: call(gateway_port, "/api/health")[0] == 200)
    return gateway


def lifecycle():
    state = disk / "HeadroomState" / "headroom"
    pidfile, ownerfile, logfile = [state / item for item in ("proxy.pid", "proxy.owner.json", "proxy.log")]
    source = disk / "Fixture tools" / "headroom.exe"

    def action(name, headers=None):
        return call(gateway_port, f"/api/headroom/{name}", "POST", headers=headers)

    def current():
        status, data = call(gateway_port, "/api/headroom/status")
        assert status == 200, data
        return data

    def configure(**fields):
        status, observed = call(gateway_port, "/api/settings", "PATCH", fields)
        assert status == 200, observed

    def worker():
        return call(headroom_port, "/health", headers={})[1]["fixturePid"]

    def assert_unavailable():
        try:
            call(headroom_port, "/health", headers={})
        except (OSError, http.client.HTTPException):
            return
        raise AssertionError("Stopped Headroom child is still listening")

    gateway = start_gateway()
    for operation in ("start", "stop", "restart"):
        assert action(operation, {}) == (403, {"error": "Local only: CLI token required"})
    assert action("start")[1]["code"] == "NOT_INSTALLED"
    assert action("stop") == (409, {"stopped": False, "reason": "not_running"})
    shutil.copyfile(fake, source)
    assert current()["path"].endswith(r"Fixture tools\headroom.exe"), current()
    configure(headroomUrl="https://example.invalid:1234")
    for operation in ("start", "restart"):
        status, data = action(operation)
        assert status == 400 and data["code"] == "EXTERNAL_PROXY"
    configure(headroomUrl=f"http://127.0.0.1:{headroom_port}", headroomCodeAware=True, headroomKompress=False)

    foreign = app_child(r"C:\Fixture tools\headroom.exe", "serve", str(foreign_port))
    foreign_pid = ready(lambda: call(foreign_port, "/health", headers={})[1]["fixturePid"])
    wine(r"C:\Fixture tools\headroom.exe", "write-pid", r"C:\HeadroomState\headroom\proxy.pid", str(foreign_pid))
    for operation in ("start", "stop", "restart"):
        status, data = action(operation)
        assert status == 409 and data["code"] == "UNVERIFIED_PROCESS", data
    assert call(foreign_port, "/health", headers={})[1]["fixturePid"] == foreign_pid
    assert current()["managedPid"] is None
    assert call(foreign_port, "/quit", "POST", headers={})[0] == 200
    assert foreign.wait(8) == 0
    assert action("stop") == (409, {"stopped": False, "reason": "not_running"})
    assert not pidfile.exists()

    source.write_text("Not a portable executable")
    status, failed = action("start")
    assert status == 500 and failed["code"] == "SPAWN_FAILED", failed
    assert not pidfile.exists() and not ownerfile.exists()
    shutil.copyfile(fake, source)
    marker = source.with_name("exit-early")
    marker.touch()
    status, failed = action("start")
    assert status == 500 and failed["code"] == "EARLY_EXIT" and "code=42" in failed["error"], failed
    marker.unlink()
    assert not pidfile.exists() and not ownerfile.exists()

    with ThreadPoolExecutor(max_workers=2) as concurrent:
        responses = list(concurrent.map(action, ["start", "start"]))
    assert all(status == 200 and result["success"] for status, result in responses), responses
    pids = {item["pid"] for _, item in responses}
    assert len(pids) == 1
    first = pids.pop()
    assert {result["alreadyRunning"] for _, result in responses} == {True, False}
    assert int(pidfile.read_text()) == first
    first_worker = worker()
    assert first != first_worker and current()["managedPid"] == first
    assert logfile.read_text().splitlines() == [
        f"proxy|--port|{headroom_port}|--code-aware|--disable-kompress",
    ]

    record = ownerfile.read_text()
    for field, value in (("nonce", "00000000-0000-4000-8000-000000000000"), ("creationTime", 1)):
        tampered = json.loads(record)
        if field == "nonce":
            tampered[field] = value
        else:
            tampered["process"][field] = value
        ownerfile.write_text(json.dumps(tampered))
        assert action("stop")[1]["code"] == "UNVERIFIED_PROCESS"
        assert current()["managedPid"] is None and worker() == first_worker
    ownerfile.write_text(record)

    halt_gateway(gateway)
    assert worker() == first_worker
    gateway = start_gateway()
    assert current()["managedPid"] == first
    assert action("start") == (200, {"success": True, "pid": first, "alreadyRunning": True})

    configure(headroomCodeAware=False, headroomKompress=True)
    status, result = action("restart")
    assert status == 200 and result["success"] and not result["alreadyRunning"], result
    second = result["pid"]
    assert second != first and worker() != first_worker
    assert logfile.read_text().splitlines()[-1] == f"proxy|--port|{headroom_port}"
    assert action("stop") == (200, {"stopped": True, "pid": second})
    assert not pidfile.exists() and not ownerfile.exists()
    assert_unavailable()
    assert current()["managedPid"] is None
    halt_gateway(gateway)
    assert (disk / "HeadroomState" / "fallback.log").read_bytes() == b""


with tempfile.TemporaryDirectory(prefix="9rh.", dir="/tmp") as directory:
    workspace = Path(directory)
    prefix, home, runtime, fake = [workspace / item for item in ("w", "h", "r", "headroom.exe")]
    home.mkdir()
    runtime.mkdir(mode=0o700)
    compile = ["rustup", "run", "stable", "rustc", "--edition=2024", "--target", "x86_64-pc-windows-gnu",
               "-C", "linker=x86_64-w64-mingw32-gcc", str(root / "rust/reference/windows-headroom-fixture.rs"),
               "-o", str(fake)]
    subprocess.run(compile, check=True, timeout=60, cwd=workspace)
    gateway_port, headroom_port, foreign_port = (port() for _ in range(3))
    env = {
        "HOME": str(home), "PATH": os.environ["PATH"], "XDG_RUNTIME_DIR": str(runtime),
        "WINEPREFIX": str(prefix), "WINEARCH": "win64", "WINEDEBUG": "-all", "WINEDLLOVERRIDES": "mscoree,mshtml=",
        "WINEPATH": r"C:\Fixture tools", "DATA_DIR": r"C:\HeadroomState", "HOSTNAME": "127.0.0.1",
        "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1",
        "RUST_ANYTHING_FALLBACK_LOG": r"C:\HeadroomState\fallback.log",
    }
    with (workspace / "wine.log").open("w+") as output:
        try:
            command("wineboot", "-u")
            disk = prefix / "drive_c"
            (disk / "Gateway").mkdir()
            (disk / "Fixture tools").mkdir()
            shutil.copyfile(binary, disk / "Gateway" / "9router.exe")
            subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(disk / "HeadroomState")],
                           env={"PATH": env["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"}, check=True)
            lifecycle()
        except BaseException:
            output.seek(0)
            print(output.read(), file=sys.stderr)
            raise
        finally:
            command("wineserver", "-k")
            command("wineserver", "-w")
print("Windows Headroom verified through the actual gateway: owned Job, flags, restart recovery and outsider PID refusal.")
