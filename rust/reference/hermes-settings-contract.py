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
mode = sys.argv[1] if sys.argv[1] in ("original", "native") else "compare"
binary = Path(sys.argv[-1]).resolve() if mode != "original" else None
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
base = "http://127.0.0.1:9"
settings = "/api/cli-tools/hermes-settings"
profiles = "/api/cli-tools/hermes-profiles"
reference_key = "$" + "{OPENAI_API_KEY}"


def model(default, provider="custom", origin=base):
    return (f'model:\n  default: "{default}"\n  provider: "{provider}"\n  base_url: "{origin}/v1"\n'
            f"  api_key: {reference_key}\n")


def write(path, body):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body.encode())
    path.chmod(0o640)


def content(path):
    return path.read_bytes().decode() if path.exists() else None


def run(implementation, folder):
    home, data, commands = (folder / name for name in ["home", "data", "bin"])
    home.mkdir(parents=True)
    commands.mkdir()
    for command in ["node", "sh", "which"]:
        (commands / command).symlink_to(shutil.which(command))
    marker = folder / "hermes-executed"
    stub = commands / "hermes"
    stub.write_text(f"#!/bin/sh\n: > '{marker}'\nexit 1\n")
    stub.chmod(0o700)
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    fallback = folder / "fallbacks"
    env = {"PATH":str(commands), "PORT":str(port), "HOSTNAME":"127.0.0.1",
           "HOME":str(home), "DATA_DIR":str(data), "NINEROUTER_MIGRATION_FIXTURE":"1",
           "NEXT_TELEMETRY_DISABLED":"1", "RUST_ANYTHING_FALLBACK_LOG":str(fallback)}
    subprocess.run([sys.executable,"rust/reference/seed.py",str(data)],cwd=repository,check=True,
                   stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    command = [str(commands / "node"), ".next/standalone/custom-server.js"] if implementation == "original" else [str(binary)]
    process = subprocess.Popen(command, cwd=repository, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    observations = {}

    def raw(path, method="GET", data=None, headers=cli):
        headers = headers.copy()
        if data is not None:
            data = json.dumps(data)
            headers["content-type"] = "application/json"
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        try:
            connection.request(method, path, body=data, headers=headers)
            response = connection.getresponse()
            return response.status, response.headers, response.read()
        finally:
            connection.close()

    def call(name, path=settings, method="GET", data=None, headers=cli, compare=True):
        status, _, data = raw(path, method, data, headers)
        result = status, json.loads(data)
        if compare:
            observations[name] = result
        return result

    def remember(name, files):
        observations[name] = {str(path.relative_to(home)):content(path) for path in files}

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
        root = home / ".hermes"
        default = root / "config.yaml"
        default_env = root / ".env"
        alpha = root / "profiles" / "alpha"
        foreign = root / "profiles" / "foreign"
        fresh = root / "profiles" / "fresh"
        bare = root / "profiles" / "bare"
        assert call("blank-installed")[1]["installed"] and not root.exists()
        assert call("protected-settings", headers={})[0] == 401
        assert call("protected-profiles", profiles, headers={})[0] == 401
        assert not marker.exists()

        write(default,
            '# home layout\r\nmodel: ""\r\nagent:\r\n  reasoning_effort: medium\r\n'
            'delegation:\r\n  provider: "foreign"\r\n  model: "foreign/sub"\r\n'
            'auxiliary:\r\n  extract:\r\n    provider: "foreign"\r\n    model: "foreign/extract"\r\n'
            'stt:\r\n  enabled: false\r\n  provider: "local"\r\n  language: "en"\r\n'
            '  local:\r\n    model: "base"\r\n    auto_install: false\r\n'
            '  openai:\r\n    model: "foreign/stt"\r\n    timeout: 11\r\n'
            '# end layout\r\n')
        write(default_env,"TELEGRAM_BOT_TOKEN=offline-bot\r\nOPENAI_API_KEY=offline-old\r\n")
        write(alpha / "config.yaml", model("initial/alpha") +
            'delegation:\n  provider: "custom"\n  model: "custom/sub"\n'
            'auxiliary:\n  compression:\n    provider: "custom"\n    model: "custom/compress"\n')
        write(alpha / ".env","BOT_MODE=private")
        write(alpha / "profile.yaml",'display_name: " Alpha Agent "\n')
        write(foreign / "config.yaml", model("foreign/main","unrelated"))
        write(fresh / "SOUL.md","Existing Hermes profile")
        write(bare / "other.log","Not a profile")
        write(root / "profiles" / "Bad.Dot" / "config.yaml",model("excluded"))
        before_foreign = content(foreign / "config.yaml")
        status, listed = call("list-initial",profiles)
        assert status == 200 and [item["name"] for item in listed["profiles"]] == ["default","alpha","foreign","fresh"]
        assert listed["profiles"][1]["displayName"] == "Alpha Agent"
        assert listed["profiles"][1]["has9Router"]
        assert call("bare-explicit",settings+"?profile=bare")[1]["settings"]["model"] is None
        assert call("invalid-profile",settings+"?profile=Default")[0] == 400
        assert call("absent-profile",settings+"?profile=missing")[0] == 404

        status, applied = call("apply-main",settings+"?profile=alpha","POST",{
            "profile":"default","baseUrl":base,"apiKey":"offline-default",
            "selections":[{"role":"default","model":"fixture/main"},{"role":"vision","model":"fixture/vision"}],
            "audio":{"enabled":"1","model":" cx/transcribe "},
        })
        assert status == 200 and applied["stt9Router"] and applied["profile"]["isDefault"]
        assert content(default).count("\nmodel:") == 1 and 'model: ""' not in content(default)
        for line in ["# home layout\r\n","agent:\r\n  reasoning_effort: medium\r\n",
                     '  language: "en"\r\n','    timeout: 11\r\n',"# end layout\r\n"]:
            assert line in content(default)
        assert content(default_env) == "TELEGRAM_BOT_TOKEN=offline-bot\r\nOPENAI_API_KEY=offline-default\r\n"
        remember("default-applied", [default,default_env])
        status, configured = call("configured")
        assert status == 200 and configured["has9Router"] and configured["has9RouterStt"]
        assert configured["settings"]["stt"]["model"] == "cx/transcribe"
        assert configured["settings"]["auxiliary"]["vision"]["model"] == "fixture/vision"

        assert call("disable-audio",settings,"POST",{"baseUrl":base,"audio":{"enabled":False}})[1]["stt9Router"] is False
        assert '    model: "base"\r\n' in content(default)
        assert '    timeout: 11\r\n' in content(default)
        remember("without-audio",[default])
        assert call("enable-audio",settings,"POST",{"baseUrl":base,"audio":{"enabled":True,"model":"cx/transcribe"}})[1]["stt9Router"]
        assert call("apply-profile",settings+"?profile=alpha","POST",{
            "baseUrl":base + "/v1", "model":"new/alpha", "apiKey":"offline-alpha",
        })[1]["profile"]["name"] == "alpha"
        assert content(alpha / ".env") == "BOT_MODE=private\nOPENAI_API_KEY=offline-alpha\n"
        assert call("profile-after-apply",settings+"?profile=alpha")[1]["settings"]["model"]["default"] == "new/alpha"

        status, bulk = call("bulk",settings+"?applyToAll=true","POST",{
            "baseUrl":"https://fixture.invalid", "model":"fallback/model", "apiKey":"offline-bulk",
            "audio":{"enabled":False},
        })
        assert status == 200 and bulk["updated"] == 3 and bulk["skipped"] == 1
        assert [row["profile"] for row in bulk["results"]] == ["default","alpha","foreign","fresh"]
        assert bulk["results"][2]["status"] == "skipped" and content(foreign / "config.yaml") == before_foreign
        assert bulk["results"][1]["model"] == "new/alpha" and bulk["results"][3]["model"] == "fallback/model"
        assert 'base_url: "https://fixture.invalid/v1"' in content(alpha / "config.yaml")
        assert f'base_url: "{base}/v1"' in content(default)
        all_documents = [default,default_env,alpha/"config.yaml",alpha/".env",fresh/"config.yaml",fresh/".env",foreign/"config.yaml"]
        remember("after-bulk",all_documents)
        status, result = call("reset-alpha",settings+"?profile=foreign","DELETE",{"profile":"alpha"})
        assert status == 200 and result["removed"] == {
            "model":True,"delegation":True,"auxiliary":["compression"],"stt":False}
        assert result["kept"] == [] and content(alpha / ".env").endswith("OPENAI_API_KEY=offline-bulk\n")
        status, result = call("reset-default",settings,"DELETE")
        assert status == 200 and result["removed"] == {
            "model":True,"delegation":False,"auxiliary":["vision"],"stt":True}
        assert result["kept"] == ["delegation","stt"]
        assert '    provider: "foreign"\r\n' in content(default)
        remember("after-reset",all_documents)
        assert call("list-final",profiles)[0] == 200
        assert not marker.exists()

        if implementation == "native":
            page = "/dashboard/cli-tools/hermes"
            status, headers, _ = raw(page, headers={})
            assert status == 307 and headers["location"] == "/login"
            status, headers, _ = raw("/api/auth/login", "POST", {"password":"123456"}, headers={})
            assert status == 200
            cookie = headers["set-cookie"].split(";", 1)[0]
            status, headers, markup = raw(page, headers={"cookie":cookie})
            assert status == 200 and headers["cache-control"] == "no-store"
            for text in [b'id="hermes-profile"', b'id="hermes-model"', b'id="hermes-roles"',
                         b'id="hermes-stt"', b'id="hermes-bulk"', b'id="hermes-reset"',
                         b'id="hermes-copy-command"', b'id="hermes-yaml"', b"id=\"hermes-copy-env\""]:
                assert text in markup
            assert b'/__9router-ui/hermes.js' in markup and b"/_next/" not in markup
            status, headers, script = raw("/__9router-ui/hermes.js", headers={})
            assert status == 200 and headers["content-type"].startswith("text/javascript")
            for text in [b"/api/cli-tools/hermes-settings", b"/api/cli-tools/hermes-profiles",
                         b"applyToAll: true", b"selections: selected", b"/v1/models/stt"]:
                assert text in script

            initial = content(default)
            env_initial = content(default_env)
            requested = {"baseUrl":base,"model":"fixture/another"}
            assert default_env.stat().st_mode & 0o777 == 0o600

            literal = "offline-literal-token"
            write(default, model("mock/manual").replace(reference_key, literal) +
                  f"stt:\n  provider: openai\n  openai:\n    api_key: {literal}\n")
            status, data = call("redacted", compare=False)
            assert status == 200 and literal not in json.dumps(data)
            write(default, initial)

            assert call("env-injection",settings,"POST",{
                **requested,"apiKey":"offline-key\nTELEGRAM_BOT_TOKEN=bad",
            },compare=False)[0] == 400
            assert content(default_env) == env_initial and content(default) == initial

            outside = home / "other"
            write(outside / "config.yaml", model("other/original", "unrelated"))
            (root / "profiles" / "external").symlink_to(outside, target_is_directory=True)
            assert call("outside",settings+"?profile=external","POST",requested,compare=False)[0] == 400
            assert content(outside / "config.yaml") == model("other/original","unrelated")

            restricted = root / "readonly" / ".env"
            write(restricted,env_initial)
            default_env.unlink()
            default_env.symlink_to(default)
            assert call("same-file",settings,"POST",{**requested,"apiKey":"offline-new"},compare=False)[0] == 500
            assert content(default) == initial and default_env.is_symlink()
            default_env.unlink()
            default_env.symlink_to(restricted)
            restricted.parent.chmod(0o500)
            try:
                assert not os.access(restricted.parent,os.W_OK),"fixture requires ordinary file permissions"
                assert call("rollback",settings,"POST",{**requested,"apiKey":"offline-new"},compare=False)[0] == 500
                assert content(default) == initial and content(default_env) == env_initial
                assert default_env.is_symlink() and default.stat().st_mode & 0o777 == 0o640
            finally:
                restricted.parent.chmod(0o700)
            assert not fallback.exists() or not fallback.read_text().strip()

        return json.loads(json.dumps(observations).replace(str(home),"/fixture-home"))
    finally:
        process.terminate()
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


with tempfile.TemporaryDirectory(prefix="9router-hermes-contract-") as temporary:
    root = Path(temporary)
    if mode == "compare":
        original = run("original",root/"original")
        native = run("native",root/"native")
        assert original.keys() == native.keys()
        for step in original:
            assert original[step] == native[step], step
        print(f"{len(original)} Hermes actions and exact file comparisons passed, plus path containment and rollback")
    else:
        print(json.dumps(run(mode,root/mode),sort_keys=True))
