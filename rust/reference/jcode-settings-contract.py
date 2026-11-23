import concurrent.futures
import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

repository = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
endpoint = "/api/cli-tools/jcode-settings"
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
base_url = "http://127.0.0.1:9"
key_name = "JCODE_9ROUTER_API_KEY"


def request(port, method="GET", body=None, headers=auth, path=endpoint):
    headers = dict(headers)
    if body is not None:
        body = json.dumps(body)
        headers["content-type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


def expected_provider(url, model):
    return {"type": "openai-compatible", "base_url": url, "auth": "bearer",
            "api_key_env": key_name, "env_file": "provider-9router.env",
            "default_model": model, "requires_api_key": True}


def load(path):
    return tomllib.loads(path.read_text())


def credentials(path):
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, value = line.split("=", 1)
        values[name.strip()] = value.strip().strip("\"'")
    return values


def run(folder):
    home, data, xdg, commands = (folder / name for name in ("home", "data", "xdg", "bin"))
    home.mkdir()
    commands.mkdir()
    for name in ("which", "sh", "node"):
        (commands / name).symlink_to(shutil.which(name))
    subprocess.run([sys.executable, str(repository / "rust/reference/seed.py"), str(data)],
                   cwd=repository, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    fallback = folder / "fallback"
    env = {"HOME": str(home), "DATA_DIR": str(data), "XDG_CONFIG_HOME": str(xdg),
           "PATH": str(commands), "PORT": str(port), "HOSTNAME": "127.0.0.1",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1", "MODEL_CATALOG_SYNC": "off",
           "RUST_ANYTHING_FALLBACK_LOG": str(fallback)}
    command = [str(commands / "node"), ".next/standalone/custom-server.js"]
    if mode == "native":
        command = [str(Path(args[0]).resolve()), "--no-browser"]
        env["NINEROUTER_MIGRATION_FIXTURE"] = "1"
    process = subprocess.Popen(command, cwd=repository, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    observations = {}
    config = home / ".jcode/config.toml"
    dotenv = xdg / "jcode/provider-9router.env"

    def call(name, method="GET", body=None, headers=auth):
        result = request(port, method, body, headers)
        observations[name] = result
        return result

    try:
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            try:
                if request(port, headers={}, path="/api/health")[0] == 200:
                    break
            except OSError:
                time.sleep(.03)
        else:
            raise RuntimeError(f"{mode} fixture did not start")
        assert call("protected", headers={})[0] == 401
        assert call("uninstalled")[1]["installed"] is False
        assert call("required", "POST", {"baseUrl": base_url}) == (
            400, {"error": "baseUrl and apiKey are required"})
        assert not config.exists() and not dotenv.exists()

        marker = folder / "jcode-called"
        executable = commands / "jcode"
        executable.write_text(f"#!/bin/sh\n: > '{marker}'\n")
        executable.chmod(0o700)
        assert call("executable-present")[1]["installed"] is True and not marker.exists()

        request_body = {"baseUrl": base_url, "apiKey": "offline-jcode-initial"}
        assert call("initialize", "POST", request_body)[0] == 200
        assert load(config)["providers"]["9router"] == expected_provider(base_url + "/v1", "cc/claude-opus-4-7")
        assert credentials(dotenv) == {key_name: "offline-jcode-initial"}
        assert not (home / ".config/jcode/provider-9router.env").exists()
        assert call("after-initialize")[1]["has9Router"] is True

        foreign_toml = ('# keep the personal preference\n[ui]\ncolor = true # inline note\n'
                        '\n[providers.other]\nbase_url = "http://localhost:20128/v1"\n')
        foreign_dotenv = b"# private profile note\r\nMODE = 'strict'\r\n"
        config.write_text(foreign_toml)
        config.chmod(0o640)
        dotenv.write_bytes(foreign_dotenv)
        dotenv.chmod(0o640)
        assert call("detect-local-endpoint")[1]["has9Router"] is True
        request_body = {"baseUrl": base_url + "/v1", "apiKey": "offline-jcode-revised",
                        "models": ["mock/first", "mock/second"]}
        assert call("apply-existing", "POST", request_body)[0] == 200
        expected = expected_provider(base_url + "/v1", "mock/first")
        assert load(config) == {"ui": {"color": True}, "providers": {
            "other": {"base_url": "http://localhost:20128/v1"}, "9router": expected}}
        assert credentials(dotenv) == {"MODE": "strict", key_name: "offline-jcode-revised"}
        assert call("after-update")[1]["config"]["providers"]["9router"] == expected
        assert call("reset", "DELETE") == (
            200, {"success": True, "message": "9router configuration removed from jcode"})
        assert load(config) == tomllib.loads(foreign_toml)
        assert credentials(dotenv) == {"MODE": "strict"}
        assert call("after-reset")[1]["has9Router"] is True
        observations["remaining-config"] = load(config)
        observations["remaining-env"] = credentials(dotenv)

        if mode == "native":
            assert config.read_text() == foreign_toml and dotenv.read_bytes() == foreign_dotenv
            assert config.stat().st_mode & 0o777 == 0o640 and dotenv.stat().st_mode & 0o777 == 0o600
            config.write_text(foreign_toml + 'api_key = "offline-unrelated-secret"\n')
            status, body = request(port)
            assert status == 200 and "offline-unrelated-secret" not in json.dumps(body)
            config.write_text(foreign_toml)

            malformed = "[broken"
            config.write_text(malformed)
            assert request(port, "POST", request_body)[0] == 500
            assert config.read_text() == malformed and dotenv.read_bytes() == foreign_dotenv
            config.write_text(foreign_toml)
            assert request(port, "POST", {**request_body, "apiKey": "offline-key\nMODE=changed"})[0] == 400
            assert config.read_text() == foreign_toml and dotenv.read_bytes() == foreign_dotenv

            dotenv.parent.chmod(0o500)
            try:
                assert not os.access(dotenv.parent, os.W_OK), "fixture requires ordinary file permissions"
                assert request(port, "POST", request_body)[0] == 500
                assert config.read_text() == foreign_toml and dotenv.read_bytes() == foreign_dotenv
                assert config.stat().st_mode & 0o777 == 0o640
            finally:
                dotenv.parent.chmod(0o700)

            def update(index):
                body = {**request_body, "apiKey": f"offline-concurrent-{index}", "models": [f"mock/{index}"]}
                assert request(port, "POST", body)[0] == 200

            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                list(executor.map(update, range(8)))
            last = load(config)["providers"]["9router"]["default_model"].removeprefix("mock/")
            assert credentials(dotenv)[key_name] == f"offline-concurrent-{last}"
            assert fallback.is_file() and not fallback.read_text().strip()
        assert not marker.exists()
        return json.loads(json.dumps(observations).replace(str(home), "/fixture-home"))
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


with tempfile.TemporaryDirectory(prefix="9router-jcode-contract-") as directory:
    print(json.dumps(run(Path(directory)), sort_keys=True))
