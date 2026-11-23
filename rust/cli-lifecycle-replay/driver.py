import contextlib
import fcntl
import json
import pathlib
import select
import shutil
import socket
import subprocess
import sys
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent
MODE, *args = sys.argv[1:]
if MODE in {"native", "safety"}:
    binary, case = args
    command = [binary]
else:
    case, = args
    command = ["node", str(ROOT / "original.cjs")]


@contextlib.contextmanager
def child(root, port=0, state="normal", host="127.0.0.1"):
    process = subprocess.Popen(command + [str(root), str(port), state, host], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        yield process
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2)


def ready(process):
    if not select.select([process.stdout], [], [], 7)[0]:
        raise AssertionError("launcher did not publish readiness")
    line = process.stdout.readline()
    if not line.startswith("ready "):
        _, error = process.communicate(timeout=2)
        raise AssertionError(f"launcher exited before readiness: {error!r}")
    return int(line.split()[1])


def available(port, host="127.0.0.1"):
    with socket.socket(socket.AF_INET6 if ":" in host else socket.AF_INET, socket.SOCK_STREAM) as probe:
        return probe.connect_ex((host, port)) != 0


def published(root, process, port):
    file = root / "launcher.pid"
    record = json.loads(file.read_text())
    assert record["cliPid"] == process.pid
    assert record["port"] == port
    assert record["exe"]
    if MODE != "original":
        assert file.stat().st_mode & 0o777 == 0o600
    return record


def reject(root, port, reason):
    with child(root, port) as process:
        out, err = process.communicate(timeout=8)
        assert process.returncode and not out, (out, err)
        assert reason in err, err


with tempfile.TemporaryDirectory(prefix="9router-launcher-fixture-") as work:
    work = pathlib.Path(work)
    home = work / "profile"
    home.mkdir()
    file = home / "launcher.pid"
    if case == "stale":
        file.write_text(json.dumps({"cliPid": 2147483647, "serverPid": None, "port": 20128, "exe": "not-running"}))
    if case in {"normal", "stale", "replacement", "new-port", "foreign-record-cleanup"}:
        with child(home) as first:
            port = ready(first)
            record = published(home, first, port)
            assert not available(port)
            if case in {"replacement", "new-port"}:
                next_port = port
                if case == "new-port":
                    with socket.socket() as reservation:
                        reservation.bind(("127.0.0.1", 0))
                        next_port = reservation.getsockname()[1]
                with child(home, next_port) as second:
                    replacement_port = ready(second)
                    old_status = first.wait(timeout=3)
                    if MODE != "original":
                        assert old_status == 0
                    published(home, second, replacement_port)
                    if case == "replacement":
                        assert replacement_port == port
                    else:
                        assert replacement_port != port
                        assert available(port)
                    assert not available(replacement_port)
                assert available(replacement_port)
            elif case == "foreign-record-cleanup":
                record["cliPid"] = 2147483647
                file.write_text(json.dumps(record))
        if case == "foreign-record-cleanup":
            assert json.loads(file.read_text()) == record
        else:
            assert not file.exists()
        assert available(port)
        print(json.dumps({"first_bound": True, "previous_stopped": case in {"replacement", "new-port"}, "preserved_replaced_pid_file": file.exists(), "ports_released": True}, sort_keys=True))
    elif case == "unrelated-port":
        with socket.socket() as foreign:
            foreign.bind(("127.0.0.1", 0))
            foreign.listen()
            port = foreign.getsockname()[1]
            reject(home, port, "Address already in use")
            assert not file.exists()
            assert not available(port)
    elif case in {"different-occupied-port", "address-mismatch"}:
        with child(home) as first, socket.socket() as foreign:
            old_port = ready(first)
            foreign.bind(("127.0.0.1", 0))
            foreign.listen()
            foreign_port = foreign.getsockname()[1]
            if case == "address-mismatch":
                record = published(home, first, old_port)
                record["port"] = foreign_port
                record["native"]["address"] = f"127.0.0.1:{foreign_port}"
                file.write_text(json.dumps(record))
            reject(home, foreign_port, "Address already in use")
            assert first.poll() is None
            assert not available(old_port)
            assert not available(foreign.getsockname()[1])
    elif case in {"start-time-mismatch", "lock-proof-mismatch", "unpublished", "stubborn"}:
        state = case if case in {"unpublished", "stubborn"} else "normal"
        with child(home, state=state) as first:
            port = ready(first)
            if case not in {"unpublished", "stubborn"}:
                record = published(home, first, port)
                if case == "start-time-mismatch":
                    record["native"]["startTime"] += 1
                else:
                    record["native"]["lockFd"] = record["native"]["listenerFd"]
                file.write_text(json.dumps(record))
            if case == "unpublished":
                assert not file.exists()
            reject(home, port, "did not shut down" if case == "stubborn" else "identity could not be verified")
            assert first.poll() is None
            assert not available(port)
    elif case == "startup-lock":
        with (home / "launcher.start.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with child(home) as waiting:
                assert not select.select([waiting.stdout], [], [], 0.2)[0]
                fcntl.flock(lock, fcntl.LOCK_UN)
                port = ready(waiting)
                published(home, waiting, port)
        assert not file.exists()
        assert available(port)
    elif case == "ipv6-replacement":
        with child(home, host="::1") as first:
            port = ready(first)
            assert not available(port, "::1")
            with child(home, port, host="::1") as second:
                assert ready(second) == port
                assert first.wait(timeout=3) == 0
                published(home, second, port)
        assert available(port, "::1")
        assert not file.exists()
    elif case == "executable-replaced":
        installed = work / "installed-launcher"
        replacement = work / "new-launcher"
        shutil.copy2(binary, installed)
        command[0] = str(installed)
        with child(home) as first:
            port = ready(first)
            published(home, first, port)
            shutil.copy2(binary, replacement)
            replacement.replace(installed)
            with child(home, port) as second:
                assert ready(second) == port
                assert first.wait(timeout=3) == 0
                published(home, second, port)
        assert not file.exists()
    elif case == "legacy-live":
        with child(work / "other") as other:
            port = ready(other)
            record = published(work / "other", other, port)
            legacy = {key: record[key] for key in ("cliPid", "serverPid", "port", "exe")}
            file.write_text(json.dumps(legacy))
            reject(home, 0, "upstream launcher still owns")
            assert other.poll() is None
            assert not available(port)
            assert json.loads(file.read_text()) == legacy
    elif case == "copied-identity":
        with child(home) as first, child(work / "other") as other:
            first_port = ready(first)
            other_port = ready(other)
            record = published(work / "other", other, other_port)
            file.write_text(json.dumps(record))
            reject(home, first_port, "identity could not be verified")
            assert first.poll() is None and other.poll() is None
            assert not available(first_port)
            assert not available(other_port)
    else:
        raise ValueError(case)
    if MODE == "safety":
        print(f"{case}: passed")
