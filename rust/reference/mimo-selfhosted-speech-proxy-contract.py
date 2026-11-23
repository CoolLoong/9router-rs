from base64 import b64decode, b64encode
from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import urlsplit

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "proxy_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py"),
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

MIMO = "api.xiaomimimo.com"
SELF = "speech-fixture.example.com"
MIMO_PATH = "/v1/chat/completions"
SELF_PATH = "/tenant/v1/audio/speech"
SELF_LOCAL = "http://127.0.0.1:52989" + SELF_PATH + "/"
SELF_REMOTE = "https://" + SELF + SELF_PATH + "/"
MIMO_KEY = "sk-offline-mimo-proxy"
LEGACY_KEY = "sk-offline-mimo-legacy"
SELF_KEY = "offline-selfhosted-proxy"
RELAY = "https://tts-finishing.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/tts-finishing.offline-lane.workers.dev"
RELAY_TOKEN = "M" * 43
MIMO_MODEL = "mimo/mimo-v2.5-tts/Mia"
SELF_MODEL = "selfhosted-tts/kokoro/en/FixtureVoice"
MIMO_AUDIO = b"RIFF fixture audio"
SELF_AUDIO = b"PCM fixture payload"
shared.ZED_HOST, shared.CLINE_HOST = MIMO, SELF
shared.SECRETS = (MIMO_KEY, LEGACY_KEY, SELF_KEY, RELAY_TOKEN)
CALLS, ERRORS = [], []
MODE = None


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
        self.wfile.write(raw)

    def do_GET(self):
        ERRORS.append(self.path)
        self.reply(422, b"{}")

    def do_POST(self):
        try:
            assert self.headers["Content-Type"] == "application/json"
            assert not any(self.headers[name] for name in ("Cookie", "Proxy-Authorization", "X-Fixture-Danger"))
            url = urlsplit(self.path)
            direct = self.server.server_port == 52989
            if url.path == RELAY_PATH:
                lane, host, path = "relay", MIMO, MIMO_PATH
                assert self.headers.get_all("x-relay-target") == ["https://" + MIMO]
                assert self.headers.get_all("x-relay-path") == [MIMO_PATH]
                assert self.headers.get_all("x-relay-auth") == [RELAY_TOKEN]
            elif direct:
                lane = "direct"
                host, path = (MIMO, url.path.removeprefix("/xiaomi-mimo")) if url.path.startswith("/xiaomi-mimo/") else (SELF, url.path)
            else:
                lane, host, path = "tunnel", self.headers["Host"], url.path
            if lane != "relay":
                assert not any(self.headers[name] for name in ("x-relay-auth", "x-relay-path", "x-relay-target"))
            assert not url.query and host in (MIMO, SELF)
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            authorization = self.headers.get_all("Authorization")
            if host == MIMO:
                assert path == MIMO_PATH and authorization in (["Bearer " + MIMO_KEY], ["Bearer " + LEGACY_KEY])
                assert payload == {"model": "mimo-v2.5-tts", "stream": False,
                                   "messages": [{"role": "user", "content": "Speak in Mandarin. Softly."},
                                                {"role": "assistant", "content": "MiMo wording"}],
                                   "audio": {"format": "wav", "voice": "Mia"}}
            else:
                assert path == SELF_PATH and authorization in (None, ["Bearer " + SELF_KEY])
                assert payload == {"model": "kokoro", "voice": "en/FixtureVoice",
                                   "input": "Self-hosted wording", "response_format": payload["response_format"]}
                assert payload["response_format"] in ("json", "pcm")
            CALLS.append((host, lane, authorization))
            if MODE == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            if MODE == "error":
                reflected = authorization[0] if authorization else ""
                message = "Offline upstream refused " + reflected
                if lane == "relay":
                    message += " " + RELAY_TOKEN
                return self.reply(503, json.dumps({"error": {"message": message}}).encode())
            if host == SELF:
                return self.reply(200, SELF_AUDIO, "audio/pcm")
            encoded = b64encode(MIMO_AUDIO).decode().rstrip("=")
            raw = json.dumps({"choices": [{"message": {"audio": {"format": "wav", "data": encoded}}}]}).encode()
            self.reply(200, raw)
        except (AssertionError, TypeError, ValueError, KeyError) as error:
            ERRORS.append((urlsplit(self.path).path, repr(error)))
            self.reply(422, b"{}")


