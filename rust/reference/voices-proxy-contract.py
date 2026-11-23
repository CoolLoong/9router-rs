from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import re
import sqlite3
import sys
import tempfile
from urllib.parse import urlencode

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "voices_proxy_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

ADMIN = "/api/media-providers/tts/"
PUBLIC = "/v1/audio/voices"
CLI = {"x-9r-cli-token": "68977b6fd9422fbb", "x-internal-token": "do-not-forward"}
API = {"authorization": "Bearer offline-chat-key", "origin": "https://example.invalid"}
EDGE_HOST = "speech.platform.bing.com"
EDGE_PATH = "/consumer/speech/synthesize/readaloud/voices/list"
EDGE_FIXTURE = "/edge/consumer/voices/list?fixture=voices"
EDGE_SOURCE = Path(__file__).resolve().parents[2] / "open-sse/handlers/ttsProviders/edgeTts.js"
EDGE_TOKEN = re.search(r"trustedclienttoken=([A-F0-9]{32})", EDGE_SOURCE.read_text())[1]
VENDORS = {
    "elevenlabs": ("api.elevenlabs.io", "/v1/voices", "GET", "xi-api-key", ""),
    "deepgram": ("api.deepgram.com", "/v1/models", "GET", "authorization", "Token "),
    "inworld": ("api.inworld.ai", "/tts/v1/voices", "GET", "authorization", "Basic "),
    "minimax": ("api.minimax.io", "/v1/get_voice", "POST", "authorization", "Bearer "),
    "minimax-cn": ("api.minimaxi.com", "/v1/get_voice", "POST", "authorization", "Bearer "),
    "edge-tts": (EDGE_HOST, EDGE_PATH, "GET", None, ""),
}
KEYS = {provider: f"offline-audited-voice-{provider}" for provider in VENDORS if provider != "edge-tts"}
EXPLICIT_KEY = "offline-explicit-voice-api-key"
shared.SECRETS = (*KEYS.values(), EXPLICIT_KEY, EDGE_TOKEN)
MODE, ITEM = "ok", "direct"
REQUESTS, ERRORS = [], []


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, location=None):
        raw = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.process()

    do_POST = do_GET

    def process(self):
        try:
            direct = self.server.server_port == 52989
            if direct:
                provider = self.path.split("/")[1]
                if provider == "edge":
                    provider = "edge-tts"
            else:
                provider = next(p for p, vendor in VENDORS.items() if vendor[0] == self.headers["Host"])
            host, path, method, header, prefix = VENDORS[provider]
            expected = ("/" + provider if direct else "") + path
            if provider == "edge-tts":
                expected = EDGE_FIXTURE if direct else f"{path}?trustedclienttoken={EDGE_TOKEN}"
            assert self.path == expected
            assert self.command == method
            if header is None:
                key = None
                assert self.headers["Authorization"] is None
                assert "Chrome/146.0.0.0" in self.headers["User-Agent"]
            else:
                key = EXPLICIT_KEY if self.headers[header] == EXPLICIT_KEY else KEYS[provider]
                assert self.headers.get_all(header) == [prefix + key]
            assert not any(self.headers[name] for name in ("x-internal-token", "Proxy-Authorization", "x-relay-auth",
                                                           "x-relay-target", "x-relay-path", "Cookie"))
            if header == "xi-api-key":
                assert self.headers["Authorization"] is None and self.headers["Content-Type"] == "application/json"
            else:
                assert self.headers["xi-api-key"] is None
            body = None
            if method == "POST":
                assert self.headers["Content-Type"] == "application/json"
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                assert body == {"voice_type": "system" if provider == "minimax-cn" else "all"}
            REQUESTS.append((provider, "direct" if direct else "tunnel", key, body))
            if MODE == "redirect":
                return self.reply(307, {}, f"https://{host}/another-endpoint")
            if MODE == "error":
                return self.reply(503, {"error": f"mock upstream refused {key or EDGE_TOKEN}"})
            if provider == "elevenlabs":
                response = {"voices": [{"voice_id": "chosen-" + ITEM, "name": ITEM, "labels": {"language": "en"}}]}
            elif provider == "edge-tts":
                response = [{"Locale": "en-US", "ShortName": "en-US-" + ITEM, "Gender": "Female"}]
            elif provider == "deepgram":
                response = {"tts": [{"canonical_name": "aura-fixture-en", "languages": ["en"]}]}
            elif provider == "inworld":
                response = {"voices": [{"voiceId": "fixture-inworld", "displayName": "Fixture", "languages": ["en"]}]}
            else:
                response = {"system_voice": [{"voice_id": "English_Ada", "voice_name": "Ada"}]}
            self.reply(200, response)
        except (AssertionError, KeyError, StopIteration, TypeError, ValueError) as error:
            ERRORS.append((self.path.partition("?")[0], repr(error)))
            self.reply(422, {})


