import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path, PurePosixPath
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import zipfile


def require(condition, description):
    if not condition:
        raise ValueError(description)


def extract(archive, directory):
    def safe_entries(names):
        paths = [PurePosixPath(name) for name in names]
        roots = {path.parts[0] for path in paths if path.parts}
        require(len(roots) == 1 and all(len(path.parts) >= 2 for path in paths)
                and all(".." not in path.parts and not path.is_absolute() for path in paths)
                and len(set(paths)) == len(paths)
                and all("\\" not in name and ":" not in name for name in names),
                "Unexpected archive paths")
        return directory / roots.pop()

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as package:
            entries = package.infolist()
            require(all((entry.external_attr >> 28) == 8 for entry in entries), "Archive contains a non-file member")
            root = safe_entries([entry.filename for entry in entries])
            package.extractall(directory)
    else:
        with tarfile.open(archive, mode="r:gz") as package:
            entries = package.getmembers()
            require(all(entry.isfile() for entry in entries), "Archive contains a non-file member")
            root = safe_entries([entry.name for entry in entries])
            package.extractall(directory, filter="data")
    return root


def request(port, path):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def check_gateway(command, directory, env):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]

    with (directory / "gateway.log").open("w+") as log:
        with subprocess.Popen([*command, "--host", "127.0.0.1", "--port", str(port), "--no-browser"],
                              cwd=directory, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log) as server:
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and server.poll() is None:
                    try:
                        status, body = request(port, "/api/health")
                    except OSError:
                        time.sleep(.05)
                        continue
                    if status == 200:
                        break
                    time.sleep(.05)
                else:
                    log.seek(0)
                    raise ValueError(f"Gateway never became healthy\n{log.read()}")
                health = json.loads(body)
                require(health.get("ok") is True, "Unexpected health payload")
                status, body = request(port, "/v1/models")
                require(status == 200 and isinstance(json.loads(body).get("data"), list),
                        "Native model list did not respond")
                status, body = request(port, "/api/oauth/trae/authorize")
                require(status == 503 and json.loads(body) == {"error": "Legacy runtime unavailable for this route"},
                        "Legacy route did not report its native-preview status")
            finally:
                if server.poll() is None:
                    server.send_signal(signal.SIGTERM)
                    try:
                        server.wait(timeout=6)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=3)
                        raise ValueError("Gateway did not shut down after SIGTERM")


def smoke(archive, runner=()):
    with tempfile.TemporaryDirectory(prefix="9router-native-smoke-") as temporary:
        directory = Path(temporary).resolve()
        package = extract(archive, directory)
        manifest = json.loads((package / "manifest.json").read_text(encoding="utf-8"))
        require(manifest["name"] == "9router" and manifest["kind"] == "native-preview"
                and manifest["legacyRuntimeIncluded"] is False, "Unexpected package manifest")
        executable = manifest["executable"]
        require(executable == ("9router.exe" if os.name == "nt" else "9router"), "Wrong executable for the current system")
        binary = package / executable
        command = [*runner, binary]
        with binary.open("rb") as stream:
            require(hashlib.file_digest(stream, "sha256").hexdigest() == manifest["sha256"], "Binary digest mismatch")
        for filename in ("LICENSE", "licenses/CLI-LICENSE", "licenses/embedded/UNIFONT_LICENSE.txt"):
            require((package / filename).stat().st_size > 0, f"Missing license: {filename}")
        notices = json.loads((package / "licenses/dependencies.json").read_text(encoding="utf-8"))
        require(any(notice["name"] == "axum" for notice in notices)
                and all(notice["files"] and all((package / name).is_file() for name in notice["files"]) for notice in notices),
                "Dependency notices are incomplete")

        home, empty, cwd = (directory / name for name in ("home", "empty-path", "work"))
        for path in (home, empty, cwd):
            path.mkdir()
        env = {"PATH": str(empty), "HOME": str(home), "USERPROFILE": str(home),
               "DATA_DIR": str(directory / "data"), "XDG_CONFIG_HOME": str(home / ".config"),
               "MODEL_CATALOG_SYNC": "off", "DISABLE_BACKGROUND_TOKEN_REFRESH": "1"}
        if os.name == "nt":
            env.update({key: os.environ[key] for key in ("SystemRoot", "TMP", "TEMP") if key in os.environ})

        def run(*options):
            return subprocess.check_output([*command, *options], cwd=cwd, env=env, timeout=8, text=True)

        version = manifest["version"]
        require(run("--version").strip() == version, "CLI and manifest version differ")
        require("Usage: 9router" in run("--help"), "CLI help did not run")
        require(isinstance(json.loads(run("show", "claude", "--json")), list), "Native CLI config inspection failed")
        check_gateway(command, cwd, env)
        print(f"9router {version} ({manifest['target']}) passed CLI, health and model list checks with an empty PATH.")
        print("Trae authorization returned HTTP 503 with the preview's legacy-runtime message.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Extract and smoke-test a locally built native 9router package")
    parser.add_argument("archive", type=Path)
    parser.add_argument("--runner", nargs=argparse.REMAINDER, default=(), help="Executable and arguments preceding the packaged binary")
    args = parser.parse_args()
    smoke(args.archive.resolve(), args.runner)
