import contextlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "rust/reference"
AUTH = {"x-9r-cli-token": "68977b6fd9422fbb"}
OLD = "https://fixture-legacy.trycloudflare.com"
NEW = "https://fixture-cutover.trycloudflare.com"
OWNERSHIP = (409, {"error": "Existing Quick Tunnel ownership could not be verified"})
FAKE = f'''#!{sys.executable}
import os
from pathlib import Path
import time

data = Path(os.environ["DATA_DIR"])
with (data / "fake-attempts").open("a") as trace:
    print(os.getpid(), file=trace)
print("{NEW}", flush=True)
while True:
    time.sleep(.1)
'''


def until(probe, wait=12):
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        try:
            if result := probe():
                return result
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.025)
    raise AssertionError("fixture did not reach expected state")


def prefill(data):
    state = data / "tunnel/state.json"
    state.parent.mkdir()
    state.write_text(json.dumps({"shortId": "mig123", "tunnelUrl": OLD}))
    return state


def original(data, scenario):
    prefill(data)
    (data / "fixture-settings.json").write_text(json.dumps({"tunnelEnabled": True, "tunnelUrl": OLD}))
    env = {"PATH": os.environ["PATH"], "HOME": str(data), "DATA_DIR": str(data)}
    completed = subprocess.run(["node", str(FIXTURES / "cloudflare-cutover-reference.mjs"), scenario],
                               env=env, cwd=data, text=True, check=True, capture_output=True)
    return json.loads(completed.stdout)


def set_settings(data, **fields):
    with sqlite3.connect(data / "db/data.sqlite", timeout=3) as db:
        row = db.execute("SELECT data FROM settings WHERE id = 1").fetchone()
        settings = dict(json.loads(row[0]) if row else {}, **fields)
        db.execute("INSERT INTO settings(id,data) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data", (json.dumps(settings),))


def read_settings(data):
    with sqlite3.connect(data / "db/data.sqlite", timeout=3) as db:
        row, = db.execute("SELECT data FROM settings WHERE id = 1").fetchone()
        value = json.loads(row)
    return {key: value[key] for key in ("tunnelEnabled", "tunnelUrl")}


def free_port():
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        return reserve.getsockname()[1]


def dead_fixture_pid():
    process = subprocess.Popen([sys.executable, "-c", "pass"])
    assert process.wait(timeout=5) == 0
    assert not Path(f"/proc/{process.pid}").exists()
    return process.pid


