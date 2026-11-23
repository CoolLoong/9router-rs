import json
import plistlib
import subprocess
import sys
import tempfile
from pathlib import Path

probe = Path(sys.argv[1]).resolve()
label = "com.9router.autostart"


with tempfile.TemporaryDirectory(prefix="9router-mac-login-") as directory:
    root = Path(directory)
    home = root / "Test user"
    home.mkdir()
    exe = root / 'installed "Router" & 安卓 <production>'
    exe.touch(mode=0o700)
    plist = home / "Library/LaunchAgents" / f"{label}.plist"
    launchctl = root / "fake LaunchCtl"
    commands = root / "calls"
    deny = root / "deny"
    launchctl.write_text(f"#!{sys.executable}\n" + f"""import json, sys
from pathlib import Path
with Path({str(commands)!r}).open("a") as ledger:
    ledger.write(json.dumps(sys.argv[1:]) + "\\n")
sys.exit(1 if Path({str(deny)!r}).exists() else 0)
""")
    launchctl.chmod(0o700)

    def call(action, result=True):
        invoked = subprocess.run([str(probe), action, str(home), str(launchctl), str(exe)],
                                 check=False, timeout=7, capture_output=True, text=True)
        assert (invoked.returncode == 0) == result, invoked.stderr
        return invoked.stdout.strip()

    def history():
        return [json.loads(line) for line in commands.read_text().splitlines()]

    assert call("status") == "false" and not plist.exists() and not commands.exists()
    assert call("enable") == "true"
    saved = plistlib.loads(plist.read_bytes())
    assert saved == {
        "Label": label, "ProgramArguments": [str(exe), "--tray", "--skip-update", "--no-browser"],
        "LimitLoadToSessionType": "Aqua", "RunAtLoad": True, "KeepAlive": False,
    }
    assert plist.stat().st_mode & 0o777 == 0o600
    assert history() == [["enable", f"gui/501/{label}"]]
    assert call("enable") == "true" and history() == [["enable", f"gui/501/{label}"]]
    assert call("toggle") == "false" and not plist.exists()
    assert history()[-1] == ["disable", f"gui/501/{label}"]

    deny.touch()
    call("toggle", result=False)
    assert not plist.exists() and call("status") == "false"
    deny.unlink()

    legacy = {"Label":label, "ProgramArguments":["/opt/Node bin/node","/opt/package/cli.js","--tray","--skip-update"],
              "RunAtLoad":True,"KeepAlive":False,"StandardOutPath":"/tmp/9router.log"}
    plist.write_bytes(plistlib.dumps(legacy))
    assert call("status") == "true"
    assert call("toggle") == "false" and not plist.exists()

    foreign = plistlib.dumps({"Label":label, "Program":"/opt/other", "ProgramArguments":["/opt/other","--unrelated"]})
    plist.write_bytes(foreign)
    count = len(history())
    call("toggle", result=False)
    assert plist.read_bytes() == foreign and len(history()) == count
    plist.unlink()
    other = root / "unrelated.plist"
    other.write_bytes(foreign)
    plist.symlink_to(other)
    call("toggle", result=False)
    assert plist.is_symlink() and other.read_bytes() == foreign and len(history()) == count
    assert history() == [[verb, f"gui/501/{label}"] for verb in ("enable","disable","enable","disable")]
    print("macOS LaunchAgent file lifecycle, state, Unicode arguments, rollback and bounded launchctl verbs verified.")
