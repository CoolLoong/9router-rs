import fcntl
import http.client
import json
import os
import select
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
from contextlib import closing
from pathlib import Path


binary = Path(sys.argv[1]).resolve()
api = "/api/tunnel/tailscale-"
token = {"x-9r-cli-token": "68977b6fd9422fbb"}
hostname = "owned.fixture.ts.net"
approval = "https://login.tailscale.com/a/userspacefixture"
mock = textwrap.dedent(r"""
    import json
    import os
    import signal
    import socket
    import sys
    import time
    import traceback
    from pathlib import Path

    base = Path(os.environ["TAILSCALE_USERSPACE_FIXTURE"])
    profile = base / "data/tailscale/native"
    cli = Path(sys.argv[0]).name == "tailscale"
    logs = base / "events"
    hostname = "owned.fixture.ts.net"
    approval = "https://login.tailscale.com/a/userspacefixture"
    host = hostname + ":443"

    def exception(kind, value, trace):
        with (base / "mock.error").open("a") as file:
            traceback.print_exception(kind, value, trace, file=file)

    sys.excepthook = exception

    def emit(*data):
        with logs.open("a") as file:
            file.write(json.dumps(data) + "\n")

    if cli:
        assert sys.argv[1] == "--socket", sys.argv
        path = Path(sys.argv[2])
        args = sys.argv[3:]
        if path == Path("/var/run/tailscale/tailscaled.sock"):
            emit("system", args)
            assert args == ["status", "--json"]
            if (base / "system").exists():
                print(json.dumps({"BackendState":"Running","Self":{"ID":"os-node","Online":True,"DNSName":"os.fixture.ts.net."}}))
                sys.exit(0)
            sys.exit(1)
        assert path.parent.parent == profile and path.name == "ts.sock", path
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(path))
            with client.makefile("rw") as wire:
                wire.write(json.dumps(args) + "\n")
                wire.flush()
                output = json.loads(wire.readline())
        if output.get("approval"):
            print(output["approval"], file=sys.stderr)
            sys.exit(1)
        print(json.dumps(output))
        sys.exit(0)

    opts = dict(argument.split("=", 1) for argument in sys.argv[1:])
    assert set(opts) == {"--tun", "--socket", "--statedir", "--state"}, opts
    assert opts["--tun"] == "userspace-networking"
    assert os.getcwd() == opts["--statedir"] == str(profile)
    assert profile.stat().st_mode & 0o777 == 0o700
    state = Path(opts["--state"])
    assert state == profile / "tailscaled.state"
    if not state.exists():
        state.touch(mode=0o600)
    socket_path = Path(opts["--socket"])
    assert socket_path.parent.parent == profile
    assert socket_path.parent.stat().st_mode & 0o777 == 0o700
    ready_at = time.monotonic() + 0.25
    emit("start", os.getpid(), str(socket_path))

    def stop(signum, _frame):
        emit("stop", os.getpid(), signum)
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    config = base / "funnel.json"
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(socket_path))
        listener.listen()
        while True:
            client, _ = listener.accept()
            with client, client.makefile("rw") as wire:
                args = json.loads(wire.readline())
                emit("private", args)
                stuck = (base / "stuck").exists()
                logged_in = bool(state.read_text())
                if args == ["status", "--json"]:
                    if stuck or time.monotonic() < ready_at:
                        result = {"BackendState":"NoState"}
                    elif logged_in:
                        result = {"BackendState":"Running","Self":{"ID":"owned-node","Online":True,"DNSName":hostname+"."}}
                    else:
                        result = {"BackendState":"NeedsLogin","AuthURL":approval if (profile / "pending").exists() else ""}
                elif args == ["up", "--accept-routes", "--timeout=6s", "--hostname=9rtest"]:
                    assert not stuck and not logged_in and time.monotonic() >= ready_at
                    (profile / "pending").touch()
                    result = {"approval":approval}
                elif args == ["serve", "status", "--json"]:
                    result = json.loads(config.read_text())
                else:
                    assert logged_in
                    assert args[:4] == ["funnel","--bg","--https=443","--set-path=/"], args
                    result = {}
                    if len(args) == 5:
                        assert args[4] == f"http://127.0.0.1:{os.environ['TAILSCALE_USERSPACE_PORT']}"
                        assert json.loads(config.read_text()) == {}
                        result = {"TCP":{"443":{"HTTPS":True}},"Web":{host:{"Handlers":{"/":{"Proxy":args[4]}}}},"AllowFunnel":{host:True}}
                    else:
                        assert len(args) == 6 and args[-1] == "off"
                        assert json.loads(config.read_text())["Web"][host]["Handlers"]["/"]["Proxy"] == args[4]
                    staged = config.with_suffix(".new")
                    staged.write_text(json.dumps(result))
                    staged.replace(config)
                try:
                    wire.write(json.dumps(result) + "\n")
                    wire.flush()
                except BrokenPipeError:
                    pass
""")


def request(port, path, method="GET", headers=token):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=20)) as client:
        client.request(method, path, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())


def wait_for(predicate):
    deadline = time.monotonic() + 12
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError("Timed out waiting for the fixture")
        time.sleep(0.05)


