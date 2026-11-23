import json
import os
import pathlib
import plistlib
import stat
import subprocess
import sys
import tempfile


FAKE_LAUNCHCTL = '''
import json
import os
import pathlib
import plistlib
import sys

state = pathlib.Path(os.environ["FIXTURE_JOB"])
trace = pathlib.Path(os.environ["FIXTURE_TRACE"])
mode = os.environ.get("FIXTURE_MODE", "normal")
operation, target, *rest = sys.argv[1:]
with trace.open("a") as stream:
    print(json.dumps(sys.argv[1:]), file=stream)
jobs = json.loads(state.read_text()) if state.exists() else {}
if operation == "bootstrap":
    assert target == f"gui/{os.geteuid()}" and len(rest) == 1
    plist = plistlib.loads(pathlib.Path(rest[0]).read_bytes())
    if mode == "bootstrap-wrong-domain":
        sys.exit(5)
    key = target + "/" + plist["Label"]
    assert key not in jobs
    jobs[key] = [rest[0], plist["Program"], 41000 + len(trace.read_text().splitlines()), mode == "early-exit"]
    state.write_text(json.dumps(jobs))
elif operation == "print":
    if mode == "unknown-lookup":
        print("permission denied", file=sys.stderr)
        sys.exit(1)
    if target not in jobs:
        print("Bad request.", file=sys.stderr)
        print(f'Could not find service "{target.rsplit("/", 1)[1]}" in domain for user gui: {os.geteuid()}', file=sys.stderr)
        sys.exit(113)
    path, program, pid, exited = jobs[target]
    if mode == "wrong-program":
        program += ".other"
    if mode == "wrong-path":
        path += ".other"
    print(target + " = {")
    print(f"\\tpath = {path}")
    print(f"\\tprogram = {program}")
    print("\\tenvironment = {")
    print("\\t\\tpid = 12345")
    print("\\t}")
    if mode == "duplicate-program":
        print(f"\\tprogram = {program}")
    if exited:
        print("\\tlast exit code = 42")
    else:
        print(f"\\tpid = {pid}")
    if mode != "truncated":
        print("}")
elif operation == "bootout":
    assert len(rest) == 0 and target in jobs
    if mode == "failed-bootout":
        sys.exit(5)
    del jobs[target]
    state.write_text(json.dumps(jobs))
else:
    sys.exit(1)
'''.lstrip()


