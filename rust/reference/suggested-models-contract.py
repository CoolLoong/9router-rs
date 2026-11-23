import http.client
import json
import os
import shutil
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlencode, urlsplit

repo = Path(__file__).resolve().parents[2]
mode, *args = sys.argv[1:]
endpoint = "/api/providers/suggested-models"
prefix = "/catalog-suggestions/"
auth = {
    "x-9r-cli-token": "68977b6fd9422fbb",
    "authorization": "Bearer synthetic-catalog-auth",
    "cookie": "synthetic_session=not-a-provider-key",
    "x-api-key": "synthetic-catalog-key",
}
targets = {
    "openrouter": ("openrouter-free", "https://openrouter.ai/api/v1/models"),
    "kilo": ("openrouter-free", "https://api.kilo.ai/api/gateway/models"),
    "zen": ("opencode-free", "https://opencode.ai/zen/v1/models"),
    "go": ("opencode-go", "https://opencode.ai/inference/go/v1/models"),
    "mimo": ("mimo-free", "https://models.dev/api.json"),
    "minimax-cn": ("minimax-code", "https://agent.minimax.cn/mavis/api/v1/models?region=cn&buildEnv=prod"),
    "minimax-global": ("minimax-code", "https://agent.minimax.io/mavis/api/v1/models?region=en&buildEnv=prod"),
    "airforce": ("airforce-free", "https://api.airforce/v1/models"),
    "nous": ("nous-free", "https://inference-api.nousresearch.com/v1/models"),
}
free = {"prompt": "0", "completion": "0"}
fixtures = {
    "openrouter": {"data": [
        {"id": "fixture/capped", "name": "Capped", "pricing": free, "context_length": 200_000},
        {"id": "fixture/largest", "name": "Largest", "pricing": free, "context_length": "300000"},
        {"id": "fixture/short", "pricing": free, "context_length": 199_999},
        {"id": "fixture/paid", "pricing": {"prompt": 0, "completion": 0}, "context_length": 300_000},
    ], "models": [{"id": "ignored"}]},
    "kilo": {"models": [{"id": "fixture/kilo", "pricing": free, "context_length": 262_144}]},
    "zen": {"models": [{"id": identifier} for identifier in [
        "qwen-free", "big-pickle", "deepseek-v4-flash-free", "paid", None,
    ]]},
    "go": [None, {"id": "normal"}, {"id": 12}, {}, {"id": ""}],
    "mimo": [
        {"id": "mimo-v2", "name": ""},
        {"id": "pool/vendor", "name": "MiMo Vision"},
        {"id": "another", "name": "Excluded"},
    ],
    "minimax-cn": {"providers": [
        {"providerId": "ignored", "config": {"models": {"hidden": {}}}},
        {"providerId": "minimax", "config": {"models": {
            "slow": {"name": "Small", "limit": {"context": "9000"}},
            "long": {"name": "Large", "limit": {"context": "0x80000"}},
            "2": {"limit": {"context": ""}},
            "1": {"limit": {"context": 0}},
            " ": {},
            "null": None,
        }}},
    ]},
    "minimax-global": {"data": [{"providers": [{"providerId": "minimax", "config": {
        "models": {"global": {"limit": {"context": 200_000}}},
    }}]}]},
    "airforce": {"data": [
        {"id": "foo/b", "tier": "free", "supports_chat": True, "media_type": "chat"},
        {"id": "Foo-b", "tier": "free", "supports_chat": True, "media_type": "text"},
        {"id": "foo_b", "tier": "free", "supports_chat": True, "context_length": 32_000},
        {"id": "foo-b", "tier": "free", "supports_chat": True},
        {"id": "foo:b:free", "supports_chat": True, "media_type": ""},
        {"id": "paid", "supports_chat": True},
        {"id": "image:free", "supports_chat": True, "media_type": "image"},
        {"id": "unsupported:free", "supports_chat": False},
    ]},
    "nous": {"data": [
        {"id": "other/tie", "pricing": {"prompt": 0, "completion": ""}, "context_length": 250_000},
        {"id": " nousresearch/tie  ", "pricing": free, "top_provider": {
            "context_length": "250000", "max_completion_tokens": "8192"}, "description": "Description"},
        {"id": "fixture/longest", "name": "Largest", "pricing": free, "context_length": 300_000,
         "architecture": {"output_modalities": ["image", "text"]}},
        {"id": "image-only", "pricing": free, "context_length": 300_000,
         "architecture": {"output_modalities": ["image"]}},
        {"id": "priced", "pricing": {"prompt": None, "completion": 0}, "context_length": 300_000},
        {"id": "short", "pricing": free, "context_length": 20_000},
    ]},
}


def fixture_path(target):
    url = urlsplit(target)
    return prefix + url.netloc + url.path + (("?" + url.query) if url.query else "")


paths = {fixture_path(url): name for name, (_, url) in targets.items()}
events = []
overrides = {}


