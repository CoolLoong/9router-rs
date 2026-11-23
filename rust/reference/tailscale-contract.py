import fcntl
import http.client
import json
import mmap
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
node = "fixture-machine.example-tailnet.ts.net"
host = node + ":443"
other = node + ":8443"
prefix = "/api/tunnel/tailscale-"

with Path(binary).open("rb") as executable, mmap.mmap(executable.fileno(), 0, access=mmap.ACCESS_READ) as image:
    assert image.find(b"native-funnel.json") >= 0, "The executable does not contain the native Tailscale controller"


def request(port, path, method="GET", payload=None, headers=cli):
    headers = headers.copy()
    if payload is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(payload).encode()
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, path, body=payload, headers=headers)
        response = client.getresponse()
        raw = response.read().decode()
        if (response.getheader("Content-Type") or "").startswith("text/event-stream"):
            events = []
            for block in raw.split("\n\n"):
                if block:
                    kind, value = block.split("\n")
                    events.append((kind.removeprefix("event: "), json.loads(value.removeprefix("data: "))))
            return response.status, events
        return response.status, json.loads(raw)
    finally:
        client.close()


def finish(server):
    if server is None or server.poll() is not None:
        return
    server.terminate()
    try:
        server.wait(timeout=12)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()


with tempfile.TemporaryDirectory(prefix="9router-tailscale-contract-") as folder:
    base = Path(folder)
    commands = base / "bin"
    commands.mkdir()
    for command in ["sh", "dirname", "mktemp", "env", "node", "python3"]:
        (commands / command).symlink_to(shutil.which(command))
    directory = base / "data"
    install = directory / "bin"
    install.mkdir(parents=True)
    fake = install / "tailscale"
    program = textwrap.dedent(r"""
        import json
        import os
        import sys
        from pathlib import Path

        args = sys.argv[1:]
        status_file = Path(os.environ["TAILSCALE_FIXTURE_NODE"])
        endpoints = Path(os.environ["TAILSCALE_FIXTURE_CONFIG"])
        log = Path(os.environ["TAILSCALE_FIXTURE_LOG"])
        assert args[:2] == ["--socket", "/var/run/tailscale/tailscaled.sock"]
        args = args[2:]
        with log.open("a") as output:
            output.write(json.dumps(args) + "\n")
        node = json.loads(status_file.read_text())
        if args == ["status", "--json"]:
            print(json.dumps(node))
            raise SystemExit(0)
        if args == ["serve", "status", "--json"]:
            print(endpoints.read_text())
            raise SystemExit(0)
        assert args[:4] == ["funnel", "--bg", "--https=443", "--set-path=/"], args
        assert len(args) in [5, 6]
        target = args[4]
        dns = node["Self"]["DNSName"].rstrip(".")
        host = dns + ":443"
        cfg = json.loads(endpoints.read_text())
        if len(args) == 6:
            assert args[5] == "off"
            assert cfg["Web"][host]["Handlers"]["/"]["Proxy"] == target
            del cfg["Web"][host]
            del cfg["TCP"]["443"]
            del cfg["AllowFunnel"][host]
        else:
            assert target.startswith("http://127.0.0.1:")
            assert "443" not in cfg.get("TCP", {})
            assert host not in cfg.get("Web", {})
            if node.get("FixtureRequireApproval"):
                print("Funnel is not enabled for this Tailnet")
                print("https://login.tailscale.com/f/funnel-fixture")
                raise SystemExit(1)
            cfg.setdefault("TCP", {})["443"] = {"HTTPS": True}
            cfg.setdefault("Web", {})[host] = {"Handlers": {"/": {"Proxy": target}}}
            cfg.setdefault("AllowFunnel", {})[host] = True
        temp = endpoints.with_suffix(".temp")
        temp.write_text(json.dumps(cfg))
        temp.replace(endpoints)
    """)
    compile(program, str(fake), "exec")
    fake.write_text(f"#!{sys.executable}\n" + program)
    fake.chmod(0o700)
    install_fixture = directory / "tailscale-install-fixture"
    install_fixture.mkdir()
    (install_fixture / "os-release").write_text('ID=cachyos\nID_LIKE="arch"\n')

    def write_installer(name, code):
        script = install_fixture / name
        script.write_text(f"#!{sys.executable}\n" + textwrap.dedent(code))
        script.chmod(0o700)

    write_installer("sudo", """
        from pathlib import Path
        import subprocess
        import sys
        if sys.stdin.readline().strip() != 'fixture-install-password':
            print('Sorry, try again.', file=sys.stderr)
            sys.exit(1)
        assert sys.argv[1:5] == ["-S", "-p", "", "--"]
        assert Path(sys.argv[5]).parent == Path(__file__).parent
        sys.exit(subprocess.run(sys.argv[5:]).returncode)
    """)
    write_installer("pacman", """
        import shutil
        import sys
        from pathlib import Path
        assert sys.argv[1:] == ["-S", "--needed", "--noconfirm", "tailscale"]
        base = Path(__file__).parent
        shutil.copy2(base / "fake-cli", base / "tailscale")
    """)
    write_installer("systemctl", """
        import sys
        from pathlib import Path
        assert sys.argv[1:] == ["start", "tailscaled"]
        Path(__file__).with_name("ready").touch()
    """)
    write_installer("fake-cli", """
        import json
        import sys
        from pathlib import Path
        root = Path(__file__).parent
        args = sys.argv[1:]
        if args == ["--socket", "/var/run/tailscale/tailscaled.sock", "status", "--json"]:
            if not (root / "ready").exists():
                sys.exit(1)
            status = {"BackendState":"NeedsLogin"}
            if (root / "pending").exists():
                status["AuthURL"] = "https://login.tailscale.com/a/fixture123"
            print(json.dumps(status))
        elif args[:3] == ["up", "--accept-routes", "--timeout=3s"] and len(args) == 4 and args[3].startswith("--hostname="):
            (root / "pending").touch()
        else:
            raise AssertionError(args)
    """)
    node_config = base / "node.json"
    config_path = base / "endpoints.json"
    scope = directory / "tailscale" / "native-funnel.json"
    log = base / "commands.jsonl"
    fallback_log = base / "fallbacks.txt"
    logged_in = {"BackendState":"Running","Self":{"ID":"node-fixture-1","Online":True,"DNSName":node+"."}}
    foreign = {
        "TCP": {"8443":{"HTTPS":True}},
        "Web": {other:{"Handlers":{"/":{"Text":"untouched"}}}},
        "AllowFunnel": {other:True},
    }
    node_config.write_text(json.dumps(logged_in))
    config_path.write_text(json.dumps(foreign))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        **os.environ, "PORT":"0", "DATA_DIR":str(directory), "TMPDIR":folder,
        "PATH":str(commands),
        "NINEROUTER_MIGRATION_DATA_DIR":str(directory), "NINEROUTER_MIGRATION_FIXTURE":"1",
        "TAILSCALE_FIXTURE_NODE":str(node_config), "TAILSCALE_FIXTURE_CONFIG":str(config_path),
        "TAILSCALE_FIXTURE_LOG":str(log), "RUST_ANYTHING_FALLBACK_LOG":str(fallback_log),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(name, None)

    def spawn():
        server = subprocess.Popen([binary, "--port", str(port)], cwd=root, env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 15
            while True:
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Tailscale fixture gateway did not start")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.2):
                        return server
                except OSError:
                    time.sleep(.03)
        except BaseException:
            finish(server)
            raise

    def inspect():
        return json.loads(config_path.read_text())

    def action(name, headers=cli):
        return request(port, prefix + name, "GET" if name == "check" else "POST", headers=headers)

    def changes():
        return [command for line in log.read_text().splitlines()
                if (command := json.loads(line))[0] == "funnel"]

    server = None
    try:
        server = spawn()
        assert not log.exists()
        for name in ["check", "enable", "disable"]:
            assert action(name,{})[0] == 403
        assert not log.exists()
        assert request(port, prefix + "install", "POST", {}, {})[0] == 403
        assert request(port, prefix + "install", "POST", {}) == (400,{"error":"Sudo password is required"})
        status, failed = request(port, prefix + "install", "POST", {"sudoPassword":"fixture-wrong-password"})
        assert status == 200 and failed[-1] == ("error",{"error":"Wrong sudo password"})
        assert not (install_fixture / "tailscale").exists()
        status, installed = request(port, prefix + "install", "POST", {"sudoPassword":"fixture-install-password"})
        assert status == 200 and [event for event, _ in installed] == ["progress","progress","progress","done"]
        assert installed[-1][1] == {"success":True,"authUrl":"https://login.tailscale.com/a/fixture123"}
        assert "fixture-install-password" not in json.dumps(installed)
        status, check = action("check")
        assert status == 200 and check == {
            "installed":True,"loggedIn":True,"platform":"linux","brewAvailable":False,
            "daemonRunning":True,"customDaemonRunning":False,"systemDaemonRunning":True,"hasCachedPassword":False,
        }
        assert not changes()
        scope.parent.mkdir(parents=True, exist_ok=True)
        with scope.with_name("native-funnel.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            assert action("enable")[0] == 409
            assert not changes()

        blocked = json.loads(json.dumps(foreign))
        blocked["TCP"]["443"] = {"HTTPS":True}
        target = f"http://127.0.0.1:{port}"
        blocked["Web"][host] = {"Handlers":{"/":{"Proxy":target}}}
        blocked["AllowFunnel"][host] = True
        config_path.write_text(json.dumps(blocked))
        assert action("enable")[0] == 409
        assert action("disable") == (200, {"success":True})
        assert not changes() and inspect() == blocked

        config_path.write_text(json.dumps(foreign))
        required = {**logged_in,"FixtureRequireApproval":True}
        node_config.write_text(json.dumps(required))
        assert action("enable") == (200, {
            "success":False,"funnelNotEnabled":True,"enableUrl":"https://login.tailscale.com/f/funnel-fixture",
        })
        assert inspect() == foreign and not scope.exists()
        login = {**logged_in,"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/fake"}
        node_config.write_text(json.dumps(login))
        assert action("enable") == (200, {"success":False,"needsLogin":True,"authUrl":login["AuthURL"]})
        assert len(changes()) == 1

        node_config.write_text(json.dumps(logged_in))
        opened = {"success":True,"tunnelUrl":"https://"+node}
        assert action("enable") == (200, opened)
        assert action("enable") == (200, opened)
        assert len(changes()) == 2 and scope.exists()
        assert scope.stat().st_mode & 0o777 == 0o600
        status, state = request(port, "/api/settings")
        assert status == 200 and state["tailscaleEnabled"] is True and state["tailscaleUrl"] == opened["tunnelUrl"]
        status, state = request(port, "/api/tunnel/status")
        assert status == 200 and state["tailscale"] == {
            "enabled":True,"settingsEnabled":True,"tunnelUrl":opened["tunnelUrl"],"running":True,"loggedIn":True,
        }

        changed = inspect()
        changed["Web"][host]["Handlers"]["/other"] = {"Text":"foreign handler"}
        config_path.write_text(json.dumps(changed))
        assert action("disable")[0] == 409
        assert inspect() == changed and len(changes()) == 2 and scope.exists()
        del changed["Web"][host]["Handlers"]["/other"]
        config_path.write_text(json.dumps(changed))

        wrong_node = {**logged_in,"Self":{**logged_in["Self"],"ID":"different-node"}}
        node_config.write_text(json.dumps(wrong_node))
        assert action("disable")[0] == 503
        assert inspect() == changed and len(changes()) == 2

        node_config.write_text(json.dumps(logged_in))
        finish(server)
        server = spawn()
        assert action("disable") == (200, {"success":True})
        assert action("disable") == (200, {"success":True})
        assert inspect() == foreign and not scope.exists()
        assert changes() == [
            ["funnel", "--bg", "--https=443", "--set-path=/", target],
            ["funnel", "--bg", "--https=443", "--set-path=/", target],
            ["funnel", "--bg", "--https=443", "--set-path=/", target, "off"],
        ]
        settings = request(port, "/api/settings")[1]
        assert settings["tailscaleEnabled"] is False and settings["tailscaleUrl"] == ""
        status, state = request(port, "/api/tunnel/status")
        assert status == 200 and state["tailscale"]["running"] is False
        assert not fallback_log.exists() or not fallback_log.read_text().strip()
        print("Tailscale local guards, real listener port, durable ownership and scoped Funnel controls passed")
    finally:
        finish(server)
