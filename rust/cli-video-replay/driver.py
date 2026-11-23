import http.server
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import threading


MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE == "native":
    program, scenario = ARGS
    command = [program]
else:
    scenario, = ARGS
    command = ["node", str(ROOT / "original.cjs")]

API_KEY = "sk-synthetic-video-key"
OUTPUT = bytes.fromhex("00000018667479706d703432000000006d703432000000086d646174") + b"\x00\xfffixture\n"
ID = "job/fixture A"
CONNECTION = "fixture-affinity"
LOG = []
POLL = 0
DOWNLOAD_ENTERED = threading.Event()
DOWNLOAD_RELEASE = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, data, headers=None, announced=None):
        self.send_response(status)
        self.send_header("Content-Length", str(len(data) if announced is None else announced))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if data:
            self.wfile.write(data)
            self.wfile.flush()

    def json(self, status, body, headers=None):
        self.send(status, json.dumps(body).encode(), {"Content-Type": "application/json", **(headers or {})})

    def do_POST(self):
        content = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        LOG.append({
            "path": self.path,
            "auth": self.headers.get("Authorization") == f"Bearer {API_KEY}",
            "accept": self.headers.get("Accept"),
            "body": json.loads(content),
        })
        if scenario == "missing-credentials":
            return self.json(400, {"error": {"message": "No credentials for xAI"}})
        if scenario == "unauthorized-create":
            return self.send(401, f"Authorization invalid: bEaReR \t{API_KEY}".encode())
        if scenario == "no-job":
            return self.json(200, {"state": "no id"})
        return self.json(200, {"request_id": ID}, {"X-9Router-Connection-Id": CONNECTION})

    def do_GET(self):
        global POLL
        if self.path.startswith("/v1/videos/"):
            LOG.append({
                "path": self.path,
                "auth": self.headers.get("Authorization") == f"Bearer {API_KEY}",
                "affinity": self.headers.get("x-connection-id"),
                "accept": self.headers.get("Accept"),
            })
            POLL += 1
            if scenario == "poll-blocked":
                return self.send(403, f"Denied with Bearer {API_KEY}".encode())
            if scenario == "job-failed":
                return self.json(200, {"status": "FAILED", "progress": 31.5, "error": {"message": f"Bad request Bearer {API_KEY}"}})
            if scenario == "no-video":
                return self.json(200, {"status": "done", "video": {}})
            if scenario == "retry-and-redirect" and POLL <= 4:
                if POLL == 1:
                    return self.send(429, b"back off")
                if POLL == 2:
                    return self.send(503, b"retry")
                return self.json(200, {"status": "running", "progress": 14})
            link = f"http://127.0.0.1:{self.server.server_port}/cdn/start"
            if scenario == "retry-and-redirect":
                return self.json(200, {"status": "COMPLETED", "video": {"file_output": {"public_url": link}}})
            return self.json(200, {"status": "done", "progress": 100, "video": {"url": link}})
        LOG.append({"path": self.path, "download_auth": self.headers.get("Authorization") is not None})
        if scenario == "download-error":
            return self.send(503, b"cdn failure")
        if scenario == "redirect-limit":
            index = self.path.rsplit("/", 1)[1]
            step = int(index) + 1 if index.isdecimal() else 1
            return self.send(302, b"", {"Location": f"/cdn/{step}"})
        if scenario == "truncated-cdn":
            return self.send(200, OUTPUT[:7], announced=len(OUTPUT))
        if scenario == "cancel-cdn":
            self.send(200, OUTPUT[:7], announced=len(OUTPUT))
            DOWNLOAD_ENTERED.set()
            DOWNLOAD_RELEASE.wait(6)
            return
        if scenario == "retry-and-redirect" and self.path == "/cdn/start":
            return self.send(307, b"", {"Location": "target"})
        self.send(200, OUTPUT, {"Content-Type": "video/mp4"})


with tempfile.TemporaryDirectory(prefix="9router-video-fixture-") as work:
    cwd = pathlib.Path(work)
    is_old = scenario in {"replace-existing", "truncated-cdn", "cancel-cdn"}
    if is_old:
        (cwd / "sample.mp4").write_bytes(b"old file")
    extra = []
    if scenario == "local-png":
        (cwd / "image.JPG.PNG").write_bytes(b"\x89PNG\x00\xff")
        extra = ["--image", "image.JPG.PNG", "--duration", "007seconds", "--aspect-ratio", "16:9", "--resolution", "720p"]
    if scenario == "local-webp":
        (cwd / "image.webp").write_bytes(bytes.fromhex("52494646ff57454250"))
        extra = ["--image", "image.webp"]
    if scenario == "url-image":
        extra = ["--image", "https://fixture.invalid/never-contacted.webp"]
    if scenario == "data-image":
        extra = ["--image", "data:image/jpeg;base64,AAECAw=="]
    if scenario == "timeout":
        extra = ["--timeout", "-1"]

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        args = ["--prompt", "synthetic only", "--port", str(server.server_port), "-o", "sample.mp4", "--poll-interval-ms", "1", *extra]
        env = os.environ.copy()
        if scenario == "env-auth":
            env["NINE_ROUTER_API_KEY"] = API_KEY
        else:
            args += ["--api-key", API_KEY]
        child = subprocess.Popen(command + args, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            if scenario == "cancel-cdn":
                if not DOWNLOAD_ENTERED.wait(5):
                    raise RuntimeError("mock download did not start")
                child.send_signal(signal.SIGINT)
            stdout, stderr = child.communicate(timeout=6)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            DOWNLOAD_RELEASE.set()
    finally:
        server.shutdown()
        server.server_close()

    sys.stdout.write(stdout)
    sys.stderr.write(stderr)
    output = cwd / "sample.mp4"
    print(json.dumps({
        "requests": LOG,
        "output": output.read_bytes().hex() if output.exists() else None,
        "residual_parts": sorted(path.name for path in cwd.glob("*.part")),
    }, ensure_ascii=False, separators=(",", ":")))
    raise SystemExit(child.returncode)
