import base64
import http.client
import json
import os
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())
auth = {"x-9r-cli-token": "68977b6fd9422fbb"}
target_port = 52983
ready = threading.Event()
unblock = threading.Event()


class Destination(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def write_message(self, status, data=b"", content_type="application/json", **headers):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers.items():
            self.send_header(name.replace("_", "-"), value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def do_HEAD(self):
        assert self.path == "/proxy-pool-test" and self.headers["User-Agent"] == "9Router"
        self.write_message(200)

    def do_GET(self):
        if self.path in ["/relay-ok", "/relay-fail", "/relay-delayed"]:
            assert self.headers["x-relay-target"] == "https://httpbin.org"
            assert self.headers["x-relay-path"] == "/get"
            if self.path.endswith("delayed"):
                ready.set()
                assert unblock.wait(5)
            self.write_message(503 if self.path.endswith("fail") else 200)
        elif self.path == "/health":
            self.write_message(200, b'{"healthy":true}')
        elif self.path == "/dashboard":
            text = b"<script>fetch('/stats');fetch('/health');fetch('/stats-history');fetch('/transformations/feed');fetch('/other')</script>"
            self.write_message(200, text, "text/html")
        elif self.path == "/redirect":
            self.write_message(302, Location="/health")
        else:
            self.write_message(404)

    def do_POST(self):
        data = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path == "/cowork-mcp":
            assert json.loads(data)["method"] == "initialize"
            assert "Authorization" not in self.headers and "Cookie" not in self.headers
            self.server.cowork_probes += 1
            self.write_message(401)
            return
        assert self.path == "/stats?limit=2" and json.loads(data) == {"sample": True}
        watched = ["Authorization", "Cookie", "X-API-Key", "X-Goog-API-Key"]
        assert all(name not in self.headers for name in ["X-9r-Cli-Token", "X-Custom-Hop"])
        if self.headers["Host"].startswith("127.0.0.2"):
            assert all(name not in self.headers for name in watched)
        else:
            assert self.headers["Authorization"] == "Bearer synthetic-headroom"
            assert self.headers["Cookie"] == "fixture=manual"
        self.write_message(202, b'{"accepted":true}', Set_Cookie="upstream=restricted; Path=/")


class HttpProxy(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_HEAD(self):
        destination = urlsplit(self.path)
        expected = base64.b64encode(b"fixture-user:fixture-password").decode()
        assert destination.hostname == "127.0.0.1" and destination.port == target_port
        assert self.headers["Proxy-Authorization"] == f"Basic {expected}"
        upstream = http.client.HTTPConnection("127.0.0.1", target_port)
        try:
            upstream.request("HEAD", destination.path, headers={"User-Agent": self.headers["User-Agent"]})
            result = upstream.getresponse()
            self.send_response(result.status)
            self.end_headers()
        finally:
            upstream.close()


class SocksProxy(socketserver.StreamRequestHandler):
    def handle(self):
        head = self.rfile.read(2)
        assert head[0] == 5 and 0 in self.rfile.read(head[1])
        self.wfile.write(b"\x05\x00")
        assert self.rfile.read(4) == b"\x05\x01\x00\x01"
        assert self.rfile.read(4) == b"\x7f\x00\x00\x01"
        assert int.from_bytes(self.rfile.read(2), "big") == target_port
        self.wfile.write(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
        line = self.rfile.readline()
        assert line == b"HEAD /proxy-pool-test HTTP/1.1\r\n"
        headers = {}
        while (line := self.rfile.readline()) != b"\r\n":
            name, value = line.decode().split(":", 1)
            headers[name.lower()] = value.strip()
        upstream = http.client.HTTPConnection("127.0.0.1", target_port)
        try:
            upstream.request("HEAD", "/proxy-pool-test", headers={"User-Agent": headers["user-agent"]})
            result = upstream.getresponse()
            self.wfile.write(f"HTTP/1.1 {result.status} {result.reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".encode())
        finally:
            upstream.close()


def request(port, path, method="GET", payload=None, headers=auth):
    options = headers.copy()
    body = None
    if payload is not None:
        body = json.dumps(payload).encode()
        options["content-type"] = "application/json"
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        client.request(method, path, body=body, headers=options)
        response = client.getresponse()
        raw = response.read().decode()
        if raw and (response.headers.get("content-type") or "").startswith("application/json"):
            raw = json.loads(raw)
        return response.status, raw, response.headers
    finally:
        client.close()


def start(server):
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


with tempfile.TemporaryDirectory(prefix="9router-network-admin-contract-") as directory:
    home = Path(directory) / "home"
    bins = home / ".local" / "bin"
    bins.mkdir(parents=True)
    headroom = bins / "headroom"
    headroom.write_text("#!/bin/sh\nexit 99\n")
    headroom.chmod(0o700)
    python = bins / "python3"
    python.write_text("""#!/bin/sh
if [ "$1" = "--version" ]; then
    printf 'Python 3.13.1\n'
else
    printf '[{"name":"headroom-ai","version":"1.2.3"},{"name":"tree-sitter","version":"1"},{"name":"torch","version":"1"}]\n'
fi
""")
    python.chmod(0o700)
    target = start(ThreadingHTTPServer(("0.0.0.0", target_port), Destination))
    direct_proxy = start(ThreadingHTTPServer(("127.0.0.1", 0), HttpProxy))
    socks = start(socketserver.ThreadingTCPServer(("127.0.0.1", 0), SocksProxy))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    data = Path(directory) / "data"
    pid_file = data / "headroom" / "proxy.pid"
    pid_file.parent.mkdir(parents=True)
    pid_file.write_text(str(os.getpid()))
    fallbacks = Path(directory) / "fallbacks.txt"
    env = {
        **os.environ, "PORT": str(port), "DATA_DIR": str(data), "TMPDIR": directory,
        "HOME": str(home), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallbacks),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "HEADROOM_URL"]:
        env.pop(name, None)
    process = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("network administration fixture did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        def call(path, method="GET", payload=None, headers=auth):
            return request(port, path, method, payload, headers)

        def pool(name, endpoint, kind):
            status, result, _ = call("/api/proxy-pools", "POST", {
                "name":name, "proxyUrl": endpoint, "type":kind, "strictProxy": True, "isActive": False,
            })
            assert status == 201
            return f"/api/proxy-pools/{result['proxyPool']['id']}"

        def probe(path, expected, error=None):
            code, result, _ = call(path + "/test", "POST")
            assert code == 200 and result["status"] == expected and result["ok"] == (expected == 200)
            assert result["error"] == error and result["testedAt"]
            stored = call(path)[1]["proxyPool"]
            assert stored["isActive"] == result["ok"] and stored["strictProxy"] is True
            assert stored["testStatus"] == ("active" if result["ok"] else "error")
            assert stored["lastTestedAt"] == result["testedAt"]
            return result, stored

        port_http = direct_proxy.server_port
        http_pool = pool("HTTP fixture", f"http://fixture-user:fixture-password@127.0.0.1:{port_http}", "http")
        socks_pool = pool("SOCKS fixture", f"socks5h://127.0.0.1:{socks.server_address[1]}", "http")
        assert call(http_pool + "/test", "POST", headers={})[0] == 401
        time.sleep(.02)
        result, state = probe(http_pool, 200)
        assert result["statusText"] == "OK" and state["lastError"] is None
        test_proxy = "/api/settings/proxy-test"
        proxy_url = f"http://fixture-user:fixture-password@127.0.0.1:{port_http}"
        assert call(test_proxy, "POST", {"proxyUrl":proxy_url}, headers={})[0] == 401
        status, result, _ = call(test_proxy, "POST", {"proxyUrl":proxy_url, "timeoutMs":"2500"})
        assert status == 200 and result["ok"] and result["status"] == 200
        assert result["statusText"] == "OK" and result["url"] == "https://google.com/"
        assert isinstance(result["elapsedMs"], int)
        status, failed, _ = call(test_proxy, "POST", {"proxyUrl":"http://fixture-user:fixture-password@127.0.0.1:9"})
        assert status == 500 and failed == {"ok":False,"error":"Proxy connection failed"}
        status, missing, _ = call(test_proxy, "POST", {})
        assert status == 400 and missing == {"ok":False,"error":"proxyUrl is required"}
        status, rows, _ = call("/api/proxy-pools?includeUsage=true")
        assert status == 200 and rows["proxyPools"][0]["id"] == state["id"]
        probe(socks_pool, 200)

        loopback = f"http://127.0.0.1:{target_port}"
        target.cowork_probes = 0
        mcp_route = "/api/cli-tools/cowork-mcp-tools"
        mcp_target = {"url":loopback + "/cowork-mcp"}
        assert call(mcp_route, "POST", mcp_target, headers={})[0] == 401
        assert call(mcp_route, "POST", mcp_target, headers={**auth,"x-forwarded-for":"192.0.2.25"})[0] == 400
        assert call(mcp_route, "POST", {"url":loopback.replace("://", "://fixture:private@") + "/cowork-mcp"})[0] == 400
        assert target.cowork_probes == 0
        status, discovery, _ = call(mcp_route, "POST", mcp_target)
        assert status == 200 and discovery == {"requiresAuth":True,"tools":[]} and target.cowork_probes == 1
        probe(pool("Relay fixture", loopback + "/relay-ok", "vercel"), 200)
        failed_pool = pool("Rejected relay", loopback + "/relay-fail", "cloudflare")
        assert call(failed_pool, "PUT", {"isActive": True})[1]["proxyPool"]["isActive"] is True
        rejected, stored = probe(failed_pool, 503)
        assert rejected["statusText"] == "Service Unavailable"
        assert stored["lastError"] == "Proxy test failed with status 503"
        invalid = pool("Broken fixture", "http://fixture-user:fixture-password@127.0.0.1:9", "http")
        error, stored = probe(invalid, 500, "Proxy connection failed")
        assert "fixture-password" not in json.dumps(error) and "fixture-password" not in stored["lastError"]

        changed = pool("Concurrent edit", loopback + "/relay-delayed", "deno")
        results = []
        thread = threading.Thread(target=lambda: results.append(call(changed + "/test", "POST")))
        thread.start()
        assert ready.wait(5)
        assert call(changed, "PUT", {"proxyUrl": loopback + "/relay-fail"})[0] == 200
        unblock.set()
        thread.join(8)
        assert results[0][1]["ok"] is True
        assert call(changed)[1]["proxyPool"]["isActive"] is False

        assert call("/api/settings", "PATCH", {"headroomUrl": loopback})[0] == 200
        assert call("/api/headroom/status", headers={})[0] == 401
        assert call("/api/headroom/proxy/health", headers={})[0] == 403
        assert call("/api/headroom/proxy/health", headers={"x-forwarded-for": "192.0.2.25"})[0] == 403
        status, evidence, _ = call("/api/headroom/status")
        assert status == 200 and evidence == {
            "installed": True, "path": str(headroom), "running": True, "python": str(python),
            "localUrl": True, "canStart": True, "version":"1.2.3", "extras": {"code":True, "ml":True},
            "url": loopback, "managedPid": None,
        }
        status, html, _ = call("/api/headroom/proxy/dashboard")
        assert status == 200
        assert all(f"fetch('/api/headroom/proxy/{name}')" in html for name in [
            "stats", "health", "stats-history", "transformations/feed",
        ]) and "fetch('/other')" in html
        status, _, headers = call("/api/headroom/proxy/redirect")
        assert status == 302 and headers["location"] == "/health"
        credentials = {
            **auth, "Authorization": "Bearer synthetic-headroom", "Cookie": "fixture=manual",
            "X-API-Key": "synthetic", "X-Goog-API-Key": "synthetic",
            "X-Custom-Hop": "private", "Connection": "X-Custom-Hop",
        }
        status, accepted, headers = call("/api/headroom/proxy/stats?limit=2", "POST", {"sample": True}, credentials)
        assert status == 202 and accepted["accepted"] and headers.get("set-cookie")
        assert call("/api/settings", "PATCH", {"headroomUrl": f"http://127.0.0.2:{target_port}"})[0] == 200
        status, accepted, headers = call("/api/headroom/proxy/stats?limit=2", "POST", {"sample": True}, credentials)
        assert status == 202 and accepted["accepted"] and not headers.get("set-cookie")
        assert not fallbacks.exists() or not fallbacks.read_text().strip()
        print("HTTP/SOCKS/relay probes, stored state, Headroom proxy/status and Cowork discovery guards passed")
    finally:
        unblock.set()
        for server in [target, direct_proxy, socks]:
            server.shutdown()
            server.server_close()
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
