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
test_key = "offline-editor-key"


def write(file, payload):
    file.parent.mkdir(parents=True, exist_ok=True)
    file.write_text(json.dumps(payload))
    file.chmod(0o640)


def content(file):
    return json.loads(file.read_text())


def run(implementation, folder):
    home, data, tools = (folder / name for name in ["home", "data", "bin"])
    home.mkdir(parents=True)
    tools.mkdir()
    for command in ["node", "sh", "which"]:
        (tools / command).symlink_to(shutil.which(command))
    marker = folder / "editor-executed"
    for command in ["opencode", "cline"]:
        fake = tools / command
        fake.write_text(f"#!/bin/sh\n: > '{marker}'\nexit 1\n")
        fake.chmod(0o700)
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    fallback = folder / "fallbacks"
    environment = {
        "HOME": str(home), "PATH": str(tools), "PORT":str(port), "HOSTNAME":"127.0.0.1",
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_FIXTURE":"1",
        "NEXT_TELEMETRY_DISABLED":"1", "RUST_ANYTHING_FALLBACK_LOG":str(fallback),
    }
    subprocess.run(["python3", "rust/reference/seed.py", str(data)], cwd=repository, check=True)
    argv = [str(tools / "node"), ".next/standalone/custom-server.js"]
    if implementation == "native":
        argv = [str(binary)]
    process = subprocess.Popen(argv, cwd=repository, env=environment, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    observations = {}

    def call(editor, step, method="GET", payload=None, query="", headers=token, compare=True):
        headers = headers.copy()
        if payload is not None:
            headers["content-type"] = "application/json"
            payload = json.dumps(payload)
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        try:
            connection.request(method, f"/api/cli-tools/{editor}-settings{query}", payload, headers)
            response = connection.getresponse()
            result = response.status, json.loads(response.read())
        finally:
            connection.close()
        if compare:
            observations[f"{editor}-{step}"] = result
        return result

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
        config = home / ".config" / "opencode" / "opencode.json"
        state = home / ".cline" / "data" / "globalState.json"
        secrets = state.with_name("secrets.json")
        for editor in ["opencode", "cline"]:
            assert call(editor, "guard", "POST", {"baseUrl":base}, headers={})[0] == 401
            status, probe = call(editor, "path-probe")
            assert status == 200 and probe["installed"] and not probe["has9Router"]
        assert not marker.exists() and not config.exists() and not state.exists()
        assert call("opencode", "patch-absent", "PATCH", {"clearActiveModel":True}) == (
            200, {"success":True, "message":"No config file found"})

        tuned = {"custom":{"reasoningEffort":"medium"}}
        first = "alt/first"
        argon = "gemini/gemini-4-argon"
        tuned_model = "gemini/gemini-4-hydrogen"
        codex = "cx/gpt-6.1-sol"
        legacy_kiro = "kr/claude-sonnet-4.5-20250929"
        write(config, {
            "$schema":"https://fixture.invalid/opencode", "keep":"yes",
            "provider":{
                "other":{"url":"https://fixture.invalid/other"},
                "9router":{"npm":"custom-adapter", "options":{"tenant":"keep-me"},
                    "models":{first:{"name":"Existing"}, tuned_model:{"name":"Tune me","variants":tuned}}},
            },
            "model":"9router/alt/first",
            "agent":{"review":{"model":"other/review"}, "explorer":{"model":"other/old-explorer"}},
        })
        payload = {
            "baseUrl":base, "apiKey":"sk_9router",
            "models":[argon, tuned_model, codex, legacy_kiro], "activeModel":codex,
            "subagentModel":argon,
        }
        assert call("opencode", "matrix-apply", "POST", payload)[0] == 200
        settings = content(config)
        provider = settings["provider"]["9router"]
        assert provider["npm"] == "custom-adapter"
        assert provider["options"] == {
            "tenant":"keep-me", "baseURL":base + "/v1", "apiKey":"offline-active-key"}
        assert provider["models"][tuned_model]["variants"] == tuned
        assert list(provider["models"][argon]["variants"]) == ["low", "medium", "high"]
        assert list(provider["models"][codex]["variants"]) == ["low", "medium", "high", "xhigh", "max"]
        assert "variants" not in provider["models"][legacy_kiro]
        assert settings["model"] == f"9router/{codex}"
        assert settings["agent"]["explorer"]["model"] == f"9router/{argon}"
        assert call("opencode", "matrix-read")[1]["opencode"] == {
            "models":[first, tuned_model, argon, codex, legacy_kiro], "activeModel":codex,
            "baseURL":base + "/v1",
        }
        assert call("opencode", "remove-active", "DELETE", query="?model=cx%2Fgpt-6.1-sol") == (
            200, {"success":True, "message":f'Model "{codex}" removed'})
        removed = content(config)
        assert removed["model"] == f"9router/{first}" and "explorer" not in removed["agent"]
        assert removed["agent"]["review"] == settings["agent"]["review"]
        assert call("opencode", "clear-active", "PATCH", {"clearActiveModel":True})[0] == 200
        assert content(config)["model"] == "" and content(config)["provider"]["9router"]["models"]
        assert call("opencode", "legacy-model", "POST", {
            "baseUrl":base + "/v1", "apiKey":f"  {test_key}  ",
            "model":"new/legacy", "activeModel":"",
        })[0] == 200
        legacy = content(config)
        assert legacy["provider"]["9router"]["options"]["apiKey"] == test_key
        assert legacy["model"] == "" and legacy["agent"]["explorer"]["model"] == "9router/new/legacy"
        observations["opencode-legacy-file"] = legacy
        assert call("opencode", "remove-provider", "DELETE")[0] == 200
        assert content(config) == {"$schema":"https://fixture.invalid/opencode", "keep":"yes",
            "provider":{"other":{"url":"https://fixture.invalid/other"}}, "model":"",
            "agent":{"review":{"model":"other/review"}}}
        observations["opencode-reset-file"] = content(config)
        assert call("opencode", "only-model", "POST", {"baseUrl":base,"model":"new/only"})[0] == 200
        assert call("opencode", "remove-last", "DELETE", query="?model=new%2Fonly")[0] == 200
        assert "model" not in content(config) and "9router" not in content(config)["provider"]

        write(secrets, {"openAiApiKey":"latent-fixture-key","unrelatedKey":"keep-me"})
        original_secrets = secrets.read_bytes()
        assert call("cline", "reset-orphan", "DELETE") == (
            200, {"success":True, "message":"No settings file to reset"})
        assert secrets.read_bytes() == original_secrets
        write(state, {
            "actModeApiProvider":"other","planModeApiProvider":"openai",
            "openAiBaseUrl":base,"openAiModelId":"earlier/model","keepCline":17,
        })
        state.write_bytes(state.read_bytes()[:-1] + b",}")
        status, stat = call("cline", "trailing-comma")
        assert status == 200 and stat["has9Router"]
        assert stat["settings"] == {"actModeApiProvider":"other","planModeApiProvider":"openai",
                                    "openAiBaseUrl":base,"openAiModelId":"earlier/model"}
        cline_payload = {"baseUrl":base + "/v1", "apiKey":f"  {test_key}  ", "model":"new/model"}
        assert call("cline", "apply", "POST", cline_payload)[0] == 200
        assert content(state) == {
            "actModeApiProvider":"openai","planModeApiProvider":"openai",
            "openAiBaseUrl":base,"openAiModelId":"new/model","keepCline":17,
            "planModeOpenAiModelId":"new/model",
        }
        assert content(secrets) == {"openAiApiKey":f"  {test_key}  ","unrelatedKey":"keep-me"}
        observations["cline-config-file"] = content(state)
        observations["cline-secrets-file"] = content(secrets)
        status, stat = call("cline", "read")
        assert status == 200 and stat["has9Router"] and test_key not in json.dumps(stat)
        assert call("cline", "remove", "DELETE")[0] == 200
        assert content(state) == {
            "actModeApiProvider":"cline","planModeApiProvider":"cline","keepCline":17}
        assert content(secrets) == {"unrelatedKey":"keep-me"}
        observations["cline-reset-file"] = content(state)

        plan_only = {"actModeApiProvider":"other","planModeApiProvider":"openai","openAiBaseUrl":base}
        write(state, plan_only)
        secrets.unlink()
        assert call("cline", "reset-plan-only", "DELETE")[0] == 200
        assert content(state) == plan_only and content(secrets) == {}

        if implementation == "native":
            malformed = b'{"unrecognized":'
            preserved = config.read_bytes()
            config.write_bytes(malformed)
            assert call("opencode", "malformed", "POST", payload, compare=False)[0] == 500
            assert config.read_bytes() == malformed
            config.write_bytes(preserved[:-1] + b",}")
            assert call("opencode", "merge-trailing", "POST", payload, compare=False)[0] == 200
            assert content(config)["keep"] == "yes"

            state_raw = state.read_bytes()
            secrets.write_bytes(malformed)
            assert call("cline", "malformed", "POST", cline_payload, compare=False)[0] == 500
            assert state.read_bytes() == state_raw and secrets.read_bytes() == malformed
            target = home / "readonly" / "secrets.json"
            write(target, {"protected":"contents"})
            secrets.unlink()
            secrets.symlink_to(target)
            target.parent.chmod(0o500)
            try:
                assert not os.access(target.parent, os.W_OK), "fixture requires normal file permissions"
                assert call("cline", "rollback", "POST", cline_payload, compare=False)[0] == 500
                assert state.read_bytes() == state_raw
                assert secrets.is_symlink() and content(target) == {"protected":"contents"}
            finally:
                target.parent.chmod(0o700)
            assert state.stat().st_mode & 0o777 == 0o640
            assert not fallback.exists() or not fallback.read_text().strip()

        assert not marker.exists()
        return json.loads(json.dumps(observations).replace(str(home), "/fixture-home"))
    finally:
        process.terminate()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


with tempfile.TemporaryDirectory(prefix="9router-cli-editors-contract-") as temporary:
    root = Path(temporary)
    original = run("original", root / "original")
    native = run("native", root / "native")
    assert original.keys() == native.keys()
    for step in original:
        assert original[step] == native[step], step
    print(f"{len(original)} OpenCode/Cline actions and file observations matched; malformed config and rollback checks passed")
