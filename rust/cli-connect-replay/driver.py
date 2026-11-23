import errno
import fcntl
import http.server
import json
import os
import pathlib
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import tomllib
from types import SimpleNamespace

import yaml


MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE == "native":
    program, scenario = ARGS
    command = [program]
else:
    scenario, = ARGS
    command = ["node", str(ROOT / "original.cjs")]

PASSWORD = "synthetic-pass"
API_KEY = "sk-connect-fixture-16"
COOKIE = "fixture-session"
LOG = []
KEY_NAME = "cli-fixture-host"
OLD = b'{"env":{"PRESERVE":"old","ANTHROPIC_MODEL":"own-model","ANTHROPIC_AUTH_TOKEN":"old-key","ANTHROPIC_DEFAULT_SONNET_MODEL":"legacy-sonnet",},"kind":"existing settings",}'


MODEL_TOOLS = {
    "codex-settings": {"config": 'model = "gemini-4-argon"\nmodel_provider = "9router"\n'},
    "opencode-settings": {"config": {"model": "9router/gemini-sonnet"}},
    "droid-settings": {"settings": {"customModels": [{"id": "custom:unrelated", "model": "other"}, {"id": "custom:9Router-1", "model": "gemini-lite"}]}},
    "crush-settings": {"config": {"providers": {"9router": {"models": [{"id": "gemini-opus"}]}}}},
    "cline-settings": {"settings": {"actModeApiProvider": "openai", "openAiModelId": "gemini-lite"}},
    "pi-settings": {"config": {"providers": {"9router": {"models": ["gemini-sonnet"]}}}},
}
TOOL_FILES = {
    ".codex/config.toml": 'model = "previous-model"\nmodel_provider = "9router"\n\n[agents]\nmax_threads = 3\nsubagent = "legacy"\n\n[model_providers.local]\nname = "local"\n\n[model_providers.9router]\nbase_url = "http://old.invalid/v1"\n',
    ".config/opencode/opencode.json": '{"permission":"grant","provider":{"9router":{"options":{"extra":true,"apiKey":"old"},"models":{"previous":{"name":"previous"}}},"local":{"name":"local"}},"model":"9router/previous","agent":{"explorer":{"model":"9router/previous"},"other":{"model":"local/other"}}}',
    ".factory/settings.json": '{"theme":"color","customModels":[{"id":"custom:local","index":17,"model":"previous"},{"id":"custom:9Router-legacy","index":18,"model":"old"},{"model":"other"}]}',
    ".config/crush/crush.json": '{"extra":true,"providers":{"9router":{"type":"old"},"local":{"name":"local"}}}',
    ".local/share/kilo/auth.json": '{"9router":{"apiKey":"legacy"},"openai-compatible":{"apiKey":"old"},"other":{"apiKey":"other-fixture-key"}}',
    ".cline/data/globalState.json": '{"actModeApiProvider":"openai","planModeApiProvider":"openai","openAiBaseUrl":"http://old.invalid","openAiModelId":"old","planModeOpenAiModelId":"old","extra":true}',
    ".cline/data/secrets.json": '{"otherSecret":"fake","openAiApiKey":"old"}',
    ".pi/agent/models.json": '{"providers":{"9router":{"extra":true,"models":["old"]},"local":{"models":["other"]}}}',
    ".pi/models.json": '{"providers":{"9router":{"oldLegacy":true},"legacy":{"description":"keep"}}}',
    ".omp/agent/models.yml": "other: keep\nproviders:\n  9router:\n    apiKey: old\n  non_router:\n    baseUrl: http://other.invalid\n",
}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, value, headers=()):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/json")
        for name, header in headers:
            self.send_header(name, header)
        self.end_headers()
        self.wfile.write(body)

    def record(self, method, payload=None):
        cookie = self.headers.get("Cookie")
        auth = self.headers.get("Authorization")
        event = {"method": method, "path": self.path}
        if cookie is not None:
            event["cookie"] = cookie == f"auth_token={COOKIE}"
        if auth is not None:
            event["authorization"] = auth == f"Bearer {API_KEY}"
        if payload is not None:
            event["payload"] = payload
        LOG.append(event)

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        self.record("POST", payload)
        if self.path == "/api/auth/login":
            if scenario in {"bad-login", "saved-login-rejected"}:
                return self.send(403, {"error": "Forbidden\u001b[0m"})
            if scenario == "missing-cookie":
                return self.send(200, {"success": True})
            if scenario == "login-redirect":
                return self.send(302, {"error": "Not a login"}, [("Location", "/unexpected")])
            if scenario == "multi-cookie":
                return self.send(200, {"success": True}, [
                    ("Set-Cookie", "foo=x; Expires=Wed, 01 Jan 2030 00:00:00 GMT; Path=/"),
                    ("Set-Cookie", f"prefix=x; Path=/, auth_token={COOKIE}; Secure; Path=/"),
                ])
            return self.send(200, {"success": True}, [("Set-Cookie", f"auth_token={COOKIE}; HttpOnly; Path=/")])
        if self.path == "/api/keys":
            if scenario == "create-denied":
                return self.send(403, {"error": "not permitted"})
            return self.send(201, {"key": API_KEY})
        return self.send(404, {"error": "unexpected POST"})

    def do_GET(self):
        self.record("GET")
        if self.path == "/api/keys":
            if scenario == "keys-denied":
                return self.send(401, {"error": "not permitted"})
            rows = [{"name": KEY_NAME, "key": "invalid only fixture", "isActive": False}]
            if scenario not in {"new-key", "create-denied"}:
                rows.append({"name": KEY_NAME, "key": API_KEY, "isActive": True})
            return self.send(200, {"keys": rows})
        if self.path == "/api/cli-tools/claude-settings":
            if scenario == "server-settings-unavailable":
                return self.send(503, {"error": "not configured"})
            return self.send(200, {"settings": {"env": {
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gemini-lite",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "gemini-opus",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gemini-sonnet",
                "ANTHROPIC_AUTH_TOKEN": "never copied remote key",
                "ANTHROPIC_BASE_URL": "http://never-copied.invalid",
            }}})
        if self.path.startswith("/api/cli-tools/"):
            route = self.path.rsplit("/", 1)[-1]
            config = MODEL_TOOLS.get(route)
            if scenario in {"tool-inherited", "tool-skipped"} and route == "codex-settings":
                config = {"config": 'model = "bad-selection"\nmodel_provider = "unrelated"\n'}
            if scenario == "tool-skipped" and route == "opencode-settings":
                config = {"config": {"model": "elsewhere/some-model"}}
            if scenario == "tool-inherited" and route == "droid-settings":
                config = {"settings": {"customModels": [{"id": "unrelated", "model": "elsewhere"}]}}
            return self.send(200, config) if config else self.send(404, {"error": "not configured"})
        if self.path == "/v1/models":
            if scenario == "key-rejected":
                return self.send(401, {"error": "key rejected"})
            if scenario == "catalog-unavailable":
                return self.send(404, {"error": "no catalog here"})
            return self.send(200, {"data": [{"id": model} for model in ["gemini-opus", "gemini-sonnet", "gemini-lite", "gemini-4-argon"]]})
        return self.send(404, {"error": "unexpected GET"})


