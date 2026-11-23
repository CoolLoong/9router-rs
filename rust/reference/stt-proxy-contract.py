from contextlib import ExitStack, closing
from email.parser import BytesParser
from email.policy import default
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import base64
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import parse_qs, urlsplit

sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("stt_route_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

OPENAI, GEMINI = "api.openai.com", "generativelanguage.googleapis.com"
OPENAI_KEY, GEMINI_KEY = "offline-openai-stt-proxy", "offline-gemini-stt-proxy"
DEEPGRAM, HUGGING_FACE = "api.deepgram.com", "router.huggingface.co"
DEEPGRAM_KEY, HF_KEY = "offline-deepgram-stt-proxy", "offline-hf-stt-proxy"
OPENAI_PATH = "/v1/audio/transcriptions"
GEMINI_PATH = "/v1beta/models/gemini-2.5-flash-lite:generateContent"
DEEPGRAM_PATH, HF_BASE = "/v1/listen", "/hf-inference/models"
HF_PATH = HF_BASE + "/openai/whisper-large-v3"
HOST_KEYS = {OPENAI: OPENAI_KEY, GEMINI: GEMINI_KEY, DEEPGRAM: DEEPGRAM_KEY, HUGGING_FACE: HF_KEY}
PROVIDER_MODELS = {"gemini":"gemini-2.5-flash-lite", "deepgram":"nova-3", "huggingface":"openai/whisper-large-v3"}
RELAY = "https://stt-fixture.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/stt-fixture.offline-lane.workers.dev"
RELAY_KEY = "T" * 43
AUDIO = b"RIFF\x00\xff\x80\r\nopaque offline sample\x00"
SENTENCE = "Verbatim offline transcript\n"
GEMINI_TEXT = "本地模拟 transcript"
binary_batch = sys.argv[2:] == ["binary"]
shared.ZED_HOST, shared.CLINE_HOST = (DEEPGRAM, HUGGING_FACE) if binary_batch else (OPENAI, GEMINI)
shared.SECRETS = (*HOST_KEYS.values(), RELAY_KEY)
DELIVERIES, FAILURES = [], []
MODE = "normal"


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            parsed = urlsplit(self.path)
            lane = "tunnel"
            host = self.headers["Host"]
            if parsed.path == RELAY_PATH:
                lane = "relay"
                host = urlsplit(self.headers["x-relay-target"]).hostname
                assert self.headers["x-relay-auth"] == RELAY_KEY and not parsed.query
                parsed = urlsplit(self.headers["x-relay-path"])
                assert host == DEEPGRAM or not parsed.query, parsed
            elif self.server.server_port == 52989:
                lane = "direct"
                if parsed.path == DEEPGRAM_PATH:
                    host = DEEPGRAM
                elif parsed.path == HF_PATH:
                    host = HUGGING_FACE
                else:
                    host = GEMINI if parsed.path.startswith("/gemini/") else OPENAI
                parsed = urlsplit(self.path.removeprefix("/gemini"))
            assert lane == "relay" or not any(self.headers[x] for x in ("x-relay-auth", "x-relay-target", "x-relay-path"))
            assert not any(self.headers[x] for x in ("Proxy-Authorization", "Cookie", "x-internal-token"))
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            if host == OPENAI:
                assert parsed.path == OPENAI_PATH and not parsed.query
                assert self.headers.get_all("Authorization") == ["Bearer " + OPENAI_KEY]
                content_type = self.headers["Content-Type"]
                form = BytesParser(policy=default).parsebytes(
                    ("Content-Type: "+content_type+"\r\n\r\n").encode() + raw)
                assert form.is_multipart()
                fields = {part.get_param("name", header="content-disposition"): part for part in form.iter_parts()}
                file = fields.pop("file")
                assert file.get_payload(decode=True) == AUDIO
                assert file.get_filename() == "synthetic.wav" and file.get_content_type() == "application/octet-stream"
                decoded = {name: field.get_payload(decode=True).decode() for name, field in fields.items()}
                assert decoded == {"model": "whisper-1", "prompt": "  fixture prompt  ", "language": " en-US ",
                                   "temperature": "0.25", "response_format": "text"}
                result, mime = SENTENCE.encode(), "text/plain; charset=utf-8"
            elif host == GEMINI:
                assert host == GEMINI and parsed.path == GEMINI_PATH and self.headers["Authorization"] is None
                assert self.headers["Content-Type"] == "application/json"
                if lane == "relay":
                    assert self.headers.get_all("x-goog-api-key") == [GEMINI_KEY]
                else:
                    assert parse_qs(parsed.query) == {"key": [GEMINI_KEY]} and self.headers["x-goog-api-key"] is None
                decoded = json.loads(raw)
                assert decoded == {"contents":[{"parts":[{"text":"fixture prompt Language: en-US."},
                                   {"inline_data":{"mime_type":"audio/wav","data":base64.b64encode(AUDIO).decode()}}]}]}
                result, mime = json.dumps({"candidates":[{"content":{"parts":[{"text":"本地模拟 "},
                                          {"text":"transcript"}]}}]}).encode(), "application/json"
            else:
                assert raw == AUDIO and self.headers["Content-Type"] == "audio/wav"
                decoded = parse_qs(parsed.query)
                if host == DEEPGRAM:
                    assert parsed.path == DEEPGRAM_PATH
                    assert self.headers.get_all("Authorization") == ["Token " + DEEPGRAM_KEY]
                    expected = {"model":["nova-3"], "smart_format":["true"], "punctuate":["true"]}
                    if "language" in decoded:
                        expected["language"] = ["en-US"]
                    else:
                        expected["detect_language"] = ["true"]
                    if "custom" in decoded:
                        assert lane != "relay"
                        expected["custom"] = ["one", "two"]
                    assert decoded == expected, decoded
                    payload = {"results":{"channels":[{"alternatives":[{"transcript":GEMINI_TEXT}]}]}}
                else:
                    assert host == HUGGING_FACE and parsed.path == HF_PATH and not decoded
                    assert self.headers.get_all("Authorization") == ["Bearer " + HF_KEY]
                    payload = {"text":GEMINI_TEXT}
                result, mime = json.dumps(payload).encode(), "application/json"
            DELIVERIES.append((lane, host, decoded))
            if MODE == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            if MODE == "failure":
                key = HOST_KEYS[host]
                message = f"synthetic rejection {key} {RELAY_KEY}"
                return self.reply(503, json.dumps({"error":{"message":message}}).encode())
            self.reply(200, result, mime)
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            FAILURES.append((self.path, repr(error)))
            self.reply(422, b"{}")


def save(db, provider, details=None, backup=False):
    key = {"gemini":GEMINI_KEY, "deepgram":DEEPGRAM_KEY, "huggingface":HF_KEY}.get(provider, OPENAI_KEY)
    plan = {"apiKey": key, "providerSpecificData": details or {}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Offline STT',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if backup else ""), provider, 2 if backup else 1, json.dumps(plan)))


def transcribe(port, provider, language=" en-US "):
    model = provider + "/" + PROVIDER_MODELS.get(provider, "whisper-1")
    fields = {"model":model, "prompt":"  fixture prompt  ", "language":language, "temperature":"0.25",
              "response_format":"text", "ignored":"must not forward"}
    boundary = b"fixture_stt_proxy_boundary"
    raw = b""
    for name, value in fields.items():
        raw += b"--" + boundary + (f'\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n').encode()
    raw += b"--" + boundary + (b'\r\nContent-Disposition: form-data; name="file"; filename="synthetic.wav"\r\n'
                               b'Content-Type: application/octet-stream\r\n\r\n') + AUDIO + b"\r\n--" + boundary + b"--\r\n"
    headers = {"Content-Type":"multipart/form-data; boundary=" + boundary.decode(),
               "Authorization":"Bearer offline-chat-key", "Cookie":"synthetic=do-not-forward", "x-internal-token":"offline"}
    with closing(HTTPConnection("127.0.0.1", port, timeout=6)) as app:
        app.request("POST", OPENAI_PATH, raw, headers)
        response = app.getresponse()
        result = response.read()
        assert not any(secret.encode() in result for secret in shared.SECRETS)
        return response.status, response.getheader("Content-Type"), result


def requirements(port, db, connect, http_proxy, socks):
    global MODE
    local = {"baseUrl":"http://127.0.0.1:52989" + OPENAI_PATH}
    save(db, "selfhosted-stt", local)
    save(db, "openai", http_proxy)
    save(db, "openai", local, backup=True)
    save(db, "gemini")
    save(db, "gemini", backup=True)
    openai = transcribe(port, "selfhosted-stt")
    gemini = transcribe(port, "gemini")
    assert openai == (200, "text/plain; charset=utf-8", SENTENCE.encode())
    assert gemini[0] == 200 and json.loads(gemini[2]) == {"text":GEMINI_TEXT}
    assert not shared.TUNNELS and [row[0] for row in DELIVERIES] == ["direct", "direct"]

    save(db, "gemini", {"proxyPoolId":"fixture-socks"})
    assert transcribe(port, "openai") == openai and transcribe(port, "gemini") == gemini
    assert shared.TUNNELS == [("connect", OPENAI, 443), ("socks", GEMINI, 443)]
    assert shared.HELLOS == [OPENAI, GEMINI]
    assert [row[1:] for row in DELIVERIES[-2:]] == [row[1:] for row in DELIVERIES[:2]]

    save(db, "gemini", http_proxy | {"connectionNoProxy":".googleapis.com"})
    assert transcribe(port, "gemini") == gemini and DELIVERIES[-1][0] == "direct"
    shared.enable_global_proxy(db, socks)
    assert transcribe(port, "gemini") == gemini and DELIVERIES[-1][0] == "tunnel"
    assert shared.TUNNELS[-1] == ("socks", GEMINI, 443)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.googleapis.com') WHERE id='fixture-socks'")

    before = (len(DELIVERIES), len(shared.TUNNELS))
    for provider, flags in (
        ("gemini", {"proxyPoolId":"fixture-socks"}), ("gemini", {"proxyPoolId":"missing"}),
        ("gemini", {"proxyPoolId":"unmanaged-relay"}), ("openai", {"strictProxy":True}),
        ("selfhosted-stt", local | http_proxy),
    ):
        save(db, provider, flags)
        assert transcribe(port, provider)[0] == 503
    assert (len(DELIVERIES), len(shared.TUNNELS)) == before
    save(db, "selfhosted-stt", local | http_proxy | {"connectionNoProxy":"127.0.0.1"})
    assert transcribe(port, "selfhosted-stt") == openai and len(shared.TUNNELS) == before[1]

    for provider, baseline in (("gemini", gemini), ("openai", openai)):
        save(db, provider, {"proxyPoolId":"managed-relay"})
        assert transcribe(port, provider) == baseline and DELIVERIES[-1][0] == "relay"
    save(db, "openai", {"proxyPoolId":"managed-relay", "baseUrl":f"https://{OPENAI}/fixture/{OPENAI_KEY}{OPENAI_PATH}"})
    before = len(DELIVERIES)
    status, _, raw = transcribe(port, "openai")
    assert status == 400 and b"contains a credential" in raw and len(DELIVERIES) == before

    for mode, code, expected in (("failure",503,b"[redacted]"), ("redirect",502,b"redirect refused")):
        MODE = mode
        before = len(DELIVERIES)
        status, _, raw = transcribe(port, "gemini")
        assert status == code and expected in raw and len(DELIVERIES) == before + 1
    MODE = "normal"
    save(db, "openai", http_proxy)
    connect.reject = True
    before = (len(DELIVERIES), len(shared.TUNNELS))
    assert transcribe(port, "openai")[0] == 502
    assert len(DELIVERIES) == before[0] and len(shared.TUNNELS) == before[1] + 1
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("STT preserved exact multipart audio and Gemini batch body over pinned CONNECT/SOCKS, valid bypass and managed Relay. Failure never re-sent audio.")


def raw_batch(port, db, connect, http_proxy, _socks):
    global MODE
    local = "http://127.0.0.1:52989"
    deepgram_override = DEEPGRAM_PATH + "?custom=one&smart_format=false&custom=two&smart_format=duplicate"
    save(db, "deepgram", {"baseUrl":local + deepgram_override})
    save(db, "huggingface", {"baseUrl":local + HF_BASE})
    for provider, path in (("deepgram", DEEPGRAM_PATH), ("huggingface", HF_BASE)):
        save(db, provider, {"baseUrl":local + path}, backup=True)
    baseline = {provider:transcribe(port, provider) for provider in ("deepgram", "huggingface")}
    assert all(status == 200 and json.loads(raw) == {"text":GEMINI_TEXT} for status, _, raw in baseline.values())
    assert not shared.TUNNELS and [row[0] for row in DELIVERIES] == ["direct", "direct"]

    save(db, "deepgram", http_proxy | {"baseUrl":"https://" + DEEPGRAM + deepgram_override})
    save(db, "huggingface", {"proxyPoolId":"fixture-socks"})
    assert all(transcribe(port, provider) == original for provider, original in baseline.items())
    assert shared.TUNNELS == [("connect", DEEPGRAM, 443), ("socks", HUGGING_FACE, 443)]
    assert shared.HELLOS == [DEEPGRAM, HUGGING_FACE]
    assert [row[1:] for row in DELIVERIES[-2:]] == [row[1:] for row in DELIVERIES[:2]]

    count = len(shared.TUNNELS)
    save(db, "deepgram", http_proxy | {"baseUrl":local + deepgram_override,"connectionNoProxy":"127.0.0.1"})
    assert transcribe(port, "deepgram") == baseline["deepgram"] and DELIVERIES[-1][0] == "direct"
    assert len(shared.TUNNELS) == count

    for provider, baseline_response in baseline.items():
        save(db, provider, {"proxyPoolId":"managed-relay"})
        assert transcribe(port, provider) == baseline_response and DELIVERIES[-1][0] == "relay"
    assert transcribe(port, "deepgram", language=" ") == baseline["deepgram"]
    assert DELIVERIES[-1][2]["detect_language"] == ["true"]
    before = (len(DELIVERIES), len(shared.TUNNELS))
    assert transcribe(port, "deepgram", language=DEEPGRAM_KEY)[0] == 400

    for provider, details in (("deepgram", {"proxyPoolId":"managed-relay","baseUrl":"https://"+DEEPGRAM+deepgram_override}),
                              ("huggingface", {"proxyPoolId":"managed-relay","baseUrl":"https://speech.fixture-provider.com/asr"}),
                              ("deepgram", {"proxyPoolId":"unmanaged-relay"}),
                              ("huggingface", {"strictProxy":True}),
                              ("deepgram", http_proxy | {"baseUrl":local+DEEPGRAM_PATH})):
        save(db, provider, details)
        assert transcribe(port, provider)[0] == 503
    assert (len(DELIVERIES), len(shared.TUNNELS)) == before

    save(db, "deepgram", {"proxyPoolId":"managed-relay"})
    MODE = "failure"
    status, _, raw = transcribe(port, "deepgram")
    assert status == 503 and b"[redacted]" in raw and len(DELIVERIES) == before[0] + 1
    MODE = "normal"
    save(db, "huggingface", http_proxy)
    connect.reject = True
    assert transcribe(port, "huggingface")[0] == 502
    assert len(shared.TUNNELS) == before[1] + 1 and len(DELIVERIES) == before[0] + 1
    assert not FAILURES and not shared.ERRORS, (FAILURES, shared.ERRORS)
    print("Deepgram and Hugging Face preserved exact binary STT, query and authorization via CONNECT/SOCKS, valid bypass and canonical Relay.")


with tempfile.TemporaryDirectory(prefix="9router-stt-proxy-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared.tls_services(stack, scratch, Provider)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Provider)))
    connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
    for proxy, mode in ((connect, "connect"), (socks, "socks")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared.serving(proxy))
    http_proxy = {"connectionProxyEnabled":True, "connectionProxyUrl":f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools = [("fixture-socks","socks5",socks_url), ("unmanaged-relay","cloudflare",RELAY),
             ("managed-relay","cloudflare",RELAY)]
    with shared.gateway(scratch, "stt", ca, pools) as (port, db):
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed-relay'",
                        (json.dumps({"url":RELAY,"token":RELAY_KEY}),))
            sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,'{}')")
        (raw_batch if binary_batch else requirements)(port, db, connect, http_proxy, socks_url)
