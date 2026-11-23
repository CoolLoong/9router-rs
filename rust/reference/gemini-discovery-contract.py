import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
from threading import Thread
import time
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
accounts = {
    "fixture-list-first":"offline-gemini-catalog-first",
    "fixture-list-second":"offline-gemini-catalog-second",
    "fixture-list-denied":"offline-gemini-catalog-denied",
    "fixture-list-malformed":"offline-gemini-catalog-malformed",
    "fixture-list-echo":"offline-gemini-catalog-echo",
    "fixture-list-empty":"offline-gemini-catalog-empty",
}
first = [
    {"name":"models/gemini-fixture-language","displayName":"Offline language model",
     "supportedGenerationMethods":["generateContent"],"inputTokenLimit":16384},
    {"name":"models/gemini-fixture-embedding","displayName":"Offline embedding model",
     "supportedGenerationMethods":["embedContent"],"description":"fixture metadata"},
]
second = [{"name":"models/gemini-fixture-second","displayName":"Second saved key only"}]
seen = []
seen_routes = []


class Google(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/relay":
            assert self.headers["x-relay-target"] == "http://127.0.0.1:52989"
            assert self.headers["x-relay-path"] == "/gemini/v1beta/models"
            seen_routes.append("relay")
        else:
            assert url.path == "/gemini/v1beta/models", self.path
            seen_routes.append("proxy" if url.scheme else "direct")
        if implementation == "rust":
            assert not url.query and not self.headers.get("Authorization")
            key = self.headers.get("x-goog-api-key")
        else:
            assert self.headers.get("x-goog-api-key") is None
            key = parse_qs(url.query).get("key",[None])[0]
        label = next(name for name, value in accounts.items() if key == value)
        seen.append(label)
        status, payload = {
            "fixture-list-first": (200, {"models":first,"nextPageToken":"pinned-single-page"}),
            "fixture-list-second": (200, {"models":second}),
            "fixture-list-empty": (200, {}),
            "fixture-list-denied": (403, {"error":{"message":"synthetic upstream " + key}}),
            "fixture-list-malformed": (200, b"{"),
            "fixture-list-echo": (200, {"models":[{"name":"models/gemini-echo","description":key}]}),
        }[label]
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def request(port, path, method="GET", body=None, cookie=None):
    headers = {"Content-Type":"application/json"}
    if cookie:
        headers["Cookie"] = cookie
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
        res = conn.getresponse()
        return res.status, json.loads(res.read()), res.headers
    finally:
        conn.close()


def scenario(port):
    login = request(port, "/api/auth/login", "POST", {"password":"123456"})
    assert login[0] == 200
    cookie = login[2]["Set-Cookie"].split(";",1)[0]
    route = lambda name: f"/api/providers/{name}/models"
    code, payload, _ = request(port, route("fixture-list-first"))
    assert (code, payload) == (401, {"error":"Unauthorized"}) and not seen
    snapshots = {}
    for name, expected in [("fixture-list-first",first),("fixture-list-second",second),("fixture-list-empty",[])]:
        code, value, _ = request(port, route(name) + "?apiKey=offline-caller-key&pageToken=ignored", cookie=cookie)
        assert (code, value) == (200, {"provider":"gemini","connectionId":name,"models":expected}), (code,value)
        snapshots[name] = value
    for name, status, error in [
        ("fixture-list-missing",404,"Connection not found"),
        ("fixture-list-no-key",401,"No valid token found"),
        ("fixture-list-denied",403,"Failed to fetch models: 403"),
        ("fixture-list-malformed",500,"Failed to fetch models"),
    ]:
        code, value, _ = request(port, route(name), cookie=cookie)
        assert (code, value) == (status, {"error":error}), (name,code,value)
        snapshots[name] = {"code":code,"body":value}
    assert seen == ["fixture-list-first","fixture-list-second","fixture-list-empty","fixture-list-denied","fixture-list-malformed"]
    if implementation == "rust":
        code, value, _ = request(port, route("fixture-list-echo"), cookie=cookie)
        assert code == 200 and value["models"][0]["description"] == "[redacted]", value
        model = second[0]
        code, value, _ = request(port, "/api/models/custom", "POST", {
            "providerAlias":"gemini", "id":model["name"].removeprefix("models/"),
            "name":model["displayName"], "type":"llm",
        }, cookie)
        assert code in {200, 201}, (code,value)
        code, value, _ = request(port, "/api/models/custom", cookie=cookie)
        assert code == 200 and any(row["id"] == "gemini-fixture-second" and row["name"] == model["displayName"]
                                   for row in value["models"]), value
        def egress(kind, address, no_proxy=""):
            code, value, _ = request(port, "/api/proxy-pools", "POST", {
                "name":"Gemini catalog fixture", "type":kind,"proxyUrl":address,
                "strictProxy":True, "noProxy":no_proxy,
            }, cookie)
            assert code == 201
            code, _, _ = request(port, "/api/providers/fixture-list-first", "PUT", {
                "proxyPoolId":value["proxyPool"]["id"],
            }, cookie)
            assert code == 200

        for kind, mode, suffix in [("http","proxy",""),("vercel","relay","/relay")]:
            egress(kind, "http://127.0.0.1:52989" + suffix)
            code, value, _ = request(port, route("fixture-list-first"), cookie=cookie)
            assert code == 200 and value["models"] == first and seen_routes[-1] == mode
        before = len(seen_routes)
        egress("http","http://127.0.0.1:52989","127.0.0.1")
        code, value, _ = request(port, route("fixture-list-first"), cookie=cookie)
        assert (code,value) == (500,{"error":"Failed to fetch models"}) and len(seen_routes) == before
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-gemini-model-discovery-") as temporary:
    scratch = Path(temporary)
    data = scratch / "data"
    home = scratch / "home"
    bin_dir = scratch / "bin"
    home.mkdir()
    bin_dir.mkdir()
    fake_sh = bin_dir / "sh"
    fake_sh.write_text("#!/bin/sh\nexit 42\n")
    fake_sh.chmod(0o700)
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)],
                   cwd=root, env=dict(os.environ, NODE_NO_WARNINGS="1"), check=True)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES (?,'gemini','apikey','Offline Gemini discovery',10,1,?,
                       '2026-11-20T00:00:00.000Z','2026-11-20T00:00:00.000Z')""",
                       [(name,json.dumps({"apiKey":key,"providerSpecificData":{"baseUrl":"http://127.0.0.1:3"}}))
                        for name,key in accounts.items()] + [("fixture-list-no-key","{}")])
    preload = scratch / "fixture-module.mjs"
    module = json.dumps(str(root / ".next/standalone/server.js"))
    preload.write_text(f"""
