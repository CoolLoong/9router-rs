from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
route = "/api/cli-tools/claude-settings"
credential_fields = ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_CUSTOM_HEADERS")
secrets = dict(zip(credential_fields, ("offline-cli-existing", "offline-cli-key", "offline-cli-headers")))


def request(port, path, method="GET", body=None, cookie=None):
    headers = {"Content-Type": "application/json"}
    if cookie:
        headers["Cookie"] = cookie
    if body is not None and not isinstance(body, bytes):
        body = json.dumps(body)
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        client.request(method, path, body=body, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read()), response.headers
    finally:
        client.close()


def executable(path, source):
    path.write_text("#!/bin/sh\n" + source + "\n")
    path.chmod(0o700)


def scenario(port, home, commands):
    settings = home / ".claude/settings.json"
    profile = home / ".claude.json"
    profile_keep = {"user":{"theme":"dark"}, "mcpServers":{"keep":{"type":"http","url":"https://mcp.fixture.invalid"}}}
    exa = {"type":"http","url":"https://mcp.exa.ai/mcp"}
    snapshots = {}

    denied = request(port, route)
    assert denied[:2] == (401, {"error":"Unauthorized"}), denied[:2]
    denied = request(port, route, "POST", {"env":{"ANTHROPIC_BASE_URL":"https://unauthorized.fixture.invalid"}})
    assert denied[:2] == (401, {"error":"Unauthorized"}) and not settings.exists()

    status, _, headers = request(port, "/api/auth/login", "POST", {"password":"123456"})
    assert status == 200
    cookie = headers["Set-Cookie"].split(";", 1)[0]

    def check(name, method="GET", body=None, status=200, path=route):
        code, value, headers = request(port, path, method, body, cookie)
        assert code == status and headers["Content-Type"] == "application/json", (name, code, value, headers)
        if "settingsPath" in value:
            assert value["settingsPath"] == str(settings)
            value["settingsPath"] = "~/.claude/settings.json"
        if name == "existing":
            reported = value["settings"]["env"]
            if implementation == "rust":
                assert all(key not in reported for key in credential_fields)
            else:
                assert all(reported.pop(key) == token for key, token in secrets.items())
        snapshots[name] = value
        return value

    assert check("absent") == {"installed":False,"settings":None,"message":"Claude CLI is not installed"}
    assert check("empty-reset", "DELETE")["message"] == "No settings file to reset"
    assert not profile.exists() and not settings.exists()
    executable(commands / "claude", "exit 77")
    assert check("binary") == {"installed":True,"settings":None,"has9Router":False,
                                "exaMcpEnabled":False,"settingsPath":"~/.claude/settings.json"}
    assert check("no-env", "POST", {"env":None}, 400) == {"error":"Invalid env object"}
    assert check("malformed-request", "POST", b"{", 500) == {"error":"Failed to update claude settings"}
    assert check("invalid-base-url", "POST", {"env":{"ANTHROPIC_BASE_URL":37}}, 500) == {
        "error":"Failed to update claude settings"
    }
    assert not profile.exists() and not settings.exists()
    assert check("initial-apply", "POST", {"env":{"ANTHROPIC_BASE_URL":"https://router.fixture.invalid"},
                                           "exaMcpEnabled":True})["success"]
    assert json.loads(settings.read_text()) == {
        "hasCompletedOnboarding":True, "env":{"ANTHROPIC_BASE_URL":"https://router.fixture.invalid/v1"}
    }
    assert json.loads(profile.read_text()) == {"mcpServers":{"exa":exa}}
    if implementation == "rust":
        assert stat.S_IMODE(settings.stat().st_mode) == stat.S_IMODE(profile.stat().st_mode) == 0o600
        assert stat.S_IMODE(settings.parent.stat().st_mode) == 0o700
    assert check("initial-reset", "DELETE")["success"]
    assert json.loads(settings.read_text()) == {"hasCompletedOnboarding":True}
    assert json.loads(profile.read_text()) == {}

    (commands / "claude").unlink()
    profile.write_text(json.dumps(profile_keep))
    settings.write_text('{"theme":"dark","env":{"KEEP":"local",},}')
    assert check("trailing-comma")["settings"] == {"theme":"dark","env":{"KEEP":"local"}}
    regular = {"theme":"dark","env":{"KEEP":"local"} | secrets}
    settings.write_text(json.dumps(regular))
    settings.chmod(0o640)
    profile.chmod(0o640)
    assert check("existing")["settings"] == {"theme":"dark","env":{"KEEP":"local"}}
    body = {
        "env":{
            "ANTHROPIC_BASE_URL":"https://router.fixture.invalid/v1",
            "ANTHROPIC_AUTH_TOKEN":"offline-cli-new",
            "ANTHROPIC_DEFAULT_OPUS_MODEL":"fixture/one",
            "API_TIMEOUT_MS":"10000",
        },
        "exaMcpEnabled":True,
        "autoCompactWindow":88000,
    }
    assert check("apply", "POST", body)["message"] == "Settings updated successfully"
    result = json.loads(settings.read_text())
    assert result == {
        "theme":"dark", "hasCompletedOnboarding":True,
        "env":{
            "KEEP":"local", **secrets, "ANTHROPIC_BASE_URL":"https://router.fixture.invalid/v1",
            "ANTHROPIC_DEFAULT_OPUS_MODEL":"fixture/one", "API_TIMEOUT_MS":"10000",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW":"88000",
        },
    }
    assert json.loads(profile.read_text()) == profile_keep | {"mcpServers":profile_keep["mcpServers"] | {"exa":exa}}
    assert stat.S_IMODE(settings.stat().st_mode) == stat.S_IMODE(profile.stat().st_mode) == 0o640
    assert check("apply-defaults", "POST", {"env":{"ANTHROPIC_DEFAULT_HAIKU_MODEL":"fixture/two"}})["success"]
    assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW" not in json.loads(settings.read_text())["env"]
    assert json.loads(profile.read_text()) == profile_keep
    assert check("apply-exa-again", "POST", {"env":{},"exaMcpEnabled":True})["success"]
    if implementation == "rust":
        statuses = request(port, "/__9router-ui/cli-status?tool=claude", cookie=cookie)
        assert statuses[:2] == (200, {"claude":{"installed":True,"has9Router":True}})
        assert statuses[2]["Cache-Control"] == "no-store"
    assert check("reset", "DELETE")["message"] == "Settings reset successfully"
    preserved = {"KEEP":"local"} | {key:secrets[key] for key in credential_fields[1:]}
    assert json.loads(settings.read_text()) == {"theme":"dark","hasCompletedOnboarding":True,"env":preserved}
    assert json.loads(profile.read_text()) == profile_keep
    snapshots["reset-disk"] = {"settings":{"theme":"dark","hasCompletedOnboarding":True,"env":{"KEEP":"local"}},
                                "profile":profile_keep}

    if implementation == "rust":
        text = '{"theme": "keep , } // /* \\",", /* real */ "env": {"KEEP": "local",}, // trailing\n}'
        settings.write_text(text)
        profile.write_text('/* profile */ {"mcpServers":{"exa":{"type":"http",},},}')
        code, value, _ = request(port, route, cookie=cookie)
        assert code == 200 and value["settings"]["theme"] == 'keep , } // /* ",'
        assert value["exaMcpEnabled"] is True and value["settings"]["env"]["KEEP"] == "local"
        result = request(port, route, "POST", {"env":{"NEXT":"yes"}}, cookie)
        assert result[0] == 200, result[:2]
        assert json.loads(settings.read_text())["env"] == {"KEEP":"local","NEXT":"yes"}
        assert json.loads(profile.read_text()) == {}

        before = settings.read_bytes()
        profile.write_bytes(b'{"mcpServers":')
        assert request(port, route, "POST", {"env":{"UPDATE":"never"}}, cookie)[0] == 500
        assert request(port, route, "DELETE", cookie=cookie)[0] == 500
        assert settings.read_bytes() == before and profile.read_bytes() == b'{"mcpServers":'
        assert not any(file.name.startswith(".tmp") for file in settings.parent.iterdir())
        profile.write_text("{}")
        with ThreadPoolExecutor(max_workers=2) as pool:
            calls = [
                pool.submit(request, port, route, "POST", {"env":{key:"1"}}, cookie)
                for key in ("FIRST", "SECOND")
            ]
            assert all(call.result()[0] == 200 for call in calls)
        env = json.loads(settings.read_text())["env"]
        assert env == {"KEEP":"local","NEXT":"yes","FIRST":"1","SECOND":"1"}

    moved = home / "settings-dotfile.json"
    settings.rename(moved)
    settings.symlink_to(moved)
    assert check("apply-linked-settings", "POST", {"env":{"SYMLINK":"retained"}})["success"]
    assert settings.is_symlink() and json.loads(moved.read_text())["env"]["SYMLINK"] == "retained"
    assert stat.S_IMODE(moved.stat().st_mode) == 0o640

    settings.unlink()
    before = profile.read_bytes()
    assert check("missing-settings", "DELETE")["message"] == "No settings file to reset"
    assert profile.read_bytes() == before
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-claude-settings-") as temporary:
    scratch = Path(temporary)
    home = scratch / "home"
    commands = scratch / "bin"
    data = scratch / "data"
    home.mkdir()
    commands.mkdir()
    executable(commands / "which", 'command -v "$1"')
    executable(commands / "sh", "exit 47")
    subprocess.run(
        [sys.executable, str(root / "rust/reference/seed.py"), str(data)],
        cwd=root, env=dict(os.environ, NODE_NO_WARNINGS="1"), check=True,
    )
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME":str(home), "PATH":str(commands), "PORT":str(port), "HOSTNAME":"127.0.0.1",
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_DATA_DIR":str(data), "NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallback.log"), "MODEL_CATALOG_SYNC":"off",
        "NEXT_TELEMETRY_DISABLED":"1", "JWT_SECRET":"migration-fixture-jwt-secret",
    }
    if implementation == "rust":
        entry = [str(native)]
    else:
        entry = [shutil.which("node"), "--import", str(root / "rust/reference/mock-models.mjs"),
                 str(root / ".next/standalone/custom-server.js")]
    with (scratch / "server.log").open("w") as log:
        server = subprocess.Popen(entry, cwd=root, env=env, start_new_session=True, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 15
            while True:
                try:
                    request(port, "/api/health")
                    break
                except (OSError, json.JSONDecodeError):
                    if server.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError((scratch / "server.log").read_text())
                    time.sleep(0.025)
            result = scenario(port, home, commands)
            if implementation == "rust":
                assert (scratch / "fallback.log").read_text() == ""
            print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        finally:
            try:
                os.killpg(server.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(server.pid, signal.SIGKILL)
                server.wait(timeout=3)
