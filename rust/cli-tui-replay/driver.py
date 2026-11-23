import errno
import fcntl
import hashlib
import http.server
import json
import os
import pathlib
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE == "native":
    binary, scenario = ARGS
    COMMAND = [binary]
else:
    scenario, = ARGS
    COMMAND = ["node", str(ROOT / "original.cjs")]

TOKEN = hashlib.sha256(b"tui-machine9r-cli-authtui-secret").hexdigest()[:16]
KEY = {"id": "key-fixture", "name": "Old Key", "key": "sk-tui-test-fixture", "createdAt": "2026-01-01T00:00:00Z"}
KEYS = [KEY.copy()] if scenario == "keys-delete" else []
CONNECTION = {"id": "provider-fixture", "provider": "openrouter", "name": "Old Connection", "testStatus": "active", "authType": "apikey"}
REQUESTS = []
READS = set()
DENIED = []
COMBO = {"id": "combo-fixture", "name": "Old Combo", "models": ["oc/alpha", "oc/beta"]}


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authorized(self):
        if self.headers.get("x-9r-cli-token") == TOKEN:
            return True
        DENIED.append(self.path)
        self.send(401, {"error": "Unauthorized"})
        return False

    def do_GET(self):
        if not self.authorized():
            return
        READS.add(self.path)
        return self.send(200, {
            "/api/keys": {"keys": KEYS},
            "/api/tunnel/status": {"enabled": False},
            "/api/settings": {"rtkEnabled": len(REQUESTS) == 0, "headroomEnabled": False, "authMode": "password"},
            "/api/providers": {"connections": [CONNECTION]},
            "/api/provider-nodes": {"nodes": []},
            "/api/combos": {"combos": [COMBO]},
            "/v1/models": {"data": []},
            "/api/cli-tools/claude-settings": {"installed": True, "has9Router": True, "settings": {"env": {}}},
        }.get(self.path, {}))

    def mutate(self):
        if not self.authorized():
            return
        content = self.headers.get("content-length", 0)
        body = json.loads(self.rfile.read(int(content))) if content else None
        REQUESTS.append({"method": self.command, "path": self.path, "body": body})
        if self.command == "POST" and self.path == "/api/keys":
            key = dict(KEY, name=body["name"])
            KEYS.append(key)
            return self.send(201, key)
        if self.command == "DELETE" and self.path == "/api/keys/key-fixture":
            KEYS.clear()
        if self.command == "PUT" and self.path == "/api/providers/provider-fixture":
            CONNECTION["name"] = body["name"]
        return self.send(200, {"success": True})

    do_PUT = do_POST = do_PATCH = do_DELETE = mutate


CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
DOWN = b"\x1b[B"
UP = b"\x1b[A"
ENTER = b"\r"
ESC = b"\x1b"


