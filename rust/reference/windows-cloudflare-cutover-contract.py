import contextlib
import http.client
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile


FIXTURES = Path(__file__).resolve().parent
shared = runpy.run_path(str(FIXTURES / "cloudflare-cutover-contract.py"))
AUTH, OLD, NEW, OWNERSHIP = (shared[item] for item in ("AUTH", "OLD", "NEW", "OWNERSHIP"))
free_port, until, mock = (shared[item] for item in ("free_port", "until", "mock"))
prefill, set_settings, read_settings = (shared[item] for item in ("prefill", "set_settings", "read_settings"))


def call(port, path, method="GET", headers=None):
    with contextlib.closing(http.client.HTTPConnection("127.0.0.1", port, timeout=10)) as connection:
        connection.request(method, path, headers={} if headers is None else headers)
        response = connection.getresponse()
        return response.status, json.load(response)


def verify(binary):
    with tempfile.TemporaryDirectory(prefix="9rc.") as directory:
        workspace = Path(directory)
        prefix, home, runtime, fake = [workspace / item for item in ("w", "h", "r", "cloudflared-fixture.exe")]
        home.mkdir()
        runtime.mkdir(mode=0o700)
        subprocess.run(["rustup", "run", "stable", "rustc", "--edition=2024", "--target", "x86_64-pc-windows-gnu",
                        "-C", "linker=x86_64-w64-mingw32-gcc", str(FIXTURES / "windows-cloudflare-fixture.rs"),
                        "-o", str(fake)], check=True, cwd=workspace, timeout=60)
        gateway_port, worker_port, foreign_port = [free_port() for _ in range(3)]
        env = {
            "PATH": os.environ["PATH"], "HOME": str(home), "XDG_RUNTIME_DIR": str(runtime),
            "WINEPREFIX": str(prefix), "WINEARCH": "win64", "WINEDEBUG": "-all", "WINEDLLOVERRIDES": "mscoree,mshtml=",
            "DATA_DIR": r"C:\TunnelState", "HOSTNAME": "127.0.0.1", "FIXTURE_CLOUDFLARED_PORT": str(worker_port),
            "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1",
            "RUST_ANYTHING_FALLBACK_LOG": r"C:\TunnelState\fallback.log",
        }
        with (workspace / "wine.log").open("w+") as output:
            def command(*args, check=True):
                subprocess.run(args, env=env, cwd=workspace, timeout=60, check=check, stdout=output, stderr=output)

            def wine(*args):
                return subprocess.Popen(["wine", *args], env=env, cwd=workspace,
                                        stdin=subprocess.DEVNULL, stdout=output, stderr=output)

            def tunnel(action="status", method="GET", headers=AUTH):
                return call(gateway_port, f"/api/tunnel/{action}", method, headers)

            def start_gateway():
                app = wine(r"C:\Gateway\9router.exe", "--port", str(gateway_port),
                           "--no-browser", "--no-tray", "--skip-update")
                until(lambda: call(gateway_port, "/api/health")[0] == 200, wait=20)
                return app

            def stop_gateway(app):
                assert call(gateway_port, "/api/version/shutdown", "POST", AUTH)[0] == 200
                assert app.wait(timeout=12) == 0

            def worker_stopped():
                try:
                    call(worker_port, "/health")
                except OSError:
                    return True
                return False

            complete = False
            try:
                command("wineboot", "-u")
                disk = prefix / "drive_c"
                install = disk / "Gateway"
                install.mkdir()
                shutil.copyfile(binary, install / "9router.exe")
                shutil.copyfile(fake, install / "cloudflared-fixture.exe")
                data = disk / "TunnelState"
                subprocess.run([sys.executable, str(FIXTURES / "seed.py"), str(data)], check=True,
                               env={"PATH": env["PATH"], "HOME": str(home), "NODE_NO_WARNINGS": "1"})
                state = prefill(data)
                marker = state.with_name("cloudflared.pid")
                set_settings(data, tunnelEnabled=True, tunnelUrl=OLD)
                (data / "bin").mkdir()
                shutil.copyfile(fake, data / "bin/cloudflared.exe")
                assert not marker.exists()
                with mock(data) as upstream:
                    gateway = start_gateway()
                    assert tunnel(headers={})[0] == 401
                    assert tunnel("disable", "POST", {})[0] == 403
                    running = until(lambda: result[1]["tunnel"] if (result := tunnel())[0] == 200
                                    and result[1]["tunnel"]["running"] else None)
                    assert running == {"enabled": True, "settingsEnabled": True, "tunnelUrl": NEW,
                                       "shortId": "mig123", "publicUrl": "https://rmig123.abc-tunnel.us", "running": True}
                    assert upstream.registrations == [{"shortId": "mig123", "tunnelUrl": NEW}]
                    assert call(worker_port, "/health")[1]["pid"] > 0
                    arguments = (data / "cloudflared-arguments.txt").read_text().splitlines()
                    assert arguments[:3] == ["tunnel", "--url", f"http://127.0.0.1:{gateway_port}"]
                    assert json.loads(state.read_text()) == {"shortId": "mig123", "tunnelUrl": NEW, "manager": "rust"}
                    assert tunnel("enable", "POST")[1]["alreadyRunning"] is True
                    assert tunnel("disable", "POST") == (200, {"success": True})
                    until(worker_stopped)
                    assert tunnel()[1]["tunnel"] == dict(running, enabled=False, settingsEnabled=False, tunnelUrl="", running=False)
                    assert json.loads(state.read_text()) == {"shortId": "mig123", "tunnelUrl": None}
                    stop_gateway(gateway)
                    assert read_settings(data) == {"tunnelEnabled": False, "tunnelUrl": ""}

                    state.write_text(json.dumps({"shortId": "mig123", "tunnelUrl": OLD}))
                    set_settings(data, tunnelEnabled=True, tunnelUrl=OLD)
                    foreign = wine(r"C:\Gateway\cloudflared-fixture.exe", "serve", str(foreign_port))
                    pid = until(lambda: call(foreign_port, "/health")[1]["pid"])
                    marker.write_text(str(pid))
                    gateway = start_gateway()
                    saved = tunnel()[1]["tunnel"]
                    assert saved == dict(running, enabled=False, running=False, tunnelUrl=OLD)
                    assert tunnel("enable", "POST") == tunnel("disable", "POST") == OWNERSHIP
                    assert marker.read_text() == str(pid) and call(foreign_port, "/health")[1]["pid"] == pid
                    assert upstream.registrations == [{"shortId": "mig123", "tunnelUrl": NEW}] and worker_stopped()
                    assert json.loads(state.read_text()) == {"shortId": "mig123", "tunnelUrl": OLD}
                    stop_gateway(gateway)
                    assert call(foreign_port, "/quit", "POST")[0] == 200 and foreign.wait(timeout=8) == 0
                    assert read_settings(data) == {"tunnelEnabled": True, "tunnelUrl": OLD}
                    assert not (data / "fallback.log").read_bytes()
                complete = True
            except BaseException:
                output.seek(0)
                print(output.read(), file=sys.stderr)
                raise
            finally:
                if not complete:
                    command("wineserver", "-k", check=False)
                command("wineserver", "-w")
    print("Windows Cloudflare cutover: record restored; stable ID retained; owned PE stopped; foreign PID preserved; zero fallback.")


if __name__ == "__main__":
    verify(Path(sys.argv[1]).resolve(strict=True))
