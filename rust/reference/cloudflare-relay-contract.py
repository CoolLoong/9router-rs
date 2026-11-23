import base64
import http.client
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Lock, Thread

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
account = "01" * 16
api_token = "offline-cloudflare-admin"
api = "/cloudflare-management/client/v4/accounts/" + account + "/workers"
pools = "/api/proxy-pools"
deploy = pools + "/cloudflare-deploy"
backup = "/api/settings/database"
session = {"x-9r-cli-token":"68977b6fd9422fbb"}
gateway = {"Authorization":"Bearer offline-chat-key"}
private = "_managedCloudflare"
host_suffix = ".fixture-account.workers.dev"
worker = (root / "rust/app/src/cloudflare_relay_worker.mjs").read_bytes()
ledger = []
relay_calls = []
failures = []
guard = Lock()
workers = {"old-worker":{"id":"ab" * 16, "published":True, "enabled":True}}
downloads = {}
redirect = False
captures = []


class Cloudflare(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def api_reply(self, result):
        self.reply(200, {"success":True,"result":result})

    def handle_request(self):
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size)) if size else None
        try:
            if self.path == "/capture":
                captures.append(self.headers.get("x-relay-auth"))
                return self.reply(200, {})
            if self.path == "/unmanaged":
                assert self.headers.get("x-relay-auth") is None
                assert self.headers["x-relay-target"] == "https://httpbin.org"
                assert self.headers["x-relay-path"] == "/get"
                relay_calls.append(("unmanaged", "GET"))
                return self.reply(200, {})
            if self.path.startswith("/cloudflare-relay/"):
                name = self.path.removeprefix("/cloudflare-relay/").removesuffix(host_suffix)
                assert self.path == f"/cloudflare-relay/{name}{host_suffix}"
                selected = workers[name]
                assert selected["enabled"] and selected["published"]
                assert self.headers.get_all("x-relay-auth") == [selected["relay_token"]]
                relay_calls.append((name,self.command))
                if redirect:
                    return self.reply(307, {}, {"Location":"/capture"})
                if self.command == "GET":
                    assert self.headers["x-relay-target"] == "https://httpbin.org"
                    assert self.headers["x-relay-path"] == "/get"
                    assert self.headers.get("Authorization") is None
                    return self.reply(200, {"ok":True})
                assert self.command == "POST"
                assert self.headers["x-relay-target"] == "https://api.openai.com"
                assert self.headers["x-relay-path"] == "/v1/chat/completions"
                assert self.headers["Authorization"] == "Bearer offline-models-provider"
                assert body["messages"] == [{"role":"user","content":"Check the private relay"}]
                return self.reply(200, {"id":"offline","object":"chat.completion","created":1700000000,
                    "model":"fixture-managed-model","choices":[{"index":0,"message":{"role":"assistant",
                    "content":"Managed Cloudflare relay"},"finish_reason":"stop"}]})

            assert self.path.startswith(api) and self.headers.get_all("Authorization") == ["Bearer "+api_token]
            ledger.append((self.command, self.path))
            if self.path == api + "/subdomain" and self.command == "GET":
                return self.api_reply({"subdomain":"fixture-account"})
            if self.path == api + "/workers" and self.command == "POST":
                name = body["name"]
                assert body == {"name":name,"observability":{"enabled":False},"logpush":False,
                                "subdomain":{"enabled":False,"previews_enabled":False}}
                if name == "auth-rejected":
                    return self.reply(403, {"success":False,"errors":[{"message":"echo "+api_token}]})
                with guard:
                    if name in workers:
                        return self.reply(409, {"success":False,"errors":[{"message":"already exists"}]})
                    worker_id = f"{len(workers):032x}"
                    workers[name] = {"id":worker_id,"enabled":False,"published":False}
                return self.api_reply({"id":worker_id,"name":name})
            suffix = self.path.removeprefix(api + "/workers/")
            worker_id = suffix.split("/")[0]
            name, destination = next((key, value) for key, value in workers.items() if value["id"] == worker_id)
            if suffix == worker_id + "/versions?deploy=true" and self.command == "POST":
                assert not destination["published"] and not destination["enabled"]
                assert body["main_module"] == "index.mjs" and body["compatibility_flags"] == [
                    "global_fetch_strictly_public","cache_option_enabled",
                ]
                module, = body["modules"]
                assert module["name"] == "index.mjs" and module["content_type"] == "application/javascript+module"
                assert base64.b64decode(module["content_base64"], validate=True) == worker
                secret, address = body["bindings"]
                assert secret["name"] == "RELAY_AUTH" and secret["type"] == "secret_text"
                assert re.fullmatch(r"[A-Za-z0-9_-]{43}", secret["text"])
                assert address == {"name":"RELAY_URL","type":"plain_text","text":"https://"+name+host_suffix}
                if name == "upload-failed":
                    return self.reply(502, {"success":False,"errors":[{"message":"echo "+secret["text"]}]})
                assert secret["text"] not in downloads
                downloads[secret["text"]] = address["text"]
                destination["relay_token"] = secret["text"]
                destination["published"] = True
                return self.api_reply({"id":"25c1de8f-1a46-45bf-ad05-b08242449459"})
            if suffix == worker_id and self.command == "PATCH":
                assert destination["published"] and not destination["enabled"]
                assert body == {"subdomain":{"enabled":True,"previews_enabled":False}}
                destination["enabled"] = True
                label = "elsewhere" if name == "wrong-url" else name
                return self.api_reply({"id":worker_id,"name":name,
                    "subdomain":{"enabled":True,"previews_enabled":False,"url":"https://"+label+host_suffix}})
            raise AssertionError((self.command, self.path))
        except (AssertionError, KeyError, StopIteration) as error:
            failures.append(str(error) or (self.command,self.path))
            self.reply(500, {})

    do_GET = do_POST = do_PATCH = do_PUT = do_DELETE = handle_request


