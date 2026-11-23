import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

repository = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
token = {"x-9r-cli-token": "68977b6fd9422fbb"}
base = "http://127.0.0.1:9"


def request(port, tool, method="GET", body=None, headers=token):
    headers = headers.copy()
    if body is not None:
        headers["content-type"] = "application/json"
        body = body if isinstance(body, str) else json.dumps(body)
    with_client = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        with_client.request(method, f"/api/cli-tools/{tool}-settings", body, headers)
        response = with_client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        with_client.close()


def write(file, value):
    file.parent.mkdir(parents=True, exist_ok=True)
    file.write_text(json.dumps(value, ensure_ascii=False))
    file.chmod(0o640)


def contents(path):
    return json.loads(path.read_text())


def run(implementation, folder):
    home, data, tools = (folder / name for name in ["home", "data", "bin"])
    home.mkdir(parents=True)
    tools.mkdir()
    for command in ["node", "sh", "which"]:
        (tools / command).symlink_to(shutil.which(command))
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    fallback_log = folder / "fallbacks.txt"
    environment = {
        "HOME": str(home), "PATH": str(tools), "PORT": str(port), "HOSTNAME": "127.0.0.1",
        "DATA_DIR": str(data), "NINEROUTER_MIGRATION_FIXTURE": "1",
        "NEXT_TELEMETRY_DISABLED": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
    }
    subprocess.run(["python3", "rust/reference/seed.py", str(data)], cwd=repository, check=True)
    entry = [str(tools / "node"), ".next/standalone/custom-server.js"]
    if implementation == "native":
        entry = [str(binary)]
    process = subprocess.Popen(entry, cwd=repository, env=environment, stderr=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"{implementation} fixture listener did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        openclaw = home / ".openclaw" / "openclaw.json"
        pi_legacy = home / ".pi" / "models.json"
        pi_agent = home / ".pi" / "agent" / "models.json"
        seen = {}

        def call(tool, step, method="GET", body=None, headers=token):
            status, value = request(port, tool, method, body, headers)
            seen[f"{tool}-{step}"] = [status, value]
            return status, value

        for tool, path in [("openclaw", openclaw), ("pi", pi_agent)]:
            assert call(tool, "guard", "POST", {"baseUrl": base, "model": "fixture/model"}, {})[0] == 401
            assert not path.exists()

        executable = tools / "openclaw"
        marker = folder / "executed"
        executable.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 1\n")
        executable.chmod(0o700)
        status, body = call("openclaw", "path-probe")
        assert status == 200 and body["installed"] and body["agents"] == []
        assert not marker.exists()

        default_dir = openclaw.parent / "agents" / "default"
        custom_dir = openclaw.parent / "agents" / "custom"
        custom_models = custom_dir / "models.json"
        default_models = default_dir / "models.json"
        write(custom_models, {"providers": {
            "9router": {"models": [{"id": "old/custom"}]},
            "other": {"baseUrl": "https://other.invalid/v1"},
        }, "keep": 17})
        write(openclaw, {"unrelated": "preserve", "agents": {
            "defaults": {
                "model": {"primary": "9router/stale", "fallbacks": ["other/a"]},
                "models": {"9router/stale": {}, "other/a": {"alias": "a"}},
            },
            "list": [
                {"id": "custom", "model": {"primary": "9router/stale", "fallbacks": ["a"]},
                 "agentDir": str(custom_dir)},
                {"id": "default", "model": "other/a", "agentDir": str(default_dir)},
                {"id": "without-dir"},
            ],
        }, "models": {"providers": {"other": {"baseUrl": "https://other.invalid"}}}})
        old = openclaw.read_bytes()
        openclaw.write_bytes(old[:-1] + b",}")
        status, body = call("openclaw", "trailing-comma-read")
        assert status == 200 and body["settings"]["unrelated"] == "preserve"
        assert body["agents"][0]["currentModel"] == "old/custom"
        assert body["agents"][0]["model"] == "9router/stale"
        openclaw.write_bytes(old)
        backup = openclaw.with_name("openclaw.json.bak-9router")
        backup.write_bytes(b"pre-existing backup")
        openclaw_body = {
            "baseUrl": base, "model": "primary/main",
            "agentModels": {"custom": "alternate/coder", "without-dir": "alternate/coder"},
        }
        status, applied = call("openclaw", "apply", "POST", openclaw_body)
        assert status == 200 and applied["settingsPath"] == str(openclaw)
        merged = contents(openclaw)
        assert merged["agents"]["defaults"] == {
            "model": {"primary": "9router/primary/main", "fallbacks": ["other/a"]},
            "models": {"other/a": {"alias":"a"}, "9router/primary/main": {}, "9router/alternate/coder": {}},
        }
        assert merged["models"]["providers"]["9router"] == {
            "baseUrl": base + "/v1", "apiKey": "your_api_key", "api": "openai-completions",
            "models": [{"id": "primary/main", "name": "main"}, {"id": "alternate/coder", "name": "coder"}],
        }
        assert contents(custom_models)["keep"] == 17
        assert contents(default_models)["providers"]["9router"]["models"][0]["id"] == "primary/main"
        status, body = call("openclaw", "read-applied")
        assert status == 200 and body["has9Router"]
        assert [agent["currentModel"] for agent in body["agents"]] == ["alternate/coder", "primary/main", None]
        assert call("openclaw", "reset", "DELETE")[0] == 200
        status, body = call("openclaw", "read-reset")
        assert status == 200 and not body["has9Router"]
        assert body["agents"][0]["currentModel"] == "alternate/coder"
        assert contents(openclaw)["agents"]["defaults"] == {
            "model": {"fallbacks": ["other/a"]}, "models": {"other/a": {"alias": "a"}},
        }
        assert backup.read_bytes() == b"pre-existing backup"
        for file in [openclaw, custom_models, default_models]:
            seen[f"file-{file.relative_to(home)}"] = contents(file)

        write(pi_legacy, {"untouched": True, "providers": {
            "other": {"baseUrl": "http://127.0.0.1:20128/v1"},
            "9router": {
                "baseUrl": "https://old.invalid/v1", "apiKey": "offline-existing-key",
                "api": "openai-responses", "customFlag": True,
                "models": [
                    {"id": "fixture/model", "name": "Hand-tuned model", "contextWindow": 150000,
                     "maxTokens": 45000, "custom": "preserved"},
                    "fixture/legacy",
                ],
            },
        }})
        status, body = call("pi", "legacy")
        assert status == 200 and body["configPath"] == str(pi_legacy) and body["has9Router"]
        status, applied = call("pi", "merge", "POST", {
            "baseUrl": base + "/v1", "models": [
                {"id": "fixture/model", "name": "fixture/model", "maxTokens": 201000},
                "fixture/legacy",
                {"name": "Fresh model", "contextWindow": "0x100", "maxTokens": 1200},
            ],
        })
        assert status == 200 and applied["configPath"] == str(pi_legacy) and not pi_agent.exists()
        saved = contents(pi_legacy)["providers"]["9router"]
        assert saved["api"] == "openai-responses" and saved["apiKey"] == "offline-existing-key"
        assert saved["customFlag"] and saved["baseUrl"] == base + "/v1"
        assert saved["models"] == [
            {"id": "fixture/model", "name": "Hand-tuned model", "custom": "preserved",
             "contextWindow": 150000, "maxTokens": 64000},
            {"id": "fixture/legacy", "name": "fixture/legacy", "contextWindow": 200000, "maxTokens": 64000},
            {"id": "provider/model-id", "name": "Fresh model", "contextWindow": 256, "maxTokens": 1200},
        ]
        seen["pi-merged-file"] = contents(pi_legacy)
        legacy_before = pi_legacy.read_bytes()
        write(pi_agent, {"mode": "agent-only"})
        assert call("pi", "agent-precedence")[1]["configPath"] == str(pi_agent)
        assert call("pi", "fallback-key", "POST", {"baseUrl": base})[0] == 200
        new_provider = contents(pi_agent)["providers"]["9router"]
        assert new_provider["apiKey"] == "offline-active-key"
        assert new_provider["models"][0]["id"] == "provider/model-id"
        assert pi_legacy.read_bytes() == legacy_before
        seen["pi-new-file"] = contents(pi_agent)
        assert call("pi", "reset-agent", "DELETE")[0] == 200
        assert contents(pi_agent) == {"mode": "agent-only"}
        malformed = b'{ "preserve": {'
        write(pi_agent, {})
        pi_agent.write_bytes(malformed)
        assert call("pi", "reset-malformed", "DELETE")[1]["message"] == "No config file to reset"
        assert pi_agent.read_bytes() == malformed

        if implementation == "native":
            assert call("pi", "apply-malformed", "POST", {"baseUrl": base})[0] == 500
            assert pi_agent.read_bytes() == malformed
            del seen["pi-apply-malformed"]
            configuration = openclaw.read_bytes()
            openclaw.write_bytes(malformed)
            assert call("openclaw", "apply-malformed", "POST", openclaw_body)[0] == 500
            assert openclaw.read_bytes() == malformed
            del seen["openclaw-apply-malformed"]
            openclaw.write_bytes(configuration)
            previous_custom = custom_models.read_bytes()
            default_models.unlink()
            openclaw.parent.chmod(0o500)
            try:
                assert not os.access(openclaw.parent, os.W_OK), "fixture requires normal file permissions"
                status, _ = call("openclaw", "apply-rollback", "POST", {
                    **openclaw_body, "model": "replacement/main",
                    "agentModels": {"custom": "replacement/custom"},
                })
                assert status == 500
                assert custom_models.read_bytes() == previous_custom
                assert not default_models.exists()
                assert openclaw.read_bytes() == configuration
            finally:
                openclaw.parent.chmod(0o700)
            del seen["openclaw-apply-rollback"]
            assert custom_models.stat().st_mode & 0o777 == 0o640
            assert not fallback_log.exists() or not fallback_log.read_text().strip()

        return json.loads(json.dumps(seen).replace(str(home), "/fixture-home"))
    finally:
        process.terminate()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


with tempfile.TemporaryDirectory(prefix="9router-cli-tool-json-contract-") as temporary:
    root = Path(temporary)
    reference = run("original", root / "original")
    native = run("native", root / "native")
    assert reference.keys() == native.keys()
    for step in reference:
        assert reference[step] == native[step], step
    print(f"{len(reference)} OpenClaw/Pi actions and file observations matched; native malformed-config and rollback checks passed")
