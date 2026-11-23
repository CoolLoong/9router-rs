import base64
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

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
chat = "/v1/chat/completions"
messages = "/v1/messages"
responses = "/v1/responses"
generation = "/v1/images/generations"
model = "gemini/gemini-2.5-flash"
image_ip = "93.184.216.34"
valid_host = "image-prefetch.fixture.test"
mixed_host = "mixed-prefetch.fixture.test"
image_bytes = b"\x89PNG\r\n\x1a\nfixture-main"
gif_bytes = b"GIF89a" + b"fixture-extra"
seen_media = []
seen_upstream = {}


def enter_private_network():
    if os.environ.get("CHAT_PREFETCH_CONTRACT_NS") == "1":
        return
    with tempfile.TemporaryDirectory(prefix="9router-prefetch-namespace-") as tmp:
        hosts = Path(tmp) / "hosts"
        hosts.write_text(
            f"127.0.0.1 localhost {mixed_host}\n{image_ip} {valid_host} {mixed_host}\n"
        )
        nss = Path(tmp) / "nsswitch.conf"
        nss.write_text("passwd: files\ngroup: files\nhosts: files\n")
        cmd = """ip link set lo up &&
ip addr add "$1"/32 dev lo &&
mount --make-rprivate / &&
mount --bind "$2" /etc/hosts &&
mount --bind "$3" /etc/nsswitch.conf &&
shift 3 &&
export CHAT_PREFETCH_CONTRACT_NS=1 &&
exec "$@" """
        result = subprocess.run([
            "unshare", "-Urnm", "--", "/bin/sh", "-c", cmd, "prefetch", image_ip, str(hosts), str(nss),
            sys.executable, str(Path(__file__).resolve()), *sys.argv[1:],
        ])
        sys.exit(result.returncode)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def handle_payload(self, status, mime, raw, extra=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        for header, value in (extra or {}).items():
            self.send_header(header, value)
        self.end_headers()
        try:
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            pass


class Media(Handler):
    def do_GET(self):
        assert self.headers["Host"] == f"{valid_host}:{self.server.server_port}", self.headers
        assert all(self.headers.get(field) is None for field in (
            "Authorization", "Cookie", "X-Api-Key", "X-Goog-Api-Key", "X-Extra-Secret", "Proxy-Authorization",
        )), self.headers
        seen_media.append(self.path)
        name = self.path.split("?", 1)[0]
        if name == "/redirect":
            self.handle_payload(302, "text/plain", b"move", {"Location":"/not-followed"})
        elif name == "/jpeg":
            self.handle_payload(200, "application/octet-stream", b"\xff\xd8\xffoffline JPEG")
        elif name == "/gif":
            self.handle_payload(200, "application/octet-stream", gif_bytes)
        elif name == "/bad":
            self.handle_payload(200, "image/png", b"synthetic invalid image")
        elif name == "/oversized":
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.end_headers()
            try:
                self.wfile.write(image_bytes)
                for _ in range(161):
                    self.wfile.write(b"a" * 65536)
            except (BrokenPipeError, ConnectionResetError):
                pass
        elif name == "/slow":
            time.sleep(12)
            self.handle_payload(200, "image/png", image_bytes)
        else:
            self.handle_payload(200, "application/octet-stream", image_bytes)


class Provider(Handler):
    def do_POST(self):
        if self.path == "/openai" + generation:
            assert self.headers["Authorization"] == "Bearer offline-generated-image-provider"
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert payload["model"] == "dall-e-3"
            public = f"http://{valid_host}:{self.server.media_port}"
            urls = {
                "public": public + "/png?signature=offline-generated-image",
                "private": f"http://127.0.0.1:{self.server.server_port}/never",
                "redirect": public + "/redirect",
                "invalid": public + "/bad",
            }
            url = urls[payload["prompt"]]
            self.handle_payload(200, "application/json", json.dumps({"created":1700000000,"data":[{"url":url}]}).encode())
            return
        assert self.path == "/gemini/v1beta/models/gemini-2.5-flash:generateContent", self.path
        assert self.headers["X-Goog-Api-Key"] == "offline-prefetch-provider", self.headers
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        parts = body["contents"][0]["parts"]
        label = parts[0]["text"]
        seen_upstream[label] = parts[1:]
        payload = {
            "responseId":"fixture-prefetch-response",
            "modelVersion":"gemini-2.5-flash",
            "candidates":[{"index":0,"finishReason":"STOP","content":{"role":"model","parts":[{"text":"offline image receipt"}]}}],
            "usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":4,"totalTokenCount":16},
        }
        self.handle_payload(200, "application/json", json.dumps(payload).encode())