class CatalogHandler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        forbidden = ["authorization", "cookie", "x-9r-cli-token", "x-api-key", "proxy-authorization"]
        events.append((self.path, [name for name in forbidden if self.headers.get(name)]))
        name = paths.get(self.path)
        if name is None:
            return self.send_error(404)
        status, raw, additional = overrides.get(name, (
            200, json.dumps(fixtures[name]).encode(), {},
        ))
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(additional.get("length", len(raw))))
        if additional.get("location"):
            self.send_header("location", additional["location"])
        self.end_headers()
        try:
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            pass


@contextmanager
def gateway(folder, fixture=True):
    folder.mkdir()
    home = folder / "home"
    home.mkdir()
    empty_path = folder / "bin"
    empty_path.mkdir()
    data = folder / "data"
    subprocess.run([sys.executable, str(repo / "rust/reference/seed.py"), str(data)], cwd=repo,
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    log = folder / "fallback"
    env = {"HOME": str(home), "PATH": str(empty_path), "DATA_DIR": str(data),
           "PORT": str(port), "HOSTNAME": "127.0.0.1", "MODEL_CATALOG_SYNC": "off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1", "RUST_ANYTHING_FALLBACK_LOG": str(log),
           "NINEROUTER_MIGRATION_FIXTURE": "1" if fixture else "0",
           "HTTP_PROXY": "http://127.0.0.1:52989/unexpected-proxy",
           "HTTPS_PROXY": "http://127.0.0.1:52989/unexpected-proxy"}
    command = [str(Path(args[0]).resolve()), "--no-browser"] if mode == "native" else [
        shutil.which("node"), "--import", str(repo / "rust/reference/mock-suggested-models.mjs"),
        str(repo / ".next/standalone/custom-server.js"),
    ]
    process = subprocess.Popen(command, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 8
        while process.poll() is None and time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.025)
        else:
            raise RuntimeError("catalog fixture gateway did not start")
        yield port
        if mode == "native":
            assert log.is_file() and not log.read_text().strip()
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def fetch(port, path, headers=auth, method="GET", body=None):
    headers = dict(headers)
    if body is not None:
        body = json.dumps(body)
        headers["content-type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, response.headers, response.read()
    finally:
        connection.close()


def request(port, **query):
    headers = query.pop("headers", auth)
    status, _, payload = fetch(port, endpoint + "?" + urlencode(query), headers)
    return status, json.loads(payload)


def dashboard(port):
    page = "/dashboard/providers/connections/offline-catalog"
    status, headers, _ = fetch(port, page, headers={})
    assert status == 307 and headers["location"] == "/login"
    status, headers, _ = fetch(port, "/api/auth/login", headers={}, method="POST", body={"password": "123456"})
    assert status == 200
    cookie = {"cookie": headers["set-cookie"].split(";", 1)[0]}
    status, headers, html = fetch(port, page, headers=cookie)
    assert status == 200 and headers["cache-control"] == "no-store" and b"/_next/" not in html
    for marker in [
        b'id="connection-suggestions" ', b'id="connection-suggestions-load"',
        b'data-catalog-url="https://openrouter.ai/api/v1/models" data-catalog-type="openrouter-free"',
        b'data-catalog-url="https://agent.minimax.io/mavis/api/v1/models?region=en&amp;buildEnv=prod"',
    ]:
        assert marker in html
    assert b'data-catalog-url="https://tokenharbor.ai/v1/models"' not in html
    status, headers, script = fetch(port, "/__9router-ui/management.js", headers={})
    assert status == 200 and headers["content-type"].startswith("text/javascript")
    for marker in [
        b'/suggested-models?', b'request(`${modelApi}/custom`, "POST"', b'suggestedModels(connection.provider',
    ]:
        assert marker in script
    provider = {"providerAlias": "openrouter", "id": "fixture/largest", "type": "llm"}
    status, _, body = fetch(port, "/api/models/custom", headers=cookie, method="POST", body=provider)
    assert status == 200 and json.loads(body)["success"]
    status, _, body = fetch(port, "/api/models/custom", headers=cookie)
    assert status == 200 and any(
        item.get("providerAlias") == provider["providerAlias"] and item.get("id") == provider["id"]
        for item in json.loads(body)["models"]
    )


def scenario(port):
    recorded = {}
    for name, (kind, url) in targets.items():
        recorded[name] = request(port, url=url, type=kind)
    assert all(status == 200 for status, _ in recorded.values())
    assert {name: [item["id"] for item in payload["data"]] for name, (_, payload) in recorded.items()} == {
        "openrouter": ["fixture/largest", "fixture/capped"], "kilo": ["fixture/kilo"],
        "zen": ["qwen-free", "big-pickle"], "go": ["normal", ""],
        "mimo": ["mimo-v2", "pool/vendor"], "minimax-cn": ["long", "slow", "1", "2"],
        "minimax-global": ["global"],
        "airforce": ["foo_b", "foo-b", "Foo-b", "foo:b:free", "foo/b"],
        "nous": ["fixture/longest", "nousresearch/tie", "other/tie"],
    }, recorded
    assert recorded["openrouter"][1]["data"][0]["contextLength"] == "300000"
    assert recorded["minimax-cn"][1]["data"][0]["contextLength"] == 524288
    assert recorded["nous"][1]["data"][1]["maxOutputTokens"] == 8192
    assert events == [(fixture_path(url), []) for _, url in targets.values()], events

    recorded["missing"] = request(port, url=targets["kilo"][1])
    recorded["unknown"] = request(port, url="https://tokenharbor.ai/v1/models", type="openai")
    recorded["protected"] = request(port, url=targets["kilo"][1], type="openrouter-free", headers={})
    assert recorded["missing"] == (400, {"error": "Missing url or type"})
    assert recorded["unknown"] == (400, {"error": "Unknown filter type"})
    assert recorded["protected"][0] == 401 and len(events) == len(targets)

    for name, override in [
        ("kilo", (503, b"{}", {})), ("zen", (200, b"[", {})),
        ("mimo", (200, b'{"xiaomi":{"models":{}}}', {})),
        ("airforce", (200, b'[{"id":1,"supports_chat":true}]', {})),
    ]:
        overrides[name] = override
        kind, url = targets[name]
        recorded["empty-" + name] = request(port, url=url, type=kind)
        assert recorded["empty-" + name] == (200, {"data": []})
        del overrides[name]

    if mode == "native":
        in_flight = len(events)
        for address in [
            "http://127.0.0.1:52989/blocked", "http://[::1]/blocked",
            "http://169.254.169.254/blocked", "https://openrouter.ai.outside.invalid/api/v1/models",
            "https://openrouter.ai/api/v1/models?private=1", "https://openrouter.ai/api/v1/models#untrusted",
            "https://synthetic-key@openrouter.ai/api/v1/models",
            "https://api.airforce/v1/models",
        ]:
            assert request(port, url=address, type="openrouter-free") == (200, {"data": []})
        assert request(port, url=targets["kilo"][1], type="constructor")[0] == 400
        assert len(events) == in_flight

        for override in [
            (302, b"", {"location": "http://127.0.0.1:52989/blocked"}),
            (200, b"", {"length": 33 * 1024 * 1024}),
        ]:
            overrides["kilo"] = override
            assert request(port, url=targets["kilo"][1], type="openrouter-free") == (200, {"data": []})
        assert len(events) == in_flight + 2
        assert all(event == (fixture_path(targets["kilo"][1]), []) for event in events[-2:])
        dashboard(port)
    print(json.dumps(recorded, sort_keys=True))


def dns_contract(directory):
    direct_events = []

    class Connection(socketserver.BaseRequestHandler):
        def handle(self):
            direct_events.append(self.server.server_address)

    listeners = []
    try:
        for address in ("127.0.0.1", "93.184.216.34"):
            listener = socketserver.TCPServer((address, 443), Connection)
            threading.Thread(target=listener.serve_forever, daemon=True).start()
            listeners.append(listener)
        assert {result[4][0] for result in socket.getaddrinfo("openrouter.ai", 443)} == {"127.0.0.1"}
        assert {result[4][0] for result in socket.getaddrinfo("api.kilo.ai", 443)} == {"127.0.0.1", "93.184.216.34"}
        with gateway(directory / "production-resolver", fixture=False) as port:
            for name in ("openrouter", "kilo"):
                kind, url = targets[name]
                assert request(port, url=url, type=kind) == (200, {"data": []})
            assert not direct_events
    finally:
        for listener in listeners:
            listener.shutdown()
            listener.server_close()


if mode == "native" and "dns" in args and os.environ.get("SUGGESTED_DNS_NAMESPACE") != "1":
    with tempfile.TemporaryDirectory(prefix="9router-suggested-dns-") as directory:
        hosts = Path(directory) / "hosts"
        hosts.write_text("127.0.0.1 localhost openrouter.ai api.kilo.ai\n93.184.216.34 api.kilo.ai\n")
        nss = Path(directory) / "nsswitch.conf"
        nss.write_text("passwd: files\ngroup: files\nhosts: files\n")
        script = """ip link set lo up &&
ip addr add 93.184.216.34/32 dev lo &&
mount --make-rprivate / &&
mount --bind "$1" /etc/hosts &&
mount --bind "$2" /etc/nsswitch.conf &&
shift 2 &&
export SUGGESTED_DNS_NAMESPACE=1 &&
exec "$@" """
        raise SystemExit(subprocess.run([
            "unshare", "-Urnm", "--", "/bin/sh", "-c", script, "suggested-catalog-dns",
            str(hosts), str(nss), sys.executable, str(Path(__file__).resolve()), *sys.argv[1:],
        ]).returncode)


with tempfile.TemporaryDirectory(prefix="9router-suggested-models-") as directory:
    server = ThreadingHTTPServer(("127.0.0.1", 52989), CatalogHandler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        with gateway(Path(directory) / "router") as port:
            scenario(port)
        if mode == "native" and "dns" in args:
            dns_contract(Path(directory))
    finally:
        server.shutdown()
        server.server_close()
