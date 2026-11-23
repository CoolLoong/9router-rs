import base64
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
native = Path(sys.argv[2]) if len(sys.argv) > 2 else root / "rust/app/target/debug/server"
speech = "/v1/audio/speech"
voices = "/v1/audio/voices?provider=local-device"
model = "local-device/Fixture Voice"
connection = "fixture-local-device-ignored"


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    with sqlite3.connect(data / "db/data.sqlite") as db:
        payload = {"testStatus":"unavailable", "modelLock_Fixture Voice":"2099-01-01T00:00:00Z",
                   "accessToken":"offline-local-device-ignore", "providerSpecificData":{"baseUrl":"https://example.invalid/ignored-account"}}
        db.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                      VALUES (?,'local-device','oauth','Ignored local speech',1,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                   (connection, json.dumps(payload)))


def make_path(root_path):
    tools = root_path / "engine"
    runner = root_path / "runner"
    tools.mkdir()
    runner.mkdir()
    for command in ("sh", "env", "mktemp", "dirname", "node", "python3"):
        path = shutil.which(command)
        assert path, command
        (runner / command).symlink_to(path)
    return tools, f"{tools}:{runner}"


def install(tools, name):
    command = tools / name
    shutil.copyfile(root / "rust/reference/local-device-tts-tools.py", command)
    command.chmod(0o700)


def request(base, path, body=None, key="offline-chat-key", extra=None):
    try:
        data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        headers = {"Content-Type":"application/json"} | (extra or {})
        if key:
            headers["Authorization"] = "Bearer " + key
        reply = urlopen(Request(base + path, data=data, headers=headers), timeout=5)
    except HTTPError as error:
        reply = error
    with reply:
        return reply.status, reply.headers.get("Content-Type"), reply.headers.get("Content-Length"), reply.read()


def ledger(data):
    file = data / "local-device-tts-ledger.jsonl"
    return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []


def fixture_output(voice, text):
    return b"fixture-mp3\x00" + f"{voice}:{text}".encode()


def exercise(base, data, tools):
    snapshots = {}

    def answer(name, path, body=None, status=200, key="offline-chat-key", extra=None):
        actual, mime, _, raw = request(base, path, body, key, extra)
        assert actual == status and mime == "application/json", (name, actual, mime, raw)
        value = json.loads(raw)
        snapshots[name] = value
        return value

    def payload(text, voice=model, **extra):
        return {"model":voice, "input":text, **extra}

    def record(name, before):
        calls = ledger(data)[before:]
        assert all(not Path(call["directory"]).exists() for call in calls if "directory" in call), calls
        snapshots[name] = [{key:value for key,value in call.items() if key != "directory"} for call in calls]
        return calls

    empty = answer("no-voice-exec", voices)
    assert empty == {"object":"list", "data":[]}
    absent = answer("no-say", speech, payload("Fixture missing say"), 502)
    assert absent["error"]["message"] == "spawn say ENOENT" and ledger(data) == []
    install(tools, "say")
    install(tools, "ffmpeg")
    published = answer("platform-voices", voices)
    assert [row["model"] for row in published["data"]] == ["local-device/Fixture Voice", "local-device/声 音"]
    assert ledger(data) == [{"executable":"say","operation":"voices"}]
    all_voices = answer("dashboard-local-voice", "/api/media-providers/tts/voices?provider=local-device&lang=zh",
                        extra={"x-9r-cli-token":"68977b6fd9422fbb"})
    assert all_voices["voices"] == [{"id":"声 音","name":"声 音","locale":"zh-CN","lang":"zh","country":"CN","countryName":"China",
                                     "langName":"Chinese","gender":""}]
    assert len(ledger(data)) == 1

    with sqlite3.connect(data / "db/data.sqlite") as database:
        before_data = database.execute("SELECT data,updatedAt FROM providerConnections WHERE id=?", (connection,)).fetchone()
    before = len(ledger(data))
    exact = answer("json-selection-and-trim", speech + "?response_format=json", payload("  Fixture café \n ", response_format="wav", language="ignored", style="ignored"))
    assert exact == {"audio":base64.b64encode(fixture_output("Fixture Voice", "Fixture café")).decode(),"format":"mp3"}
    default = request(base, speech, payload("Fixture default", "local-device/default", response_format="wav"))
    expected = fixture_output("default", "Fixture default")
    assert default[:2] == (200, "audio/mp3") and default[3] == expected, default
    assert default[2] is None or int(default[2]) == len(expected), default
    snapshots["default-binary"] = {"mime":default[1],"payload":base64.b64encode(default[3]).decode()}
    rendered = record("pinned-command-arguments", before)
    assert [call["executable"] for call in rendered] == ["say","ffmpeg","say","ffmpeg"]
    assert all(not rendered[index]["protected"] for index in (0, 2))

    before = len(ledger(data))
    blank = answer("blank-input", speech, payload(" \t\n"), 400)
    assert blank["error"]["message"] == "Missing required field: input"
    wrong = answer("restricted-api-key", speech, payload("Fixture never invoked"), 403, key="offline-restricted-key")
    assert "not allowed to use model" in wrong["error"]["message"]
    denied = answer("missing-api-key", speech, payload("Fixture never invoked"), 401, key=None)
    assert denied["error"]["message"] == "Missing API key" and len(ledger(data)) == before

    for stage, text, code in [("say","Fixture say failure",4),("ffmpeg","Fixture ffmpeg failure",17)]:
        before = len(ledger(data))
        status, mime, _, raw = request(base, speech, payload(text))
        assert status == 502 and mime == "application/json"
        failure = json.loads(raw)["error"]["message"]
        if implementation == "rust":
            assert failure == f"{stage} exited with status {code}" and text not in failure
        else:
            assert failure.startswith("Command failed: " + stage)
        calls = record(stage + "-failure", before)
        assert [call["executable"] for call in calls] == (["say"] if stage == "say" else ["say", "ffmpeg"])

    if implementation == "rust":
        before = len(ledger(data))
        result = request(base, speech, payload("- Fixture punctuation"))
        assert result[0] == 200 and result[3] == fixture_output("Fixture Voice", "- Fixture punctuation")
        calls = ledger(data)[before:]
        assert calls[0]["protected"] is True and all(not Path(call["directory"]).exists() for call in calls)

    (tools / "ffmpeg").unlink()
    prior = len(ledger(data))
    missing = answer("no-ffmpeg", speech, payload("Fixture missing ffmpeg"), 502)
    assert missing["error"]["message"] == "spawn ffmpeg ENOENT"
    assert [call["executable"] for call in record("cleanup-after-missing-encoder", prior)] == ["say"]
    with sqlite3.connect(data / "db/data.sqlite") as database:
        after_data = database.execute("SELECT data,updatedAt FROM providerConnections WHERE id=?", (connection,)).fetchone()
    assert after_data == before_data
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-local-device-tts-") as directory:
    data = Path(directory)
    seed(data)
    tools, path = make_path(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(native)]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data), PATH=path,
               NINEROUTER_MIGRATION_FIXTURE="1", MODEL_CATALOG_SYNC="off", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(base, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        snapshots = exercise(base, data, tools)
        if implementation == "rust":
            assert (data / "fallback.log").read_text() == ""
        print(json.dumps(snapshots, separators=(",", ":"), sort_keys=True))
    finally:
        try:
            os.killpg(server.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            server.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(server.pid, signal.SIGKILL)
            server.wait(timeout=2)
