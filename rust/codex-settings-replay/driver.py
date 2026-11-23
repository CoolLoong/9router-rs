import http.client
import json
import os
import pathlib
import select
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib

MODE, *arguments = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE in {"native", "strict"}:
    binary, CASE = arguments
    COMMAND = [binary]
else:
    CASE, = arguments
    COMMAND = [shutil.which("node"), "--no-warnings", str(ROOT / "original.mjs")]

CURRENT = "fixture-selected-key"
PREVIOUS = "fixture-previous-key"
LEGACY = "fixture-legacy-key"
OAUTH = "fixture-existing-auth-token"
MANAGED = f'''model = "previous"
model_provider = "9router"
theme = "plain"

[model_providers.custom]
base_url = "https://custom.invalid/v1"

[model_providers.9router]
base_url = "https://previous.invalid/v1"

[model_providers.9router.http_headers]
Authorization = "Bearer {PREVIOUS}"

[agents]
default_subagent_model = "previous-subagent"
max_threads = 6

[agents.subagent]
model = "previous-legacy-subagent"

[agents.reviewer]
description = "keep this role"

[features]
multi_agent = true
'''
LEGACY_AUTH = {"auth_mode": "apikey", "OPENAI_API_KEY": LEGACY, "tokens": {"access_token": OAUTH}}
PAYLOAD = {"baseUrl": "https://gateway.invalid:20128/relay", "apiKey": CURRENT, "model": "new-primary"}

def snapshot(file, parser):
    if not file.is_file():
        return None
    raw = file.read_text()
    try:
        return parser(raw)
    except ValueError:
        return raw

def redact(output, home):
    text = json.dumps(output, sort_keys=True)
    for original, replacement in (
        (str(home), "<home>"),
        (CURRENT, "<selected>"),
        (PREVIOUS, "<previous>"),
        (LEGACY, "<legacy>"),
        (OAUTH, "<existing-oauth>"),
    ):
        text = text.replace(original, replacement)
    return text

