from contextlib import ExitStack, closing
from email.parser import BytesParser
from email.policy import default
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "elevenlabs_stt_tls", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "api.elevenlabs.io"
API = "/v1/speech-to-text"
APP = "/v1/audio/transcriptions"
RELAY_HOST = "elevenlabs-stt-fixture.offline-lane.workers.dev"
RELAY = "https://" + RELAY_HOST
RELAY_KEY = "E" * 43
KEY = "offline-usable-elevenlabs-stt"
SECONDARY_KEY = "offline-backup-elevenlabs-stt"
AUDIO = b"RIFF\x00\xff\x80\r\nopaque offline sample\x00"
TEXT = "Fixture transcript"
SRT = "1\n00:00:00,000 --> 00:00:01,000\nTranscript 你好\n"
VTT = "WEBVTT\n\n00:00.000 --> 00:01.000\nTranscript 你好\n"
WORDS = [{"text": "Fixture", "start": 0, "end": 0.5}]
SEGMENTS = [{"text": TEXT, "start": 0, "end": 1}]
DELIVERIES, ERRORS = [], []
MODE = "success"
shared.ZED_HOST, shared.CLINE_HOST = HOST, "other.invalid"
shared.SECRETS = (KEY, SECONDARY_KEY, RELAY_KEY)


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, output, **headers):
        body = json.dumps(output).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in headers.items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            lane = "tunnel"
            if self.path == "/cloudflare-relay/" + RELAY_HOST:
                lane = "relay"
                assert self.headers.get_all("x-relay-target") == ["https://" + HOST]
                assert self.headers.get_all("x-relay-path") == [API]
                assert self.headers.get_all("x-relay-auth") == [RELAY_KEY]
            elif self.server.server_port == 52989:
                lane = "direct"
                assert self.path == "/elevenlabs" + API
            else:
                assert self.path == API and self.headers["Host"] == HOST
            assert lane == "relay" or not any(self.headers[name] for name in
                                              ("x-relay-target", "x-relay-path", "x-relay-auth"))
            assert self.headers.get_all("xi-api-key") == [KEY]
            assert not any(self.headers[name] for name in
                           ("Proxy-Authorization", "Authorization", "Cookie", "x-internal-token"))
            body = self.rfile.read(int(self.headers["Content-Length"]))
            form = BytesParser(policy=default).parsebytes(
                f"Content-Type: {self.headers['Content-Type']}\r\n\r\n".encode() + body
            )
            assert form.is_multipart()
            parts = {part.get_param("name", header="content-disposition"): part for part in form.iter_parts()}
            file = parts.pop("file")
            assert file.get_filename() == "fixture.wav" and file.get_content_type() == "audio/wav"
            assert file.get_payload(decode=True) == AUDIO
            fields = {name: part.get_payload(decode=True).decode() for name, part in parts.items()}
            extra = json.loads(fields.pop("additional_formats", "[]"))
            assert fields.pop("model_id") == "scribe_v2"
            assert fields == ({"language_code": "fr", "timestamps_granularity": "word",
                               "tag_audio_events": "true", "diarize": "true"}
                              if extra == [{"format": "seg_json"}] else {"num_speakers": "4"})
            renders = {"seg_json": json.dumps(SEGMENTS), "srt": SRT, "vtt": VTT}
            output = {"text": TEXT, "language_code": "fra", "language_probability": .95,
                      "words": WORDS, "num_speakers": 2,
                      "additional_formats": [dict(item, content=renders[item["format"]]) for item in extra]}
            DELIVERIES.append((lane, extra))
            if MODE == "failure":
                return self.reply(503, {"error": {"message": f"fixture rejection {KEY} {RELAY_KEY}"}})
            if MODE == "redirect":
                return self.reply(307, {}, Location="http://127.0.0.1:52989/should-not-follow")
            self.reply(200, output)
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((self.path, repr(error)))
            self.reply(422, {})


def save(db, details, backup=False):
    data = {"apiKey": SECONDARY_KEY if backup else KEY, "providerSpecificData": details}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'elevenlabs','apikey','Offline ElevenLabs',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    ("elevenlabs-backup" if backup else "elevenlabs", 2 if backup else 1, json.dumps(data)))


def transcribe(port, format):
    fields = {"model": "elevenlabs/scribe_v2", "response_format": " " + format + " ",
              "language": " fr " if format == "verbose_json" else " ", "num_speakers": "4",
              "timestamps_granularity": "word" if format == "verbose_json" else "bogus",
              "tag_audio_events": " true " if format == "verbose_json" else "false",
              "diarize": "true" if format == "verbose_json" else "false", "ignored": "discard me"}
    boundary = b"fixture_elevenlabs_stt_boundary"
    raw = b"".join(b"--" + boundary + (f'\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n').encode()
                   for name, value in fields.items())
    raw += (b"--" + boundary + b'\r\nContent-Disposition: form-data; name="file"; filename="fixture.wav"\r\n'
            b"Content-Type: audio/wav\r\n\r\n" + AUDIO + b"\r\n--" + boundary + b"--\r\n")
    headers = {"Content-Type": "multipart/form-data; boundary=" + boundary.decode(),
               "Authorization": "Bearer offline-chat-key", "Cookie": "synthetic=discard", "x-internal-token": "offline"}
    with closing(HTTPConnection("127.0.0.1", port, timeout=6)) as client:
        client.request("POST", APP, raw, headers)
        response = client.getresponse()
        body = response.read()
        assert all(secret.encode() not in body for secret in shared.SECRETS), body
        return response.status, response.getheader("Content-Type"), body


