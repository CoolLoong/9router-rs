import json
import os
from pathlib import Path
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
public = "/v1/audio/voices"
admin = "/api/media-providers/tts/"
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
remote_key = {"origin": "https://example.invalid", "authorization": "Bearer offline-chat-key"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    with sqlite3.connect(data / "db/data.sqlite") as database:
        accounts = [(provider, provider, 1) for provider in ("elevenlabs", "deepgram", "inworld", "minimax", "minimax-cn")]
        accounts.append(("elevenlabs", "elevenlabs-backup", 2))
        database.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey','Fixture voice',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(f"voice-{suffix}", provider, priority, json.dumps({"apiKey": f"offline-voices-{suffix}"}))
             for provider, suffix, priority in accounts],
        )
    fixture_bin = data / "fixture-bin"
    fixture_bin.mkdir()
    say = fixture_bin / "say"
    say.write_text("#!/bin/sh\n[ \"$1\" = -v ] && [ \"$2\" = '?' ] || exit 5\nprintf '%s\\n' 'Rae  en_US # fixture' '林 健  zh_CN # fixture' '    ignore  fr_FR' 'wrong x_y'\n")
    say.chmod(0o700)
    return fixture_bin


def request(base, path, headers=None):
    try:
        response = urlopen(Request(base + path, headers=headers or {}), timeout=5)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.headers.get("Access-Control-Allow-Origin"), json.load(response)


def set_key(database, provider, key):
    with sqlite3.connect(database) as db:
        db.execute("UPDATE providerConnections SET data = ? WHERE id = ?",
                   (json.dumps({"apiKey": key}), f"voice-{provider}"))


