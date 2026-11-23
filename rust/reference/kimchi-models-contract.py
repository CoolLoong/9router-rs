import http.client
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

repo = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
local = "http://127.0.0.1:52989"
query = "/v1/models/metadata?include_in_cli=true"
official = "/kimchi" + query
tenant = "/tenant%2Falpha" + query
proxy_host = "http://account-catalog.fixtures.invalid"
cli = {"x-9r-cli-token":"68977b6fd9422fbb", "cookie":"synthetic-private=not-for-upstream",
       "authorization":"Bearer wrong-upstream-token", "x-api-key":"synthetic-other-key"}
public = {"authorization":"Bearer offline-active-key"}
labels = ("primary", "custom", "status", "empty", "missing", "same-user", "userinfo", "query",
          "fragment", "badtype", "proxy", "relay", "strict", "redirect", "oversized", "echo")
events = []


def token(label):
    return "offline-kimchi-" + label + "-catalog-credential"


def catalog(label, call):
    if label == "primary":
        vision = "fixture-kimchi-vision" + ("-refreshed" if call > 1 else "")
        return {"models":[
            {"slug":" " + vision + " ", "id":"old-wire-id", "display_name":" Vision Model ",
             "provider":"anthropic", "reasoning":True, "input_modalities":["image","video","text"],
             "limits":{"context_window":"250000", "max_output_tokens":16000}},
            {"model":"fixture-kimchi-text", "name":"Text", "provider":"custom-provider",
             "reasoning":"true", "input_modalities":None, "contextLength":"125000", "max_output_tokens":"0x4000"},
            {"id":"fixture-kimchi-text","name":"Duplicate remains on management catalog"},
            {"slug":"  ", "id":"ignored-blank-slug"}, {"id":42}, None, [],
        ]}
    if label == "empty":
        return {"models":[]}
    if label == "echo":
        return {"models":[
            {"id":token(label)},
            {"id":"fixture-kimchi-scrubbed", "name":token(label), "account":{
                "plain":token(label), "api_key":"offline-secret-unexpected-name", "auth_token":"auth-field",
            }},
        ]}
    return {"models":[{"name":"fixture-kimchi-" + label, "displayName":"Label " + label}]}


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        seen_token = self.headers.get("authorization", "").removeprefix("Bearer ")
        label = next((label for label in labels if seen_token == token(label)), "unknown")
        request_count = sum(entry == label for entry, _ in events) + 1
        allowed_path = (
            tenant if label == "custom" else
            proxy_host + "/proxy-tenant" + query if label == "proxy" else
            "/relay" if label == "relay" else official
        )
        agent = self.headers.get("user-agent", "")
        admitted = (
            self.path == allowed_path and label != "unknown"
            and self.headers.get("accept") == "application/json"
            and agent.startswith("9Router/" if mode == "native" else "kimchi/")
            and not any(self.headers.get(key) for key in ("cookie","x-api-key","x-9r-cli-token","proxy-authorization"))
        )
        if label == "relay":
            admitted &= self.headers.get("x-relay-target") == proxy_host
            admitted &= self.headers.get("x-relay-path") == "/relay-tenant" + query
        events.append((label, admitted))
        status, out, override = 200, json.dumps(catalog(label, request_count)).encode(), {}
        if not admitted:
            status, out = 422, b"unexpected request"
        elif label == "status":
            status, out = 429, b"fixture limits"
        elif label == "redirect":
            status, out, override = 307, b"", {"location":local + "/must-not-follow"}
        elif label == "oversized":
            out, override = b"", {"content-length":str(4 * 1024 * 1024 + 1)}
        self.send_response(status)
        self.send_header("content-type", "application/json")
        override.setdefault("content-length", str(len(out)))
        for name, field in override.items():
            self.send_header(name, field)
        self.end_headers()
        try:
            self.wfile.write(out)
        except (BrokenPipeError, ConnectionResetError):
            pass


