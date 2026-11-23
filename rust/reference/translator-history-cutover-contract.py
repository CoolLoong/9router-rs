from contextlib import closing, contextmanager
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode


ROOT = Path(__file__).resolve().parents[2]
BUILT = ROOT / ".next/standalone"
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
TARGET = "4_req_target.json"
OPAQUE = "5_res_provider.txt"
PLAIN = "7_res_client.json"
MISSING = "2_req_source.json"
LEGACY = "isolated legacy fixture\n"
NATIVE = "isolated new fixture\n"


def saved_history(application):
    application.mkdir()
    logs = application / ".next/standalone/logs/translator"
    logs.mkdir(parents=True)
    (logs / TARGET).write_text(LEGACY)
    (logs / OPAQUE).write_bytes(b"old fixture \xff")
    plain_logs = application / "logs/translator"
    plain_logs.mkdir(parents=True)
    (plain_logs / PLAIN).write_text("old plain fixture")
    return logs


@contextmanager
def gateway(scratch, original, binary):
    application = scratch / "app"
    logs = saved_history(application)
    standalone = application / ".next/standalone"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    data = scratch / "data"
    empty = scratch / "empty-path"
    empty.mkdir()
    shared = {"PATH": os.environ["PATH"], "HOME": str(scratch), "NODE_NO_WARNINGS": "1"}
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], env=shared, check=True)
    env = {**shared, "DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1", "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "fallback.log")}
    if original:
        for entry in ("server.js", "custom-server.js", "package.json"):
            shutil.copyfile(BUILT / entry, standalone / entry)
        for entry in (".next", "node_modules"):
            (standalone / entry).symlink_to(BUILT / entry, target_is_directory=True)
        command = ["node", str(standalone / "custom-server.js")]
    else:
        env["PATH"] = str(empty)
        command = [str(binary), "--no-browser", "--skip-update"]
    with (scratch / "server.log").open("w+") as log:
        process = subprocess.Popen(command, cwd=application, env=env,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if call(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise RuntimeError("Translator history fixture startup failed")
            yield port, data, logs
        except BaseException:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=7)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)


@contextmanager
def windows_gateway(scratch, binary):
    prefix, home, runtime = (scratch / name for name in ("w", "h", "r"))
    home.mkdir()
    runtime.mkdir(mode=0o700)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "XDG_RUNTIME_DIR": str(runtime),
           "WINEPREFIX": str(prefix), "WINEARCH": "win64", "WINEDEBUG": "-all",
           "WINEDLLOVERRIDES": "mscoree,mshtml=", "DATA_DIR": r"C:\TranslatorState",
           "HOSTNAME": "127.0.0.1", "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1",
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": r"C:\fallback.log"}
    with (scratch / "wine.log").open("w+") as log:
        def wine_command(*arguments, check=True):
            return subprocess.run(arguments, cwd=scratch, env=env, stdout=log, stderr=log,
                                  timeout=60, check=check)

        process = None
        try:
            wine_command("wineboot", "-u")
            drive = prefix / "drive_c"
            application = drive / "LegacyApp"
            logs = saved_history(application)
            data = drive / "TranslatorState"
            subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)],
                           env={"PATH": env["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"}, check=True)
            shutil.copyfile(binary, application / "9router.exe")
            process = subprocess.Popen(["wine", r"C:\LegacyApp\9router.exe", "--port", str(port),
                                        "--no-browser", "--no-tray", "--skip-update"],
                                       cwd=application, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if call(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.05)
            else:
                raise RuntimeError("Windows translator history fixture startup failed")
            yield port, data, logs
            assert call(port, "/api/version/shutdown", {})[0] == 200
            assert process.wait(timeout=12) == 0
            wine_command("wineserver", "-w")
        except BaseException:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            if process is None or process.poll() is None:
                wine_command("wineserver", "-k", check=False)
                wine_command("wineserver", "-w", check=False)


def call(port, path, body=None, headers=ADMIN):
    headers = dict(headers)
    if body is not None:
        headers["Content-Type"] = "application/json"
    with closing(HTTPConnection("127.0.0.1", port, timeout=8)) as request:
        request.request("POST" if body is not None else "GET", path,
                        json.dumps(body) if body is not None else None, headers)
        response = request.getresponse()
        return response.status, json.loads(response.read())


def load(port, name, headers=ADMIN):
    return call(port, "/api/translator/load?" + urlencode({"file": name}), headers=headers)


def cases(port, data, logs, original, windows=False):
    assert load(port, TARGET, {})[0] == 401
    snapshot = {
        "existing": load(port, TARGET),
        "invalid_utf8": load(port, OPAQUE),
        "missing": load(port, MISSING),
        "invalid_name": load(port, "../secret.json"),
    }
    assert snapshot["existing"] == (200, {"success": True, "content": LEGACY}), snapshot
    assert snapshot["invalid_utf8"] == (200, {"success": True, "content": "old fixture \ufffd"}), snapshot
    assert snapshot["missing"] == (404, {"success": False, "error": "File not found"}), snapshot
    assert snapshot["invalid_name"] == (400, {"success": False, "error": "Invalid file name"}), snapshot
    if not original:
        assert not (data / "logs").exists()
        assert load(port, PLAIN) == (200, {"success": True, "content": "old plain fixture"})
    snapshot["save"] = call(port, "/api/translator/save", {"file": TARGET, "content": NATIVE})
    snapshot["reopened"] = load(port, TARGET)
    assert snapshot["save"] == (200, {"success": True}), snapshot
    assert snapshot["reopened"] == (200, {"success": True, "content": NATIVE}), snapshot
    if not original:
        assert (logs / TARGET).read_text() == LEGACY
        assert (data / "logs/translator" / TARGET).read_text() == NATIVE
        outside = data.parent / "outside-history"
        outside.write_text("unrelated test content")
        rejected = "6_res_openai.txt"
        if not windows:
            (logs / rejected).symlink_to(outside)
            assert load(port, rejected)[0] == 500
            (logs / rejected).unlink()
        os.link(outside, logs / rejected)
        assert load(port, rejected)[0] == 500
        (logs / rejected).unlink()
        if not windows:
            (logs / rejected).write_text("untrusted test content")
            (logs / rejected).chmod(0o666)
            assert load(port, rejected)[0] == 500
        assert (data.parent / "fallback.log").read_bytes() == b""
    return snapshot


with tempfile.TemporaryDirectory(prefix="9rth.") as scratch:
    mode = sys.argv[1]
    original = mode == "original"
    binary = None if original else Path(sys.argv[2]).resolve(strict=True)
    launch = windows_gateway(Path(scratch), binary) if mode == "windows" else gateway(Path(scratch), original, binary)
    with launch as (port, data, logs):
        result = cases(port, data, logs, original, mode == "windows")
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
