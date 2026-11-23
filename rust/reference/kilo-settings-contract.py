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
cli = {"x-9r-cli-token":"68977b6fd9422fbb"}
endpoint = "/api/cli-tools/kilo-settings"
initial = {"remote":{"type":"api-key","apiKey":"synthetic-kept"}}


def request(port, method, data=None, headers=cli, path=endpoint):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = dict(headers)
        body = None
        if data is not None:
            body = json.dumps(data).encode()
            headers["content-type"] = "application/json"
        connection.request(method, path, body, headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read())
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-kilo-contract-") as folder:
    base = Path(folder)
    home = base / "home"
    home.mkdir()
    data = base / "data"
    auth = home / ".local/share/kilo/auth.json"
    vscode = home / ".config/Code/User/settings.json"
    fallback = base / "fallback.log"
    isolated_path = base / "bin"
    isolated_path.mkdir()
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)],
                   cwd=root, stdout=subprocess.DEVNULL, check=True)
    if mode == "original":
        command = [shutil.which("node"), str(root / ".next/standalone/custom-server.js")]
    else:
        command = [str(Path(args[0]).resolve()), "--no-browser"]
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = dict(os.environ, HOSTNAME="127.0.0.1", HOME=str(home), DATA_DIR=str(data),
               PATH=str(isolated_path), PORT=str(port), MODEL_CATALOG_SYNC="off",
               DISABLE_BACKGROUND_TOKEN_REFRESH="1", RUST_ANYTHING_FALLBACK_LOG=str(fallback))
    process = subprocess.Popen(command, env=env, cwd=root, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        for _ in range(300):
            try:
                if request(port, "GET", path="/api/health", headers={})[0] == 200:
                    break
            except OSError:
                time.sleep(.04)
        else:
            raise RuntimeError("Kilo fixture app did not become ready")
        assert request(port, "GET", headers={})[0] == 401
        assert request(port, "GET") == (200, {
            "installed": False, "settings": None, "message": "Kilo Code CLI is not installed",
        })
        assert request(port, "POST", {"baseUrl":"http://localhost:20929"}) == (
            400, {"error":"baseUrl, apiKey and model are required"},
        )
        auth.parent.mkdir(parents=True)
        auth.write_text(json.dumps(initial))
        vscode.parent.mkdir(parents=True)
        vscode.write_text('{"editor.fontSize": 15,}')
        settings = {"baseUrl":"http://localhost:20929", "apiKey":"synthetic-kilo",
                    "model":"provider/model"}
        expected_auth = {**initial, "openai-compatible":{
            "type":"api-key","apiKey":settings["apiKey"],"baseUrl":settings["baseUrl"]+"/v1",
            "model":settings["model"],
        }}
        status, payload = request(port, "POST", settings)
        assert status == 200 and payload["success"]
        assert Path(payload["authPath"]) == auth and json.loads(auth.read_text()) == expected_auth
        config = json.loads(vscode.read_text())
        assert config == {"editor.fontSize":15, "kilocode.defaultModel":"provider/model",
                          "kilocode.customProvider":{"name":"9Router","baseURL":settings["baseUrl"]+"/v1",
                                                    "apiKey":settings["apiKey"]}}
        status, listed = request(port, "GET")
        assert status == 200 and listed["settings"]["auth"] == ["remote","openai-compatible"]
        assert listed["has9Router"] and "synthetic-" not in json.dumps(listed)
        assert request(port, "DELETE") == (200, {
            "success":True, "message":"9Router settings removed from Kilo Code",
        })
        assert json.loads(auth.read_text()) == initial and json.loads(vscode.read_text()) == {"editor.fontSize":15}
        if mode == "native":
            auth.write_text("{malformed")
            assert request(port, "POST", settings)[0] == 500 and auth.read_text() == "{malformed"
            auth.unlink()
            assert request(port, "POST", settings)[0] == 200 and auth.stat().st_mode & 0o777 == 0o600
            assert not fallback.read_text().strip()
        print(json.dumps({"apply":True,"reset":True,"retained":True,"config_redacted":True}))
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