def initialize_reference(directory):
    module = directory / "fixture-prefetch.mjs"
    require_path = json.dumps(str(root / ".next/standalone/server.js"))
    module.write_text(f"""
import {{ createRequire }} from "node:module";
const undici = createRequire({require_path})("undici");
const remap = call => (request, options) => {{
  const input = new URL(request instanceof Request ? request.url : request);
  const prefix = input.origin === "https://generativelanguage.googleapis.com" ? "/gemini" :
    input.origin === "https://api.openai.com" && input.pathname === {json.dumps(generation)} ? "/openai" : null;
  if (!prefix) return call(request, options);
  const target = "http://127.0.0.1:52989" + prefix + input.pathname + input.search;
  return call(request instanceof Request ? new Request(target, request) : target, options);
}};
globalThis.fetch = remap(globalThis.fetch);
undici.fetch = remap(undici.fetch);
""")
    node = shutil.which("node")
    baseline = [node, "--import", str(module), str(root / ".next/standalone/custom-server.js")]
    binary_dir = directory / "bin"
    binary_dir.mkdir()
    wrapper = binary_dir / "sh"
    wrapper.write_text('#!/bin/sh\nexec "$PREFETCH_NODE" --import "$PREFETCH_MOCK" "$PREFETCH_SERVER"\n')
    wrapper.chmod(0o700)
    return baseline, {
        "PREFETCH_NODE":node, "PREFETCH_MOCK":str(module), "PREFETCH_SERVER":baseline[-1],
        "PATH":f"{binary_dir}:{os.environ['PATH']}",
    }


def ask(port, path, label, url, authorized=True):
    if path == chat:
        request = {"messages":[{"role":"user","content":[{"type":"text","text":label},
            {"type":"image_url","image_url":{"url":url,"detail":"low"}}]}]}
    elif path == messages:
        request = {"max_tokens":100,"messages":[{"role":"user","content":[{"type":"text","text":label},
            {"type":"image","source":{"type":"url","url":url}}]}]}
    else:
        request = {"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":label},
            {"type":"input_image","image_url":url,"detail":"low"}]}]}
    request |= {"model":model, "stream":False}
    headers = {
        "Content-Type":"application/json", "Cookie":"fixture=offline-cookie",
        "X-Api-Key":"offline-nonforwarded-key", "X-Extra-Secret":"offline-synthetic-only",
    }
    if authorized:
        headers["Authorization"] = "Bearer offline-chat-key"
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        client.request("POST", path, json.dumps(request), headers)
        reply = client.getresponse()
        raw = json.loads(reply.read())
        return reply.status, raw
    finally:
        client.close()


def converted(mime, data):
    return [{"inlineData":{"mime_type":mime,"data":base64.b64encode(data).decode()}}]


def generated(port, prompt, binary=True):
    path = generation + ("?response_format=binary" if binary else "")
    payload = json.dumps({"model":"openai/dall-e-3","prompt":prompt,"response_format":"url"})
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        client.request("POST", path, payload, {
            "Content-Type":"application/json", "Authorization":"Bearer offline-chat-key",
            "Cookie":"fixture=offline-cookie", "X-Extra-Secret":"offline-synthetic-only",
        })
        response = client.getresponse()
        return response.status, response.headers, response.read()
    finally:
        client.close()


