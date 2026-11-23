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
from collections import Counter
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
mode, *arguments = sys.argv[1:]
binary = Path(arguments[0]).resolve() if mode == "native" else None
fixture = repo / "rust/reference/mock-cursor-models.mjs"
cli = {
    "x-9r-cli-token": "68977b6fd9422fbb",
    "cookie": "fixture-only-dashboard-cookie=absent",
    "x-api-key": "fixture-only-router-header",
}
ingress = {"authorization": "Bearer offline-active-key"}
provider = "/api/providers/fixture-cursor-"
machines = {
    "primary": "01234567-89ab-cdef-0123-456789abcdef",
    "secondary": "89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567",
    "status": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    "empty": "33333333-2222-1111-2222-333333333333",
    "malformed": "aaaaaaaa-ffff-0000-ffff-aaaaaaaaaaaa",
    "redirect": "11111111-0000-1111-0000-111111111111",
    "echo": "abcdefab-cdef-abcd-efab-cdefabcdefab",
}


def saved_account(label):
    token = f"synthetic-cursor-{label}-fixture-only"
    if label == "primary":
        token = "fixture-user::" + token
    details = {"machineId": machines[label]} if label in machines else {}
    if label == "secondary":
        details["ghostMode"] = False
    if label == "proxy":
        details = {"machineId": machines["primary"], "connectionProxyEnabled": True,
                   "connectionProxyUrl": "http://127.0.0.1:52982"}
    if label == "invalid":
        details["machineId"] = "bogus-machine"
    return json.dumps({"accessToken": token, "providerSpecificData": details})


def seed(data):
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)],
                   cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("DELETE FROM providerConnections")
        labels = [*machines, "missing", "invalid", "proxy"]
        db.executemany(
            """INSERT INTO providerConnections(id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
               VALUES(?, 'cursor', 'oauth', ?, ?, ?, ?, '2026-11-01T00:00:00.000Z', '2026-11-01T00:00:00.000Z')""",
            [(f"fixture-cursor-{key}", f"Fixture {key}", index, int(key == "primary"), saved_account(key))
             for index, key in enumerate(labels, 1)],
        )


def wait_server(process, port):
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(.025)
    raise RuntimeError("Cursor fixture listener did not start")


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=6)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def request(port, path, headers=cli):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=11)
    try:
        client.request("GET", path, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        client.close()


def scenario(port, ledger):
    observed = {}

    def models(label, headers=cli):
        return request(port, provider + label + "/models", headers)

    observed["guard"] = models("primary", {})
    observed["missing-connection"] = models("not-in-database")
    assert observed["guard"][0] == 401 and observed["missing-connection"][0] == 404
    assert not ledger.exists()

    model_ids = [
        "fixture-cursor-live", "default", "fixture-cursor-chat", "tool/model-with-slash",
        "fixture-cursor-hidden", "fixture-cursor-whitespace",
    ]
    status, response = request(port, "/v1/models", ingress)
    published = [item["id"] for item in response["data"] if item["id"].startswith("cu/")]
    assert status == 200 and set(published) == {"cu/" + identifier for identifier in model_ids}
    observed["published"] = sorted(published)
    assert len(ledger.read_text().splitlines()) == 1

    observed["presets"] = request(port, "/api/combos/presets?source=cursor")
    status, response = observed["presets"]
    assert status == 200
    assert [item["name"] for item in response["items"]] == [identifier for identifier in model_ids if "/" not in identifier]
    assert len(ledger.read_text().splitlines()) == 1

    observed["stored"] = models("primary")
    observed["forced"] = models("primary")
    assert observed["stored"] == observed["forced"]
    status, response = observed["stored"]
    assert status == 200 and [item["id"] for item in response["models"]] == model_ids
    assert response["models"][0] == {
        "id": "fixture-cursor-live", "name": "Live Label", "defaultOn": True,
        "isChatOnly": False, "supportsAgent": True, "isHidden": False,
    }
    assert response["models"][2]["isChatOnly"] and response["models"][4]["isHidden"]
    assert response["models"][3] == {"id": "tool/model-with-slash", "name": "Slash"}
    assert response["models"][5]["name"] == ""

    observed["secondary"] = models("secondary")
    assert observed["secondary"][1]["models"] == [
        {"id":"fixture-cursor-second", "name":"fixture-cursor-second"},
        {"id":"fixture-cursor-utf8", "name":"�"},
    ]
    for label in ("missing", "status", "empty", "redirect"):
        observed[label] = models(label)
        status, result = observed[label]
        assert status == 200 and result["warning"] == "Cursor returned no live models; falling back to static catalog."
        assert result["models"][0]["id"] == "default"
    calls = [json.loads(line) for line in ledger.read_text().splitlines()]
    assert all(item["admitted"] for item in calls), calls
    assert Counter(item["name"] for item in calls) == {
        "primary": 3, "secondary": 1, "status": 1, "empty": 1, "redirect": 1,
    }

    if mode == "native":
        for label in ("malformed", "invalid", "proxy"):
            status, result = models(label)
            assert status == 200 and result["warning"], label
        assert models("echo")[1]["models"] == [{"id":"fixture-cursor-clean", "name":"fixture-cursor-clean"}]
        extra = [json.loads(line) for line in ledger.read_text().splitlines()[len(calls):]]
        assert extra == [{"name":"malformed", "admitted":True}, {"name":"echo", "admitted":True}], extra
    return observed


with tempfile.TemporaryDirectory(prefix="9router-cursor-models-") as directory:
    root = Path(directory)
    home = root / "home"
    home.mkdir()
    data = root / "data"
    seed(data)
    fallback = root / "fallback.log"
    empty_path = root / "bin"
    empty_path.mkdir()
    requests = root / "cursor-requests.jsonl"
    node = shutil.which("node")
    mock = subprocess.Popen([node, str(fixture), "serve-cursor-models", str(requests), mode],
                            cwd=repo, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_server(mock, 52982)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env = {"PATH":str(empty_path), "HOME":str(home), "PORT":str(port),
               "DATA_DIR":str(data), "HOSTNAME":"127.0.0.1", "NINEROUTER_MIGRATION_FIXTURE":"1",
               "RUST_ANYTHING_FALLBACK_LOG":str(fallback), "MODEL_CATALOG_SYNC":"off",
               "DISABLE_BACKGROUND_TOKEN_REFRESH":"1"}
        command = [str(binary), "--no-browser"] if binary else [
            node, "--import", str(fixture), str(repo / ".next/standalone/custom-server.js"),
        ]
        process = subprocess.Popen(command, env=env, cwd=repo,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            wait_server(process, port)
            result = scenario(port, requests)
            if mode == "native":
                assert fallback.is_file() and not fallback.read_text().strip()
            print(json.dumps(result, sort_keys=True))
        finally:
            stop(process)
    finally:
        stop(mock)