def save_accounts(db, providers):
    with sqlite3.connect(db) as sql:
        sql.executemany("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Offline Voice',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            [(provider, provider, json.dumps({"apiKey": KEYS[provider],
                "providerSpecificData": {"proxyPoolId": "ignored-nonexistent", "strictProxy": True,
                                         "baseUrl": "https://invalid.example"}})) for provider in providers])


def configure(db, proxy=None, no_proxy=None, login=False):
    settings = {"requireLogin": login}
    if proxy is not None:
        settings.update(outboundProxyEnabled=True, outboundProxyUrl=proxy)
    if no_proxy is not None:
        settings["outboundNoProxy"] = no_proxy
    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,?)", (json.dumps(settings),))


def read(port, provider, public=False, code=200, headers=None):
    if public:
        path = PUBLIC + "?" + urlencode({"provider": provider, "lang": "en"})
        authenticator = API
    else:
        path = ADMIN + ("minimax/voices?provider=minimax-cn&voice_type=system" if provider == "minimax-cn"
                        else "voices" if provider == "edge-tts"
                        else provider + "/voices")
        authenticator = CLI
    status, raw = shared.get(port, path, authenticator if headers is None else headers)
    assert all(key.encode() not in raw for key in shared.SECRETS)
    assert status == code, (provider, status, raw)
    return json.loads(raw)


def elevenlabs_cache(port, db, connect, http_proxy, socks_proxy, wrong_tls):
    global MODE, ITEM
    configure(db)
    ITEM = "direct"
    raw = read(port, "elevenlabs", True)
    assert raw["data"][0]["model"] == "el/chosen-direct"
    assert REQUESTS[-1][:2] == ("elevenlabs", "direct")
    assert read(port, "elevenlabs", True) == raw and len(REQUESTS) == 1

    configure(db, http_proxy)
    connect.reject = True
    read(port, "elevenlabs", True, code=502)
    assert len(REQUESTS) == 1 and shared.TUNNELS == [("connect", "api.elevenlabs.io", 443)]
    connect.reject = False
    ITEM = "connect"
    via_http = read(port, "elevenlabs", True)
    assert via_http["data"][0]["model"] == "el/chosen-connect"
    assert REQUESTS[-1][:2] == ("elevenlabs", "tunnel")
    assert read(port, "elevenlabs", True) == via_http and len(REQUESTS) == 2

    configure(db, socks_proxy)
    ITEM = "socks"
    via_socks = read(port, "elevenlabs", True)
    assert via_socks["data"][0]["model"] == "el/chosen-socks"
    assert REQUESTS[-1][:2] == ("elevenlabs", "tunnel")
    assert shared.TUNNELS[-1] == ("socks", "api.elevenlabs.io", 443) and len(REQUESTS) == 3
    status, output = shared.get(port, ADMIN + "voices?" + urlencode({"provider": "elevenlabs", "apiKey": EXPLICIT_KEY}), CLI)
    assert status == 200 and json.loads(output)["voices"][0]["id"] == "chosen-socks"
    assert REQUESTS[-1] == ("elevenlabs", "tunnel", EXPLICIT_KEY, None) and len(REQUESTS) == 4

    configure(db, socks_proxy, ".elevenlabs.io")
    ITEM = "bypass"
    bypass = read(port, "elevenlabs", True)
    assert bypass["data"][0]["model"] == "el/chosen-bypass"
    assert REQUESTS[-1][:2] == ("elevenlabs", "direct") and len(REQUESTS) == 5
    assert read(port, "elevenlabs", True) == bypass and len(REQUESTS) == 5
    configure(db)
    assert read(port, "elevenlabs", True) == raw and len(REQUESTS) == 5

    configure(db, http_proxy)
    connect.target = wrong_tls
    count = (len(REQUESTS), len(shared.TUNNELS))
    read(port, "deepgram", code=502)
    assert (len(REQUESTS), len(shared.TUNNELS)) == (count[0], count[1] + 1)
    assert shared.HELLOS[-1] == "api.deepgram.com"


