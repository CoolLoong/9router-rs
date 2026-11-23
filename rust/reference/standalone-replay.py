import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve(strict=True)


def stop(process):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def interrupted(_signal, _frame):
    raise KeyboardInterrupt


def ready(mock):
    deadline = time.monotonic() + 6
    while mock.poll() is None and time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", 52989), timeout=0.2):
                return
        except OSError:
            time.sleep(0.025)
    raise RuntimeError("isolated upstream fixture did not become ready")


signal.signal(signal.SIGTERM, interrupted)
with tempfile.TemporaryDirectory(prefix="9router-standalone-replay-") as directory:
    stage = Path(directory)
    data = stage / "data"
    tools = stage / "empty-path"
    tools.mkdir()
    fixture_environment = {"PATH": os.environ["PATH"], "HOME": directory, "DATA_DIR": str(data)}
    subprocess.run([sys.executable, str(root / "rust/reference/seed.py"), str(data)],
                   cwd=root, env=fixture_environment, check=True)
    environment = {
        "PATH": str(tools), "HOME": directory, "DATA_DIR": str(data), "PORT": os.environ["PORT"],
        "NINEROUTER_MIGRATION_FIXTURE": "1", "MODEL_CATALOG_SYNC": "off",
    }
    if fallback_log := os.environ.get("RUST_ANYTHING_FALLBACK_LOG"):
        environment["RUST_ANYTHING_FALLBACK_LOG"] = str(Path(fallback_log).absolute())
    mock = subprocess.Popen(["node", "--no-warnings", "--import", str(root / "rust/reference/mock-models.mjs"),
                             "--eval", "setInterval(() => {}, 60000)"],
                            cwd=root, env=fixture_environment, stdin=subprocess.DEVNULL)
    app = None
    try:
        ready(mock)
        app = subprocess.Popen([binary, "--no-browser"], cwd=stage, env=environment, stdin=subprocess.DEVNULL)
        sys.exit(app.wait())
    except KeyboardInterrupt:
        pass
    finally:
        stop(app)
        stop(mock)