def run_cases(base, database):
    snapshots = {}

    def read(name, path, headers=None, status=200, cors=None):
        got_status, got_cors, value = request(base, path, headers)
        assert (got_status, got_cors) == (status, cors), (name, got_status, got_cors, value)
        assert "offline-voices-" not in json.dumps(value), name
        snapshots[name] = value
        return value

    guarded = read("public-paid-dashboard-gate", public + "?provider=elevenlabs", remote_key, 401, "*")
    assert guarded == {"error": {"message": "Unauthorized", "type": "server_error"}}
    read("admin-voice-catalog-requires-auth", admin + "elevenlabs/voices", status=401)

    raw = read("local-device-public", public + "?provider=local-device", cors="*")
    assert [row["id"] for row in raw["data"]] == ["Rae", "林 健"]
    assert raw["data"][1]["model"] == "local-device/林 健"
    local = read("local-device-dashboard", admin + "voices?provider=local-device&lang=zh", cli)
    assert local["voices"] == [{"id": "林 健", "name": "林 健", "locale": "zh-CN", "lang": "zh", "country": "CN",
                                "countryName": "China", "langName": "Chinese", "gender": ""}], local

    edge = read("edge-dashboard-default", admin + "voices", cli)
    assert edge["languages"][0]["code"] == "en" and edge["byLang"]["pt"]["voices"][0]["countryName"] == "Brazil"
    edge_public = read("edge-public-language-order", "/api/v1/audio/voices?provider=edge-tts", cors="*")
    assert [row["id"] for row in edge_public["data"]] == ["en-US-AliceNeural", "en-GB-QuinnNeural", "pt-BR-BrunoNeural"]
    assert edge_public["data"][0]["name"] == "Alice (English)"

    gemini = read("gemini-built-in-dashboard", admin + "voices?provider=gemini", cli)
    assert len(gemini["voices"]) == 30 and gemini["voices"][0]["id"] == "Zephyr"
    assert gemini["voices"][3]["name"] == "Kore" and gemini["voices"][3]["gender"] == "Female"

    elevenlabs = read("elevenlabs-dashboard", admin + "elevenlabs/voices", cli)
    assert [group["name"] for group in elevenlabs["languages"]] == ["American English", "English", "French", "Spanish"]
    assert len(elevenlabs["byLang"]["fr"]["voices"]) == 1
    assert elevenlabs["byLang"]["en"]["voices"][0]["free_users_allowed"] is True
    assert elevenlabs["byLang"]["es"]["voices"][0]["free_users_allowed"] is False
    explicit = read("elevenlabs-explicit-isolated-key", admin + "voices?provider=elevenlabs&apiKey=offline-voices-explicit&lang=en", cli)
    assert [row["id"] for row in explicit["voices"]] == ["explicit-el-primary", "explicit-el-alt"]
    saved = read("elevenlabs-saved-cache", admin + "elevenlabs/voices?lang=fr", cli)
    assert saved["voices"][0]["id"] == "saved-el-primary"
    set_key(database, "elevenlabs", None)
    absent = read("first-active-account-empty", admin + "elevenlabs/voices", cli, 400)
    assert absent["error"] == "No ElevenLabs connection found"
    set_key(database, "elevenlabs", "offline-voices-elevenlabs")

    deepgram = read("deepgram-dashboard-language-fallback", admin + "deepgram/voices?lang=es", cli)
    assert deepgram["voices"] == [{"id": "aura-regional-es", "name": "aura-regional-es", "gender": "masculine", "lang": "es"}]
    inworld = read("inworld-dashboard-deduplication", admin + "inworld/voices", cli)
    assert [row["id"] for row in inworld["byLang"]["en"]["voices"]] == ["fixture-oak", "fixture-fern"]
    assert len(inworld["byLang"]["fr"]["voices"]) == 1

    minimax = read("minimax-categories", admin + "minimax/voices", cli)
    assert [group["code"] for group in minimax["languages"]] == ["English", "Mandarin", "Custom"]
    assert [voice["name"] for voice in minimax["byLang"]["English"]["voices"]] == ["Ada", "Zelda"]
    assert [voice["name"] for voice in minimax["byLang"]["Custom"]["voices"]] == ["Augmented · Music", "Beta · Generated", "Studio · Cloned"]
    cn = read("minimax-cn-provider", admin + "minimax/voices?provider=minimax-cn&lang=Chinese&voice_type=system", cli)
    assert cn["voices"] == [{"id": "Chinese_Ada", "name": "Ada", "lang": "Chinese", "category": "system_voice"}]
    fault = read("minimax-api-status", admin + "minimax/voices?voice_type=fixture-error", cli, 502)
    assert fault == {"error": "synthetic voice error"}

    with sqlite3.connect(database) as database_handle:
        database_handle.execute("""INSERT INTO settings(id,data) VALUES(1,'{"requireLogin":false}')
                                ON CONFLICT(id) DO UPDATE SET data=json_set(data,'$.requireLogin',json('false'))""")
    listed = read("public-elevenlabs-ready-model", public + "?provider=elevenlabs&lang=fr", remote_key, cors="*")
    assert listed["data"][0]["model"] == "el/saved-el-primary" and listed["data"][0]["lang"] == "fr"
    dg = read("public-deepgram-ready-model", public + "?provider=deepgram&lang=en", cors="*")
    assert dg["data"][0]["model"] == "dg/aura-fixture-en"
    iw = read("public-inworld-ready-model", "/v1/v1/audio/voices?provider=inworld&lang=en", cors="*")
    assert iw["data"][1]["model"] == "inworld/fixture-fern"
    set_key(database, "deepgram", "offline-voices-deepgram-error")
    error = read("public-provider-status", public + "?provider=deepgram", status=502, cors="*")
    assert error["error"]["message"] == 'Deepgram API 503: {"error":"synthetic quota unavailable"}'
    if implementation == "rust":
        set_key(database, "deepgram", "offline-voices-deepgram-redaction")
        status, _, safe = request(base, admin + "deepgram/voices", cli)
        assert status == 502 and safe["error"] == 'Deepgram API 503: {"error":"synthetic credential [redacted]"}'
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-voices-contract-") as directory:
    data = Path(directory)
    fixture_bin = seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(root / "rust/app/target/debug/server")]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"),
               PATH=str(fixture_bin) + ":" + os.environ["PATH"])
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
        snapshots = run_cases(base, data / "db/data.sqlite")
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