def request(port, method, endpoint, payload=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    headers = {"Content-Type": "application/json"}
    if not CASE.endswith("-unauthorized"):
        headers["x-replay-admin"] = "fixture"
    try:
        encoded = json.dumps(payload) if isinstance(payload, (dict, list)) else payload
        connection.request(method, f"/api/cli-tools/codex-{endpoint}", body=encoded, headers=headers)
        response = connection.getresponse()
        return {"status": response.status, "body": json.loads(response.read())}
    finally:
        connection.close()

def listener(process):
    deadline = time.monotonic() + 5
    result = bytearray()
    while time.monotonic() < deadline:
        if not select.select([process.stdout], [], [], 1)[0]:
            continue
        chunk = os.read(process.stdout.fileno(), 4096)
        if not chunk:
            break
        result.extend(chunk)
        for line in result.split(b"\n")[:-1]:
            if line.startswith(b"ready "):
                return int(line.split()[1])
    raise RuntimeError("Fixture launcher did not become ready")

with tempfile.TemporaryDirectory(prefix="9router-codex-settings-") as directory:
    home = pathlib.Path(directory)
    commands = home / "bin"
    commands.mkdir()
    config_dir = home / ".codex"
    config = config_dir / "config.toml"
    auth = config_dir / "auth.json"
    target = config
    if CASE == "get-installed":
        fake_codex = commands / "codex"
        fake_codex.write_text("#!/bin/sh\nexit 0\n")
        fake_codex.chmod(0o700)
    if CASE in {"get-managed", "get-unmanaged", "apply-preserve", "apply-malformed", "apply-symlink",
                "reset-managed", "reset-unmanaged", "reset-auth-empty", "reset-malformed", "settings-unauthorized"}:
        config_dir.mkdir()
        raw = MANAGED
        if CASE in {"get-unmanaged", "reset-unmanaged"}:
            raw = MANAGED.replace('model_provider = "9router"', "model_provider = 'custom'")
            if CASE == "get-unmanaged":
                raw = raw.split("[model_providers.9router]")[0]
        if CASE in {"apply-malformed", "reset-malformed"}:
            raw = 'model = "unterminated'
        if CASE == "apply-symlink":
            target = home / "linked-config.toml"
        target.write_text(raw)
        target.chmod(0o640)
        if target != config:
            config.symlink_to(target)
    if CASE in {"apply-preserve", "reset-managed", "reset-unmanaged", "reset-absent", "reset-auth-empty",
                "reset-malformed", "settings-unauthorized"}:
        config_dir.mkdir(exist_ok=True)
        auth_data = dict(LEGACY_AUTH)
        if CASE == "reset-auth-empty":
            del auth_data["tokens"]
        auth.write_text(json.dumps(auth_data))
        auth.chmod(0o640)
    profile = config_dir / "dev-fast.config.toml"
    if CASE in {"profile-overwrite", "profile-delete", "profile-delete-link", "profiles-unauthorized"}:
        config_dir.mkdir(exist_ok=True)
        if CASE == "profile-delete-link":
            target = home / "linked-profile.toml"
            target.write_text('model = "linked"\n')
            profile.symlink_to(target)
        else:
            profile.write_text('model = "old"\n')
            profile.chmod(0o640)
    if CASE == "profiles-list":
        config_dir.mkdir()
        cases = {
            "_Early": "model = 'single-quoted'\n",
            "-first": 'model = "double-quoted"\n',
            "second": '   model\n\t= \n  "split"\n',
            "Alpha": 'model = ""\n',
            "alpha": 'not_model = "wrong"\nmodel_provider="wrong"\nmodel = "correct"\n',
            "alpha_test": 'model = "nested"\n',
        }
        for name, content in cases.items():
            (config_dir / f"{name}.config.toml").write_text(content)
        (config_dir / "unreadable.config.toml").mkdir()
        (config_dir / "other.toml").write_text("left alone")
        config.write_text(MANAGED)
    present = {file: file.exists() for file in (config, auth, profile)}
    environment = dict(os.environ, HOME=directory, DATA_DIR=directory, PATH=f"{commands}:/usr/bin:/bin")
    with subprocess.Popen(COMMAND, cwd=home, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        try:
            port = listener(process)
            writes = CASE.startswith("apply-") or CASE == "settings-unauthorized" or CASE == "settings-mode"
            if writes:
                data = PAYLOAD.copy()
                if CASE == "apply-preserve":
                    data["baseUrl"] += "/v1"
                    data["subagentModel"] = "chosen-agent"
                if CASE == "apply-missing":
                    del data["apiKey"]
                if CASE == "apply-invalid-body":
                    data = '{"bad-json"'
                if CASE == "apply-bad-base":
                    data["baseUrl"] = True
                reply = request(port, "POST", "settings", data)
            elif CASE.startswith("reset-"):
                reply = request(port, "DELETE", "settings")
            elif CASE.startswith("get-"):
                reply = request(port, "GET", "settings")
            elif CASE.startswith("profile-delete") or CASE == "profiles-unauthorized":
                name = " Dev-Fast " if CASE != "profile-delete-invalid" else "../../outside"
                reply = request(port, "DELETE", "profiles", {"name": name})
            elif CASE.startswith("profile-"):
                data = {"name": " DeV-FAsT ", "model": " pro-v2 "}
                if CASE == "profile-invalid-name":
                    data["name"] = "../outside"
                if CASE == "profile-reserved-name":
                    data["name"] = " CONFIG "
                if CASE == "profile-long-name":
                    data["name"] = "a" * 65
                if CASE == "profile-invalid-model":
                    data["model"] = 'in"valid'
                if CASE == "profile-long-model":
                    data["model"] = "😀" * 129
                reply = request(port, "POST", "profiles", data)
            else:
                reply = request(port, "GET", "profiles")
            path_modes = {
                file.name: oct(stat.S_IMODE(file.stat().st_mode))
                for file, was_present in present.items() if was_present and file.exists()
            }
            if MODE == "strict":
                mode_file = config if CASE == "settings-mode" else profile
                assert reply["status"] == 200 and stat.S_IMODE(mode_file.stat().st_mode) == 0o600, reply
            print(redact({
                "reply": reply,
                "config": snapshot(config, tomllib.loads),
                "auth": snapshot(auth, json.loads),
                "profile": profile.read_text() if profile.is_file() else None,
                "configLink": config.is_symlink(),
                "profileLink": profile.is_symlink(),
                "deleteLinkTargetPreserved": target.is_file() if CASE == "profile-delete-link" else None,
                "existingModes": path_modes,
            }, home))
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=3)
