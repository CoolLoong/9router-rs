import concurrent.futures
import http.client
import http.server
import json
import os
import re
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

repository = Path(__file__).resolve().parents[2]
upstream_path = "/codex/backend-api/transcribe"
inference = {"authorization":"Bearer offline-chat-key"}
transcript = "Fixture 转录 🛰️\nsecond line"
uploads = []
unauthorized_origins = []
release_concurrent = threading.Event()

fake_ffmpeg = r'''
import os
import pathlib
import stat
import sys

args = sys.argv[1:]
fixed_prefix = [
    "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1",
    "-protocol_whitelist", "file,pipe", "-format_whitelist",
    "wav,mp3,mov,matroska,webm,ogg,flac,aac", "-i",
]
fixed_suffix = [
    "-map", "0:a:0", "-vn", "-t", "120.01", "-threads", "1", "-ac", "1",
    "-ar", "24000", "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1",
]
assert args[:len(fixed_prefix)] == fixed_prefix
path = pathlib.Path(args[len(fixed_prefix)])
assert args[len(fixed_prefix) + 1:] == fixed_suffix
assert path.name == "input"
assert stat.S_IMODE(path.stat().st_mode) == 0o600
assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
data = path.read_bytes()
marker = {
    b"fixture audio json": b"\x11",
    b"fixture audio text": b"\x22",
    b"fixture error body": b"\x33",
    b"fixture redirect": b"\x44",
    b"fixture oversized response": b"\x55",
    b"fixture long transcript": b"\x66",
    b"fixture malformed response": b"\x77",
    b"fixture retry": b"\x88",
    b"fixture concurrent": b"\x99",
}
if data == b"fixture duration":
    sys.stdout.buffer.write(bytes(120 * 24000 * 2 + 2))
elif data in marker:
    sys.stdout.buffer.write(marker[data] + b"\x00\x12\x00")
else:
    sys.exit(2)
'''


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send(self, status, body, headers=None):
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def expected_origin(self):
        return self.server.server_port == 52987 and self.path == upstream_path

    def do_POST(self):
        if not self.expected_origin():
            unauthorized_origins.append(self.path)
            return self.send(418, b'{"error":"unexpected origin"}')
        choices = {
            "Bearer offline-codex-primary":"fixture-workspace-primary",
            "Bearer offline-codex-secondary":"fixture-workspace-secondary",
        }
        token = self.headers.get("authorization")
        expected = choices.get(token)
        if (
            not expected or self.headers.get("ChatGPT-Account-Id") != expected
            or self.headers.get("User-Agent") != "codex_cli_rs/0.167.1"
            or self.headers.get("x-api-key") is not None
        ):
            return self.send(422, b'{"error":"unexpected Codex credentials"}')
        content_type = self.headers.get("content-type", "")
        match = re.fullmatch(r"multipart/form-data; boundary=(----9router-codex-[0-9a-f-]{36})", content_type)
        if not match:
            return self.send(422, b'{"error":"unexpected multipart wire"}')
        boundary = match[1].encode()
        data = self.rfile.read(int(self.headers["content-length"]))
        header = b"--" + boundary + b'\r\nContent-Disposition: form-data; name="file"; filename="audio.wav"\r\nContent-Type: audio/wav\r\n\r\n'
        tail = b"\r\n--" + boundary + b"--\r\n"
        if not data.startswith(header) or not data.endswith(tail):
            return self.send(422, b'{"error":"unexpected multipart fields"}')
        wav = data[len(header):-len(tail)]
        expected_wav_header = struct.pack(
            "<4sI8sIHHIIHH4sI", b"RIFF", 40, b"WAVEfmt ", 16, 1, 1, 24000, 48000, 2, 16, b"data", 4
        )
        if len(wav) != 48 or not wav.startswith(expected_wav_header) or wav[-3:] != b"\x00\x12\x00":
            return self.send(422, b'{"error":"unexpected WAV conversion"}')
        marker = wav[44]
        uploads.append((marker, expected))
        if marker == 0x33:
            return self.send(422, json.dumps({"error":token}).encode())
        if marker == 0x44:
            return self.send(307, b"", {"Location":"http://127.0.0.1:52988/captured"})
        if marker == 0x55:
            return self.send(200, b"x" * (1024 * 1024 + 1))
        if marker == 0x66:
            return self.send(200, json.dumps({"text":"🛰" * 125001}, ensure_ascii=False).encode())
        if marker == 0x77:
            return self.send(200, b"not valid JSON")
        if marker == 0x88 and expected == "fixture-workspace-primary":
            return self.send(401, b'{"error":"synthetic account rejected"}')
        if marker == 0x99 and not release_concurrent.wait(timeout=10):
            return self.send(503, b'{"error":"concurrent fixture timed out"}')
        return self.send(200, json.dumps({"text":transcript,"unwanted":"discard"}, ensure_ascii=False).encode())

    def do_GET(self):
        unauthorized_origins.append(self.path)
        return self.send(418, b'{"error":"unexpected request"}')


def start_upstream():
    servers = [http.server.ThreadingHTTPServer(("127.0.0.1", port), Upstream) for port in (52987, 52988)]
    for server in servers:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    return servers


def make_ffmpeg(folder):
    (folder / "bin").mkdir(parents=True, exist_ok=True)
    path = folder / "bin" / "ffmpeg"
    path.write_text("#!" + sys.executable + "\n" + fake_ffmpeg)
    path.chmod(0o700)
    for command in ["node", "python3", "sh", "env", "dirname"]:
        (path.parent / command).symlink_to(shutil.which(command))