def snapshot(path, structured=False):
    if not path.exists():
        return None
    raw = path.read_text()
    if structured and path.suffix == ".toml":
        content = {"toml": tomllib.loads(raw)}
    elif structured and path.suffix == ".yml" and raw:
        content = {"yaml": yaml.safe_load(raw)}
    else:
        content = {"text": raw}
    return {**content, "mode": oct(path.stat().st_mode & 0o777), "symlink": path.is_symlink()}


def tool_fixture(cwd, endpoint, env):
    home = cwd / "fixture-home"
    files = {name: home / name for name in TOOL_FILES}
    for name, path in files.items():
        if scenario in {"tool-missing", "tool-fresh"} or (scenario == "tool-legacy" and name == ".pi/agent/models.json"):
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(TOOL_FILES[name])
        path.chmod(0o640)
    if scenario == "tool-xdg":
        config_home = cwd / "xdg-config"
        env["XDG_CONFIG_HOME"] = str(config_home)
        file = config_home / "crush/crush.json"
        file.parent.mkdir(parents=True)
        file.write_text('{"outsideDefault":true}')
        files["xdg-config/crush/crush.json"] = file
    if scenario == "tool-reset-empty":
        files[".omp/agent/models.yml"].write_text("providers:\n  9router:\n    apiKey: old\n")
        files[".factory/settings.json"].write_text('{"customModels":[{"id":"custom:9Router-0"}]}')
        files[".config/crush/crush.json"].write_text('{"providers":{"9router":{"models":[]}}}')
        files[".pi/agent/models.json"].write_text('{"providers":{"9router": {"api":"openai-completions"}}}')
        files[".config/opencode/opencode.json"].write_text('{"provider":{"9router":{}},"model":"9router/old","agent":{"explorer":{"model":"9router/old"}}}')
    if scenario == "tool-reset-other":
        files[".codex/config.toml"].write_text('model = "own"\nmodel_provider = "own"\n')
        files[".config/opencode/opencode.json"].write_text('{"model":"own/model","agent":{"explorer":{"model":"own/model"}}}')
        files[".cline/data/globalState.json"].write_text('{"actModeApiProvider":"other","openAiModelId":"keep"}')
        files[".omp/agent/models.yml"].write_text("providers:\n  own: {}\n")
    if scenario == "tool-backup":
        backup = files[".codex/config.toml"].with_suffix(".toml.bak-9router")
        backup.write_text("# original user snapshot\n")
        backup.chmod(0o600)
    if scenario == "tool-symlink":
        config = files[".factory/settings.json"]
        target = home / "actual-factory.json"
        config.rename(target)
        config.symlink_to(target)
        files["actual-factory.json"] = target
    default = [endpoint, "--password", PASSWORD]
    if scenario.startswith("tool-reset") or scenario == "tool-missing":
        return ["--reset", "--tools", "all"], files
    if scenario == "tool-all":
        return default + ["--tools", "all,CLAUDE,droid"], files
    if scenario == "tool-inherited":
        return default + ["--tools", "factory,codex,kilocode"], files
    if scenario == "tool-skipped":
        return default + ["--tools", "codex,omp"], files
    if scenario == "tool-xdg":
        return default + ["--tools", "crush,opencode"], files
    if scenario == "tool-legacy":
        return default + ["--tools", "pi-coding-agent"], files
    if scenario == "tool-omp-only":
        return default + ["--tools", "oh-my-pi"], files
    if scenario == "tool-backup":
        return default + ["--tools", "codex"], files
    if scenario == "tool-symlink":
        return default + ["--tools", "droid"], files
    if scenario == "tool-no-selection":
        return default + ["--tools", " , , "], files
    if scenario == "tool-key-no-model":
        return [endpoint, "--api-key", API_KEY, "--tools", "cline,kilo"], files
    return [endpoint, "--api-key", API_KEY, "--tools", "pi,opencode,cline,codex,crush,omp,factory,kilocode", "--model", "unknown-model", "--print-env"], files


