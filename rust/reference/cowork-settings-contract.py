import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time


root = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
endpoint = "/api/cli-tools/cowork-settings"
authorization = {"x-9r-cli-token":"68977b6fd9422fbb"}
gateway = "http://127.0.0.1:20929/v1"
configured_models = ["provider/model-one", "fixture-combo"]
new_config = {
    "baseUrl":gateway, "apiKey":"synthetic-cowork-key", "models":[None, *configured_models, " "],
    "plugins":[], "localPlugins":[], "customPlugins":[],
}


def request(port, method="GET", body=None, headers=authorization, path=endpoint, send_headers=False):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = dict(headers)
        if body is not None:
            body = json.dumps(body)
            headers["content-type"] = "application/json"
        connection.request(method, path, body, headers)
        response = connection.getresponse()
        raw = response.read()
        content = json.loads(raw) if response.getheader("content-type", "").startswith("application/json") else raw.decode()
        result = response.status, content
        return (*result, response.headers) if send_headers else result
    finally:
        connection.close()


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(content, indent=2))


def read(path):
    return json.loads(path.read_text())


def observation(settings):
    return {name:settings["cowork"][name] for name in ["provider", "baseUrl", "models"]}


with tempfile.TemporaryDirectory(prefix="9router-cowork-settings-") as directory:
    temporary = Path(directory)
    home, data, empty_path = (temporary / name for name in ["home", "data", "bin"])
    home.mkdir()
    empty_path.mkdir()
    subprocess.run([sys.executable, "rust/reference/seed.py", str(data)],
                   cwd=root, stdout=subprocess.DEVNULL, check=True)
    fallback = temporary / "fallbacks.log"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME":str(home), "DATA_DIR":str(data), "PORT":str(port), "HOSTNAME":"127.0.0.1",
        "PATH":str(empty_path), "MODEL_CATALOG_SYNC":"off", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(fallback), "NEXT_TELEMETRY_DISABLED":"1",
    }
    command = [shutil.which("node"), str(root / ".next/standalone/custom-server.js")] if mode == "original" else [
        str(Path(args[0]).resolve()), "--no-browser",
    ]
    process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 16
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"{mode} Cowork fixture server did not start")
            try:
                request(port, headers={}, path="/api/health")
                break
            except OSError:
                time.sleep(.04)
        unauthenticated = request(port, headers={})
        assert unauthenticated == (403, {"error":"Local only: CLI token required"}), unauthenticated
        absent = request(port)
        assert absent == (200, {
            "installed":False, "config":None, "message":"Claude Desktop (Cowork mode) not detected",
        })
        status, _, login_headers = request(
            port, "POST", {"password":"123456"}, {}, "/api/auth/login", send_headers=True,
        )
        assert status == 200
        cookie = next(value.partition(";")[0] for value in login_headers.get_all("set-cookie")
                      if value.startswith("auth_token="))
        assert request(port, headers={"cookie":cookie, "origin":f"http://localhost:{port}"}) == absent
        assert request(port, headers={"cookie":cookie, "origin":"https://remote.invalid"}) == unauthenticated
        assert request(port, headers={"cookie":cookie, "x-forwarded-for":"192.0.2.42"}) == unauthenticated
        no_reset = request(port, "DELETE")
        assert no_reset == (200, {"success":True, "message":"No active config to reset"})
        no_key = request(port, "POST", {"baseUrl":gateway})
        assert no_key == (400, {"error":"baseUrl and apiKey are required"})
        no_model = request(port, "POST", {**new_config, "models":[123, " "]})
        assert no_model == (400, {"error":"At least one model is required"})

        desktop = home / ".config/Claude/claude_desktop_config.json"
        library = home / ".config/Claude-3p/configLibrary"
        meta = library / "_meta.json"
        preferences = {
            "preferences":{"terminalFontSize":14},
            "mcpServers":{"unrelated":{"command":"untouched","args":["synthetic-secret-command"]}},
        }
        write(desktop, preferences)
        activated, applied = request(port, "POST", new_config)
        assert activated == 200 and applied["bootstrapped"] is True and applied["success"]
        own_path = Path(applied["configPath"])
        own_id = own_path.stem
        assert own_path.parent == library and read(meta)["appliedId"] == own_id
        assert read(desktop) == {**preferences, "deploymentMode":"3p"}
        written = read(own_path)
        inference = {
            "inferenceProvider":"gateway", "inferenceGatewayBaseUrl":gateway,
            "inferenceGatewayApiKey":new_config["apiKey"],
            "inferenceModels":[{"name":name} for name in configured_models],
        }
        assert all(written[field] == value for field, value in inference.items())
        exposed_status, exposed = request(port)
        assert exposed_status == 200 and exposed["installed"] and exposed["has9Router"]
        configured = observation(exposed)
        assert configured == {"provider":"gateway","baseUrl":gateway,"models":configured_models}
        second_status, second = request(port, "POST", new_config)
        assert second_status == 200 and second["bootstrapped"] is False and second["configPath"] == str(own_path)
        cleared = request(port, "DELETE")
        assert cleared == (200, {"success":True,"message":"Cowork config reset"})
        assert read(own_path) == {} and read(desktop)["deploymentMode"] == "3p"
        cleared_status, cleared_profile = request(port)
        assert cleared_status == 200 and not cleared_profile["has9Router"]
        cleared_state = observation(cleared_profile)
        assert cleared_state == {"provider":None,"baseUrl":None,"models":[]}
        if mode == "native":
            local_headers = {"cookie": cookie}
            status, page = request(port, headers=local_headers, path="/dashboard/cli-tools/cowork")
            assert status == 200 and 'id="cowork-browser-preset-choice" hidden' in page
            assert 'id="cowork-browser-preset" type="checkbox" disabled' in page
            assert 'src="/__9router-ui/integrations.js"' in page
            status, other = request(port, headers=local_headers, path="/dashboard/cli-tools/codex")
            assert status == 200 and 'id="cowork-browser-preset"' not in other
            status, script = request(port, path="/__9router-ui/integrations.js")
            assert status == 200 and script == (root / "rust/app/src/dashboard_assets/integrations.js").read_text()
            assert written == inference and own_path.stat().st_mode & 0o777 == 0o600
            assert new_config["apiKey"] not in json.dumps(exposed)
            thirdparty = library.parent / "config.json"
            denied = {"operonSkipMcpApprovals":{"unrelated":False}}
            write(thirdparty, denied)
            foreign_path = library / "foreign.json"
            foreign_profile = {"inferenceProvider":"bedrock","inferenceGatewayApiKey":"synthetic-foreign-secret"}
            write(foreign_path, foreign_profile)
            prev = read(meta)
            write(meta, {**prev, "appliedId":"foreign", "entries":[*prev["entries"],{"id":"foreign","name":"Private"}]})
            before = desktop.read_bytes(), thirdparty.read_bytes(), foreign_path.read_bytes()
            kept_connector = {
                "name":"remote","url":"https://fixture.invalid/mcp?secret=synthetic-query-secret",
                "transport":"http","headers":{"Authorization":"synthetic-connector-secret"},
                "toolPolicy":{"remote-read":"deny"},
            }
            preserved = {"managedMcpServers":[kept_connector], "disabledBuiltinTools":["shell"]}
            write(own_path, preserved)
            own_path.chmod(0o644)
            assert request(port, "POST", new_config)[0] == 200 and read(meta)["appliedId"] == own_id
            assert read(own_path) == {**preserved, **inference} and own_path.stat().st_mode & 0o777 == 0o600
            _, public_profile = request(port)
            assert public_profile["cowork"]["plugins"] == [{
                "name":"remote","url":"https://fixture.invalid/mcp","transport":"http",
                "oauth":False,"toolNames":["read"],
            }]
            assert "synthetic-" not in json.dumps(public_profile)
            status, aggregate = request(port, path="/api/cli-tools/all-statuses")
            assert status == 200 and aggregate["cowork"]["has9Router"] and "synthetic-" not in json.dumps(aggregate["cowork"])
            selection = {
                **new_config,
                "plugins":[{"name":"catalog","url":"https://catalog.example.invalid/mcp",
                            "transport":"http","oauth":True,"toolPolicy":{"search":"allow"}}],
                "customPlugins":[{"name":"local metadata","url":"http://127.0.0.1:1/mcp"}],
            }
            assert request(port, "POST", selection)[0] == 200
            connector_profile = read(own_path)
            assert connector_profile["managedMcpServers"] == [
                kept_connector,
                {"name":"catalog","url":"https://catalog.example.invalid/mcp","transport":"http","oauth":True},
                {"name":"local metadata","url":"http://127.0.0.1:1/mcp","transport":"sse","custom":True},
            ]
            managed = connector_profile["managedMcpServers"][1]
            managed["toolPolicy"] = {"catalog-search":"deny"}
            write(own_path, connector_profile)
            assert request(port, "POST", selection)[0] == 200
            assert read(own_path) == connector_profile
            assert request(port, "POST", {**selection,"plugins":[{"name":"remote","url":"https://different.example.invalid/mcp"}]})[0] == 500
            assert request(port, "POST", {**selection,"customPlugins":[{"name":"bad","url":"file:///not-a-connector"}]})[0] == 400
            assert read(own_path) == connector_profile
            assert request(port, "DELETE") == cleared
            assert read(own_path) == preserved and read(meta)["appliedId"] == "foreign"
            assert (desktop.read_bytes(), thirdparty.read_bytes(), foreign_path.read_bytes()) == before
            assert request(port, "DELETE") == cleared and read(meta)["appliedId"] == "foreign"
            local_selection = {**new_config, "localPlugins": ["browsermcp"]}
            forwarded = {**authorization, "x-forwarded-for": "192.0.2.42"}
            before_preset = own_path.read_bytes(), meta.read_bytes()
            assert request(port, "POST", local_selection, forwarded) == (403, {
                "error": "Cowork local connectors require a direct loopback connection",
            })
            for invalid in (["unapproved"], ["browsermcp", "browsermcp"], [{"name": "browsermcp", "command": "fixture"}]):
                assert request(port, "POST", {**local_selection, "localPlugins": invalid})[0] == 400
            injected = {"name": "remote shell", "url": "https://fixture.invalid/mcp", "command": "synthetic-never-run"}
            assert request(port, "POST", {**new_config, "customPlugins": [injected]})[0] == 400
            raw_route = {"name": "manual alias", "url": f"http://127.0.0.1:{port}/api/mcp/browsermcp/sse"}
            assert request(port, "POST", {**new_config, "customPlugins": [raw_route]})[0] == 400
            assert (own_path.read_bytes(), meta.read_bytes()) == before_preset

            status, preset = request(port, "POST", local_selection, {"cookie": cookie, "host": "localhost:1"})
            assert status == 200 and preset["localMcp"] == {"applied": ["browsermcp"], "via": "3p-sse-bridge"}
            assert preset["skipApprovals"] is None
            bridge = {"name": "browsermcp", "url": f"http://127.0.0.1:{port}/api/mcp/browsermcp/sse",
                      "transport": "sse", "headers": authorization}
            assert read(own_path)["managedMcpServers"] == [kept_connector, bridge]
            assert own_path.stat().st_mode & 0o777 == 0o600
            status, shown = request(port, headers={"cookie": cookie})
            assert status == 200 and shown["cowork"]["localPlugins"] == ["browsermcp"]
            assert shown["localStdioPlugins"] == [{"name": "browsermcp", "title": "Browser MCP"}]
            assert authorization["x-9r-cli-token"] not in json.dumps(shown)
            status, forwarded_list = request(port, headers=forwarded)
            assert status == 200 and forwarded_list["localStdioPlugins"] == [] and forwarded_list["cowork"]["localPlugins"] == []

            remote_only = {**new_config, "plugins": [{"name": "fixture catalog", "url": "https://fixture.invalid/mcp"}]}
            del remote_only["localPlugins"]
            assert request(port, "POST", remote_only, forwarded)[0] == 200
            assert bridge in read(own_path)["managedMcpServers"]
            only_preset = {field: local_selection[field] for field in ("baseUrl", "apiKey", "models", "localPlugins")}
            assert request(port, "POST", {**only_preset, "localPlugins": []})[0] == 200
            assert read(own_path)["managedMcpServers"] == [kept_connector, {"name": "fixture catalog",
                "url": "https://fixture.invalid/mcp", "transport": "http"}]
            assert request(port, "POST", only_preset)[0] == 200
            snapshot = own_path.read_bytes(), meta.read_bytes()
            assert request(port, "DELETE", headers=forwarded)[0] == 403
            assert (own_path.read_bytes(), meta.read_bytes()) == snapshot
            assert request(port, "DELETE", headers={"cookie": cookie}) == cleared
            assert read(own_path) == preserved and read(meta)["appliedId"] == "foreign"
            assert (desktop.read_bytes(), thirdparty.read_bytes(), foreign_path.read_bytes()) == before
            assert request(port, "POST", {**new_config,"baseUrl":"https://me:forgotten@fixture.invalid/v1"})[0] == 400
            write(desktop, {**preferences,"deploymentMode":"1p"})
            refused = desktop.read_bytes(), meta.read_bytes(), own_path.read_bytes()
            assert request(port, "POST", new_config)[0] == 409
            assert (desktop.read_bytes(), meta.read_bytes(), own_path.read_bytes()) == refused
            outside = library.parent / "hidden.json"
            write(outside, {"inferenceProvider":"gateway","inferenceModels":["outside/model"]})
            write(meta, {**read(meta),"appliedId":"../hidden"})
            assert request(port)[1]["cowork"]["models"] == []
            assert request(port, "DELETE") == cleared
            assert read(outside)["inferenceModels"] == ["outside/model"]
            assert not fallback.read_text().strip()
        print(json.dumps({
            "absent":absent, "resetWithoutConfig":no_reset, "missingKey":no_key, "missingModel":no_model,
            "bootstrapMessage":applied["message"], "active":configured,
            "refreshMessage":second["message"], "reset":cleared, "cleared":cleared_state,
            "unauthenticated":unauthenticated,
        }, sort_keys=True))
    finally:
        process.terminate()
        process.wait(timeout=10)