def verify_provider(port, db, provider, http_proxy, socks_proxy):
    global MODE
    public = provider in ("elevenlabs", "deepgram", "inworld")
    configure(db, socks_proxy, login=True)
    read(port, provider, code=401, headers={})
    if public:
        count = (len(REQUESTS), len(shared.TUNNELS))
        read(port, provider, True, code=401)
        assert (len(REQUESTS), len(shared.TUNNELS)) == count

    configure(db)
    direct = read(port, provider)
    assert REQUESTS[-1][:2] == (provider, "direct")
    direct_public = read(port, provider, True) if public else None
    for proxy, mode in ((http_proxy, "connect"), (socks_proxy, "socks")):
        configure(db, proxy)
        assert read(port, provider) == direct
        assert REQUESTS[-1][:2] == (provider, "tunnel")
        assert shared.TUNNELS[-1] == (mode, VENDORS[provider][0], 443)
        if public:
            assert read(port, provider, True) == direct_public

    configure(db, http_proxy, "." + VENDORS[provider][0])
    count = len(shared.TUNNELS)
    assert read(port, provider) == direct and REQUESTS[-1][:2] == (provider, "direct")
    assert len(shared.TUNNELS) == count

    configure(db, http_proxy)
    for mode, expected in (("redirect", "redirect refused"), ("error", "[redacted]")):
        MODE = mode
        count = (len(REQUESTS), len(shared.TUNNELS))
        result = read(port, provider, code=502)
        assert expected in result["error"], result
        assert (len(REQUESTS), len(shared.TUNNELS)) == (count[0] + 1, count[1] + 1)
    MODE = "ok"

    for proxy in ("socks4a://127.0.0.1:12345", "https://fixture.invalid/relay", "obviously invalid"):
        configure(db, proxy, "*")
        count = (len(REQUESTS), len(shared.TUNNELS))
        assert read(port, provider, code=503)["error"] == "Voice catalog proxy is unavailable"
        assert (len(REQUESTS), len(shared.TUNNELS)) == count