def call(port, path, method="GET", body=None, headers=session):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=15)) as client:
        if body is not None:
            headers = headers | {"Content-Type":"application/json"}
        client.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
        answer = client.getresponse()
        return answer.status, json.loads(answer.read())


def open_gateway(parent):
    parent.mkdir()
    data = parent / "data"
    home = parent / "home"
    commands = parent / "bin"
    home.mkdir()
    commands.mkdir()
    subprocess.run([sys.executable,"rust/reference/seed.py",str(data)],cwd=root,check=True,stdout=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    environment = {
        "PATH":str(commands),"HOME":str(home),"USERPROFILE":str(home),
        "XDG_CONFIG_HOME":str(home / "config"),"XDG_DATA_HOME":str(home / "data"),
        "XDG_STATE_HOME":str(home / "state"),"XDG_CACHE_HOME":str(home / "cache"),
        "DATA_DIR":str(data),"PORT":str(port),"HOST":"127.0.0.1","MODEL_CATALOG_SYNC":"off",
        "NINEROUTER_MIGRATION_FIXTURE":"1","RUST_ANYTHING_FALLBACK_LOG":str(parent / "fallbacks"),
    }
    gateway = subprocess.Popen([str(binary),"--no-browser"],cwd=root,env=environment,
                               stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 12
    while gateway.poll() is None and time.monotonic() < deadline:
        try:
            if call(port, "/api/health")[0] == 200:
                return gateway, port, data
        except OSError:
            pass
        time.sleep(0.05)
    close_gateway(gateway)
    raise AssertionError("Fixture gateway failed to start")


def close_gateway(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise


def public(payload):
    nested = json.dumps(payload)
    assert private not in nested and api_token not in nested
    assert not any(secret in nested for secret in downloads)


def deploy_params(name):
    return {"accountId":account,"apiToken":api_token,"projectName":name}


def chat(port):
    return call(port, "/v1/chat/completions", "POST", {
        "model":"fixture-via-vercel/fixture-managed-model","stream":False,
        "messages":[{"role":"user","content":"Check the private relay"}],
    }, gateway)


def configure(data, pool, base):
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("""UPDATE providerNodes SET data=json_set(data,'$.baseUrl',?)
                      WHERE id='openai-compatible-chat-fixture-vercel'""", (base,))
        db.execute("""UPDATE providerConnections SET isActive=1,
                      data=json_set(data,'$.providerSpecificData.proxyPoolId',?,
                      '$.providerSpecificData.baseUrl',?)
                      WHERE id='openai-compatible-chat-fixture-vercel-connection'""", (pool,base))


with tempfile.TemporaryDirectory(prefix="9router-managed-cf-") as temporary:
    remote = ThreadingHTTPServer(("127.0.0.1",52989), Cloudflare)
    Thread(target=remote.serve_forever,daemon=True).start()
    running = []
    try:
        process, port, data = open_gateway(Path(temporary) / "first")
        running.append(process)
        assert call(port, deploy, "POST", deploy_params("unauthorized"), {})[0] == 401
        assert call(port, deploy, "POST", {}) == (
            400, {"error":"Cloudflare Account ID and API Token are required"},
        )
        assert call(port, deploy, "POST", deploy_params("../routed"))[0] == 400
        assert not ledger
        assert call(port, deploy, "POST", deploy_params("old-worker"))[0] == 409
        assert workers["old-worker"] == {"id":"ab"*16,"published":True,"enabled":True}
        rejected = call(port, deploy, "POST", deploy_params("auth-rejected"))
        assert rejected[0] == 403
        public(rejected[1])
        assert call(port, deploy, "POST", deploy_params("upload-failed"))[0] == 502
        assert call(port, deploy, "POST", deploy_params("wrong-url"))[0] == 502
        assert call(port, pools)[0] == 200
        assert all(name not in json.dumps(call(port,pools)[1]) for name in ("old-worker","upload-failed","wrong-url"))

        ledger.clear()
        status, result = call(port, deploy, "POST", deploy_params("managed"))
        assert status == 201, (status,result,failures)
        url = "https://managed" + host_suffix
        assert result["deployUrl"] == result["proxyPool"]["proxyUrl"] == url
        assert result["proxyPool"]["type"] == "cloudflare" and result["proxyPool"]["isActive"] is True
        public(result)
        pool = result["proxyPool"]["id"]
        route = pools + "/" + pool
        assert ledger == [
            ("GET",api+"/subdomain"),("POST",api+"/workers"),
            ("POST",api+"/workers/"+workers["managed"]["id"]+"/versions?deploy=true"),
            ("PATCH",api+"/workers/"+workers["managed"]["id"]),
        ]
        with ThreadPoolExecutor(max_workers=2) as duplicate:
            statuses = list(duplicate.map(lambda _:call(port,deploy,"POST",deploy_params("one-name"))[0],range(2)))
        assert sorted(statuses) == [201,409]
        assert len(downloads) == 3
        assert call(port, route, "PUT", {"proxyUrl":"https://elsewhere"+host_suffix})[0] == 409
        assert call(port, route, "PUT", {"type":"vercel"})[0] == 409
        status, renamed = call(port, route, "PUT", {"name":"Renamed managed relay",private:{"token":"ignored"}})
        assert status == 200 and renamed["proxyPool"]["name"] == "Renamed managed relay"
        public(renamed)
        public(call(port, route)[1])
        public(call(port, pools + "?includeUsage=true")[1])
        assert call(port, route + "/test", "POST")[1]["ok"]

        configure(data, pool, "https://api.openai.com/v1")
        status, response = chat(port)
        assert status == 200 and response["choices"][0]["message"]["content"] == "Managed Cloudflare relay", (status,response,failures)
        assert relay_calls == [("managed","GET"),("managed","POST")]

        status, unsafe = call(port, pools, "POST", {
            "name":"Unmanaged relay","type":"cloudflare","proxyUrl":"http://127.0.0.1:52989/unmanaged",
        })
        assert status == 201
        ordinary = pools + "/" + unsafe["proxyPool"]["id"]
        assert call(port, ordinary + "/test", "POST")[1]["ok"] and relay_calls[-1] == ("unmanaged","GET")

        redirect = True
        result = call(port, route + "/test", "POST")[1]
        assert result["status"] == 307 and not result["ok"] and not captures
        redirect = False
        assert call(port, route, "PUT", {"isActive":True})[0] == 200
        status, saved = call(port, backup)
        assert status == 200 and api_token not in json.dumps(saved)
        record = next(row for row in saved["proxyPools"] if row["id"] == pool)
        assert record[private] == {"url":url,"token":workers["managed"]["relay_token"]}
        assert call(port, backup, headers={})[0] == 401

        replacement, replaced_port, _ = open_gateway(Path(temporary) / "replacement")
        running.append(replacement)
        assert call(replaced_port, backup, "POST", saved) == (200,{"success":True})
        public(call(replaced_port,route)[1])
        assert call(replaced_port,route+"/test","POST")[1]["ok"]
        status, response = chat(replaced_port)
        assert status == 200 and response["choices"][0]["message"]["content"] == "Managed Cloudflare relay", (status,response,failures)

        configure(data, pool, "http://127.0.0.1:52989/capture")
        previous = list(relay_calls)
        assert chat(port)[0] in (502,503)
        assert relay_calls == previous and not captures

        record["proxyUrl"] = "http://127.0.0.1:52989/capture"
        assert call(replaced_port,backup,"POST",saved)[0] == 200
        before = list(relay_calls)
        assert call(replaced_port,route+"/test","POST")[1]["status"] == 400
        assert relay_calls == before and not captures
        assert not failures, failures
        for parent in ["first","replacement"]:
            log = Path(temporary) / parent / "fallbacks"
            assert not log.exists() or log.read_text() == ""
    finally:
        for process in running:
            close_gateway(process)
        remote.shutdown()
        remote.server_close()

print("Cloudflare create-only deployment, exact authenticated relay, redacted CRUD and portable backup passed")
