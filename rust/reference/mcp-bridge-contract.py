import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
sse_route = "/api/mcp/browsermcp/sse"
messages = "/api/mcp/browsermcp/message"
limit = 8 * 1024 * 1024
expected_args = ["-y", "@browsermcp/mcp@latest"]


def request(port, route, method="GET", payload=None, headers=cli, raw=None):
    headers = headers.copy()
    body = raw
    if payload is not None or raw is not None:
        body = raw if raw is not None else json.dumps(payload).encode()
        headers["content-type"] = "application/json"
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=9)
    try:
        client.request(method, route, body=body, headers=headers)
        response = client.getresponse()
        raw = response.read()
        if raw and "application/json" in (response.headers.get("content-type") or ""):
            raw = json.loads(raw)
        return response.status, raw, response.headers
    finally:
        client.close()


class Events:
    def __init__(self, port, headers):
        self.connection = http.client.HTTPConnection("127.0.0.1", port, timeout=7)
        self.connection.request("GET", sse_route, headers=headers)
        self.response = self.connection.getresponse()
        assert self.response.status == 200
        assert self.response.headers["content-type"] == "text/event-stream"
        assert self.response.headers["cache-control"] == "no-cache, no-transform"
        assert self.response.headers["x-accel-buffering"] == "no"
        event, self.route = self.read()
        assert event == "endpoint" and urlsplit(self.route).path == messages
        assert parse_qs(urlsplit(self.route).query)["sessionId"]

    def read(self):
        start = self.response.readline()
        if not start:
            return None
        assert start.startswith(b"event: ")
        data = self.response.readline()
        assert data.startswith(b"data: ") and self.response.readline() == b"\n"
        return start[7:-1].decode(), data[6:-1].decode()

    def rpc(self):
        event, wire = self.read()
        assert event == "message"
        return json.loads(wire)

    def close(self):
        self.response.close()
        self.connection.close()


def gone(pid):
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1]
        return state.startswith(("Z ", "X "))
    except FileNotFoundError:
        return True


def await_gone(pid):
    deadline = time.monotonic() + 8
    while not gone(pid):
        assert time.monotonic() < deadline, "MCP fixture subprocess remained alive"
        time.sleep(.03)