import {{createRequire}} from "node:module";
const undici = createRequire({module})("undici");
const wrap = call => (input, options) => {{
  const url = new URL(input instanceof Request ? input.url : input);
  if (url.origin !== "https://generativelanguage.googleapis.com" || url.pathname !== "/v1beta/models")
    return call(input, options);
  const dest = "http://127.0.0.1:52989/gemini" + url.pathname + url.search;
  return call(input instanceof Request ? new Request(dest, input) : dest, options);
}};
globalThis.fetch = wrap(globalThis.fetch);
undici.fetch = wrap(undici.fetch);
""")
    google = ThreadingHTTPServer(("127.0.0.1",52989),Google)
    Thread(target=google.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0))
        port = sock.getsockname()[1]
    env = {
        "HOME":str(home), "PATH":os.environ["PATH"], "PORT":str(port), "HOSTNAME":"127.0.0.1",
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_FIXTURE":"1", "MODEL_CATALOG_SYNC":"off",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch/"fallbacks"), "NEXT_TELEMETRY_DISABLED":"1",
        "JWT_SECRET":"offline-gemini-fixture-auth-key", "NODE_NO_WARNINGS":"1",
    }
    if implementation == "rust":
        env["PATH"] = str(bin_dir)
        command = [str(binary)]
    else:
        command = [shutil.which("node"),"--import",str(preload),".next/standalone/custom-server.js"]
    with (scratch/"server.log").open("w") as log:
        proc = subprocess.Popen(command,cwd=root,env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log,
                                start_new_session=True)
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    with socket.create_connection(("127.0.0.1",port), timeout=.15):
                        break
                except OSError:
                    if proc.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError("fixture server did not start")
                    time.sleep(.025)
            snapshots = scenario(port)
            if implementation == "rust":
                assert (scratch/"fallbacks").read_text() == ""
            print(json.dumps(snapshots,sort_keys=True,separators=(",",":")))
        except Exception:
            print((scratch/"server.log").read_text(),file=sys.stderr)
            raise
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid,signal.SIGTERM)
            try:
                proc.wait(timeout=4)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid,signal.SIGKILL)
                proc.wait(timeout=2)
            google.shutdown()
            google.server_close()
