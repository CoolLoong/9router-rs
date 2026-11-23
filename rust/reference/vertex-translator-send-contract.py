from base64 import urlsafe_b64decode
from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qs, quote_plus, urlsplit

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "vertex_translator_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST, OAUTH = "aiplatform.googleapis.com", "oauth2.googleapis.com"
ORIGIN = "https://" + HOST
MODEL, PARTNER = "gemini-2.5-flash", "deepseek-ai/deepseek-v3.2-maas"
RAW, SPECIAL = "offline-vertex-translator-key", "fixture-encoded-vertex-+/=key"
BEARER, SERVICE, ADC = "offline-vertex-translator-bearer", "offline-minted-vertex-service", "offline-minted-vertex-adc"
ADC_SECRET, ADC_REFRESH = "offline-adc-secret", "offline-adc-refresh"
EMAIL = "vertex-translator@example.invalid"
RELAY_HOST = "vertex-translator.offline-lane.workers.dev"
RELAY = "https://" + RELAY_HOST
RELAY_KEY = "S" * 43
ADMIN = {"x-9r-cli-token": "68977b6fd9422fbb"}
shared.ZED_HOST, shared.CLINE_HOST = HOST, OAUTH
shared.SECRETS = (RAW, SPECIAL, quote_plus(SPECIAL), BEARER, SERVICE, ADC, ADC_SECRET, ADC_REFRESH, RELAY_KEY)
CALLS, MINTS, ERRORS = [], [], []
BEHAVIOR, ON_REJECT, expected = "normal", None, None


def payload(label, partner=False, streamed=False):
    body = {"model": "keep-already-translated-model", "stream_options": {"include_usage": True},
            "temperature": .44, "custom": ["保持", None, 7]}
    if not streamed:
        body["stream"] = False
    if partner:
        body["messages"] = [{"role": "assistant", "content": label, "tool_calls": [{"id": "keep-call"}]}]
    else:
        body["contents"] = [{"role": "model", "parts": [
            {"text": label, "thoughtSignature": "keep-exact-signature"},
            {"functionCall": {"name": "lookup", "args": {"q": label}, "id": "keep-function-id"}},
        ]}]
    return body