def run(address, home):
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 150, 0, 0))
    before = termios.tcgetattr(slave)
    flags = dict(os.environ, HOME=str(home), DATA_DIR=str(home), TERM="xterm-256color")
    os.set_blocking(master, False)
    output = bytearray()

    def read():
        try:
            output.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno not in {errno.EAGAIN, errno.EIO}:
                raise

    def until(expected, since=0):
        stop = time.monotonic() + 4
        needle = expected.encode()
        while needle not in CSI.sub(b"", output[since:]):
            if time.monotonic() > stop:
                sample = CSI.sub(b"", output[since:])[-1500:].decode(errors="replace")
                raise RuntimeError(f"{scenario}: missing {expected!r} in {sample!r}")
            if select.select([master], [], [], 0.05)[0]:
                read()

    def send(keys, expected=None):
        while select.select([master], [], [], 0.01)[0]:
            read()
        since = len(output)
        os.write(master, keys)
        if expected:
            until(expected, since)

    try:
        with subprocess.Popen(COMMAND + [address], cwd=home, env=flags, stdin=slave, stdout=slave, stderr=slave) as child:
            try:
                if MODE == "native":
                    until("Choose Interface")
                    send(DOWN + ENTER, "9Router Terminal UI")
                else:
                    until("9Router Terminal UI")
                if scenario == "settings-rtk":
                    send(UP + ENTER, "Reset Auth Mode")
                    send(DOWN * 3 + ENTER, "Press Enter to continue")
                    send(ENTER, "Reset Auth Mode")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "keys-create":
                    send(DOWN * 2 + ENTER, "Create New API Key")
                    send(DOWN + ENTER, "Enter key name")
                    send(b"CLI Test Key" + ENTER, "Copy key to clipboard")
                    send(b"n" + ENTER, "Press Enter to continue")
                    send(ENTER, "Create New API Key")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "keys-delete":
                    send(DOWN * 2 + ENTER, "Old Key")
                    send(DOWN * 2 + ENTER, "Delete Key")
                    send(DOWN * 2 + ENTER, "Are you sure")
                    send(b"y" + ENTER, "Press Enter to continue")
                    send(ENTER, "Create New API Key")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "providers-rename":
                    send(DOWN + ENTER, "Custom Providers")
                    send(DOWN * 10 + ENTER, "Add New Connection")
                    send(DOWN * 2 + ENTER, "Rename Connection")
                    send(DOWN + ENTER, "New name")
                    send(b"New Connection" + ENTER, "Press Enter to continue")
                    send(ENTER, "Rename Connection")
                    send(ESC, "Add New Connection")
                    send(ESC, "Providers Management")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "providers-add":
                    send(DOWN + ENTER, "Custom Providers")
                    send(DOWN * 10 + ENTER, "Add New Connection")
                    send(DOWN + ENTER, "Connection Name")
                    send(b"Fixture Conn" + ENTER, "API Key")
                    send(b"provider-fixture-key" + ENTER, "Press Enter to continue")
                    send(ENTER, "Add New Connection")
                    send(ESC, "Providers Management")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "combos-rename":
                    send(DOWN * 3 + ENTER, "Old Combo")
                    send(DOWN * 2 + ENTER, "Edit Combo")
                    send(DOWN + ENTER, "New name")
                    send(b"Renamed Combo" + ENTER, "No connected providers found")
                    send(ESC if MODE == "native" else b"0" + ENTER, "Press Enter to continue")
                    send(ENTER, "Edit Combo")
                    send(ESC, "Create New Combo")
                    send(ESC, "9Router Terminal UI")
                elif scenario == "tools-reset":
                    send(DOWN * 4 + ENTER, "Hermes")
                    send(DOWN + ENTER, "Reset to Default")
                    send(bytes(DOWN * 5) + ENTER, "Press Enter to continue")
                    send(ENTER, "Reset to Default")
                    send(ESC, "CLI Tools")
                    send(ESC, "9Router Terminal UI")
                if MODE == "native":
                    send(ESC, "Choose Interface")
                send(ESC)
                stop = time.monotonic() + 4
                while child.poll() is None:
                    if time.monotonic() > stop:
                        raise RuntimeError(f"{scenario}: terminal did not exit")
                    if select.select([master], [], [], 0.05)[0]:
                        read()
                read()
                code = child.returncode
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
        assert code == 0, CSI.sub(b"", output).decode(errors="replace")
        if MODE == "native":
            assert termios.tcgetattr(slave) == before, "raw terminal was left enabled"
        return CSI.sub(b"", output).decode(errors="replace")
    finally:
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="9router-tui-fixture-") as directory:
    home = pathlib.Path(directory)
    (home / "machine-id").write_text("tui-machine\n")
    auth = home / "auth"
    auth.mkdir(mode=0o700)
    (auth / "cli-secret").write_text("tui-secret\n")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.02), daemon=True)
    thread.start()
    try:
        capture = run(f"127.0.0.1:{server.server_port}", home)
    finally:
        server.shutdown()
        server.server_close()

expected = {
    "settings-rtk": "Token Saver",
    "keys-create": KEY["key"],
    "keys-delete": "API key deleted successfully",
    "providers-rename": "Connection renamed!",
    "providers-add": "Connection created successfully!",
    "combos-rename": "Combo updated!",
    "tools-reset": "Settings reset successfully!",
}
assert expected[scenario].casefold() in capture.casefold()
assert not DENIED, DENIED
if scenario == "providers-add" and MODE == "native":
    assert "provider-fixture-key" not in capture
print(json.dumps({"requests": REQUESTS, "reads": sorted(READS)}, sort_keys=True))
