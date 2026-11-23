import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib

repository = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1]).resolve()
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
inference = {"x-api-key": "offline-chat-key"}
captures = []
model = "anthropic/claude-fable-5"
reference = repository / "rust/pxpipe-transform-replay/rust/app-cases/actual-fable-system.stdout"
fixtures = reference.parent
custom_fixture = json.loads((fixtures / "profile-flat-cost-allow.stdin").read_text())["actual"]
expected = json.loads(reference.read_text())
pixels = [
    item["source"]["data"]["pixelSha256"]
    for item in expected["body"]["messages"][0]["content"] if item["type"] == "image"
]
system = "Please read the fictional fixture before answering.\nUse the output named omega. " * 1500


def pixel_hash(image):
    raw = base64.b64decode(image)
    assert raw.startswith(b"\x89PNG\r\n\x1a\n")
    width, height = struct.unpack_from("!II", raw, 16)
    length = struct.unpack_from("!I", raw, 33)[0]
    encoded = zlib.decompress(raw[41:41 + length])
    channels = 3 if raw[25] == 2 else 1
    row_bytes = width * channels
    raster = bytearray(row_bytes * height)
    stride = row_bytes + 1
    for y in range(height):
        assert encoded[y * stride] == 3
        for x in range(row_bytes):
            index = y * row_bytes + x
            left = raster[index - channels] if x >= channels else 0
            above = raster[index - row_bytes] if y else 0
            raster[index] = (encoded[y * stride + x + 1] + ((left + above) // 2)) % 256
    return hashlib.sha256(raster).hexdigest()


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        assert self.path == "/anthropic/v1/messages"
        assert self.headers["x-api-key"] == "offline-pxpipe-anthropic"
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        captures.append(body)
        result = json.dumps({
            "id": "msg_pxpipe", "model": body["model"], "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "PXPIPE loopback OK"}],
            "stop_reason": "end_turn", "stop_sequence": None,
            "usage": {"input_tokens": 5, "output_tokens": 12},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(result)))
        self.end_headers()
        self.wfile.write(result)


def request(port, path, method="GET", payload=None, headers=cli):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        headers = dict(headers)
        if payload is not None:
            headers["content-type"] = "application/json"
            payload = json.dumps(payload, ensure_ascii=False).encode()
        client.request(method, path, payload, headers)
        response = client.getresponse()
        response_body = response.read()
        if response.headers.get("content-type", "").startswith("application/json"):
            response_body = json.loads(response_body)
        else:
            response_body = response_body.decode()
        return response.status, response_body
    finally:
        client.close()


def until(check, message, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.025)
    raise AssertionError(message)


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


with tempfile.TemporaryDirectory(prefix="9router-native-pxpipe-") as temporary:
    root = Path(temporary)
    commands = root / "path"
    commands.mkdir()
    home = root / "home"
    home.mkdir()
    data = root / "data"
    fallback = root / "fallbacks"
    subprocess.run([sys.executable, str(repository / "rust/reference/seed.py"), str(data)], check=True, cwd=repository)
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES ('offline-pxpipe-connection','anthropic','apikey','PXPIPE fixture',1,1,?,?,?)""",
            (json.dumps({"apiKey": "offline-pxpipe-anthropic"}), "2026-01-01T00:00:00.000Z", "2026-01-01T00:00:00.000Z"))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        "PATH": str(commands), "HOME": str(home), "PORT": str(port), "DATA_DIR": str(data),
        "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(fallback),
        "PXPIPE_MODELS": "claude-fable-5,claude-opus-5-5,gemini,gpt-5.6-sol,model-alpha",
        "PXPIPE_GPT_PROFILES": custom_fixture["overrides"],
    }
    mock = ThreadingHTTPServer(("127.0.0.1", 52989), Upstream)
    worker = threading.Thread(target=mock.serve_forever, daemon=True)
    worker.start()
    process = None
    try:
        def start():
            process = subprocess.Popen(
                [str(binary), "--no-browser"], cwd=repository, env=env,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            def ready():
                try:
                    return request(port, "/api/health", headers={})[0] == 200
                except OSError:
                    return False
            until(ready, "native PXPIPE fixture did not start")
            return process

        def admin(action, method="GET"):
            return request(port, f"/api/pxpipe/{action}", method)

        def chat(messages=None, tools=None, system_value=system, model_value=model):
            body = {
                "model": model_value, "max_tokens": 40, "stream": False,
                "messages": messages or [{"role": "user", "content": "fixture user input"}],
            }
            if system_value is not None:
                body["system"] = system_value
            if tools is not None:
                body["tools"] = tools
            status, value = request(port, "/v1/messages", "POST", body, inference)
            assert status == 200, value
            assert value["content"] == [{"type": "text", "text": "PXPIPE loopback OK"}]
            return captures[-1]

        def event(reason):
            def found():
                status, result = admin("logs?limit=1")
                assert status == 200
                return result["events"] and result["events"][0]["reason"] == reason
            until(found, f"missing native PXPIPE event {reason}")
            return admin("logs?limit=1")[1]["events"][0]

        process = start()
        status, script = request(port, "/__9router-ui/pxpipe.js", headers={})
        assert status == 200 and 'request("/api/pxpipe/" + action, "POST")' in script
        assert request(port, "/api/pxpipe/install", "POST", headers={})[0] == 401
        assert admin("status")[1]["installed"] is False
        assert request(port, "/api/settings", "PATCH", {
            "pxpipeEnabled": True, "pxpipeAutoInstall": False, "pxpipeMinChars": 20,
            "rtkEnabled": False,
            "enableObservability": True, "observabilityBatchSize": 1,
            "observabilityFlushIntervalMs": 50,
        })[0] == 200
        uncompressed = chat()
        assert uncompressed["system"] == system
        assert event("not_installed")["applied"] is False
        assert admin("start", "POST")[0] == 409

        installation_status, installation = admin("install", "POST")
        assert installation_status == 200 and installation["installed"] is True and installation["health"]["healthy"] is True
        assert (data / "pxpipe/native.json").is_file()
        assert not (data / "pxpipe/node_modules").exists()
        assert admin("start", "POST")[1]["running"] is True
        outgoing = chat()
        assert "system" not in outgoing
        blocks = outgoing["messages"][0]["content"]
        assert blocks[-2:] == [
            {"type": "text", "text": "[End of rendered context.]"},
            {"type": "text", "text": "fixture user input"},
        ]
        assert [pixel_hash(block["source"]["data"]) for block in blocks if block["type"] == "image"] == pixels
        applied = event("applied")
        assert applied["imageCount"] == len(pixels) and applied["imagedChars"] == len(system.rstrip())
        chars = len(json.dumps(uncompressed, ensure_ascii=False, separators=(",", ":")).encode("utf-16-le")) // 2
        image_pixels = sum(
            image["source"]["data"]["width"] * image["source"]["data"]["height"]
            for image in expected["body"]["messages"][0]["content"] if image["type"] == "image"
        )
        assert applied["originalChars"] == chars
        assert applied["tokensBeforeEst"] == (chars + 2) // 4
        assert applied["tokensAfterEst"] == (chars - applied["imagedChars"] + 2) // 4 + (image_pixels + 375) // 750
        assert applied["tokensSavedEst"] == applied["tokensBeforeEst"] - applied["tokensAfterEst"]
        def detail_recorded():
            status, records = request(port, "/api/usage/request-details?provider=anthropic")
            return status == 200 and any(
                detail.get("pxpipe", {}).get("tokensSavedEst") == applied["tokensSavedEst"]
                for detail in records["details"]
            )
        until(detail_recorded, "applied PXPIPE summary missing from request detail")

        documentation = "A fixture tool for data with a required description. " * 40
        with_tools = chat(tools=[{
            "name": "FixtureDoc", "description": documentation, "input_schema": {
                "type": "object", "description": "Root schema docs",
                "properties": {"description": {"type": "string", "description": "Field docs"}},
                "required": ["description"],
            },
        }])
        assert with_tools["tools"][0]["description"].startswith('ⓘ Full docs: see "## Tool: FixtureDoc"')
        assert with_tools["tools"][0]["input_schema"] == {
            "type": "object", "properties": {"description": {"type": "string"}},
            "required": ["description"],
        }
        assert with_tools["messages"][0]["content"][0]["type"] == "image"
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 2, "tool-doc event not recorded")

        tool_result = chat([
            {"role": "user", "content": "Check the synthetic result."},
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_px_fixture", "name": "FixtureDoc", "input": {},
            }]},
            {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_px_fixture", "content": [{
                    "type": "text", "text": "Fixture report example result value.\n" * 350,
                    "cache_control": {"type": "ephemeral", "ttl": "1h", "scope": "global"},
                }],
            }]},
        ])
        assert [message["role"] for message in tool_result["messages"]] == ["user", "assistant", "user"]
        result = tool_result["messages"][-1]["content"][0]
        assert result["tool_use_id"] == "toolu_px_fixture"
        images = [part for part in result["content"] if part["type"] == "image"]
        assert images, [(part["type"], len(part.get("text", ""))) for part in result["content"]]
        assert images[-1]["cache_control"] == {"type": "ephemeral", "ttl": "1h"}
        assert pixel_hash(images[-1]["source"]["data"])
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 3, "tool-result event not recorded")

        raw_history = json.loads((fixtures / "history-rgb.stdin").read_text())["actual"]
        original_history = json.loads((fixtures / "history-rgb.stdout").read_text())
        original_messages = original_history["body"]["messages"]
        transcript = raw_history["body"]["messages"]
        for turn in transcript:
            turn["content"] = turn["content"].replace("{result}", raw_history["resultText"] * raw_history["resultRepeat"])
        history = chat(messages=transcript, system_value=None)
        def text_turns(turns):
            return [
                (turn["role"], turn["content"] if isinstance(turn["content"], str)
                 else "".join(part["text"] for part in turn["content"]))
                for turn in turns
            ]
        assert text_turns(history["messages"][1:]) == text_turns(original_messages[1:])
        history_blocks = history["messages"][0]["content"]
        expected_blocks = original_messages[0]["content"]
        assert [part["type"] for part in history_blocks] == [part["type"] for part in expected_blocks]
        for actual, expected in zip(history_blocks, expected_blocks):
            if actual["type"] == "image":
                image_data = actual["source"]["data"]
                assert base64.b64decode(image_data)[25] == 2
                assert pixel_hash(image_data) == expected["source"]["data"]["pixelSha256"]
            else:
                assert actual == expected
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 4, "history event not recorded")

        assert admin("stop", "POST")[1]["stopped"] is True
        assert admin("status")[1]["running"] is False
        assert admin("restart", "POST")[1]["running"] is True
        conversation = [
            {"role": "user", "content": "@pxpipe pin a removable earlier directive"},
            {"role": "assistant", "content": "@pxpipe Added to the synthetic fixture."},
            {"role": "user", "content": (
                "Return the current answer.\n@pxpipe unpin all\n@pxpipe pin synthetic fixture directive"
            )},
        ]
        with_sections = system + (
            "\nx-anthropic-billing-header: synthetic-turn-tag-only\n"
            "# Environment\nWorking directory: /tmp/pxpipe-fixture\n"
        )
        pins = chat(conversation, system_value=with_sections)
        assert len(pins["messages"]) == 1
        assert pins["system"] == [{"type": "text", "text": "# Environment\nWorking directory: /tmp/pxpipe-fixture"}]
        pin_blocks = pins["messages"][0]["content"]
        assert pin_blocks[-2] == {"type": "text", "text": "Return the current answer."}
        assert "\n- synthetic fixture directive\n" in pin_blocks[-1]["text"]
        assert "a removable earlier directive" not in pin_blocks[-1]["text"]
        assert [pixel_hash(block["source"]["data"]) for block in pin_blocks if block["type"] == "image"] == pixels
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 5, "pin and environment event not recorded")

        recorded_model = json.loads((fixtures / "gemini-flat-slab.stdin").read_text())["actual"]
        expected_model = json.loads((fixtures / "gemini-flat-slab.stdout").read_text())
        gemini_system = recorded_model["body"]["system"] * recorded_model["repeat"]
        target = chat(system_value=gemini_system, model_value="anthropic/gemini-4-argon")
        assert target["model"] == "gemini-4-argon"
        gemini_pages = [
            pixel_hash(block["source"]["data"])
            for block in target["messages"][0]["content"] if block["type"] == "image"
        ]
        assert gemini_pages == [
            block["source"]["data"]["pixelSha256"]
            for block in expected_model["body"]["messages"][0]["content"] if block["type"] == "image"
        ]
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 6, "Gemini Messages event not recorded")

        profile_tests = [
            ("profile-gpt-56-sol", "gpt-5.6-sol-2026"),
            ("profile-flat-cost-allow", "model-alpha"),
        ]
        for fixture, model_id in profile_tests:
            source = json.loads((fixtures / f"{fixture}.stdin").read_text())["actual"]
            actual = chat(
                system_value=source["body"]["system"] * source["repeat"],
                model_value=f"anthropic/{model_id}",
            )
            assert actual["model"] == model_id
            expected = json.loads((fixtures / f"{fixture}.stdout").read_text())["body"]
            assert [
                pixel_hash(block["source"]["data"])
                for block in actual["messages"][0]["content"] if block["type"] == "image"
            ] == [
                block["source"]["data"]["pixelSha256"]
                for block in expected["messages"][0]["content"] if block["type"] == "image"
            ]
        until(lambda: admin("stats")[1]["windows"]["all"]["compressed"] == 8, "profile events not recorded")

        assert request(port, "/api/settings", "PATCH", {"pxpipeTimeoutMs": 1})[0] == 200
        assert chat()["system"] == system
        assert event("timeout")["applied"] is False
        stop(process)
        process = start()
        persisted = admin("status")[1]
        assert persisted["installed"] and not persisted["running"]
        assert admin("health", "POST")[1]["healthy"]
        status, metrics = admin("stats")
        assert status == 200 and metrics["windows"]["all"]["requests"] == 10
        assert metrics["windows"]["all"]["compressed"] == 8
        assert metrics["windows"]["all"]["errors"] == 1
        assert fallback.read_text() == ""
        print("Native PXPIPE lifecycle, exact loopback image pixels, passthrough and timeout verified without Node")
    finally:
        if process is not None:
            stop(process)
        mock.shutdown()
        mock.server_close()
