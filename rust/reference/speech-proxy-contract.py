from base64 import b64decode, b64encode
from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import importlib.util
import json
from pathlib import Path
import sqlite3
import struct
import sys
import tempfile
from threading import Event
from urllib.parse import parse_qs, quote_plus, urlsplit

sys.dont_write_bytecode = True
source = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("speech_proxy_fixture", source)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

OPENAI, GEMINI = "api.openai.com", "generativelanguage.googleapis.com"
OPENAI_PATH = "/v1/audio/speech"
GEMINI_PATH = "/v1beta/models/gemini-2.5-flash-preview-tts:generateContent"
KEYS = {OPENAI: "offline-speech-proxy-openai", GEMINI: "offline-speech-proxy-gemini+key"}
HOSTS = {"openai": OPENAI, "gemini": GEMINI}
MODELS = {"openai": "openai/tts-1/nova", "gemini": "gemini/gemini-2.5-flash-preview-tts/Puck"}
PROMPTS = {"openai": " Read this sentence ", "gemini": "你好"}
RELAY = "https://speech-fixture.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/speech-fixture.offline-lane.workers.dev"
RELAY_TOKEN = "T" * 43
AUTH = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json",
        "x-goog-api-key": "offline-request-google-key", "x-relay-auth": "offline-request-relay-token"}
MP3, PCM = b"ID3\x00\x00offline audio", b"\x01\x02\x03\x04"
WAV = struct.pack("<4sI8sIHHIIHH4sI", b"RIFF", 36 + len(PCM), b"WAVEfmt ", 16, 1, 1,
                  24000, 48000, 2, 16, b"data", len(PCM)) + PCM
CALLS, ERRORS = [], []
RELEASE = Event()
MODE = "normal"
shared.ZED_HOST, shared.CLINE_HOST = OPENAI, GEMINI
shared.SECRETS = (*KEYS.values(), RELAY_TOKEN, AUTH["x-goog-api-key"], AUTH["x-relay-auth"])


class SpeechVendor(BaseHTTPRequestHandler):
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
        ERRORS.append("Unexpected speech GET " + self.path)
        self.reply(422, b"{}")

    def do_POST(self):
        try:
            self.handle_audio()
        except (AssertionError, ValueError, KeyError) as error:
            ERRORS.append((urlsplit(self.path).path, repr(error)))
            self.reply(422, b"{}")

    def handle_audio(self):
        original = urlsplit(self.path)
        relay = original.path == RELAY_PATH
        direct = self.server.server_port == 52989 and not relay
        host = self.headers["Host"]
        if relay:
            target = urlsplit(self.headers["x-relay-target"])
            host = target.hostname
            assert self.headers["x-relay-target"] == "https://" + host
            assert self.headers.get_all("x-relay-auth") == [RELAY_TOKEN] and not original.query
            target = urlsplit(self.headers["x-relay-path"])
            assert not target.query
        elif direct:
            prefix = original.path.split("/")[1]
            host = HOSTS[prefix]
            target = urlsplit(self.path.removeprefix("/" + prefix))
        else:
            target = original
        assert host in KEYS and self.headers["Content-Type"] == "application/json"
        assert not any(self.headers[name] for name in ("Cookie", "Proxy-Authorization", "x-api-key"))
        if not relay:
            assert not any(self.headers[name] for name in ("x-relay-target", "x-relay-path", "x-relay-auth"))
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if host == OPENAI:
            assert target.path == OPENAI_PATH and not target.query
            assert self.headers.get_all("Authorization") == ["Bearer " + KEYS[host]]
            assert self.headers["x-goog-api-key"] is None
            assert payload == {"model": "tts-1", "voice": "nova", "input": PROMPTS["openai"].strip()}
        else:
            assert target.path == GEMINI_PATH and self.headers["Authorization"] is None
            if relay:
                assert self.headers.get_all("x-goog-api-key") == [KEYS[host]]
            else:
                assert parse_qs(target.query) == {"key": [KEYS[host]]} and self.headers["x-goog-api-key"] is None
            assert payload == {
                "contents": [{"parts": [{"text": "Say in Mandarin: " + PROMPTS["gemini"]}]}],
                "generationConfig": {
                    "responseModalities": ["AUDIO"],
                    "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": "Puck"}}},
                },
            }
        CALLS.append(("relay" if relay else "direct" if direct else "tunnel", host))
        if MODE == "redirect":
            return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
        if MODE == "error":
            message = f"Simulated refusal {KEYS[host]} {quote_plus(KEYS[host])} {RELAY_TOKEN}"
            return self.reply(503, json.dumps({"error": {"message": message}}).encode())
        if MODE == "missing-audio":
            return self.reply(200, json.dumps({"promptFeedback": {"blockReason": KEYS[host]}}).encode())
        if MODE == "stream":
            self.send_response(200)
            self.send_header("Content-Type", "audio/mpeg")
            self.send_header("Content-Length", str(len(MP3)))
            self.end_headers()
            self.wfile.write(MP3[:3])
            self.wfile.flush()
            assert RELEASE.wait(4)
            self.wfile.write(MP3[3:])
        elif host == OPENAI:
            self.reply(200, MP3, "audio/mpeg")
        else:
            parts = [{"inlineData": {"mimeType": "audio/L16", "data": b64encode(PCM).decode()}}]
            self.reply(200, json.dumps({"candidates": [{"content": {"parts": parts}}]}).encode())


