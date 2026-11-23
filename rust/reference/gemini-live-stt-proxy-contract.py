from base64 import b64decode, b64encode
from contextlib import ExitStack, closing
from hashlib import sha1
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import select
import socket
import socketserver
import sqlite3
import struct
import sys
import tempfile
from urllib.parse import parse_qs, quote, urlsplit

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "proxy_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py"),
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "generativelanguage.googleapis.com"
MODEL = "gemini-2.5-flash-native-audio-preview-09-17"
KEY = "offline-gemini-live+proxy?key"
PROXY_USER, PROXY_PASS = b"offline-proxy-user", b"offline:proxy-pass"
PATH = f"/ws/api/v1beta/models/{MODEL}:bidiGenerateContent"
INSTRUCTION = "Fixture Live proxy transcription"
PCM = bytes(index % 113 for index in range(21003))
TUNNELS, HANDSHAKES, TURNS, ERRORS = [], [], [], []
MODE = "ok"
shared.ZED_HOST, shared.CLINE_HOST = HOST, "other.fixture.example.com"


class Vendor(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        try:
            self.websocket()
        except (AssertionError, TypeError, ValueError, KeyError, struct.error) as error:
            ERRORS.append(repr(error))
        except OSError as error:
            ERRORS.append(type(error).__name__)
        finally:
            self.close_connection = True

    def websocket(self):
        url = urlsplit(self.path)
        direct = self.server.server_port == 52988
        assert url.path == PATH and parse_qs(url.query) == {"key": [KEY]}
        assert self.headers["Host"] == ("127.0.0.1:52988" if direct else HOST)
        assert self.headers["Upgrade"].lower() == "websocket"
        assert self.headers["Sec-WebSocket-Version"] == "13"
        assert not any(self.headers[name] for name in ("Authorization", "Proxy-Authorization", "Cookie", "x-goog-api-key"))
        HANDSHAKES.append("direct" if direct else "tunnel")
        if MODE == "redirect":
            self.send_response(307)
            self.send_header("Location", "ws://127.0.0.1:52988/capture")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        secret = self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        accept = b64encode(sha1(secret.encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        opcode, raw = self.read_frame()
        assert opcode == 1
        assert json.loads(raw) == {
            "setup": {"model": "models/" + MODEL,
                      "generationConfig": {"responseModalities": ["TEXT"], "inputAudioTranscription": {}},
                      "systemInstruction": {"parts": [{"text": INSTRUCTION}]}},
        }
        self.send_frame({"setupComplete": {}})
        chunks = []
        while True:
            opcode, raw = self.read_frame()
            assert opcode == 1
            value = json.loads(raw)
            if "realtimeInput" not in value:
                assert value == {"clientContent": {"turns": [{"parts": [{"text": INSTRUCTION}]}], "turnComplete": True}}
                break
            audio, = value["realtimeInput"]["mediaChunks"]
            assert audio["mimeType"] == "audio/pcm"
            chunks.append(b64decode(audio["data"], validate=True))
        TURNS.append([len(chunk) for chunk in chunks])
        resumed = MODE == "rotate" and len(TURNS) == 2
        assert (chunks == [] if resumed else b"".join(chunks) == PCM and TURNS[-1] == [16384, len(PCM) - 16384])
        if MODE == "echo":
            self.send_frame({"error": {"status": "INVALID_ARGUMENT", "message": f"Fixture denied {KEY} {quote(KEY, safe='')}"}})
        elif MODE == "rotate" and not resumed:
            self.send_frame({"serverContent": {"inputTranscription": {"text": "first "}}})
            self.send_frame({"goAway": {"time": "2000-01-01T00:00:00Z"}})
        else:
            self.send_frame({"serverContent": {"inputTranscription": {"text": "pass" if resumed else "synthetic speech"}}})
            self.send_frame({"serverContent": {"turnComplete": True}})
        opcode, _ = self.read_frame()
        assert opcode == 8

    def read_frame(self):
        first, flags = self.rfile.read(2)
        assert first & 128 and flags & 128
        length = flags & 127
        if length >= 126:
            length = struct.unpack("!H" if length == 126 else "!Q", self.rfile.read(2 if length == 126 else 8))[0]
        assert length <= 25000
        mask = self.rfile.read(4)
        encoded = self.rfile.read(length)
        assert len(mask) == 4 and len(encoded) == length
        return first & 15, bytes(byte ^ mask[index % 4] for index, byte in enumerate(encoded))

    def send_frame(self, frame):
        data = json.dumps(frame).encode()
        prefix = bytes([129, len(data)]) if len(data) < 126 else b"\x81\x7e" + struct.pack("!H", len(data))
        self.wfile.write(prefix + data)
        self.wfile.flush()


class Tunnel(socketserver.StreamRequestHandler):
    def handle(self):
        mode = self.server.mode
        try:
            if mode == "connect":
                line = self.rfile.readline(4096)
                assert line == f"CONNECT {HOST}:443 HTTP/1.1\r\n".encode()
                headers = []
                while line := self.rfile.readline(4096).strip():
                    headers.append(line)
                expected = b"Basic " + b64encode(PROXY_USER + b":" + PROXY_PASS)
                assert headers == [f"Host: {HOST}:443".encode(), b"Proxy-Authorization: " + expected]
            else:
                version, length = self.rfile.read(2)
                assert version == 5 and self.rfile.read(length) == b"\x02"
                self.request.sendall(b"\x05\x02")
                version, size = self.rfile.read(2)
                assert version == 1 and self.rfile.read(size) == PROXY_USER
                size, = self.rfile.read(1)
                assert self.rfile.read(size) == PROXY_PASS
                self.request.sendall(b"\x01\x00")
                assert self.rfile.read(4) == b"\x05\x01\x00\x03"
                size, = self.rfile.read(1)
                assert self.rfile.read(size).decode() == HOST and int.from_bytes(self.rfile.read(2), "big") == 443
            TUNNELS.append(mode)
            if mode == "connect":
                reply = (b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"
                         if self.server.reject else b"HTTP/1.1 200 Connection Established\r\n\r\n")
            else:
                status = b"\x01" if self.server.reject else b"\x00"
                reply = b"\x05" + status + b"\x00\x01\x00\x00\x00\x00\x00\x00"
            self.request.sendall(reply)
            if self.server.reject:
                return
            with socket.create_connection(self.server.target, timeout=3) as upstream:
                while ready := select.select((self.request, upstream), [], [], 6)[0]:
                    for reader in ready:
                        data = reader.recv(65536)
                        if not data:
                            return
                        assert KEY.encode() not in data and quote(KEY, safe="").encode() not in data
                        (upstream if reader is self.request else self.request).sendall(data)
        except (AssertionError, ValueError) as error:
            ERRORS.append((mode, repr(error)))
        except OSError:
            pass


def account(db, details, fallback=False):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'gemini','apikey','Offline Gemini Live',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    ("live-backup" if fallback else "live-primary", 2 if fallback else 1,
                     json.dumps({"apiKey": "unused-backup-key" if fallback else KEY, "providerSpecificData": details})))


def configure_global(db, **values):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO settings(id,data) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET data=json_patch(settings.data,excluded.data)""",
                    (json.dumps(values),))


def transcribe(port):
    boundary = "synthetic-live-ws"
    data = b""
    for key, value in (("model", "gemini/" + MODEL), ("response_format", "verbose_json"),
                       ("system_instruction", INSTRUCTION), ("setup_timeout_ms", "1800")):
        data += f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
    data += f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="fixture.pcm"\r\nContent-Type: audio/pcm\r\n\r\n'.encode()
    data += PCM + f"\r\n--{boundary}--\r\n".encode()
    headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": f"multipart/form-data; boundary={boundary}"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as client:
        client.request("POST", "/v1/audio/transcriptions", data, headers)
        reply = client.getresponse()
        response = reply.read()
        assert KEY.encode() not in response and quote(KEY, safe="").encode() not in response
        assert PROXY_USER not in response and PROXY_PASS not in response
        return reply.status, json.loads(response)


def verify(port, db, connect, socks, wrong_host, http_proxy, socks_url):
    global MODE
    account(db, {})
    account(db, {}, True)
    original = transcribe(port)
    assert original == (200, {"text": "synthetic speech", "segments": [{"id": 0, "text": "synthetic speech"}]})
    assert HANDSHAKES == ["direct"] and not TUNNELS
    for config, lane in ((http_proxy, "connect"), ({"proxyPoolId": "live-socks"}, "socks")):
        account(db, config)
        assert transcribe(port) == original
        assert TUNNELS[-1] == lane and HANDSHAKES[-1] == "tunnel"
    assert shared.HELLOS == [HOST, HOST]

    account(db, {**http_proxy, "connectionNoProxy": ".googleapis.com"})
    assert transcribe(port) == original and HANDSHAKES[-1] == "direct" and len(TUNNELS) == 2
    configure_global(db, outboundProxyEnabled=True, outboundProxyUrl=socks_url)
    assert transcribe(port) == original and TUNNELS[-1] == "socks" and HANDSHAKES[-1] == "tunnel"
    account(db, {})
    configure_global(db, outboundNoProxy=HOST)
    assert transcribe(port) == original and HANDSHAKES[-1] == "direct" and len(TUNNELS) == 3
    configure_global(db, outboundProxyEnabled=False, outboundNoProxy="")

    before = len(TUNNELS), len(HANDSHAKES)
    for config in ({"proxyPoolId": "live-relay"}, {"proxyPoolId": "missing-pool"},
                   {"strictProxy": True}, {"vercelRelayUrl": "https://fake.workers.dev"},
                   {**http_proxy, "connectionProxyUrl": socks_url.replace("socks5h:", "socks5:")},
                   {**http_proxy, "connectionProxyUrl": http_proxy["connectionProxyUrl"] + "/broken"},
                   {**http_proxy, "baseUrl": "https://saved-bad-target.example.com"}):
        account(db, config)
        assert transcribe(port)[0] == 503
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','*') WHERE id='live-socks'")
    account(db, {"proxyPoolId": "live-socks"})
    assert transcribe(port)[0] == 503
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE proxyPools SET data=json_remove(data,'$.noProxy') WHERE id='live-socks'")
    assert (len(TUNNELS), len(HANDSHAKES)) == before

    for proxy, config, lane in ((connect, http_proxy, "connect"), (socks, {"proxyPoolId": "live-socks"}, "socks")):
        account(db, config)
        proxy.reject = True
        before = len(TUNNELS), len(HANDSHAKES)
        failed = transcribe(port)
        assert failed[0] == 502 and failed[1]["error"]["message"] == "Gemini Live websocket connection failed", failed
        assert TUNNELS[before[0]:] == [lane] and len(HANDSHAKES) == before[1]
        proxy.reject = False

    account(db, http_proxy)
    target = connect.target
    connect.target = wrong_host
    before = len(TUNNELS), len(HANDSHAKES)
    assert transcribe(port)[0] == 502
    assert TUNNELS[before[0]:] == ["connect"] and len(HANDSHAKES) == before[1]
    connect.target = target

    for mode in ("redirect", "echo"):
        MODE = mode
        before = len(TUNNELS), len(HANDSHAKES)
        status, data = transcribe(port)
        assert status == 502 and (mode != "echo" or "Fixture denied [redacted] [redacted]" in data["error"]["message"])
        assert TUNNELS[before[0]:] == ["connect"] and HANDSHAKES[before[1]:] == ["tunnel"]

    MODE = "rotate"
    TURNS.clear()
    account(db, {"proxyPoolId": "live-socks"})
    before = len(TUNNELS), len(HANDSHAKES)
    assert transcribe(port) == (200, {"text": "first pass", "segments": [{"id": 0, "text": "first "}, {"id": 1, "text": "pass"}]})
    assert TURNS == [[16384, len(PCM) - 16384], []]
    assert TUNNELS[before[0]:] == ["socks", "socks"] and HANDSHAKES[before[1]:] == ["tunnel", "tunnel"]
    assert not ERRORS, ERRORS


with tempfile.TemporaryDirectory(prefix="9router-gemini-live-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, wrong_host = shared.tls_services(stack, scratch, Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52988), Vendor)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    authority = "offline-proxy-user:offline%3Aproxy-pass@127.0.0.1:"
    http_proxy = {"connectionProxyEnabled": True,
                  "connectionProxyUrl": "http://" + authority + str(connect.server_address[1])}
    socks_url = "socks5h://" + authority + str(socks.server_address[1])
    pools = [("live-socks", "socks5", socks_url), ("live-relay", "cloudflare", "https://fixture.workers.dev")]
    with shared.gateway(scratch, "live", ca, pools) as (port, db):
        verify(port, db, connect, socks, wrong_host, http_proxy, socks_url)
    with shared.gateway(scratch, "untrusted", None, pools) as (port, db):
        before = len(HANDSHAKES)
        account(db, http_proxy)
        assert transcribe(port)[0] == 502 and len(HANDSHAKES) == before
print("Gemini Live verified with authenticated CONNECT/SOCKS5h, pinned TLS, bypass policy and no audio replay after GoAway.")
