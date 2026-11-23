import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
fake = r"""#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
folder = Path(os.environ["DATA_DIR"])
with (folder / "cloudflare-invocations").open("a") as log:
    log.write(json.dumps({"pid":os.getpid(),"args":sys.argv[1:],
                          "protocol":os.environ["TUNNEL_TRANSPORT_PROTOCOL"]}) + "\n")
os.write(2, b"ignore https://api.trycloudflare.com\nhttps://fixture-")
time.sleep(.03)
os.write(2, b"first.trycloudflare.com\n")
while not (folder / "rotate-url").exists():
    time.sleep(.01)
os.write(1, b"https://fixture-second.trycloudflare.com\n")
while True:
    if (folder / "exit-worker").exists():
        (folder / "exit-worker").unlink()
        sys.exit(7)
    time.sleep(.2)
"""


def request(port, path, method="GET", headers=auth):
    with socket.create_connection(("127.0.0.1", port), timeout=8) as connection:
        message = f"{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n"
        for key, value in headers.items():
            message += f"{key}: {value}\r\n"
        connection.sendall((message + "\r\n").encode())
        response = http.client.HTTPResponse(connection)
        response.begin()
        data = response.read()
        return response.status, json.loads(data)


def until(operation, timeout=4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if result := operation():
                return result
        except (OSError, ValueError):
            pass
        time.sleep(.03)
    raise AssertionError("tunnel fixture did not reach expected state")


with tempfile.TemporaryDirectory(prefix="9router-cloudflare-contract-") as folder:
    data = Path(folder)
    binary_dir = data / "bin"
    binary_dir.mkdir()
    cloudflared = binary_dir / "cloudflared"
    cloudflared.write_text(fake)
    cloudflared.chmod(0o700)
    state_path = data / "tunnel/state.json"
    history = data / "cloudflare-invocations"
    fallbacks = data / "fallback.log"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], check=True)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    env = dict(os.environ, DATA_DIR=folder, PORT=str(port), HOME=folder,
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(fallbacks),
               TUNNEL_TRANSPORT_PROTOCOL="QUIC")
    process = subprocess.Popen([binary, "--no-browser"], cwd=root, env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        until(lambda: request(port, "/api/health", headers={})[0] == 200, timeout=18)
        assert request(port, "/api/tunnel/status", headers={})[0] == 401
        assert request(port, "/api/tunnel/enable", "POST", {})[0] == 403
        result, status = request(port, "/api/tunnel/status")
        assert result == 200 and not status["tunnel"]["enabled"] and not history.exists()
        state_path.parent.mkdir(exist_ok=True)
        state_path.write_text('{"shortId":"fail01","tunnelUrl":null}')
        refused = request(port, "/api/tunnel/enable", "POST")
        assert refused == (500, {"error": "Tunnel registration failed"}), refused
        failed, = [json.loads(row) for row in history.read_text().splitlines()]
        until(lambda: not Path(f"/proc/{failed['pid']}").exists())
        assert json.loads(state_path.read_text()) == {"shortId": "fail01", "tunnelUrl": None}
        state_path.write_text('{"shortId":"test42","tunnelUrl":null}')
        status, enabled = request(port, "/api/tunnel/enable", "POST")
        assert status == 200 and enabled == {
            "success": True, "tunnelUrl": "https://fixture-first.trycloudflare.com",
            "shortId":"test42", "publicUrl":"https://rtest42.abc-tunnel.us",
        }
        _, running = [json.loads(row) for row in history.read_text().splitlines()]
        assert running["protocol"] == "quic"
        assert running["args"][:4] == ["tunnel", "--url", f"http://127.0.0.1:{port}", "--config"]
        assert running["args"][-3:] == ["--no-autoupdate", "--retries", "99"]
        assert request(port, "/api/tunnel/status")[1]["tunnel"]["running"] is True
        assert request(port, "/api/tunnel/enable", "POST")[1]["alreadyRunning"] is True
        assert len(history.read_text().splitlines()) == 2
        (data / "rotate-url").write_text("ready")
        until(lambda: request(port, "/api/tunnel/status")[1]["tunnel"]["tunnelUrl"] ==
              "https://fixture-second.trycloudflare.com")
        with sqlite3.connect(data / "db/data.sqlite") as db:
            saved, = db.execute("SELECT data FROM settings WHERE id=1").fetchone()
        assert json.loads(saved)["tunnelUrl"] == "https://fixture-second.trycloudflare.com"
        (data / "exit-worker").write_text("test")
        until(lambda: len(history.read_text().splitlines()) == 3)
        until(lambda: not Path(f"/proc/{running['pid']}").exists())
        resumed = json.loads(history.read_text().splitlines()[-1])
        until(lambda: request(port, "/api/tunnel/status")[1]["tunnel"]["running"] is True)
        process.terminate()
        assert process.wait(timeout=10) == 0
        until(lambda: not Path(f"/proc/{resumed['pid']}").exists())
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        until(lambda: len(history.read_text().splitlines()) == 4, timeout=18)
        until(lambda: request(port, "/api/tunnel/status")[1]["tunnel"]["running"] is True)
        active = json.loads(history.read_text().splitlines()[-1])
        assert request(port, "/api/tunnel/disable", "POST") == (200, {"success": True})
        until(lambda: not Path(f"/proc/{active['pid']}").exists())
        assert json.loads(state_path.read_text()) == {"shortId":"test42", "tunnelUrl":None}
        assert request(port, "/api/tunnel/status")[1]["tunnel"]["running"] is False
        assert not fallbacks.read_text().strip()
        print("Cloudflare fixture verified authenticated fast path, rollback, URL rotation, recovery and child cleanup")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