def account(db, provider, details, backup=False, key=None):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Synthetic speech provider',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if backup else "-primary"), provider, 2 if backup else 1,
                     json.dumps({"apiKey": KEYS[HOSTS[provider]] if key is None else key, "providerSpecificData": details})))


def settings(db, **fields):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES(1,?)
            ON CONFLICT(id) DO UPDATE SET data=json_patch(settings.data,excluded.data)""", (json.dumps(fields),))


def audio_request(conn, provider, fmt, model=None, path="/v1/audio/speech"):
    query = "?response_format=" + fmt
    conn.request("POST", path + query, json.dumps({
        "model": model or MODELS[provider], "input": PROMPTS[provider],
        "language": "Mandarin", "response_format": "wrong-format", "baseUrl": "http://127.0.0.1:52989/capture",
    }), AUTH)
    return conn.getresponse()


def speech(port, provider, fmt="json", model=None, path="/v1/audio/speech"):
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as conn:
        response = audio_request(conn, provider, fmt, model, path)
        raw = response.read()
        assert all(secret.encode() not in raw for secret in shared.SECRETS)
        return response.status, response.getheader("Content-Type"), response.getheader("Content-Length"), raw


def combo(db, name, models):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
            VALUES (?,?,'tts',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (name, name, json.dumps(models)))


def verify(port, db, connect, http_proxy, socks_url):
    global MODE
    for provider in HOSTS:
        account(db, provider, {})
        account(db, provider, {}, backup=True, key="offline-proxy-backup-must-not-be-used")
    baseline = {}
    for provider, expected, fmt in (("openai", MP3, "mp3"), ("gemini", WAV, "wav")):
        result = speech(port, provider)
        assert result[:2] == (200, "application/json") and json.loads(result[-1])["format"] == fmt
        assert b64decode(json.loads(result[-1])["audio"]) == expected
        binary = speech(port, provider, fmt)
        assert binary == (200, "audio/" + fmt, str(len(expected)), expected)
        baseline[provider] = result, binary
    assert all(lane == "direct" for lane, _ in CALLS) and not shared.TUNNELS

    account(db, "openai", http_proxy)
    account(db, "gemini", {"proxyPoolId": "speech-socks"})
    assert speech(port, "openai", "mp3", path="/api/v1/audio/speech") == baseline["openai"][1]
    assert speech(port, "gemini", path="/v1/v1/audio/speech") == baseline["gemini"][0]
    assert CALLS[-2:] == [("tunnel", OPENAI), ("tunnel", GEMINI)]
    assert shared.TUNNELS == [("connect", OPENAI, 443), ("socks", GEMINI, 443)]
    assert shared.HELLOS == [OPENAI, GEMINI]

    MODE = "stream"
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=3)) as conn:
        response = audio_request(conn, "openai", "mp3")
        try:
            assert response.status == 200 and response.read(3) == MP3[:3]
        finally:
            RELEASE.set()
        assert response.read() == MP3[3:]
    MODE = "normal"

    account(db, "openai", http_proxy | {"connectionNoProxy": ".openai.com"})
    assert speech(port, "openai") == baseline["openai"][0] and CALLS[-1] == ("direct", OPENAI)
    settings(db, outboundProxyEnabled=True, outboundProxyUrl=socks_url)
    assert speech(port, "openai") == baseline["openai"][0] and shared.TUNNELS[-1] == ("socks", OPENAI, 443)
    account(db, "openai", {})
    settings(db, outboundNoProxy=OPENAI)
    assert speech(port, "openai") == baseline["openai"][0] and CALLS[-1] == ("direct", OPENAI)
    settings(db, outboundProxyEnabled=False, outboundNoProxy="")
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.googleapis.com') WHERE id='speech-socks'")
    before = len(CALLS), len(shared.TUNNELS)
    assert speech(port, "gemini")[0] == 503
    for details in ({"proxyPoolId": "missing"}, {"proxyPoolId": "speech-unmanaged"},
                    {"proxyPoolId": "speech-invalid"}, {"connectionProxyEnabled": True, "strictProxy": True},
                    {"vercelRelayUrl": RELAY}, http_proxy | {"connectionProxyUrl": http_proxy["connectionProxyUrl"] + "/wrong"}):
        account(db, "openai", details)
        assert speech(port, "openai")[0] == 503
    account(db, "openai", http_proxy, key="invalid key with spaces")
    assert speech(port, "openai")[0] == 503
    assert (len(CALLS), len(shared.TUNNELS)) == before

    for provider in HOSTS:
        account(db, provider, {"proxyPoolId": "speech-managed"})
        assert speech(port, provider) == baseline[provider][0]
    assert CALLS[-2:] == [("relay", OPENAI), ("relay", GEMINI)]

    combo(db, "speech-proxy-combo", [MODELS["gemini"], MODELS["openai"]])
    combo(db, "speech-mixed-combo", ["not-a-speech-adapter/model", MODELS["gemini"]])
    before = len(CALLS)
    assert speech(port, "gemini", model="speech-mixed-combo")[0] == 503 and len(CALLS) == before
    for mode, message in (("redirect", b"redirect refused"), ("error", b"[redacted]"),
                          ("missing-audio", b"Gemini TTS returned no audio")):
        MODE = mode
        before = len(CALLS)
        status, _, _, raw = speech(port, "gemini", model="speech-proxy-combo")
        assert status == 502 and message in raw and quote_plus(KEYS[GEMINI]).encode() not in raw, raw
        assert CALLS[before:] == [("relay", GEMINI)]
    MODE = "normal"
    account(db, "gemini", http_proxy)
    connect.reject = True
    before = len(CALLS), len(shared.TUNNELS)
    assert speech(port, "gemini", model="speech-proxy-combo")[0] == 502
    assert (len(CALLS), len(shared.TUNNELS)) == (before[0], before[1] + 1)
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)
    print("OpenAI and Gemini speech matched direct audio and request shaping through TLS tunnels and managed Relay without fallback or replay.")


with tempfile.TemporaryDirectory(prefix="9router-speech-proxy-") as folder, ExitStack() as stack:
    scratch = Path(folder)
    ca, target, _ = shared.tls_services(stack, scratch, SpeechVendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), SpeechVendor)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http_proxy = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("speech-socks", "socks5", socks_url), ("speech-unmanaged", "cloudflare", RELAY),
             ("speech-managed", "cloudflare", RELAY), ("speech-invalid", "cloudflare", RELAY)]
    with shared.gateway(scratch, "speech", ca, pools) as (port, db):
        with sqlite3.connect(db) as sql:
            for key, url in (("speech-managed", RELAY), ("speech-invalid", RELAY + "/wrong")):
                sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id=?",
                            (json.dumps({"url": url, "token": RELAY_TOKEN}), key))
        verify(port, db, connect, http_proxy, socks_url)