def multipart(fields):
    boundary = "9router-codex-stt-contract"
    parts = []
    for name, value in fields:
        head = f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"'
        if isinstance(value, bytes):
            parts.append(head.encode() + b'; filename="input.mp3"\r\nContent-Type: audio/mpeg\r\n\r\n' + value + b"\r\n")
        else:
            parts.append((head + "\r\n\r\n" + value + "\r\n").encode())
    return b"".join(parts) + f"--{boundary}--\r\n".encode(), "multipart/form-data; boundary=" + boundary


def request(port, method, path, data, headers):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        conn.request(method, path, body=data, headers=headers)
        response = conn.getresponse()
        return response.status, response.headers, response.read()
    finally:
        conn.close()


def contract(binary):
    with tempfile.TemporaryDirectory(prefix="9router-codex-stt-contract-") as temporary:
        folder = Path(temporary)
        make_ffmpeg(folder)
        data = folder / "data"
        fallback = folder / "fallback"
        scratch = folder / "scratch"
        scratch.mkdir()
        subprocess.run([sys.executable, str(repository / "rust/reference/seed.py"), str(data)], check=True)
        db_file = data / "db" / "data.sqlite"
        with sqlite3.connect(db_file) as db:
            db.execute(
                "UPDATE providerConnections SET isActive=1, data=json_set(data,'$.providerSpecificData.baseUrl',?) WHERE id=?",
                ("http://127.0.0.1:52988/captured", "fixture-codex-primary"),
            )
            db.execute(
                "UPDATE providerConnections SET isActive=1, data=json_set(json_remove(data,"
                "'$.providerSpecificData.workspaceId'),'$.providerSpecificData.chatgptAccountId',?) "
                "WHERE id='fixture-codex-secondary'",
                ("fixture-workspace-secondary",),
            )
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]
        environment = {
            "PATH":str(folder / "bin"), "HOME":str(folder), "TMPDIR":str(scratch),
            "DATA_DIR":str(data), "PORT":str(port), "NINEROUTER_MIGRATION_FIXTURE":"1",
            "RUST_ANYTHING_FALLBACK_LOG":str(fallback),
        }
        child = subprocess.Popen([binary,"--no-browser"], cwd=repository, env=environment,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 18
            while True:
                if child.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Codex STT fixture gateway did not start")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.2):
                        break
                except OSError:
                    time.sleep(.04)

            def infer(audio, additions=(), path="/v1/audio/transcriptions"):
                data, content_type = multipart([("model","cx/transcribe"), ("file",audio), *additions])
                return request(port, "POST", path, data, {**inference, "content-type":content_type})

            status, headers, value = infer(b"fixture audio json")
            assert status == 200 and json.loads(value) == {"text":transcript}
            assert headers["cache-control"] == "no-store"
            assert infer(b"fixture audio text", [("response_format","text")], "/v1/v1/audio/transcriptions")[2].decode() == transcript
            count = len(uploads)
            status, _, value = infer(b"x" * (25 * 1024 * 1024 + 1))
            assert status == 413 and json.loads(value)["error"]["message"] == "Audio exceeds 25 MiB"
            assert len(uploads) == count
            status, _, value = infer(b"fixture error body")
            assert status == 422 and json.loads(value)["error"]["message"] == "Codex transcription failed"
            assert b"offline-codex" not in value
            status, _, value = infer(b"fixture retry")
            assert status == 200 and json.loads(value) == {"text":transcript}
            assert uploads[-2:] == [(0x88,"fixture-workspace-primary"),(0x88,"fixture-workspace-secondary")]
            for label, message in [
                (b"fixture oversized response", "Body exceeds size limit"),
                (b"fixture malformed response", "Invalid transcription response"),
                (b"fixture long transcript", "Invalid transcription response"),
                (b"fixture redirect", "Codex transcription failed"),
            ]:
                with sqlite3.connect(db_file) as db:
                    db.execute("UPDATE providerConnections SET data=json_remove(data,'$.modelLock_transcribe') WHERE provider='codex'")
                status, _, value = infer(label)
                assert status == 502 and message in json.loads(value)["error"]["message"], value
                assert b"offline-codex" not in value
            with sqlite3.connect(db_file) as db:
                db.execute("UPDATE providerConnections SET data=json_remove(data,'$.modelLock_transcribe') WHERE provider='codex'")
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                pending = [pool.submit(infer, b"fixture concurrent") for _ in range(2)]
                try:
                    deadline = time.monotonic() + 5
                    while sum(marker == 0x99 for marker, _ in uploads) < 2 and time.monotonic() < deadline:
                        time.sleep(.02)
                    assert sum(marker == 0x99 for marker, _ in uploads) == 2
                    status, _, value = infer(b"fixture concurrent")
                    assert status == 503 and b"Transcription capacity reached" in value
                    assert sum(marker == 0x99 for marker, _ in uploads) == 2
                finally:
                    release_concurrent.set()
                assert all(job.result()[0] == 200 for job in pending)
            assert not unauthorized_origins
            assert not list(scratch.glob("9router-codex-stt-*"))
            assert not fallback.exists() or not fallback.read_text().strip()
            print("Codex STT fake transport passed WAV, OAuth rotation, byte/character and concurrency limits, redirect isolation and cleanup")
        finally:
            child.terminate()
            try:
                child.wait(timeout=12)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == "__main__":
    if sys.argv[1] == "prepare":
        make_ffmpeg(Path(sys.argv[2]))
        sys.exit()

    servers = start_upstream()
    try:
        if sys.argv[1] == "serve":
            Path(sys.argv[2]).touch()
            threading.Event().wait()
        else:
            contract(str(Path(sys.argv[1]).resolve()))
    finally:
        for server in servers:
            server.shutdown()
            server.server_close()
