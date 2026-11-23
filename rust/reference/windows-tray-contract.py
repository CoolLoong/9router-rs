import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time


binary, xvfb, controller = map(lambda arg: str(Path(arg).resolve()), sys.argv[1:])


def endpoint(port, cookie=None, method="GET", path="/api/health", payload=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=4)
    headers = {"content-type":"application/json"} if payload else {}
    if cookie:
        headers["cookie"] = cookie
    try:
        connection.request(method, path, body=payload, headers=headers)
        response = connection.getresponse()
        return response.status, response.read(), response.headers
    finally:
        connection.close()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def ready(process, port):
    deadline = time.monotonic() + 45
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if endpoint(port, path="/api/health")[0] == 200:
                return
        except OSError:
            pass
        time.sleep(.04)
    raise AssertionError("Windows gateway never became reachable")


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=7)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def stop_wineserver(env):
    subprocess.run(["wineserver","-k"],env=env,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    subprocess.run(["wineserver","-w"],env=env,check=True,timeout=10,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)


def winepath(path):
    return "Z:" + str(path).replace("/", "\\")


def launch(env, temporary, display, mode, executable=binary):
    port = free_port()
    data = temporary / ("data-" + mode)
    data.mkdir()
    win_data = winepath(data)
    env = {**env,"DATA_DIR":win_data}
    if display:
        env["DISPLAY"] = display
    else:
        env.pop("DISPLAY", None)
    process = subprocess.Popen(["wine",str(executable),"--tray","--port",str(port),"--no-browser"], env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return process, port


with tempfile.TemporaryDirectory(prefix="9router-win-tray-") as directory:
    temporary = Path(directory)
    display_number = next(number for number in range(270, 875)
                          if not Path(f"/tmp/.X11-unix/X{number}").exists()
                          and not Path(f"/tmp/.X{number}-lock").exists())
    display = f":{display_number}"
    authority = temporary / "Xauthority"
    authority.touch(mode=0o600)
    subprocess.run(["xauth","-f",str(authority),"add",display,"MIT-MAGIC-COOKIE-1",secrets.token_hex(16)], check=True)
    env = {"PATH":os.environ["PATH"],"LANG":"C.UTF-8","DISPLAY":display,
           "XAUTHORITY":str(authority),"WINEPREFIX":str(temporary / "prefix"),
           "WINEDEBUG":"-all","WINEDLLOVERRIDES":"mscoree,mshtml=","BROWSER":"/usr/bin/false"}
    xserver = subprocess.Popen([xvfb,display,"-auth",str(authority),"-screen","0","800x600x24",
                                "-nolisten","tcp","-noreset"], env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    app = None
    try:
        deadline = time.monotonic() + 5
        while xserver.poll() is None and not Path(f"/tmp/.X11-unix/X{display_number}").exists():
            if time.monotonic() >= deadline:
                raise AssertionError("isolated X server never started")
            time.sleep(.02)
        assert xserver.poll() is None
        subprocess.run(["wineboot","-u"],env=env,check=True,timeout=45,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        deployment = temporary / "Installed Router"
        deployment.mkdir()
        executable = deployment / "9Router 启动 & test.exe"
        shutil.copy2(binary, executable)
        app, port = launch(env,temporary,display,"desktop",executable)
        ready(app,port)
        probe = subprocess.run(["wine",controller,"status"],env=env,check=True,timeout=12,
                               capture_output=True,text=True)
        assert "hidden tray window responded" in probe.stdout
        assert endpoint(port,path="/api/health")[0] == 200
        startup_path = subprocess.run(["wine",controller,"startup-path"],env=env,check=True,
                                      timeout=12,capture_output=True,text=True).stdout.strip()
        assert startup_path.startswith("C:\\"), startup_path
        startup = temporary / "prefix/drive_c" / startup_path[3:].replace("\\","/")
        assert startup.is_dir()
        shortcut, script = startup / "9router.lnk", startup / "9router.vbs"

        def toggle():
            subprocess.run(["wine",controller,"autostart"],env=env,check=True,
                           timeout=12,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            assert endpoint(port,path="/api/health")[0] == 200

        assert not shortcut.exists() and not script.exists()
        toggle()
        assert shortcut.is_file() and not script.exists()
        result = subprocess.run(["wine",controller,"shortcut",winepath(shortcut),winepath(executable)],
                                env=env,check=False,timeout=12,capture_output=True,text=True)
        assert result.returncode == 0 and "Unicode shortcut target and launch arguments matched" in result.stdout, (
            result.returncode, result.stdout, result.stderr,
        )
        toggle()
        assert not shortcut.exists()

        legacy = ('Set WshShell = CreateObject("WScript.Shell")\r\n'
                  'WshShell.Run """C:\\Tools\\Node JS\\node.exe"" ""C:\\Apps\\9router\\cli.js"" --tray --skip-update", 0, False\r\n')
        script.write_bytes(legacy.encode())
        toggle()
        assert not script.exists() and not shortcut.exists()
        script.write_bytes(b"' independent setting, never execute this fixture")
        toggle()
        assert script.read_bytes() == b"' independent setting, never execute this fixture" and not shortcut.exists()
        script.unlink()

        subprocess.run(["wine",controller,"quit"],env=env,check=True,timeout=12,
                       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        output,_ = app.communicate(timeout=14)
        assert app.returncode == 0 and "Windows system tray." in output, output
        assert f"http://127.0.0.1:{port}/dashboard" in output, output
        assert "Shutting down from tray..." in output, output

        stop_wineserver(env)
        stop(xserver)
        app, port = launch(env,temporary,None,"headless")
        ready(app,port)
        status,_,headers = endpoint(port,method="POST",path="/api/auth/login",
                                    payload=b'{"password":"123456"}')
        assert status == 200
        cookie = headers["Set-Cookie"].split(";",1)[0]
        status,data,_ = endpoint(port,cookie=cookie,method="POST",path="/api/version/shutdown",payload=b"{}")
        assert status == 200 and json.loads(data)["success"]
        output,_ = app.communicate(timeout=14)
        assert app.returncode == 0 and "The Windows tray is unavailable. The server is still running." in output, output
        print("Windows tray registered, toggled a Unicode Startup shortcut, recognized the old script, kept unrelated entries, and served headlessly.")
    finally:
        if app is not None:
            stop(app)
        stop_wineserver(env)
        stop(xserver)
