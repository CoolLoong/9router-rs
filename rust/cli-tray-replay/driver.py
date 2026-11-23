import json
import os
import pathlib
import subprocess
import sys
import tempfile

MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
if MODE == "native":
    binary, scenario = ARGS
    program = [binary]
else:
    scenario, = ARGS
    program = ["node", str(ROOT / "original.cjs")]

with tempfile.TemporaryDirectory(prefix="9router-tray-fixture-") as directory:
    home = pathlib.Path(directory)
    entry = home / ".config/autostart/9router.desktop"
    command = home / "xdg-open"
    command.write_text("#!/bin/sh\nprintf '%s\\n' \"$1\" > opened-dashboard\n")
    command.chmod(0o700)
    if scenario == "pre-enabled":
        entry.parent.mkdir(parents=True)
        entry.write_text("previous auto-start fixture\n")
    env = dict(os.environ, HOME=str(home), PATH=f"{home}:/usr/bin:/bin")
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    if scenario != "headless":
        env["DISPLAY"] = ":99"
    result = subprocess.run(
        ["dbus-run-session", "--", *program, scenario],
        env=env,
        cwd=home,
        text=True,
        capture_output=True,
        timeout=10,
    )
    if result.returncode:
        print(result.stdout, end="")
        print(result.stderr, end="", file=sys.stderr)
        raise SystemExit(result.returncode)
    line = next(line for line in reversed(result.stdout.splitlines()) if line.startswith("{"))
    snapshot = json.loads(line)
    if MODE == "native":
        message = "running in system tray" if snapshot["ready"] else "No tray icon — the server keeps running"
        if message not in result.stdout:
            raise RuntimeError(f"Missing tray status: {message}")
    if entry.is_file():
        settings = dict(value.split("=", 1) for value in entry.read_text().splitlines() if "=" in value)
        launcher = settings.pop("Exec")
        snapshot["autostart_file"] = {"options": launcher.split(" ")[-2:], **settings}
    else:
        snapshot["autostart_file"] = None
    print(json.dumps(snapshot, ensure_ascii=False, sort_keys=True))
