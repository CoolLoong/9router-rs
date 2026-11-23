import http.client
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

import yaml

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
tools = ["forge", "crush", "smelt", "codewhale", "omp"]
base_url = "http://127.0.0.1:9"
explicit_key = "offline-cli-explicit"


def request(port, path, method="GET", payload=None, headers=cli):
    headers = headers.copy()
    data = None
    if payload is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(payload)
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, path, body=data, headers=headers)
        response = client.getresponse()
        json_body = json.loads(response.read())
        return response.status, json_body, response.headers
    finally:
        client.close()


def encode_config(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(data)
    path.chmod(0o640)


def decode_config(path):
    if path.suffix == ".toml":
        return tomllib.loads(path.read_text())
    if path.suffix == ".yml":
        return yaml.safe_load(path.read_text())
    return json.loads(path.read_text())


def run(kind, run_dir):
    home = run_dir / "home"
    xdg = run_dir / "xdg"
    data_dir = run_dir / "data"
    bin_dir = run_dir / "bin"
    for path in [home, xdg, bin_dir]:
        path.mkdir(parents=True)
    for command in ["dirname", "env", "mktemp", "node", "python3", "sh"]:
        (bin_dir / command).symlink_to(shutil.which(command))
    which = bin_dir / "which"
    which.write_text('#!/bin/sh\ncommand -v "$1"\n')
    which.chmod(0o700)
    fake_tool = bin_dir / "forge"
    fake_tool.write_text("#!/bin/sh\nexit 0\n")
    fake_tool.chmod(0o700)

    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "PATH": str(bin_dir), "HOME": str(home), "XDG_CONFIG_HOME": str(xdg),
        "PORT": str(port), "DATA_DIR": str(data_dir), "HOSTNAME": "127.0.0.1",
        "NINEROUTER_MIGRATION_FIXTURE": "1", "NEXT_TELEMETRY_DISABLED": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(run_dir / "fallbacks.txt"),
    }
    subprocess.run(["python3", "rust/reference/seed.py", str(data_dir)], cwd=root, check=True)
    entry = [str(bin_dir / "node"), ".next/standalone/custom-server.js"] if kind == "original" else [binary]
    process = subprocess.Popen(entry, cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"{kind} fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        paths = {
            "forge": home / ".forge" / "config.toml",
            "crush": xdg / "crush" / "crush.json",
            "smelt": home / ".smelt" / "config.json",
            "codewhale": home / ".codewhale" / "config.toml",
            "omp": home / ".omp" / "agent" / "models.yml",
        }

        def call(tool, method="GET", payload=None):
            return request(port, f"/api/cli-tools/{tool}-settings", method, payload)

        assert call("forge")[:2] == (200, {
            "installed": True, "config": None, "has9Router": False, "configPath": str(paths["forge"]),
        })
        assert call("omp")[1]["installed"] is False
        initial = {
            "forge": 'format = "keep"\n[other]\nenabled = true\n',
            "crush": '{"providers":{"unrelated":{"base_url":"https://offline.example.invalid"}},"ui":{"theme":"dark"}}',
            "smelt": '{"model":"previous-model","unrelated":["kept"]}',
            "codewhale": '[other]\nvalue = 17\n',
            "omp": "providers:\n  unrelated:\n    baseUrl: https://offline.example.invalid\nquiet: true\n",
        }
        for tool, file in paths.items():
            encode_config(file, initial[tool])
        omp_db = paths["omp"].with_name("agent.db")
        with sqlite3.connect(omp_db) as database:
            database.executescript("""
              CREATE TABLE auth_credentials (
                provider TEXT, credential_type TEXT, data TEXT,
                disabled_cause TEXT, identity_key TEXT, created_at INTEGER, updated_at INTEGER
              );
              INSERT INTO auth_credentials VALUES ('other','api_key','{}',NULL,NULL,0,0);
            """)

        collected = {}
        for tool, file in paths.items():
            original_value = decode_config(file)
            status, applied, _ = call(tool, "POST", {
                "baseUrl": base_url, "apiKey": f"  {explicit_key}  ",
                **({"model": "sample/cli-model"} if tool != "smelt" else {}),
            })
            assert status == 200 and applied["success"] and applied["configPath"] == str(file)
            applied.pop("configPath")
            status, configured, _ = call(tool)
            assert status == 200 and configured["installed"] and configured["has9Router"]
            assert configured.pop("configPath") == str(file)
            updated = decode_config(file)
            if tool == "crush":
                assert updated["providers"]["9router"]["api_key"] == explicit_key
                assert updated["providers"]["9router"]["models"][0]["context_window"] == 128000
            elif tool == "smelt":
                assert updated["apiKey"] == explicit_key and updated["model"] == "previous-model"
            elif tool == "omp":
                assert updated["providers"]["9router"]["apiKey"] == explicit_key
                assert updated["providers"]["9router"]["discovery"] == {"type": "proxy"}
            else:
                assert updated["openai"]["api_key"] == explicit_key
                assert updated["openai"]["base_url"] == base_url + "/v1"
            assert file.stat().st_mode & 0o777 == 0o640
            result = {"applied": applied, "config": configured, "file": updated}
            if tool != "omp":
                status, removed, _ = call(tool, "DELETE")
                assert status == 200 and removed["success"]
                wanted = dict(original_value)
                if tool == "smelt":
                    wanted.pop("model", None)
                assert decode_config(file) == wanted
                result["restore"] = removed
            collected[tool] = result

        assert call("forge", "POST", {"baseUrl": base_url + "/v1", "apiKey": "sk_9router"})[0] == 200
        restored_default = decode_config(paths["forge"])["openai"]
        assert restored_default["api_key"] == "offline-active-key" and restored_default["base_url"] == base_url + "/v1"
        assert call("forge", "DELETE")[0] == 200

        if kind == "rust":
            status, _, headers = request(port, "/api/auth/login", "POST", {"password": "123456"}, {})
            assert status == 200
            cookie = headers["set-cookie"].split(";", 1)[0]
            status, safe, _ = request(port, "/__9router-ui/cli-status?tool=omp", headers={"cookie": cookie})
            assert status == 200 and safe == {"omp": {"installed": True, "has9Router": True}}
            assert explicit_key not in json.dumps(safe)
            with sqlite3.connect(omp_db) as database:
                credential = database.execute(
                    "SELECT data FROM auth_credentials WHERE provider = '9router'"
                ).fetchone()[0]
                assert json.loads(credential) == {"apiKey": explicit_key, "baseUrl": base_url}

            assert call("omp", "POST", {"baseUrl": base_url, "apiKey": "quoted: # synthetic"})[0] == 200
            assert decode_config(paths["omp"])["providers"]["9router"]["apiKey"] == "quoted: # synthetic"
            assert call("omp", "DELETE")[0] == 200
            assert decode_config(paths["omp"]) == yaml.safe_load(initial["omp"])
            with sqlite3.connect(omp_db) as database:
                assert database.execute("SELECT provider FROM auth_credentials").fetchall() == [("other",)]

            invalid = b"not valid ["
            paths["codewhale"].write_bytes(invalid)
            assert call("codewhale", "POST", {"baseUrl": base_url})[0] == 500
            assert paths["codewhale"].read_bytes() == invalid

            previous_config = paths["omp"].read_bytes()
            with sqlite3.connect(omp_db) as database:
                database.execute("ALTER TABLE auth_credentials DROP COLUMN credential_type")
                database.execute("INSERT INTO auth_credentials(provider,data) VALUES ('9router','{}')")
            assert call("omp", "POST", {"baseUrl": base_url})[0] == 500
            assert paths["omp"].read_bytes() == previous_config
            with sqlite3.connect(omp_db) as database:
                assert database.execute("SELECT data FROM auth_credentials WHERE provider='9router'").fetchone() == ("{}",)
            paths["omp"].unlink()
            assert call("omp", "DELETE")[0] == 200
            assert not paths["omp"].exists()
            with sqlite3.connect(omp_db) as database:
                assert database.execute("SELECT provider FROM auth_credentials").fetchall() == [("other",)]
            assert not (home / ".config" / "crush").exists()
            log = run_dir / "fallbacks.txt"
            assert not log.exists() or not log.read_text().strip()
        return collected
    finally:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


with tempfile.TemporaryDirectory(prefix="9router-cli-tools-contract-") as folder:
    scratch = Path(folder)
    original = run("original", scratch / "source")
    native = run("rust", scratch / "rust")
    assert native == original
    print("CLI tool API shapes, HOME/XDG config files, key resolution and rollback passed")