def verify_edge(port, db, connect, http_proxy, socks_proxy):
    global MODE, ITEM
    start = len(REQUESTS)
    configure(db, login=True)
    ITEM = "direct"
    baseline = read(port, "edge-tts", True)
    assert baseline["data"][0]["model"] == "edge-tts/en-US-direct"
    assert read(port, "edge-tts")["voices"][0]["id"] == "en-US-direct"
    assert len(REQUESTS) == start + 1 and REQUESTS[-1][:2] == ("edge-tts", "direct")

    configure(db, "https://relay.invalid/worker", "*")
    result = read(port, "edge-tts", True, code=503)
    assert result["error"]["message"] == "Voice catalog proxy is unavailable"
    assert len(REQUESTS) == start + 1 and not shared.TUNNELS

    configure(db, http_proxy)
    connect.reject = True
    assert read(port, "edge-tts", code=502)["error"] == "fetch failed"
    assert len(REQUESTS) == start + 1 and shared.TUNNELS == [("connect", EDGE_HOST, 443)]
    connect.reject = False
    for mode, error in (("redirect", "Voice catalog upstream redirect refused"),
                        ("error", "Edge TTS voices fetch failed: 503")):
        MODE = mode
        count = (len(REQUESTS), len(shared.TUNNELS))
        assert read(port, "edge-tts", code=502)["error"] == error
        assert (len(REQUESTS), len(shared.TUNNELS)) == (count[0] + 1, count[1] + 1)
    MODE = "ok"

    for proxy, item, lane, tunnel in ((http_proxy, "connect", "tunnel", "connect"),
                                       (socks_proxy, "socks", "tunnel", "socks"),
                                       (socks_proxy, "bypass", "direct", None)):
        configure(db, proxy, ".platform.bing.com" if tunnel is None else None)
        ITEM = item
        count = (len(REQUESTS), len(shared.TUNNELS))
        via = read(port, "edge-tts", True)
        assert via["data"][0]["model"] == "edge-tts/en-US-" + item
        assert REQUESTS[-1][:2] == ("edge-tts", lane)
        assert len(shared.TUNNELS) == count[1] + (1 if tunnel else 0)
        if tunnel:
            assert shared.TUNNELS[-1] == (tunnel, EDGE_HOST, 443) and shared.HELLOS[-1] == EDGE_HOST
        code, raw = shared.get(port, ADMIN + "voices?provider=edge-tts&lang=en", CLI)
        assert code == 200 and EDGE_TOKEN.encode() not in raw
        assert json.loads(raw)["voices"][0]["id"] == "en-US-" + item
        assert len(REQUESTS) == count[0] + 1
    configure(db)
    count = len(REQUESTS)
    assert read(port, "edge-tts", True) == baseline and len(REQUESTS) == count


with tempfile.TemporaryDirectory(prefix="9router-voices-proxy-") as directory, ExitStack() as outer:
    outer.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Provider)))
    for index, pair in enumerate((("elevenlabs", "deepgram"), ("inworld", "minimax"),
                                  ("minimax-cn", "deepgram"), ("edge-tts", "deepgram"))):
        shared.ZED_HOST, shared.CLINE_HOST = (VENDORS[provider][0] for provider in pair)
        with ExitStack() as stack:
            scratch = Path(directory) / str(index)
            scratch.mkdir()
            ca, tls, wrong_tls = shared.tls_services(stack, scratch, Provider)
            connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
            for proxy, mode in ((connect, "connect"), (socks, "socks")):
                proxy.mode, proxy.reject, proxy.target = mode, False, tls
                stack.enter_context(shared.serving(proxy))
            http_proxy = f"http://127.0.0.1:{connect.server_address[1]}"
            socks_proxy = f"socks5h://127.0.0.1:{socks.server_address[1]}"
            with shared.gateway(scratch, "voices", ca, []) as (port, db):
                save_accounts(db, [provider for provider in pair if provider in KEYS])
                if index == 0:
                    elevenlabs_cache(port, db, connect, http_proxy, socks_proxy, wrong_tls)
                    connect.target = tls
                for provider in pair:
                    if provider == "edge-tts":
                        verify_edge(port, db, connect, http_proxy, socks_proxy)
                    elif provider != "elevenlabs" and (provider != "deepgram" or index == 0):
                        verify_provider(port, db, provider, http_proxy, socks_proxy)
        assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)
        assert set(shared.HELLOS) == set(shared_host for _, shared_host, _ in shared.TUNNELS)
        shared.HELLOS.clear()
        shared.TUNNELS.clear()

print("Provider voice catalogs use global CONNECT/SOCKS5h or explicit bypass, with canonical TLS and route-scoped caching.")