def account(db, host, details=None, key=None, legacy=False, backup=False):
    provider = "xiaomi-mimo" if host == MIMO else "selfhosted-tts"
    value = {"providerSpecificData": details or {}}
    if key is not None:
        value["accessToken" if legacy else "apiKey"] = key
    if legacy:
        value["providerSpecificData"]["baseUrl"] = "http://127.0.0.1:52989/capture"
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,?,'Offline speech',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if backup else "-primary"), provider, "api_key" if legacy else "apikey",
                     2 if backup else 1, json.dumps(value)))


def request(port, host, response_format, model=None):
    payload = {"model": model or (MIMO_MODEL if host == MIMO else SELF_MODEL),
               "input": " MiMo wording " if host == MIMO else " Self-hosted wording ",
               "language": "Mandarin", "style": "Softly.", "response_format": "ignored",
               "baseUrl": "http://127.0.0.1:52989/capture"}
    headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json",
               "X-Fixture-Danger": "ignore this", "x-relay-auth": "ignore this"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as connection:
        connection.request("POST", "/v1/audio/speech?response_format=" + response_format, json.dumps(payload), headers)
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type"), response.getheader("Content-Length"), response.read()


def audio(port, host, lane, key=None):
    raw = MIMO_AUDIO if host == MIMO else SELF_AUDIO
    format = "wav" if host == MIMO else "pcm"
    auth = ["Bearer " + key] if key else None
    assert request(port, host, format) == (200, "audio/" + format, str(len(raw)), raw)
    status, mime, _, body = request(port, host, "json")
    data = json.loads(body)
    assert (status, mime, data["format"]) == (200, "application/json", "wav" if host == MIMO else "json")
    if host == MIMO:
        assert data["audio"] == b64encode(raw).decode().rstrip("=")
    else:
        assert b64decode(data["audio"]) == raw
    assert CALLS[-2:] == [(host, lane, auth)] * 2


