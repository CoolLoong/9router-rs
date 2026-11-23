import fcntl
import json
import os
from pathlib import Path
import runpy
import socket
import sqlite3
import subprocess
import sys
import tempfile


FIXTURES = Path(__file__).resolve().parent
shared = runpy.run_path(str(FIXTURES / "cloudflare-cutover-contract.py"))
gateway, set_settings = (shared[key] for key in ("gateway", "set_settings"))
URL = "https://owned.fixture.ts.net"
INITIAL = {"tailscaleEnabled": True, "tailscaleUrl": URL}


def original(data):
    settings = data / "settings.json"
    settings.write_text(json.dumps(INITIAL))
    env = {"PATH": os.environ["PATH"], "HOME": str(data), "DATA_DIR": str(data),
           "TAILSCALE_FIXTURE_SETTINGS": str(settings), "TAILSCALE_FIXTURE_COMMANDS": str(data / "commands")}
    result = subprocess.run(["node", str(FIXTURES / "tailscale-offline-disable-reference.mjs")],
                            env=env, cwd=data, check=True, text=True, capture_output=True)
    return json.loads(result.stdout)


def saved(data):
    with sqlite3.connect(data / "db/data.sqlite", timeout=3) as db:
        raw, = db.execute("SELECT data FROM settings WHERE id = 1").fetchone()
    settings = json.loads(raw)
    return {key: settings[key] for key in INITIAL}


def native(data, binary):
    subprocess.run([sys.executable, str(FIXTURES / "seed.py"), str(data)], check=True,
                   env=dict(os.environ, NODE_NO_WARNINGS="1"))
    set_settings(data, **INITIAL)
    directory = data / "tailscale"
    directory.mkdir(mode=0o700)
    profile = directory / "native"
    profile.mkdir(mode=0o700)
    identity = profile / "tailscaled.state"
    identity.write_bytes(b"fixture-profile-opaque-data")
    identity.chmod(0o600)
    owner = directory / "native-funnel.json"
    owner.write_text(json.dumps({"version": 1, "backend": "userspace", "node": "owned-fixture-node",
                                 "hostname": "owned.fixture.ts.net", "target": "http://127.0.0.1:20128"}))
    owner_before = owner.read_bytes()
    identity_before = identity.read_bytes()
    lease = profile / "daemon.lock"
    socket_path = directory / "tailscaled.sock"
    with socket.socket(socket.AF_UNIX) as unrelated, gateway(binary, data) as request:
        unrelated.bind(str(socket_path))
        unrelated.listen()
        before = socket_path.lstat().st_ino
        assert request("tailscale-disable", "POST", {})[0] == 403
        with os.fdopen(os.open(lease, os.O_CREAT | os.O_RDWR, 0o600), "wb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            assert request("tailscale-disable", "POST")[0] == 409
            assert saved(data) == INITIAL
        foreign_url = "https://different.fixture.ts.net"
        set_settings(data, tailscaleUrl=foreign_url)
        assert request("tailscale-disable", "POST")[0] == 409
        assert saved(data) == dict(INITIAL, tailscaleUrl=foreign_url)
        set_settings(data, **INITIAL)
        commands = data / "invoked-cli"
        bin_dir = data / "bin"
        bin_dir.mkdir()
        for name in ("tailscale", "tailscaled"):
            command = bin_dir / name
            command.write_text(f"#!{sys.executable}\nfrom pathlib import Path\nPath({str(commands)!r}).touch()\n")
            command.chmod(0o700)
        status, response = request("tailscale-disable", "POST")
        assert status == 200 and response == {"success": True}
        assert request("tailscale-disable", "POST") == (200, response)
        status, body = request()
        assert status == 200
        assert not commands.exists()
        assert socket_path.lstat().st_ino == before
        assert owner.read_bytes() == owner_before and identity.read_bytes() == identity_before
        assert saved(data) == {"tailscaleEnabled": False, "tailscaleUrl": ""}
        return {"response": response, "tailscale": body["tailscale"], "settings": saved(data)}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="9rtso.") as directory:
        data = Path(directory)
        result = original(data) if sys.argv[1] == "original" else native(data, Path(sys.argv[2]).resolve(strict=True))
        print(json.dumps(result, sort_keys=True))