class Worker(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        assert self.path == "/cloudflare-quick/api/tunnel/register"
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert request == {"tunnelUrl": NEW, "shortId": "mig123"}
        self.server.registrations.append(request)
        self.send_response(503 if self.server.scratch.joinpath("fail-register").exists() else 204)
        self.end_headers()

    def do_GET(self):
        assert self.path == "/cloudflare-quick/api/health"
        self.send_response(200)
        self.end_headers()


@contextlib.contextmanager
def mock(data):
    with ThreadingHTTPServer(("127.0.0.1", 52989), Worker) as server:
        server.scratch = data
        server.registrations = []
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            yield server
        finally:
            server.shutdown()
            thread.join()


@contextlib.contextmanager
def gateway(binary, data):
    port = free_port()
    fallback = data / "fallback.log"
    env = {"PATH": os.environ["PATH"], "HOME": str(data), "DATA_DIR": str(data), "PORT": str(port),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off", "RUST_ANYTHING_FALLBACK_LOG": str(fallback)}
    with (data / "gateway.log").open("w") as log:
        process = subprocess.Popen([binary, "--no-browser"], env=env, cwd=data, stdout=log, stderr=subprocess.STDOUT)

    def call(path="status", method="GET", headers=AUTH):
        url = f"http://127.0.0.1:{port}/api/tunnel/{path}" if path != "/health" else f"http://127.0.0.1:{port}/api/health"
        request = urllib.request.Request(url, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=6) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as failure:
            return failure.code, json.load(failure)

    try:
        until(lambda: call("/health", headers={})[0] == 200)
        yield call
        assert not fallback.read_text().strip()
    finally:
        if process.poll() is None:
            process.terminate()
            assert process.wait(timeout=10) == 0, (data / "gateway.log").read_text()


def native(data, binary, scenario):
    subprocess.run([sys.executable, str(FIXTURES / "seed.py"), str(data)], check=True,
                   env=dict(os.environ, NODE_NO_WARNINGS="1"))
    state = prefill(data)
    marker = state.with_name("cloudflared.pid")
    set_settings(data, tunnelEnabled=True, tunnelUrl=OLD)
    if scenario in {"restore", "retry", "unverified"}:
        bin_path = data / "bin/cloudflared"
        bin_path.parent.mkdir()
        bin_path.write_text(FAKE)
        bin_path.chmod(0o700)
    output = {}
    with mock(data) as upstream:
        if scenario == "unverified":
            return unverified(data, binary, marker, upstream)
        if scenario in {"restore", "retry"}:
            marker.write_text(str(dead_fixture_pid()))
        blocked = data / "fail-register"
        if scenario == "retry":
            blocked.touch()
        with gateway(binary, data) as request:
            if scenario == "retry":
                until(lambda: upstream.registrations)
                trace = data / "fake-attempts"
                failed_pid = int(trace.read_text().splitlines()[0])
                until(lambda: not Path(f"/proc/{failed_pid}").exists())
                assert json.loads(state.read_text()) == {"shortId": "mig123", "tunnelUrl": OLD}
                assert read_settings(data) == {"tunnelEnabled": True, "tunnelUrl": OLD}
                assert request()[1]["tunnel"]["running"] is False
                blocked.unlink()
            if scenario in {"restore", "retry"}:
                def running():
                    status, body = request()
                    return body["tunnel"] if status == 200 and body["tunnel"]["running"] else None

                output["resumed"] = until(running)
                persisted = json.loads(state.read_text())
                assert persisted == {"shortId": "mig123", "tunnelUrl": NEW, "manager": "rust"}
                assert not marker.exists()
                registrations = len(upstream.registrations)
                assert registrations == 1 if scenario == "restore" else registrations >= 2
                active_pid = int((data / "fake-attempts").read_text().splitlines()[-1])
                status, output["reused"] = request("enable", "POST")
                assert status == 200
            else:
                status, body = request()
                assert status == 200
                output["saved"] = body["tunnel"]
                assert not upstream.registrations
            status, output["disabled"] = request("disable", "POST")
            assert status == 200
            if scenario in {"restore", "retry"}:
                until(lambda: not Path(f"/proc/{active_pid}").exists())
            status, body = request()
            assert status == 200
            output["current"] = body["tunnel"]
            output["state"] = json.loads(state.read_text())
            output["settings"] = read_settings(data)
        return output


def unverified(data, binary, marker, upstream):
    set_settings(data, tailscaleEnabled=True, tailscaleUrl="https://fixture.example.ts.net")
    with subprocess.Popen([sys.executable, "-c", "import time; time.sleep(45)"]) as unrelated:
        try:
            marker.write_text(str(unrelated.pid))
            with gateway(binary, data) as request:
                assert request("status", headers={})[0] == 401
                assert request("enable", "POST", {})[0] == 403
                status, body = request()
                assert status == 200 and body["tunnel"]["settingsEnabled"] and not body["tunnel"]["running"]
                assert body["tailscale"]["settingsEnabled"] and not body["tailscale"]["running"]
                assert request("enable", "POST") == request("disable", "POST") == OWNERSHIP
                assert marker.read_text() == str(unrelated.pid) and unrelated.poll() is None
                target = data / "marker-target"
                target.write_text(str(dead_fixture_pid()))
                replacement = data / "marker-link"
                replacement.symlink_to(target)
                replacement.replace(marker)
                assert request("enable", "POST") == request("disable", "POST") == OWNERSHIP
                assert marker.is_symlink() and target.is_file()
                set_settings(data, tunnelUrl="https://fixture-mismatch.trycloudflare.com")
                marker.unlink()
                assert request("enable", "POST") == request("disable", "POST") == OWNERSHIP
                assert not (data / "fake-attempts").exists() and not upstream.registrations
        finally:
            if unrelated.poll() is None:
                unrelated.terminate()
                unrelated.wait(timeout=5)
    return {"legacyLivePreserved": True, "symlinkIntact": True, "inconsistentStateRejected": True}


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="9router-cloudflare-cutover-") as folder:
        data = Path(folder)
        if sys.argv[1] == "original":
            output = original(data, sys.argv[2])
        else:
            output = native(data, Path(sys.argv[2]).resolve(strict=True), sys.argv[3])
        print(json.dumps(output, sort_keys=True))