with tempfile.TemporaryDirectory(prefix="ts-owned-") as folder:
    base = Path(folder)
    home = base / "home"
    home.mkdir()
    data = base / "data"
    bin_dir = data / "bin"
    bin_dir.mkdir(parents=True)
    (data / "auth").mkdir()
    (data / "machine-id").write_text("migration-fixture-machine-id\n")
    (data / "auth/cli-secret").write_text("migration-fixture-cli-secret\n")
    (data / "tunnel").mkdir()
    (data / "tunnel/state.json").write_text('{"shortId":"9rtest"}')
    for name in ["tailscale", "tailscaled"]:
        program = bin_dir / name
        compile(mock, str(program), "exec")
        program.write_text(f"#!{sys.executable}\n" + mock)
        program.chmod(0o700)
    profile = data / "tailscale/native"
    profile.mkdir(mode=0o700, parents=True)
    state = profile / "tailscaled.state"
    scope = profile.parent / "native-funnel.json"
    config = base / "funnel.json"
    config.write_text("{}")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "PATH": str(bin_dir), "HOME": str(home), "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
        "XDG_STATE_HOME": str(home / "state"), "XDG_CACHE_HOME": str(home / "cache"),
        "DATA_DIR": str(data), "TMPDIR": folder, "HOST": "127.0.0.1",
        "NINEROUTER_MIGRATION_DATA_DIR": str(data), "NINEROUTER_MIGRATION_FIXTURE": "1",
        "TAILSCALE_USERSPACE_FIXTURE": folder, "TAILSCALE_USERSPACE_PORT": str(port),
    }
    server = None
    errors = base / "gateway.log"

    def start():
        global server
        with errors.open("a") as output:
            server = subprocess.Popen([binary, "--port", str(port)], cwd=base, env=env,
                                      stdout=output, stderr=output)

        def health():
            if server.poll() is not None:
                raise AssertionError(errors.read_text())
            try:
                return request(port, "/api/health")[0] == 200
            except OSError:
                return False

        wait_for(health)

    def finish():
        if server is not None and server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
                raise

    def check():
        status, payload = request(port, api + "check")
        assert status == 200, (status, payload)
        return payload

    def event(kind):
        try:
            return [item for line in (base / "events").read_text().splitlines()
                    if (item := json.loads(line))[0] == kind]
        except FileNotFoundError:
            return []

    foreign = profile / "unrelated.sock"
    with socket.socket(socket.AF_UNIX) as unrelated:
        unrelated.bind(str(foreign))
        unrelated.listen()
        foreign_inode = foreign.stat().st_ino
        try:
            start()
            assert request(port, api + "enable", "POST", {})[0] == 403
            assert not event("start") and check()["daemonRunning"] is False
            assert not event("start")
            ledger = profile / "daemon.lock"
            ledger.touch(mode=0o600)
            with ledger.open("r") as locked:
                fcntl.flock(locked, fcntl.LOCK_EX)
                assert request(port, api + "enable", "POST")[0] == 503
                assert not event("start")
            untouched = base / "external-state"
            untouched.write_text("outside profile")
            state.symlink_to(untouched)
            assert request(port, api + "enable", "POST")[0] == 503
            assert not event("start") and untouched.read_text() == "outside profile"
            state.unlink()

            wait_login = (200, {"success": False, "needsLogin": True, "authUrl": approval})
            actual = request(port, api + "enable", "POST")
            mock_error = base / "mock.error"
            assert actual == wait_login, (
                actual, event("start"), event("private"), errors.read_text(),
                mock_error.read_text() if mock_error.exists() else "",
            )
            assert request(port, api + "enable", "POST") == wait_login
            launched, = event("start")
            assert sum(args[1][0] == "up" for args in event("private")) == 1
            status = check()
            assert status["customDaemonRunning"] and not status["systemDaemonRunning"] and not status["loggedIn"]
            socket_path = Path(launched[2])
            assert socket_path.is_socket() and not scope.exists()
            state.write_text("synthetic authenticated profile")
            (base / "system").touch()
            enabled = (200, {"success": True, "tunnelUrl": "https://" + hostname})
            assert request(port, api + "enable", "POST") == enabled
            assert request(port, api + "enable", "POST") == enabled
            target = f"http://127.0.0.1:{port}"
            assert json.loads(scope.read_text()) == {
                "version": 1, "backend": "userspace", "node": "owned-node",
                "hostname": hostname, "target": target,
            }
            full_status = request(port, "/api/tunnel/status")
            assert full_status[0] == 200 and full_status[1]["tailscale"]["enabled"] is True
            assert check()["loggedIn"]
            finish()
            assert event("stop") == [["stop", launched[1], 15]]
            assert not socket_path.parent.exists()

            config.write_text("{}")
            start()
            wait_for(lambda: json.loads(config.read_text()).get("AllowFunnel") is not None)
            second = event("start")[-1]
            assert second[1:] != launched[1:] and state.read_text() == "synthetic authenticated profile"
            assert request(port, api + "disable", "POST") == (200, {"success": True})
            assert json.loads(config.read_text()) == {} and not scope.exists()
            assert all(args == ["status", "--json"] for _, args in event("system"))
            finish()
            assert event("stop")[-1] == ["stop", second[1], 15]

            (base / "system").unlink()
            (base / "stuck").touch()
            start()
            assert request(port, api + "enable", "POST")[0] == 503
            third = event("start")[-1]
            assert event("stop")[-1] == ["stop", third[1], 15]
            assert not Path(third[2]).parent.exists() and not check()["daemonRunning"]
            (base / "stuck").unlink()
            assert request(port, api + "enable", "POST") == enabled
            fourth = event("start")[-1]
            assert fourth[1:] != third[1:]
            child = os.pidfd_open(fourth[1])
            try:
                server.kill()
                server.wait(timeout=5)
                assert select.select([child], [], [], 5)[0], "Owned child survived gateway SIGKILL"
            finally:
                os.close(child)
            start()
            wait_for(lambda: len(event("start")) == 5)
            wait_for(lambda: request(port, "/api/tunnel/status")[1]["tailscale"]["enabled"])
            assert request(port, api + "disable", "POST") == (200, {"success": True})
            assert foreign.stat().st_ino == foreign_inode
        finally:
            finish()

print("Linux Tailscale userspace login, scoped Funnel, restart, timeout and child-only cleanup passed")
