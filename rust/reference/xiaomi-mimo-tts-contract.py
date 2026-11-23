from base64 import b64decode, b64encode
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
from threading import Thread
import time
import wave

root = Path(__file__).resolve().parents[2]
mode = sys.argv[1]
reference = mode == "reference"
gateway_key = {"Authorization": "Bearer offline-chat-key"}
key = "sk-offline-xiaomi-mimo-speech"
legacy_key = "sk-offline-xiaomi-mimo-imported"
session_secret = "offline-account-service-cookie"
endpoint = "/xiaomi-mimo/v1/chat/completions"
speech_path = "/v1/audio/speech"
pcm = b"".join(struct.pack("<h", (i % 16 - 8) * 128) for i in range(64))
container = BytesIO()
with wave.open(container, "wb") as writer:
    writer.setparams((1, 2, 24_000, 0, "NONE", "not compressed"))
    writer.writeframes(pcm)
wav = container.getvalue()
ledger = []
failures = []


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        ledger.append(self.path)
        try:
            assert self.path == endpoint
            assert self.headers.get_all("Authorization") == ["Bearer " + upstream_key]
            assert self.headers.get("Content-Type") == "application/json"
            assert self.headers.get("X-Mimo-Source") is None and self.headers.get("Cookie") is None
            actual = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert actual == expected_wire, (actual, expected_wire)
            if scenario == "upstream-failure":
                return self.reply(403, {"error": {"message": "Fixture synthesis rejected"}})
            if scenario == "reflect":
                return self.reply(400, {"error": {"message": "Reflected bearer " + upstream_key}})
            if scenario == "no-audio":
                return self.reply(200, {"choices": [{"message": {}}]})
            if scenario == "declared-pcm":
                return self.reply(200, {"choices": [{"message": {"audio": {"data": b64encode(pcm).decode(), "format": "pcm"}}}]})
            encoded = b64encode(wav).decode()
            if scenario == "json-style":
                encoded = encoded.rstrip("=")
            self.reply(200, {"choices": [{"message": {"audio": {"data": encoded}}}]})
        except AssertionError as error:
            failures.append(str(error) or self.path)
            self.reply(500, {})

    def reply(self, status, data):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def configure(name, text, voice="mimo_default", language="", style="", auth_kind="apikey", custom_base=None):
    global scenario, upstream_key, expected_wire
    scenario = name
    upstream_key = legacy_key if auth_kind == "api_key" else key
    hints = ([f"Speak in {language}."] if language else []) + ([style] if style else [])
    messages = ([{"role": "user", "content": " ".join(hints)}] if hints else []) + [{"role": "assistant", "content": text.strip()}]
    expected_wire = {"model": "mimo-v2.5-tts", "stream": False, "messages": messages, "audio": {"format": "wav", "voice": voice}}
    ledger.clear()
    failures.clear()
    if not reference:
        value = {"apiKey": upstream_key} if auth_kind == "apikey" else {"accessToken": upstream_key}
        if auth_kind == "oauth":
            value = {"providerSpecificData": {"authMethod": "session", "mimoPassToken": session_secret}}
        if custom_base:
            value.setdefault("providerSpecificData", {})["baseUrl"] = custom_base
        with sqlite3.connect(database) as db:
            db.execute("DELETE FROM providerConnections WHERE provider='xiaomi-mimo'")
            db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES ('mimo-tts-fixture','xiaomi-mimo',?,'MiMo Fixture',1,1,?,
                        '2026-11-19T00:00:00Z','2026-11-19T00:00:00Z')""", (auth_kind, json.dumps(value)))


def run_reference(values):
    return json.loads(subprocess.check_output([
        "node", "--disable-warning=MODULE_TYPELESS_PACKAGE_JSON",
        str(root / "rust/reference/xiaomi-mimo-tts-reference.mjs"), json.dumps(values),
    ], cwd=root, env={"PATH": os.environ["PATH"]}))


def request(path, method="GET", body=None, headers=None):
    conn = HTTPConnection("127.0.0.1", port, timeout=8)
    try:
        conn.request(method, path, body=body, headers=headers or {})
        response = conn.getresponse()
        raw = response.read()
        assert all(secret.encode() not in raw for secret in (key, legacy_key, session_secret))
        return response.status, response.headers, raw
    finally:
        conn.close()


def say(path, body, authorized=True):
    data = json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if authorized:
        headers.update(gateway_key)
    status, reply, raw = request(path, "POST", data, headers)
    if status != 200:
        failure = json.loads(raw)["error"]
        return {"status": status, "error": failure["message"] if isinstance(failure, dict) else failure}
    return {"status": status, "mime": reply["Content-Type"], "cors": reply["Access-Control-Allow-Origin"],
            "body": b64encode(raw).decode()}


def compare():
    reports = {}
    for name, model, alias, voice, language, style, response, expected_error in [
        ("default-wav", "mimo-v2.5-tts", "mimo", "mimo_default", "", "", "mp3", None),
        ("json-style", "mimo-v2.5-tts/Mia", "xiaomi-mimo", "Mia", "Mandarin", "Read softly.", "json", None),
        ("declared-pcm", "Mia", "mimo", "mimo_default", "", "", "wav", None),
        ("no-audio", "mimo-v2.5-tts", "xiaomi-mimo", "mimo_default", "", "", "mp3", "MiMo TTS returned no audio"),
        ("upstream-failure", "mimo-v2.5-tts", "mimo", "mimo_default", "", "", "mp3", "Fixture synthesis rejected"),
    ]:
        text = "  合成测试 Synthetic audio. \n"
        configure(name, text, voice, language, style)
        if reference:
            result = run_reference({"model": model, "alias": alias, "input": text, "credentials": {"apiKey": key},
                                    "language": language, "style": style, "responseFormat": response})
        else:
            path = speech_path + ("?response_format=json" if response == "json" else "")
            result = say(path, {"model": alias + "/" + model, "input": text, "language": language, "style": style,
                                "response_format": "mp3" if response == "json" else response})
        assert ledger == [endpoint] and not failures, (name, ledger, failures)
        if expected_error:
            assert result["status"] == 502 and expected_error in result["error"], (name, result)
        else:
            assert result["status"] == 200 and result["cors"] == "*"
            wire = b64decode(result["body"])
            if response == "json":
                assert result["mime"] == "application/json"
                assert json.loads(wire) == {"audio": b64encode(wav).decode().rstrip("="), "format": "wav"}
            else:
                assert result["mime"] == ("audio/pcm" if name == "declared-pcm" else "audio/wav")
                assert wire == (pcm if name == "declared-pcm" else wav)
        reports[name] = result
    if not reference:
        assert fallback_log.read_bytes() == b""
    print(json.dumps(reports, sort_keys=True))


def strict():
    model = "mimo/mimo-v2.5-tts"
    text = "  Imported cloud key.  "
    configure("legacy-import", text, auth_kind="api_key", custom_base="http://127.0.0.1:52989/wrong")
    body = {"model": model, "input": text}
    original_import = run_reference({"core": True, "model": "mimo-v2.5-tts", "input": text, "credentials": {"accessToken": legacy_key}})
    assert original_import == {"status": 502, "error": "xiaomi-mimo API key required"} and not ledger
    assert say(speech_path, body, authorized=False) == {"status": 401, "error": "Missing API key"} and not ledger
    result = say(speech_path, body)
    assert result["status"] == 200 and b64decode(result["body"]) == wav and ledger == [endpoint] and not failures

    configure("reflect", text)
    result = say(speech_path, body)
    assert result["status"] == 502 and "Reflected bearer [redacted]" in result["error"] and ledger == [endpoint] and not failures
    with sqlite3.connect(database) as db:
        raw, = db.execute("SELECT data FROM providerConnections WHERE id='mimo-tts-fixture'").fetchone()
    error = json.loads(raw).get("lastError")
    assert not error or key not in error
    assert fallback_log.read_bytes() == b""
    assert all(secret.encode() not in output_log.read_bytes() for secret in (key, legacy_key, session_secret))

    unregistered = run_reference({"unregistered": True})
    assert unregistered == {"stt": {"status": 400, "error": "Provider 'xiaomi-tokenplan' does not support STT"},
                            "tts": {"status": 400, "error": "Provider 'xiaomi-tokenplan' does not support TTS via this route."}}
    configure("desktop", text, auth_kind="oauth")
    assert say(speech_path, body) == {"status": 502, "error": "xiaomi-mimo API key required"} and not ledger
    assert say(speech_path, dict(body, model="xmtp/mimo-v2.5-tts")) == unregistered["tts"] and not ledger
    audio = (b"--fixture\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n"
             b"xmtp/mimo-v2.5-asr\r\n--fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n"
             b"Content-Type: audio/wav\r\n\r\n" + wav + b"\r\n--fixture--\r\n")
    status, _, raw = request("/v1/audio/transcriptions", "POST", audio,
                           {"Content-Type": "multipart/form-data; boundary=fixture"} | gateway_key)
    assert status == unregistered["stt"]["status"] and json.loads(raw)["error"]["message"] == unregistered["stt"]["error"] and not ledger
    assert fallback_log.read_bytes() == b""
    print("MiMo cloud TTS verified: fixed bearer endpoint, imported key, synthetic WAV, typed output, redaction and session-only error")


def boot(directory, binary):
    global port, database, fallback_log, output_log
    location = Path(directory)
    home = location / "home"
    home.mkdir()
    data = location / "data"
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)], stderr=subprocess.DEVNULL, check=True)
    database = data / "db/data.sqlite"
    fallback_log = location / "fallback.log"
    output_log = location / "output.log"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with output_log.open("wb") as out:
        process = subprocess.Popen([binary, "--no-browser"], cwd=root, env={
            "PATH": "", "HOME": str(home), "DATA_DIR": str(data), "NINEROUTER_MIGRATION_DATA_DIR": str(data),
            "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback_log),
            "PORT": str(port), "HOSTNAME": "127.0.0.1",
        }, stdout=out, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        try:
            if request("/api/health")[0] == 200:
                return process
        except OSError:
            pass
        time.sleep(.03)
    process.terminate()
    process.wait(timeout=8)
    raise RuntimeError("Native gateway did not start")


with ThreadingHTTPServer(("127.0.0.1", 52989), Provider) as provider, tempfile.TemporaryDirectory(prefix="9router-mimo-tts-") as directory:
    runner = Thread(target=provider.serve_forever, daemon=True)
    runner.start()
    child = None
    try:
        if not reference:
            child = boot(directory, str(Path(sys.argv[2]).resolve()))
        if mode == "strict":
            strict()
        else:
            compare()
    finally:
        if child is not None:
            child.terminate()
            child.wait(timeout=8)
        provider.shutdown()
        runner.join()