def vertex_path(stream=False, project=None, region=None):
    action = "streamGenerateContent" if stream else "generateContent"
    prefix = f"projects/{project}/locations/{region}/" if project else ""
    return f"/v1/{prefix}publishers/google/models/{MODEL}:{action}"


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw, mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        if len(raw):
            middle = len(raw) // 2
            self.wfile.write(raw[:middle])
            self.wfile.flush()
            self.wfile.write(raw[middle:])

    def token(self, form, direct):
        assert direct and self.headers["Authorization"] is None
        assert self.headers["Content-Type"] == "application/x-www-form-urlencoded"
        fields = parse_qs(form.decode())
        grant = fields["grant_type"][0]
        if grant == "refresh_token":
            assert fields == {"grant_type": ["refresh_token"], "client_id": ["offline-adc-client"],
                              "client_secret": [ADC_SECRET], "refresh_token": [ADC_REFRESH]}
            MINTS.append("adc")
            return self.reply(200, json.dumps({"access_token": ADC, "expires_in": 3600}).encode())
        assert grant == "urn:ietf:params:oauth:grant-type:jwt-bearer" and len(fields) == 2
        header, claim, signature = fields["assertion"][0].split(".")
        decode = lambda value: urlsafe_b64decode(value + "=" * (-len(value) % 4))
        assert json.loads(decode(header)) == {"alg": "RS256"}
        claims = json.loads(decode(claim))
        assert claims["iss"] == EMAIL and claims["aud"] == f"https://{OAUTH}/token"
        assert claims["scope"] == "https://www.googleapis.com/auth/cloud-platform"
        assert claims["exp"] - claims["iat"] == 3600
        public, signature_file = self.server.public, self.server.public.with_suffix(".sig")
        signature_file.write_bytes(decode(signature))
        subprocess.run(["openssl", "dgst", "-sha256", "-verify", str(public), "-signature", str(signature_file)],
                       input=(header + "." + claim).encode(), check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        MINTS.append("sa")
        self.reply(200, json.dumps({"access_token": SERVICE, "expires_in": 3600}).encode())

    def do_POST(self):
        path = urlsplit(self.path)
        try:
            direct = self.server.server_port == 52989
            lane = "direct" if direct else "tunnel"
            data = self.rfile.read(int(self.headers["Content-Length"]))
            if path.path == "/vertex-oauth/token":
                return self.token(data, direct)
            if path.path == "/cloudflare-relay/" + RELAY_HOST:
                lane = "relay"
                assert self.headers["x-relay-auth"] == RELAY_KEY and self.headers["x-relay-target"] == ORIGIN
                path = urlsplit(self.headers["x-relay-path"])
            else:
                assert not any(self.headers[field] for field in ("x-relay-auth", "x-relay-target", "x-relay-path"))
                if direct:
                    path = urlsplit(self.path.removeprefix("/vertex"))
                else:
                    assert self.headers["Host"] == HOST
            assert not any(self.headers[field] for field in ("x-ignored-incoming", "Cookie", "Proxy-Authorization", "x-api-key"))
            if path.path == "/v1/publishers/google/models/__probe__:generateContent":
                assert direct and data == b"{}" and parse_qs(path.query) == {"key": [RAW]}
                assert self.headers["Authorization"] is None
                CALLS.append(("direct", "probe"))
                return self.reply(404, b'{"error":{"message":"projects/probed-project/locations/global/models/__probe__ missing"}}')
            target, body, credential, auth = expected
            assert path.path == target and json.loads(data) == body
            streamed = body.get("stream") is not False
            query = {"alt": ["sse"]} if streamed and ":streamGenerateContent" in target else {}
            if auth == "query":
                query["key"] = [credential]
            assert parse_qs(path.query) == query
            assert self.headers.get_all("x-goog-api-key") == ([credential] if auth == "header" else None)
            assert self.headers.get_all("Authorization") == (["Bearer " + credential] if auth == "bearer" else None)
            assert (self.headers["Accept"] == "text/event-stream") == streamed
            assert self.headers["Content-Type"] == "application/json"
            CALLS.append((lane, target))
            global BEHAVIOR
            if BEHAVIOR == "reject-once":
                BEHAVIOR = "normal"
                if ON_REJECT:
                    ON_REJECT()
                return self.reply(401, b'{"error":"retry once"}')
            if BEHAVIOR == "redirect":
                return self.reply(307, b"{}", location="https://untrusted.invalid/capture")
            if BEHAVIOR == "reject":
                detail = f"upstream echoed {credential} {quote_plus(credential)}"
                if lane == "relay":
                    detail += " " + RELAY_KEY
                return self.reply(401, json.dumps({"error": detail}).encode())
            message = "raw upstream 世界"
            if BEHAVIOR == "echo-stream":
                message += " " + credential + (f" {RELAY_KEY}" if lane == "relay" else "")
            reply = json.dumps({"response": {"raw": message}}, ensure_ascii=False).encode()
            if streamed:
                return self.reply(200, b"data: " + reply + b"\r\n\r\ndata: [DONE]\n\n", "text/event-stream")
            self.reply(200, reply)
        except (AssertionError, KeyError, TypeError, ValueError, subprocess.CalledProcessError) as error:
            ERRORS.append((path.path, repr(error)))
            self.reply(422, b'{"error":"unexpected Vertex translator wire"}')


def save(db, provider, credentials, details=None):
    fields = {**credentials, "providerSpecificData": {"baseUrl": "https://untrusted.invalid", **(details or {})}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Offline Vertex',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider, provider, json.dumps(fields)))


def stored(db, provider="vertex"):
    with sqlite3.connect(db) as sql:
        item, = sql.execute("SELECT data FROM providerConnections WHERE id=?", (provider,)).fetchone()
    return json.loads(item)


def send(port, body, provider="vertex", status=200, admin=True, clean=True):
    request = {"provider": provider, "model": MODEL if provider == "vertex" else PARTNER, "body": body,
               "url": "https://untrusted.invalid", "headers": {"Authorization": "ignored"}}
    headers = {"Content-Type": "application/json", "x-ignored-incoming": "never forward", "Cookie": "ignore"}
    if admin:
        headers.update(ADMIN)
    with closing(HTTPConnection("127.0.0.1", port, timeout=8)) as conn:
        conn.request("POST", "/api/translator/send", json.dumps(request).encode(), headers)
        response = conn.getresponse()
        raw = response.read()
        if clean:
            assert not any(secret.encode() in raw for secret in shared.SECRETS)
        assert response.status == status, (response.status, raw, ERRORS)
        if status == 200:
            assert response.headers["content-type"] == "text/event-stream"
            assert response.headers["cache-control"] == "no-cache"
        return raw.decode()


@contextmanager
def original(scratch):
    home = scratch / "reference"
    home.mkdir()
    data = home / "data"
    env = {"DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "HOME": str(home),
           "PATH": os.environ["PATH"], "ENABLE_TRANSLATOR": "true", "MODEL_CATALOG_SYNC": "off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], check=True, env=env,
                   stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        env["PORT"] = str(listener.getsockname()[1])
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
    rewrite = home / "vertex-map.mjs"
    rewrite.write_text("""
import { createRequire } from "node:module";
const remap = request => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  let local;
  if (url.origin === "https://aiplatform.googleapis.com") local = "/vertex" + url.pathname + url.search;
  else if (url.href === "https://oauth2.googleapis.com/token") local = "/vertex-oauth/token";
  else return request(source, options);
  const target = "http://127.0.0.1:52989" + local;
  return request(source instanceof Request ? new Request(target, source) : target, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(REFERENCE_SERVER)("undici");
undici.fetch = remap(undici.fetch);
""".replace("REFERENCE_SERVER", json.dumps(str(ROOT / ".next/standalone/server.js"))))
    with subprocess.Popen(["node", "--import", str(rewrite), ".next/standalone/custom-server.js"], cwd=ROOT, env=env,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            port = int(env["PORT"])
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if shared.get(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise RuntimeError("Pinned Vertex Translator did not start")
            yield port, db
        finally:
            process.terminate()
            process.wait(timeout=7)


def direct_cases(port, db, service, adc):
    global expected, BEHAVIOR
    results = {}
    save(db, "vertex", {"apiKey": RAW})
    send(port, payload("auth guard"), status=401, admin=False)
    assert not CALLS and not MINTS
    for name, body, path, credential, auth in (
        ("gemini-json", payload("translated json"), vertex_path(), RAW, "query"),
        ("gemini-sse", payload("translated sse", streamed=True), vertex_path(True), RAW, "query"),
        ("saved-bearer", payload("stored bearer"), vertex_path(project="approved", region="eu"), BEARER, "bearer"),
        ("sa-retry", payload("direct service"), vertex_path(project="service-account", region="us-central1"), SERVICE, "bearer"),
        ("adc", payload("direct adc"), vertex_path(project="adc-project", region="us-central1"), ADC, "bearer"),
    ):
        if name == "saved-bearer":
            save(db, "vertex", {"accessToken": BEARER}, {"projectId": "approved", "location": "eu"})
        elif name == "sa-retry":
            save(db, "vertex", {"apiKey": service})
            BEHAVIOR = "reject-once"
        elif name == "adc":
            save(db, "vertex", {"apiKey": adc})
        expected = (path, body, credential, auth)
        count = len(CALLS)
        results[name] = send(port, body)
        assert len(CALLS) == count + (2 if name == "sa-retry" else 1)
        if name == "sa-retry":
            value = stored(db)
            assert value["accessToken"] == SERVICE and value["expiresAt"] > time.time() * 1000 and MINTS == ["sa"]
    assert MINTS == ["sa", "adc"]
    save(db, "vertex-partner", {"apiKey": RAW})
    body = payload("partner unchanged", partner=True)
    expected = ("/v1/projects/probed-project/locations/global/endpoints/openapi/chat/completions", body, RAW, "query")
    before = len(CALLS)
    results["partner-probe"] = send(port, body, "vertex-partner")
    assert CALLS[before][1] == "probe" and len(CALLS) == before + 2
    results["partner-cached"] = send(port, body, "vertex-partner")
    assert len(CALLS) == before + 3
    results["outbound"] = CALLS
    results["tokenMints"] = MINTS
    return results


def protected_cases(port, db, tunnel, socks_address, connect, wrong_tls, adc, service):
    global expected, BEHAVIOR, ON_REJECT
    body = payload("already-translated", streamed=True)
    peer = "/v1/projects/approved/locations/global/endpoints/openapi/chat/completions"
    expected = (vertex_path(True), body, RAW, "query")
    save(db, "vertex", {"apiKey": RAW})
    direct = send(port, body)
    shared.enable_global_proxy(db, socks_address)
    assert send(port, body) == direct and CALLS[-1][0] == "tunnel"
    assert shared.TUNNELS == [("socks", HOST, 443)] and shared.HELLOS == [HOST]
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
    save(db, "vertex", {"apiKey": RAW}, tunnel)
    assert send(port, body) == direct and shared.TUNNELS[-1] == ("connect", HOST, 443)
    save(db, "vertex", {"apiKey": RAW}, tunnel | {"connectionNoProxy": ".googleapis.com"})
    count = len(shared.TUNNELS)
    assert send(port, body) == direct and CALLS[-1][0] == "direct" and len(shared.TUNNELS) == count
    save(db, "vertex", {"apiKey": RAW}, {"proxyPoolId": "managed"})
    expected = (vertex_path(True), body, RAW, "header")
    assert send(port, body) == direct and CALLS[-1][0] == "relay"
    BEHAVIOR = "echo-stream"
    echo = send(port, body)
    assert "[redacted] [redacted]" in echo and echo.endswith("\r\n\r\ndata: [DONE]\n\n")
    BEHAVIOR = "normal"

    partner = payload("preserve partner object", partner=True)
    saved = {"accessToken": BEARER, "expiresAt": "2099-01-01T00:00:00Z"}
    details = {"proxyPoolId": "fixture-socks", "projectId": "approved"}
    save(db, "vertex-partner", saved, details)
    expected = (peer, partner, BEARER, "bearer")
    assert "raw upstream 世界" in send(port, partner, "vertex-partner") and CALLS[-1][0] == "tunnel"
    assert shared.TUNNELS[-1] == ("socks", HOST, 443) and stored(db, "vertex-partner")["accessToken"] == BEARER
    before = (len(CALLS), len(shared.TUNNELS), len(MINTS))
    for provider, wire, credentials, route in (
        ("vertex", body, {"apiKey": service}, tunnel),
        ("vertex", body, {"apiKey": adc}, tunnel),
        ("vertex", body, {"accessToken": BEARER, "expiresAt": 0}, tunnel | {"projectId": "approved"}),
        ("vertex", body, {"apiKey": RAW}, {"proxyPoolId": "missing"}),
        ("vertex", body, {"apiKey": RAW}, {"vercelRelayUrl": RELAY}),
        ("vertex", body, {"apiKey": RAW}, tunnel | {"strictProxy": True, "connectionNoProxy": "*"}),
        ("vertex-partner", partner, {"apiKey": RAW}, tunnel),
        ("vertex-partner", partner, saved, {"proxyPoolId": "managed", "projectId": "approved"}),
    ):
        save(db, provider, credentials, route)
        send(port, wire, provider, status=502)
        assert (len(CALLS), len(shared.TUNNELS), len(MINTS)) == before

    save(db, "vertex", {"apiKey": SPECIAL}, tunnel)
    expected = (vertex_path(True), body, SPECIAL, "query")
    for behavior, status, marker in (("reject", 401, "[redacted] [redacted]"), ("redirect", 502, "redirect refused")):
        BEHAVIOR = behavior
        count = (len(CALLS), len(shared.TUNNELS))
        assert marker in send(port, body, status=status)
        assert (len(CALLS), len(shared.TUNNELS)) == (count[0] + 1, count[1] + 1)
    BEHAVIOR = "normal"
    count = len(CALLS)
    connect.target = wrong_tls
    assert "proxy request failed" in send(port, body, status=502)
    connect.reject = True
    assert "proxy request failed" in send(port, body, status=502)
    assert len(CALLS) == count and not MINTS

    save(db, "vertex", {"apiKey": service})
    sa_body = payload("change route while request in flight")
    expected = (vertex_path(project="service-account", region="us-central1"), sa_body, SERVICE, "bearer")
    ON_REJECT = lambda: save(db, "vertex", {"apiKey": service}, tunnel)
    BEHAVIOR = "reject-once"
    count = len(CALLS)
    assert "retry once" in send(port, sa_body, status=401)
    assert len(CALLS) == count + 1 and MINTS == ["sa"] and "accessToken" not in stored(db)
    assert all(host == HOST for host in shared.HELLOS)


with tempfile.TemporaryDirectory(prefix="9router-vertex-translator-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    private, public = scratch / "fixture-vertex.pem", scratch / "fixture-vertex.pub"
    shared.run_openssl("genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", private)
    shared.run_openssl("pkey", "-in", private, "-pubout", "-out", public)
    service = json.dumps({"type": "service_account", "project_id": "service-account", "client_email": EMAIL,
                          "private_key": private.read_text().replace("\n", "\\n")})
    adc = json.dumps({"type": "authorized_user", "quota_project_id": "adc-project", "client_id": "offline-adc-client",
                      "client_secret": ADC_SECRET, "refresh_token": ADC_REFRESH})
    direct = stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    direct.public = public
    mode = sys.argv[2]
    if mode in ("original", "native"):
        server = original(scratch) if mode == "original" else shared.gateway(scratch, "native-direct", None, [])
        with server as (port, db):
            result = direct_cases(port, db, service, adc)
        print(json.dumps(result, sort_keys=True, ensure_ascii=False))
    else:
        assert mode == "protected"
        ca, tls, wrong_tls = shared.tls_services(stack, scratch, Vendor)
        connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
        for proxy, kind in ((connect, "connect"), (socks, "socks")):
            proxy.mode, proxy.reject, proxy.target = kind, False, tls
            stack.enter_context(shared.serving(proxy))
        tunnel = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
        socks_address = f"socks5h://127.0.0.1:{socks.server_address[1]}"
        pools = [("fixture-socks", "socks5", socks_address), ("managed", "cloudflare", RELAY)]
        with shared.gateway(scratch, "native-protected", ca, pools) as (port, db):
            with sqlite3.connect(db) as sql:
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed'",
                            (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
            protected_cases(port, db, tunnel, socks_address, connect, wrong_tls, adc, service)
        print("Vertex Translator preserves raw JSON/SSE, selects one protected route, and scrubs upstream credential echoes.")
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)
