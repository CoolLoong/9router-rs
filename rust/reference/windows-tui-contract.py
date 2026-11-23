import http.client
import os
import secrets
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

binary, xvfb, probe = (Path(value).resolve() for value in sys.argv[1:])


def windows(path):
    return "Z:" + str(path).replace("/", "\\")


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def free_port():
    with socket.socket() as socket_:
        socket_.bind(("127.0.0.1", 0))
        return socket_.getsockname()[1]


def healthy(port):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        client.request("GET", "/api/health")
        response = client.getresponse()
        return response.status == 200
    finally:
        client.close()


with tempfile.TemporaryDirectory(prefix="9router-windows-tui-") as directory:
    root = Path(directory)
    number = next(
        number for number in range(270,875)
        if not Path(f"/tmp/.X11-unix/X{number}").exists() and not Path(f"/tmp/.X{number}-lock").exists()
    )
    display = f":{number}"
    authority = root / "Xauthority"
    authority.touch(mode=0o600)
    subprocess.run(["xauth","-f",str(authority),"add",display,"MIT-MAGIC-COOKIE-1",secrets.token_hex(16)],check=True)
    env = {"PATH":os.environ["PATH"],"LANG":"C.UTF-8","DISPLAY":display,"XAUTHORITY":str(authority),
           "WINEPREFIX":str(root / "wine"),"WINEDEBUG":"-all","WINEDLLOVERRIDES":"mscoree,mshtml=",
           "NINEROUTER_MIGRATION_FIXTURE":"1","BROWSER":"/usr/bin/false"}
    xserver = subprocess.Popen([str(xvfb),display,"-auth",str(authority),"-screen","0","900x600x24",
                                "-nolisten","tcp","-noreset"],env=env,
                               stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    child = None
    try:
        deadline = time.monotonic() + 5
        while xserver.poll() is None and not Path(f"/tmp/.X11-unix/X{number}").exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("isolated X display did not start")
            time.sleep(.02)
        assert xserver.poll() is None
        subprocess.run(["wineboot","-u"],env=env,check=True,timeout=48,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for mode, reports in [
            ("exit",["menu","terminal-and-exit","restored-and-stopped"]),
            ("hide",["menu","same-process-detached","restored-and-stopped"]),
            ("tray",["explicit-tray","restored-and-stopped"]),
        ]:
            data, report, allow = root / f"data-{mode}", root / f"report-{mode}", root / f"allow-{mode}"
            data.mkdir()
            port = free_port()
            child = subprocess.Popen(["wineconsole",windows(probe),windows(binary),str(port),mode,windows(report),windows(allow)],
                                    env={**env,"DATA_DIR":windows(data)},stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            if mode == "hide":
                deadline = time.monotonic() + 22
                while child.poll() is None and time.monotonic() < deadline:
                    if report.exists() and "same-process-detached" in report.read_text():
                        break
                    time.sleep(.05)
                assert report.exists() and "same-process-detached" in report.read_text(), (
                    child.poll(), report.read_text() if report.exists() else "no probe output",
                )
                assert healthy(port)
                entries = list((root / "wine/drive_c/users").glob(
                    "*/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/*/9router.lnk"
                ))
                assert len(entries) == 1, entries
                allow.touch()
            code = child.wait(timeout=30)
            assert code == 0 and report.read_text().splitlines() == reports, (
                mode, code, report.read_text() if report.exists() else "no probe output",
            )
        print("Wine Console TUI navigated the interface, exited cleanly, and hid the same serving process to tray.")
    finally:
        if child is not None:
            stop(child)
        subprocess.run(["wineserver","-k"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        subprocess.run(["wineserver","-w"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        stop(xserver)
