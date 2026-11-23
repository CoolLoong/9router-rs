import base64
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
from threading import Thread
import time

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
secrets = ("offline-request-header", "offline-request-query", "offline-path-credential", "offline-request-body")
modes = [("startup", []), ("logs", ["--log"]), ("skip", ["--skip-update", "-l"])]


class SignedFeed(BaseHTTPRequestHandler):
    events = []
    envelope = b""

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.events.append(self.path)
        if self.path != "/manifest":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(self.envelope)))
        self.end_headers()
        self.wfile.write(self.envelope)


def b64(raw):
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def configure_feed(directory):
    key = directory / "offline-feed.ed25519"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", key], check=True)
    certificate = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
    target = subprocess.check_output(["rustc","-vV"], text=True).split("host: ",1)[1].splitlines()[0]
    payload = directory / "signed-payload.json"
    payload.write_text(json.dumps({
        "schema":"9router-native-update/v1","channel":"preview","version":"90.0.1",
        "target":target,"artifact":"/fixture-payload","expiresAt":int(time.time()) + 600,
        "size":1,"sha256":"00" * 32,
    }, separators=(",",":")))
    signature = subprocess.check_output(["openssl","pkeyutl","-sign","-rawin","-inkey",key,"-in",payload])
    SignedFeed.envelope = json.dumps({"payload":b64(payload.read_bytes()),"signature":b64(signature)}).encode()
    feed = ThreadingHTTPServer(("127.0.0.1",0), SignedFeed)
    Thread(target=feed.serve_forever, daemon=True).start()
    return feed, {
        "NINEROUTER_NATIVE_UPDATE_MANIFEST":f"http://127.0.0.1:{feed.server_port}/manifest",
        "NINEROUTER_NATIVE_UPDATE_VERIFY_KEY":b64(certificate[-32:]),
        "NINEROUTER_MIGRATION_FIXTURE":"1",
    }


def prepare_reference(directory):
    cli = directory / "cli"
    shutil.copytree(root / "cli", cli, ignore=shutil.ignore_patterns("app","node_modules",".git"))
    (cli / "app").mkdir()
    (cli / "app/server.js").write_text("""
const http = require("node:http");
const server = http.createServer((req, res) => {
  if (req.url.startsWith("/api/health")) {
    console.log("fixture server reports a successful health request");
    res.writeHead(200, {"content-type":"application/json"});
    res.end('{"status":"ok"}');
  } else {
    res.writeHead(401, {"content-type":"application/json"});
    res.end('{"error":"Unauthorized"}');
  }
});
server.listen(Number(process.env.PORT), "127.0.0.1");
""")
    preload = directory / "fixture-cli.cjs"
    preload.write_text("""
const fs = require("node:fs");
const https = require("node:https");
const Module = require("node:module");
const { EventEmitter } = require("node:events");
const load = Module._load;
Module._load = function(name, ...args) {
  if (name === "./hooks/sqliteRuntime") return {ensureSqliteRuntime() {}, buildEnvWithRuntime: env => env};
  if (name === "./hooks/trayRuntime") return {ensureTrayRuntime() {}};
  if (name.includes("systray")) throw new Error("No fixture tray");
  return load.call(this, name, ...args);
};
https.get = (url, opts, callback) => {
  if (new URL(url).hostname !== "registry.npmjs.org") throw new Error("Unexpected fixture registry request");
  fs.appendFileSync(process.env.CLI_FIXTURE_EVENTS, "update\\n");
  const request = new EventEmitter();
  request.destroy = () => {};
  process.nextTick(() => {
    const response = new EventEmitter();
    callback(response);
    response.emit("data", '{"version":"90.0.1"}');
    response.emit("end");
  });
  return request;
};
""")
    return [shutil.which("node"), str(cli / "cli.js")], {"NODE_OPTIONS":f"--require={preload}"}