TOOL_PROMPT = b"Select CLI tools to configure"
PASSWORD_PROMPT = b"9router dashboard password"


def run_with_pty(command, cwd, env):
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 150, 0, 0))
    before = termios.tcgetattr(slave)
    out = bytearray()
    os.set_blocking(master, False)

    def read():
        try:
            out.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno not in {errno.EAGAIN, errno.EIO}:
                raise

    def until(marker):
        while marker not in out:
            if not select.select([master], [], [], 3)[0]:
                raise TimeoutError(f"{scenario}: missing PTY marker {marker!r} in {bytes(out)!r}")
            read()

    env["TERM"] = "xterm-256color"
    try:
        with subprocess.Popen(command, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave) as child:
            try:
                if scenario != "tty-password-cancel":
                    until(TOOL_PROMPT)
                if scenario == "tty-chooser-cancel":
                    os.write(master, b"\x03")
                elif scenario == "tty-password-cancel":
                    until(PASSWORD_PROMPT)
                    os.write(master, b"\x1b")
                elif scenario == "tty-multi-reset":
                    os.write(master, b"\x1b[B \x1b[B \r")
                elif scenario == "tty-empty":
                    os.write(master, b"\r")
                    until(b"Select at least one tool")
                    os.write(master, b"\x1b[B \r")
                else:
                    os.write(master, b" \r")
                    until(PASSWORD_PROMPT)
                    os.write(master, b"synthetic-pasx\x7fs\r")
                while child.poll() is None:
                    if not select.select([master], [], [], 3)[0] and child.poll() is None:
                        raise TimeoutError(f"{scenario}: PTY child stalled: {bytes(out)!r}")
                    read()
                read()
                code = child.returncode
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()
        flags = {
            "tools": TOOL_PROMPT in out,
            "password": PASSWORD_PROMPT in out,
            "masked": b"*" * len(PASSWORD) in out,
            "secret_in_output": b"synthetic-pas" in out,
            "reset_terminal": termios.tcgetattr(slave) == before,
        }
        text = bytes(out).decode().replace("\r", "")
        offset = min(
            (index for marker in ("• Logging in", "• Using provided API key", "✅ ", "Cancelled.")
             if (index := text.find(marker)) >= 0),
            default=len(text),
        )
        return SimpleNamespace(stdout=text[offset:], stderr="", returncode=code), flags
    finally:
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="9router-connect-fixture-") as work:
    cwd = pathlib.Path(work)
    config = cwd / "fixture-home/.claude/settings.json"
    backup = config.with_name(f"{config.name}.bak-9router")
    saved = cwd / "fixture-data/connect.env"
    target = cwd / "symlink-target"
    env = {key: value for key, value in os.environ.items() if key not in {"NINE_ROUTER_PASSWORD", "NINE_ROUTER_API_KEY", "XDG_CONFIG_HOME"}}

    if scenario not in {"tool-missing", "tool-fresh", "reset-no-file", "missing-cookie", "keys-denied", "create-denied"}:
        config.parent.mkdir(parents=True)
        if scenario in {"symlink-apply", "symlink-reset"}:
            target.write_bytes(OLD)
            target.chmod(0o640)
            config.symlink_to(target)
        else:
            config.write_bytes(OLD)
            config.chmod(0o644)
    if scenario == "keep-existing-backup":
        backup.write_bytes(b"previous saved settings")
        backup.chmod(0o600)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.02), daemon=True)
    thread.start()
    endpoint = f"http://127.0.0.1:{server.server_port}"
    try:
        args = [endpoint + "/dashboard/ignored", "--tools", "claudecode", "--password", PASSWORD]
        if scenario in {"with-api-key", "key-rejected", "catalog-unavailable"}:
            args = [endpoint, "--api-key", API_KEY, "--sonnet", "gemini-4-argon[1m]", "--fable", "extra-model", "--print-env"]
        if scenario == "password-env":
            args = [endpoint]
            env["NINE_ROUTER_PASSWORD"] = PASSWORD
        if scenario == "new-key":
            args += ["--key-name", "custom-fixture-name", "--save"]
        if scenario == "override-models":
            args += ["--sonnet", "gemini-4-argon[1m]", "--opus", "", "--haiku", "unknown-haiku"]
        if scenario in {"same-origin-saved", "saved-login-rejected", "wrong-origin-saved", "bare-saved"}:
            saved.parent.mkdir(parents=True)
            if scenario == "bare-saved":
                text = f"NINE_ROUTER_SERVER={endpoint}\nNINE_ROUTER_PASSWORD={PASSWORD}\n"
            else:
                server_url = endpoint if scenario != "wrong-origin-saved" else "http://other.invalid"
                text = "NINE_ROUTER_SERVER=" + json.dumps(server_url) + "\nNINE_ROUTER_PASSWORD=" + json.dumps(PASSWORD) + "\n"
            saved.write_text(text)
            saved.chmod(0o600)
            args = [endpoint, "--tools", "claude", "--save"]
        if scenario == "missing-password":
            args = [endpoint]
        if scenario in {"reset-existing", "reset-no-file", "symlink-reset"}:
            args = ["--reset", "--tools", "claude"]
        files = {}
        if scenario.startswith("tool-"):
            args, files = tool_fixture(cwd, endpoint, env)
        tty_result = None
        if scenario.startswith("tty-"):
            if scenario == "tty-password-cancel":
                args = [endpoint, "--tools", "claude"]
            elif scenario == "tty-multi-reset":
                args, files = tool_fixture(cwd, endpoint, env)
                args = ["--reset"]
            elif scenario == "tty-empty":
                args = [endpoint, "--api-key", API_KEY, "--model", "gemini-sonnet"]
            else:
                args = [endpoint]
            child, tty_result = run_with_pty(command + args, cwd, env)
        else:
            child = subprocess.run(command + args, cwd=cwd, env=env, capture_output=True, text=True, timeout=7)
    finally:
        server.shutdown()
        server.server_close()

    def normal(text):
        if text is None:
            return None
        return text.replace(work, "<fixture>").replace(endpoint, "http://127.0.0.1:<port>")
    sys.stdout.write(normal(child.stdout))
    sys.stderr.write(normal(child.stderr))
    normalize = lambda value: normal(json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True))
    concurrent = [i for i, item in enumerate(LOG) if item["path"].rsplit("/", 1)[-1] in MODEL_TOOLS]
    reads = sorted((LOG[i] for i in concurrent), key=lambda item: item["path"])
    for i, request in zip(concurrent, reads):
        LOG[i] = request
    result = {"requests": LOG, "config": snapshot(config), "backup": snapshot(backup), "saved": snapshot(saved), "target": snapshot(target)}
    if tty_result is not None:
        result["tty"] = tty_result
    if files:
        result["tools"] = {
            name: {
                "config": snapshot(path, structured=True),
                "backup": snapshot(path.with_name(path.name + ".bak-9router")),
            }
            for name, path in files.items()
        }
    print(normalize(result))
    raise SystemExit(child.returncode)