def scenario(port, media_port):
    base = f"http://{valid_host}:{media_port}"
    assert {result[4][0] for result in socket.getaddrinfo(mixed_host, media_port)} == {image_ip, "127.0.0.1"}
    snapshots = {}
    for route, label, url, expected in [
        (chat, "chat-image", base + "/png?signature=offline", converted("image/png", image_bytes)),
        (messages, "claude-image", base + "/gif", converted("image/gif", gif_bytes)),
        (chat, "inline-image", "data:image/png;base64,ZmFrZQ==",
         [{"inlineData":{"mime_type":"image/png","data":"ZmFrZQ=="}}]),
    ]:
        status, value = ask(port, route, label, url)
        assert status == 200, (status, value)
        text = value["content"][0]["text"] if "content" in value else value["choices"][0]["message"]["content"]
        assert text == "offline image receipt", value
        assert seen_upstream[label] == expected, (label, seen_upstream[label])
        snapshots[label] = {"content":text, "image":expected}
    assert seen_media == ["/png?signature=offline", "/gif"], seen_media
    assert ask(port, chat, "unauthorized", base + "/unauth", authorized=False)[0] == 401
    assert "unauthorized" not in seen_upstream and "/unauth" not in seen_media

    if implementation == "rust":
        result = ask(port, responses, "responses-image", base + "/jpeg")
        assert result[0] == 200 and seen_upstream["responses-image"] == converted("image/jpeg", b"\xff\xd8\xffoffline JPEG"), result
        before = seen_media.copy()
        for label, url in [
            ("loopback-literal", f"http://127.0.0.1:{media_port}/never"),
            ("mixed-resolver", f"http://{mixed_host}:{media_port}/never"),
            ("embedded-credentials", f"http://offline-user:offline-password@{valid_host}:{media_port}/never"),
            ("blocked-port", f"http://{valid_host}:22/never"),
        ]:
            status, value = ask(port, chat, label, url)
            assert status == 400 and "public HTTP address" in value["error"]["message"], (label, status, value)
            assert "offline-password" not in json.dumps(value) and label not in seen_upstream
        assert seen_media == before
        started = time.monotonic()
        status, value = ask(port, chat, "slow", base + "/slow")
        elapsed = time.monotonic() - started
        assert status == 200 and 9 <= elapsed < 12, (status, elapsed, value)
        assert seen_upstream["slow"] == [{"fileData":{"fileUri":base + "/slow","mimeType":"image/png"}}]
        assert seen_media == [*before, "/slow"]
    for label in ("bad", "redirect", "oversized"):
        url = base + "/" + label
        before = len(seen_media)
        status, value = ask(port, chat, label, url)
        assert status == 200, (label, status, value)
        assert seen_upstream[label] == [{"fileData":{"fileUri":url,"mimeType":"image/png"}}], seen_upstream[label]
        assert seen_media[before:] == ["/" + label], (label, seen_media[before:])
        snapshots[label] = {"remoteReferencePreserved":True, "mediaRequests":1}
    assert "/not-followed" not in seen_media
    checkpoint = len(seen_media)
    status, headers, raw = generated(port, "public")
    assert status == 200 and raw == image_bytes and headers["content-type"] == "image/png"
    assert headers["content-disposition"] == 'inline; filename="image.png"'
    assert seen_media[checkpoint:] == ["/png?signature=offline-generated-image"]
    snapshots["generated-binary"] = {"base64":base64.b64encode(raw).decode(), "type":headers["content-type"]}
    checkpoint = len(seen_media)
    status, headers, raw = generated(port, "public", binary=False)
    assert status == 200 and headers["content-type"] == "application/json"
    assert json.loads(raw)["data"][0]["url"].endswith("/png?signature=offline-generated-image")
    assert seen_media[checkpoint:] == []
    if implementation == "rust":
        for label in ("private", "redirect", "invalid"):
            status, headers, raw = generated(port, label)
            assert status == 200 and headers["content-type"] == "application/json", (label,status,raw)
            assert json.loads(raw)["data"][0]["url"]
        assert seen_media[checkpoint:] == ["/redirect", "/bad"]
    return snapshots


enter_private_network()
with tempfile.TemporaryDirectory(prefix="9router-prefetch-contract-") as directory:
    scratch = Path(directory)
    data = scratch / "data"
    home = scratch / "home"
    home.mkdir()
    baseline, wrapper = initialize_reference(scratch)
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], cwd=root,
                   env=dict(os.environ, NODE_NO_WARNINGS="1"), check=True)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("""UPDATE providerConnections SET isActive=1,data=json_set(data,'$.apiKey','offline-prefetch-provider')
                      WHERE id='fixture-gemini-two'""")
        db.execute("""INSERT INTO providerConnections
                     (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                     VALUES ('image-output-fixture','openai','apikey','Image output fixture',1,1,
                             '{"apiKey":"offline-generated-image-provider"}',
                             '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""")
        db.execute("""INSERT INTO settings(id,data) VALUES(1,'{"requireApiKey":true}')
                      ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.requireApiKey',json('true'))""")
    providers = ThreadingHTTPServer(("127.0.0.1", 52989), Provider)
    media = ThreadingHTTPServer((image_ip, 0), Media)
    providers.media_port = media.server_port
    for server in (media, providers):
        Thread(target=server.serve_forever, daemon=True).start()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "HOME":str(home), "PATH":os.environ["PATH"], "HOSTNAME":"127.0.0.1", "PORT":str(port),
        "DATA_DIR":str(data), "NINEROUTER_MIGRATION_DATA_DIR":str(data), "MODEL_CATALOG_SYNC":"off",
        "NEXT_TELEMETRY_DISABLED":"1", "NODE_NO_WARNINGS":"1", "NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallback.log"),
    }
    if implementation == "rust":
        env.update(wrapper)
        env["HTTP_PROXY"] = "http://127.0.0.1:2"
        env["NO_PROXY"] = "127.0.0.1,localhost,generativelanguage.googleapis.com"
    command = [str(native)] if implementation == "rust" else baseline
    with (scratch / "server.log").open("w") as log:
        process = subprocess.Popen(command, cwd=root, env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            deadline = time.monotonic() + 15
            while True:
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.15):
                        break
                except OSError:
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError("fixture server did not start")
                    time.sleep(.025)
            result = scenario(port, media.server_port)
            if implementation == "rust":
                assert (scratch / "fallback.log").read_text() == ""
            print(json.dumps(result, sort_keys=True, separators=(",", ":")))
        except Exception:
            print((scratch / "server.log").read_text(), file=sys.stderr)
            raise
        finally:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=2)
            for server in (media, providers):
                server.shutdown()
                server.server_close()