with tempfile.TemporaryDirectory(prefix="9router-mcp-contract-") as folder:
    directory = Path(folder)
    commands = directory / "bin"
    commands.mkdir()
    for name in ["dirname", "env", "mktemp", "node", "python3", "sh"]:
        (commands / name).symlink_to(shutil.which(name))
    npx = commands / "npx"
    child_program = textwrap.dedent(r"""
        import json
        import os
        import subprocess
        import sys
        import time

        if sys.argv[1:] != ["-y", "@browsermcp/mcp@latest"]:
            raise SystemExit(1)
        with open(os.environ["MCP_FIXTURE_LAUNCH_LOG"], "a") as log:
            log.write(json.dumps({"pid": os.getpid(), "args": sys.argv[1:]}) + "\n")

        def send(frame):
            data = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"
            if "🌿".encode() in data:
                position = data.index("🌿".encode()) + 2
                sys.stdout.buffer.write(data[:position])
                sys.stdout.buffer.flush()
                time.sleep(.03)
                sys.stdout.buffer.write(data[position:])
            else:
                sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()

        print("synthetic launcher note", flush=True)
        for raw in sys.stdin.buffer:
            message = json.loads(raw)
            method = message.get("method")
            if method == "fixture/hold":
                descendant = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                                stderr=subprocess.DEVNULL)
                with open(os.environ["MCP_FIXTURE_DESCENDANT_LOG"], "w") as log:
                    log.write(str(descendant.pid))
                time.sleep(30)
                continue
            if method == "fixture/oversize":
                sys.stdout.buffer.write(b"x" * (8 * 1024 * 1024 + 5))
                sys.stdout.buffer.flush()
                continue
            if method == "fixture/snapshot":
                text = "\n".join(f'- element: Fixture text at row {n} **payload** ' + "a"*40 + f" [ref=e{n}]" for n in range(40))
                answer = {"content":[{"type":"text","text":"- generic:\n"+text}]}
            else:
                answer = {"pid":os.getpid(), "tag":message.get("params",{}).get("tag")}
            send({"jsonrpc":"2.0","id":message["id"],"result":answer})
    """)
    compile(child_program, str(npx), "exec")
    npx.write_text(f"#!{sys.executable}\n" + child_program)
    npx.chmod(0o700)
    launcher_log = directory / "worker-log.jsonl"
    descendant_log = directory / "descendant-pid"
    fallback_log = directory / "fallbacks.txt"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    data = directory / "data"
    home = directory / "home"
    home.mkdir()
    env = {
        **os.environ,
        "PORT":str(port), "DATA_DIR":str(data), "TMPDIR":folder, "HOME":str(home),
        "PATH":str(commands), "MCP_FIXTURE_LAUNCH_LOG":str(launcher_log),
        "MCP_FIXTURE_DESCENDANT_LOG":str(descendant_log),
        "NINEROUTER_MIGRATION_FIXTURE":"1", "NINEROUTER_MIGRATION_DATA_DIR":str(data),
        "RUST_ANYTHING_FALLBACK_LOG":str(fallback_log),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(name, None)
    server = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    sessions = []
    try:
        deadline = time.monotonic() + 15
        while True:
            if server.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("MCP fixture gateway did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        def post(route, method, id=1, **params):
            return request(port, route, "POST", {"jsonrpc":"2.0","id":id,"method":method,"params":params})

        assert request(port, sse_route, headers={})[0] == 403
        assert request(port, messages, "POST", {}, {})[0] == 403
        assert request(port, "/api/mcp/not-a-preset/sse")[0] == 404
        assert request(port, sse_route, "HEAD")[0] == 200
        assert not launcher_log.exists()
        assert post(messages, "fixture/init")[0] == 400
        assert post(messages + "?sessionId=00000000-0000-4000-8000-000000000000", "fixture/init")[0] == 404

        status, _, headers = request(port, "/api/auth/login", "POST", {"password":"123456"}, {})
        assert status == 200
        session_headers = {"cookie": headers["set-cookie"].split(";", 1)[0]}
        assert request(port, sse_route, headers={**session_headers, "x-forwarded-for":"192.0.2.15"})[0] == 403
        first = Events(port, session_headers)
        sessions.append(first)
        second = Events(port, cli)
        sessions.append(second)
        assert first.route != second.route

        for stream, tag in [(first, "first 🌿"), (second, "second")]:
            assert post(stream.route, "fixture/init", tag=tag)[0] == 202
            response = stream.rpc()
            assert response["id"] == 1 and response["result"]["tag"] == tag
            stream.pid = response["result"]["pid"]
        assert first.pid != second.pid
        assert {record["pid"] for record in map(json.loads, launcher_log.read_text().splitlines())} == {first.pid,second.pid}
        for line in launcher_log.read_text().splitlines():
            assert json.loads(line)["args"] == expected_args

        assert post(second.route, "fixture/snapshot", id=2)[0] == 202
        snapshot = second.rpc()["result"]["content"][0]["text"]
        assert "- generic" not in snapshot and "[ref=e0]" in snapshot and "[ref=e39]" in snapshot
        assert "[ref=e11]" not in snapshot and "25 similar" in snapshot
        assert post(first.route + "&sessionId=00000000-0000-4000-8000-000000000000", "fixture/init")[0] == 400
        assert request(port, first.route, "POST", raw=b'{"invalid"')[0] == 400
        assert request(port, first.route, "POST", raw=b'"' + b'x' * limit + b'"')[0] == 413

        first.close()
        await_gone(first.pid)
        assert post(first.route, "fixture/init")[0] == 404
        assert not gone(second.pid)

        assert post(second.route, "fixture/oversize")[0] == 202
        assert second.read() is None
        await_gone(second.pid)
        assert post(second.route, "fixture/init")[0] == 404

        pressure = Events(port, cli)
        sessions.append(pressure)
        assert post(pressure.route, "fixture/init", tag="pressure")[0] == 202
        pressure.pid = pressure.rpc()["result"]["pid"]
        assert post(pressure.route, "fixture/hold")[0] == 202
        statuses = [post(pressure.route, "fixture/buffered", padding="a" * 128_000)[0] for _ in range(10)]
        assert 429 in statuses and set(statuses) <= {202,429}
        deadline = time.monotonic() + 5
        while not descendant_log.exists() or not descendant_log.read_text().strip():
            assert time.monotonic() < deadline
            time.sleep(.03)
        descendant = int(descendant_log.read_text())
        assert not gone(descendant)

        assert not fallback_log.exists() or not fallback_log.read_text().strip()
        server.terminate()
        assert server.wait(timeout=10) == 0
        assert gone(pressure.pid)
        await_gone(descendant)
        print("MCP guarded preset, isolated sessions, frame limits, backpressure and child shutdown passed")
    finally:
        for session in sessions:
            session.close()
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