def verify(probe):
    with tempfile.TemporaryDirectory(prefix="9r-mac-headroom-") as temp:
        scratch = pathlib.Path(temp).resolve()
        data = scratch / "isolated data & 中文"
        data.mkdir(mode=0o700)
        state = scratch / "jobs.json"
        trace = scratch / "trace.jsonl"
        launchctl = scratch / "launchctl"
        launchctl.write_text(f"#!{sys.executable}\n{FAKE_LAUNCHCTL}")
        launchctl.chmod(0o700)
        binary = scratch / "Headroom & package"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o700)
        record = data / "headroom" / "launchd" / "proxy.plist"
        legacy = data / "headroom" / "proxy.pid"
        env = dict(os.environ, FIXTURE_JOB=str(state), FIXTURE_TRACE=str(trace))

        def invoke(action, mode="normal", binary=binary, root=data, concurrent=False):
            args = [str(probe), str(launchctl), action, str(root), str(binary)]
            custom = dict(env, FIXTURE_MODE=mode)
            child = subprocess.Popen(args, env=custom, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            return child if concurrent else reply(child)

        def reply(child):
            stdout, stderr = child.communicate(timeout=12)
            assert child.returncode == 0, stderr
            return json.loads(stdout)

        def calls(operation):
            return [json.loads(line) for line in trace.read_text().splitlines() if json.loads(line)[0] == operation] if trace.exists() else []

        assert invoke("stop") == {"stopped": False, "reason": "not_running"}
        assert invoke("status") == {"managedPid": None}
        assert invoke("start", binary="-")["code"] == "NOT_INSTALLED"
        assert not calls("bootstrap")

        first = invoke("start")
        assert first["success"] and not first["alreadyRunning"]
        first_plist = plistlib.loads(record.read_bytes())
        label = first_plist["Label"]
        assert first_plist["ProgramArguments"] == [str(binary), "proxy", "--port", "18787", "--code-aware", "--disable-kompress"]
        assert first_plist["StandardOutPath"] == first_plist["StandardErrorPath"] == str(data / "headroom" / "proxy.log")
        assert first_plist["RunAtLoad"] is True and first_plist["KeepAlive"] is False
        assert stat.S_IMODE(record.stat().st_mode) == 0o600
        assert stat.S_IMODE(record.parent.stat().st_mode) == 0o700
        assert invoke("status") == {"managedPid": first["pid"]}
        assert invoke("start") == dict(first, alreadyRunning=True)
        assert invoke("restart", binary=scratch / "missing")["code"] == "SPAWN_FAILED"
        assert not calls("bootout")

        for mode in ("wrong-program", "wrong-path", "duplicate-program", "truncated", "unknown-lookup"):
            assert invoke("stop", mode)["code"] == "UNVERIFIED_PROCESS", mode
            assert invoke("status", mode) == {"managedPid": None}, mode
        assert not calls("bootout") and record.exists()
        assert invoke("restart", "failed-bootout")["code"] == "STOP_FAILED"
        assert invoke("status") == {"managedPid": first["pid"]}
        assert len(calls("bootstrap")) == 1

        other = scratch / "other app data"
        other.mkdir(mode=0o700)
        other_job = invoke("start", root=other)
        other_record = other / "headroom" / "launchd" / "proxy.plist"
        other_label = plistlib.loads(other_record.read_bytes())["Label"]
        assert label.split(".")[-2] != other_label.split(".")[-2]
        restarted = invoke("restart")
        assert restarted["pid"] != first["pid"] and not restarted["alreadyRunning"]
        assert plistlib.loads(record.read_bytes())["Label"] != label
        assert calls("bootout")[-1] == ["bootout", f"gui/{os.geteuid()}/{label}"]
        assert invoke("stop") == {"stopped": True, "pid": restarted["pid"]}
        assert invoke("status", root=other) == {"managedPid": other_job["pid"]}
        assert invoke("stop", root=other)["stopped"] and not other_record.exists()
        assert not record.exists() and json.loads(state.read_text()) == {}

        with subprocess.Popen(["/bin/sleep", "15"]) as unrelated:
            legacy.write_text(str(unrelated.pid))
            stops = calls("bootout")
            assert invoke("stop")["code"] == "UNVERIFIED_PROCESS"
            assert invoke("start")["code"] == "UNVERIFIED_PROCESS"
            assert unrelated.poll() is None and stops == calls("bootout")
            unrelated.terminate()
            unrelated.wait(timeout=3)
            assert invoke("stop") == {"stopped": False, "reason": "not_running"}
        legacy.unlink()

        failed = invoke("start", "bootstrap-wrong-domain")
        assert failed["code"] == "SPAWN_FAILED" and record.exists()
        assert invoke("status") == {"managedPid": None}
        failed = invoke("start", "early-exit")
        assert failed["code"] == "EARLY_EXIT" and "code=42" in failed["error"] and not record.exists()

        backup = scratch / "stable-file"
        backup.write_text("untouched")
        record.symlink_to(backup)
        assert invoke("start")["code"] == "STATE_FAILED"
        assert backup.read_text() == "untouched" and record.is_symlink()
        record.unlink()
        record.parent.chmod(0o775)
        assert invoke("start")["code"] == "STATE_FAILED"
        record.parent.chmod(0o700)

        before = len(calls("bootstrap"))
        one = invoke("start", concurrent=True)
        two = invoke("start", concurrent=True)
        duet = [reply(one), reply(two)]
        assert {item["alreadyRunning"] for item in duet} == {True, False}
        assert duet[0]["pid"] == duet[1]["pid"] and len(calls("bootstrap")) == before + 1
        assert invoke("stop")["stopped"] and json.loads(state.read_text()) == {}
        print("macOS Headroom local launchctl contract passed (lifecycle, adoption, ownership rejection, missing session, early exit, symlink, concurrency)")


if __name__ == "__main__":
    verify(pathlib.Path(sys.argv[1]).resolve())