def seed(data):
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)],
                   cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("DELETE FROM providerConnections")

        def pool(id, active, content):
            db.execute("""INSERT INTO proxyPools(id,isActive,testStatus,data,createdAt,updatedAt)
                        VALUES(?,?,'unknown',?,'2026-11-01T00:00:00Z','2026-11-01T00:00:00Z')""",
                       (id, active, json.dumps(content)))

        pool("kimchi-relay", 1, {"type":"vercel","proxyUrl":local + "/relay"})
        pool("kimchi-strict", 0, {"type":"http","proxyUrl":local,"strictProxy":True})
        for priority, label in enumerate(labels, 1):
            details = {"username":"shared-name" if label in ("primary", "same-user") else "catalog-" + label}
            account = {"accessToken": token(label), "providerSpecificData":details}
            auth_type = "oauth"
            if label == "primary":
                account["apiKey"] = "unused-lower-priority-api-key"
            elif label == "custom":
                details.update(kimchiEndpoint="  " + local + "/tenant%2Falpha///  ",
                               apiKey="unused-stored-lower-priority-api-key")
                account.update(accessToken=None, apiKey=token(label))
                auth_type = "apikey"
            elif label == "empty":
                account["accessToken"] = None
                details["apiKey"] = token(label)
            elif label == "missing":
                account["accessToken"] = None
            elif label == "userinfo":
                details["kimchiEndpoint"] = local.replace("://","://synthetic@")
            elif label == "query":
                details["kimchiEndpoint"] = local + "?secret=serialized"
            elif label == "fragment":
                details["kimchiEndpoint"] = local + "#redirect"
            elif label == "badtype":
                details["kimchiEndpoint"] = {"url":local}
            elif label == "proxy":
                details.update(kimchiEndpoint=proxy_host + "/proxy-tenant",
                               connectionProxyEnabled=True, connectionProxyUrl=local)
            elif label == "relay":
                details.update(kimchiEndpoint=proxy_host + "/relay-tenant", proxyPoolId="kimchi-relay")
            elif label == "strict":
                details["proxyPoolId"] = "kimchi-strict"
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                        VALUES(?,'kimchi',?,?,?,?,?,'2026-11-01T00:00:00Z','2026-11-01T00:00:00Z')""",
                       (label, auth_type, label, priority, int(label == "primary"), json.dumps(account)))


def request(port, path, headers=cli):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request("GET", path, headers=headers)
        response = client.getresponse()
        return response.status, json.loads(response.read())
    finally:
        client.close()


def active(db, label):
    with sqlite3.connect(db) as connection:
        connection.execute("UPDATE providerConnections SET isActive=(id=?)", [label])


def scenarios(port, db, fallbacks):
    def get(label, headers=cli):
        return request(port, "/api/providers/" + label + "/models", headers)

    def live_catalog():
        status, result = request(port, "/v1/models", public)
        assert status == 200
        return [item for item in result["data"] if item["id"].startswith("kimchi/")]

    observed = {"auth":get("primary", {}), "gone": get("does-not-exist")}
    assert observed["auth"][0] == 401 and observed["gone"][0] == 404
    assert not events
    observed["initial"] = live_catalog()
    assert [row["id"] for row in observed["initial"]] == ["kimchi/fixture-kimchi-vision", "kimchi/fixture-kimchi-text"]
    assert observed["initial"] == live_catalog()
    assert events == [("primary", True)]
    observed["forced"] = get("primary")
    assert observed["forced"][0] == 200
    vision, text, duplicate = observed["forced"][1]["models"]
    assert vision["id"] == "fixture-kimchi-vision-refreshed" and vision["name"] == "Vision Model"
    assert vision["type"] == vision["kind"] == "imageToText" and vision["inputModalities"] == ["image","text"]
    assert vision["contextLength"] == 250000 and vision["maxOutputTokens"] == 16000
    assert vision["compat"] == {"supportsReasoningEffort":False,"cacheControlFormat":"anthropic"}
    assert text["kind"] == "llm" and text["capabilities"]["reasoning"] is False
    assert text["contextLength"] == 125000 and text["maxOutputTokens"] == 16384
    assert duplicate["id"] == text["id"]
    observed["renewed"] = live_catalog()
    assert observed["renewed"][0]["capabilities"]["vision"] is True
    assert events == [("primary", True)] * 2

    observed["custom"] = get("custom")
    assert observed["custom"][1]["models"][0]["id"] == "fixture-kimchi-custom"
    for label in ("empty", "status", "missing"):
        observed[label] = get(label)
        assert observed[label][0] == 200
        assert observed[label][1]["warning"] == "Kimchi returned no live models; falling back to static catalog."
        assert observed[label][1]["models"][0]["id"] == "minimax-m3"
    assert events == [("primary", True)] * 2 + [("custom",True),("empty",True),("status",True)]

    if mode == "native":
        active(db, "same-user")
        assert [item["id"] for item in live_catalog()] == ["kimchi/fixture-kimchi-same-user"]
        active(db, "primary")
        assert live_catalog() == observed["renewed"]
        assert events[-1] == ("same-user",True) and len(events) == 6
        for label in ("userinfo", "query", "fragment", "badtype", "strict"):
            assert get(label)[1]["warning"].startswith("Kimchi returned no live models")
        assert len(events) == 6
        for label in ("proxy", "relay"):
            assert get(label)[1]["models"][0]["id"] == "fixture-kimchi-" + label
        for label in ("redirect", "oversized"):
            assert get(label)[1]["warning"].startswith("Kimchi returned no live models")
        result = get("echo")[1]["models"]
        assert len(result) == 1 and result[0]["id"] == "fixture-kimchi-scrubbed"
        assert result[0]["name"] == "[redacted]" and result[0]["account"] == {"plain":"[redacted]"}
        assert Counter(label for label, _ in events[-5:]) == {
            "proxy":1,"relay":1,"redirect":1,"oversized":1,"echo":1,
        }
        assert all(admitted for _, admitted in events)
        assert fallbacks.is_file() and not fallbacks.read_text().strip()
    return observed


with tempfile.TemporaryDirectory(prefix="9router-kimchi-models-") as directory:
    work = Path(directory)
    home, commands, data = work / "home", work / "bin", work / "data"
    home.mkdir()
    commands.mkdir()
    seed(data)
    fallbacks = work / "fallback"
    upstream = ThreadingHTTPServer(("127.0.0.1", 52989), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env = {"HOME":str(home), "PATH":str(commands), "DATA_DIR":str(data), "PORT":str(port),
               "HOSTNAME":"127.0.0.1", "NINEROUTER_MIGRATION_FIXTURE":"1",
               "MODEL_CATALOG_SYNC":"off", "DISABLE_BACKGROUND_TOKEN_REFRESH":"1",
               "RUST_ANYTHING_FALLBACK_LOG":str(fallbacks)}
        command = [str(Path(args[0]).resolve()),"--no-browser"] if mode == "native" else [
            shutil.which("node"), "--import", str(repo / "rust/reference/mock-kimchi-models.mjs"),
            str(repo / ".next/standalone/custom-server.js"),
        ]
        process = subprocess.Popen(command, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 10
            while process.poll() is None and time.monotonic() < deadline:
                try:
                    with socket.create_connection(("127.0.0.1",port), .2):
                        break
                except OSError:
                    time.sleep(.025)
            else:
                raise RuntimeError("Kimchi fixture server did not start")
            print(json.dumps(scenarios(port, data / "db/data.sqlite", fallbacks), sort_keys=True))
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    finally:
        upstream.shutdown()
        upstream.server_close()