def verify(port, db, connect, tunnel, socks_url, wrong_tls):
    global MODE
    save(db, {}, backup=True)
    save(db, {})
    formats = ("verbose_json", "SRT", "vtt", "text", "json")
    direct = {fmt: transcribe(port, fmt) for fmt in formats}
    assert direct["SRT"] == (200, "text/plain; charset=utf-8", SRT.encode())
    assert direct["vtt"] == (200, "text/plain; charset=utf-8", VTT.encode())
    assert direct["text"] == (200, "text/plain; charset=utf-8", TEXT.encode())
    assert direct["json"][0] == 200 and json.loads(direct["json"][2]) == {"text": TEXT}
    assert direct["verbose_json"][0] == 200 and json.loads(direct["verbose_json"][2]) == {
        "text": TEXT, "language": "fra", "language_probability": .95, "words": WORDS,
        "num_speakers": 2, "segments": SEGMENTS,
    }
    assert [lane for lane, _ in DELIVERIES] == ["direct"] * len(formats) and not shared.TUNNELS

    save(db, tunnel)
    assert transcribe(port, "verbose_json") == direct["verbose_json"]
    save(db, {"proxyPoolId": "socks"})
    assert transcribe(port, "SRT") == direct["SRT"]
    assert shared.TUNNELS == [("connect", HOST, 443), ("socks", HOST, 443)]
    assert shared.HELLOS == [HOST, HOST]
    assert DELIVERIES[-2:] == [("tunnel", [{"format": "seg_json"}]), ("tunnel", [{"format": "srt"}])]

    save(db, tunnel | {"connectionNoProxy": ".elevenlabs.io"})
    assert transcribe(port, "vtt") == direct["vtt"] and DELIVERIES[-1][0] == "direct"
    assert len(shared.TUNNELS) == 2
    shared.enable_global_proxy(db, socks_url)
    assert transcribe(port, "vtt") == direct["vtt"] and DELIVERIES[-1][0] == "tunnel"
    assert shared.TUNNELS[-1] == ("socks", HOST, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")

    save(db, {"proxyPoolId": "managed"})
    assert transcribe(port, "verbose_json") == direct["verbose_json"] and DELIVERIES[-1][0] == "relay"
    assert transcribe(port, "SRT") == direct["SRT"] and DELIVERIES[-1][0] == "relay"
    before = (len(DELIVERIES), len(shared.TUNNELS))
    for details in ({"strictProxy": True}, {"proxyPoolId": "missing"}, {"proxyPoolId": "unmanaged"},
                    tunnel | {"strictProxy": True, "connectionNoProxy": ".elevenlabs.io"},
                    {"proxyPoolId": "managed", "baseUrl": "https://" + HOST + API + "?custom=1"},
                    {"proxyPoolId": "managed", "baseUrl": "https://other.invalid" + API}):
        save(db, details)
        assert transcribe(port, "json")[0] == 503
    assert (len(DELIVERIES), len(shared.TUNNELS)) == before

    save(db, {"proxyPoolId": "managed"})
    for mode, expected_code, expected in (("redirect", 502, b"redirect refused"), ("failure", 503, b"[redacted] [redacted]")):
        MODE = mode
        count = len(DELIVERIES)
        code, _, body = transcribe(port, "json")
        assert (code, len(DELIVERIES)) == (expected_code, count + 1) and expected in body, (code, body)
    MODE = "success"

    save(db, tunnel)
    count, tunneled = len(DELIVERIES), len(shared.TUNNELS)
    connect.target = wrong_tls
    assert transcribe(port, "json")[0] == 502
    assert len(DELIVERIES) == count and len(shared.TUNNELS) == tunneled + 1 and shared.HELLOS[-1] == HOST
    connect.reject = True
    assert transcribe(port, "json")[0] == 502
    assert len(DELIVERIES) == count and len(shared.TUNNELS) == tunneled + 2
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)


with tempfile.TemporaryDirectory(prefix="9router-elevenlabs-stt-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, tls, wrong_tls = shared.tls_services(stack, scratch, Provider)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Provider)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, tls
        stack.enter_context(shared.serving(proxy))
    tunnel = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("socks", "socks5", socks_url), ("unmanaged", "cloudflare", RELAY), ("managed", "cloudflare", RELAY)]
    with shared.gateway(scratch, "elevenlabs", ca, pools) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,'{}')")
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed'",
                        (json.dumps({"url": RELAY, "token": RELAY_KEY}),))
        verify(port, db, connect, tunnel, socks_url, wrong_tls)

print("ElevenLabs STT preserved binary multipart, segments and captions across CONNECT, SOCKS5h, valid bypass and managed Relay.")