def verify(port, db, http_proxy, socks_url, http_server):
    global MODE

    def rejected(host, reason, model=None):
        status, _, _, raw = request(port, host, "json", model)
        assert status == 503 and reason.encode() in raw, (host, status, raw)

    account(db, MIMO, key=LEGACY_KEY, legacy=True)
    account(db, MIMO, key="sk-backup-never-used", backup=True)
    account(db, SELF, {"baseUrl": SELF_LOCAL})
    account(db, SELF, {"baseUrl": SELF_LOCAL}, "unused-backup", backup=True)
    audio(port, MIMO, "direct", LEGACY_KEY)
    audio(port, SELF, "direct")
    for details, lane, key, legacy in (
        (http_proxy, "tunnel", LEGACY_KEY, True),
        ({"proxyPoolId": "speech-socks"}, "tunnel", MIMO_KEY, False),
        ({"proxyPoolId": "speech-managed"}, "relay", MIMO_KEY, False),
        ({**http_proxy, "connectionNoProxy": ".xiaomimimo.com"}, "direct", MIMO_KEY, False),
    ):
        account(db, MIMO, details, key, legacy)
        audio(port, MIMO, lane, key)
    for details in (http_proxy, {"proxyPoolId": "speech-socks"}):
        account(db, SELF, {**details, "baseUrl": SELF_REMOTE}, SELF_KEY)
        audio(port, SELF, "tunnel", SELF_KEY)

    before = len(CALLS), len(shared.TUNNELS)
    account(db, SELF, {**http_proxy, "baseUrl": SELF_LOCAL}, SELF_KEY)
    rejected(SELF, "tunnels require an HTTPS endpoint")
    account(db, SELF, {"baseUrl": SELF_REMOTE, "proxyPoolId": "speech-managed"}, SELF_KEY)
    rejected(SELF, "does not support managed Relay")
    for base in (SELF_REMOTE + "?fixture=ignored", SELF_REMOTE + "#fragment",
                 SELF_REMOTE.replace("https://", "https://invalid:fixture@")):
        account(db, SELF, {**http_proxy, "baseUrl": base}, SELF_KEY)
        rejected(SELF, "Invalid protected speech endpoint")
    assert (len(CALLS), len(shared.TUNNELS)) == before

    account(db, SELF, {**http_proxy, "baseUrl": SELF_LOCAL, "connectionNoProxy": "127.0.0.1"})
    audio(port, SELF, "direct")
    with sqlite3.connect(db) as sql:
        sql.execute("""UPDATE settings SET data=json_patch(data,?) WHERE id=1""",
                    (json.dumps({"outboundProxyEnabled": True, "outboundProxyUrl": socks_url,
                                 "outboundNoProxy": "127.0.0.1"}),))
    account(db, SELF, {"baseUrl": SELF_LOCAL}, SELF_KEY)
    audio(port, SELF, "direct", SELF_KEY)
    with sqlite3.connect(db) as sql:
        sql.execute("""UPDATE settings SET data=json_patch(data,?) WHERE id=1""",
                    (json.dumps({"outboundProxyEnabled": False, "outboundNoProxy": ""}),))
    account(db, SELF, {"baseUrl": SELF_REMOTE, "proxyPoolId": "speech-socks"}, SELF_KEY)
    account(db, MIMO, {"proxyPoolId": "speech-managed"}, MIMO_KEY)
    for host, model, lane, key in ((MIMO, MIMO_MODEL, "relay", MIMO_KEY), (SELF, SELF_MODEL, "tunnel", SELF_KEY)):
        other = SELF_MODEL if host == MIMO else MIMO_MODEL
        combo = "protected-speech-" + host
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                VALUES (?,?,'tts',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (combo, combo, json.dumps([model, other])))
        MODE = "redirect"
        previous = len(CALLS)
        status, _, _, raw = request(port, host, "json", combo)
        assert status == 502 and b"redirect refused" in raw
        assert CALLS[previous:] == [(host, lane, ["Bearer " + key])]
        MODE = "error"
        previous = len(CALLS)
        status, _, _, raw = request(port, host, "json", combo)
        assert status == 502 and b"[redacted]" in raw
        assert all(secret.encode() not in raw for secret in shared.SECRETS)
        assert CALLS[previous:] == [(host, lane, ["Bearer " + key])]
        MODE = None
    account(db, SELF, {**http_proxy, "baseUrl": SELF_REMOTE}, SELF_KEY)
    http_server.reject = True
    previous = len(CALLS), len(shared.TUNNELS)
    assert request(port, SELF, "json")[0] == 502
    assert (len(CALLS), len(shared.TUNNELS)) == (previous[0], previous[1] + 1)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)
    assert set(shared.HELLOS) == {MIMO, SELF}


with tempfile.TemporaryDirectory(prefix="9router-mimo-self-tts-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca, target, _ = shared.tls_services(stack, scratch, Vendor, (MIMO, SELF))
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http_proxy = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("speech-socks", "socks5", socks_url), ("speech-managed", "cloudflare", RELAY)]
    with shared.gateway(scratch, "speech", ca, pools) as (port, database):
        with sqlite3.connect(database) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='speech-managed'",
                        (json.dumps({"url": RELAY, "token": RELAY_TOKEN}),))
        verify(port, database, http_proxy, socks_url, connect)
print("MiMo cloud and self-hosted speech transport verified with local audio, fixed auth, protected TLS and explicit HTTP bypass.")