def request(port, path, method="GET", body=None):
    headers = {"Authorization":"Bearer " + secrets[0], "Content-Type":"application/json"}
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        conn.request(method, path, headers=headers, body=body)
        response = conn.getresponse()
        return response.status, json.loads(response.read())
    finally:
        conn.close()


def wait_until(test, timeout=6):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(.035)
    assert test(), "fixture condition timed out"


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=2)


def run_mode(workspace, command, extra, name, args):
    directory = workspace / name
    home = directory / "home"
    home.mkdir(parents=True)
    data = directory / "data"
    events = directory / "events"
    events.touch()
    empty_bin = directory / "empty-bin"
    empty_bin.mkdir()
    log_path = directory / "terminal.txt"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {
        "HOME":str(home),"PATH":os.environ["PATH"],"DATA_DIR":str(data),
        "CLI_FIXTURE_EVENTS":str(events),"MODEL_CATALOG_SYNC":"off",
        "NEXT_TELEMETRY_DISABLED":"1","NODE_NO_WARNINGS":"1",
    } | extra
    prior = len(SignedFeed.events)
    if implementation == "rust":
        env["PATH"] = str(empty_bin)
    with log_path.open("w") as output:
        proc = subprocess.Popen([
            *command, "--no-browser", "--tray", "--host", "127.0.0.1",
            "--port", str(port), *args,
        ], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=output, start_new_session=True)
        try:
            def ready():
                assert proc.poll() is None, proc.returncode
                try:
                    return request(port, f"/api/health?canary={secrets[1]}")[0] == 200
                except OSError:
                    return False
            wait_until(ready, timeout=10)
            skip = name == "skip"
            show = name != "startup"
            manifest_count = lambda: (len(SignedFeed.events) - prior if implementation == "rust"
                                      else len(events.read_text().splitlines()))
            if not skip:
                wait_until(lambda: manifest_count() == 1)
            if implementation == "rust":
                assert request(port, f"/api/keys/{secrets[2]}?canary={secrets[1]}", "DELETE")[0] == 401
                status, _ = request(port, "/v1/chat/completions", "POST", json.dumps({"secret":secrets[3]}))
                assert status in {400, 401}
                if not skip:
                    wait_until(lambda: "Signed preview update available: v90.0.1" in log_path.read_text())
            marker = "GET /api/health 200" if implementation == "rust" else "fixture server reports a successful health request"
            if show:
                wait_until(lambda: marker in log_path.read_text())
            else:
                time.sleep(.11)
            observed = log_path.read_text()
            assert (marker in observed) is show, observed
            assert manifest_count() == (0 if skip else 1), observed
            if implementation == "rust":
                assert all(secret not in observed for secret in secrets), observed
                assert ("DELETE /api/keys/{id} 401" in observed) is show, observed
                assert ("Signed preview update available: v90.0.1" in observed) is not skip, observed
                status, value = request(port, "/api/version")
                assert status == 200 and value["latestVersion"] == "90.0.1" and value["hasUpdate"] is True, value
                assert manifest_count() == 1
                status, _ = request(port, "/api/version/update", "POST")
                assert status == 401 and manifest_count() == 1
                assert all(path == "/manifest" for path in SignedFeed.events)
            return {"terminalRequestLogs":show,"startupDiscovery":not skip}
        except Exception:
            print(log_path.read_text(), file=sys.stderr)
            raise
        finally:
            stop(proc)


with tempfile.TemporaryDirectory(prefix="9router-cli-flags-") as directory:
    workspace = Path(directory)
    feed = None
    if implementation == "rust":
        feed, extra = configure_feed(workspace)
        command = [str(binary)]
    else:
        command, extra = prepare_reference(workspace)
    try:
        output = {name:run_mode(workspace, command, extra, name, args) for name, args in modes}
        print(json.dumps(output, sort_keys=True, separators=(",",":")))
    finally:
        if feed is not None:
            feed.shutdown()
            feed.server_close()
