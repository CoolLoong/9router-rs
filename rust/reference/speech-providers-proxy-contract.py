from base64 import b64decode
from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "proxy_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py"),
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

PROVIDERS = {
    "elevenlabs": ("api.elevenlabs.io", "/v1/text-to-speech/FixtureVoice",
                   "eleven_turbo_v2_5/FixtureVoice", "ElevenLabs fixture", "mp3",
                   b"ElevenLabs fixture bytes " * 50),
    "inworld": ("api.inworld.ai", "/tts/v1/voice", "inworld-tts-1.5-max/Alex", "Inworld fixture",
                "mp3", b"\xfb\xff"),
    "minimax": ("api.minimax.io", "/v1/t2a_v2", "speech-2.8-hd/fixture-narrator", "MiniMax global fixture",
                "wav", b"Fixture MiniMax wav bytes"),
    "minimax-cn": ("api.minimaxi.com", "/v1/t2a_v2", "speech-2.8-turbo", "MiniMax CN fixture",
                   "mp3", b"Fixture MiniMax CN bytes"),
}
HOSTS = {config[0]: provider for provider, config in PROVIDERS.items()}
KEYS = {provider: f"offline-{provider}-speech" for provider in PROVIDERS}
RELAY = "https://speech-providers.offline-lane.workers.dev"
RELAY_PATH = "/cloudflare-relay/speech-providers.offline-lane.workers.dev"
TOKEN = "K" * 43
MODE = None
CALLS, ERRORS = [], []
shared.SECRETS = (*KEYS.values(), TOKEN)


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, raw, mime="application/json", location=None):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(raw)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        ERRORS.append(self.path)
        self.reply(422, b"{}")

    def do_POST(self):
        try:
            original = urlsplit(self.path)
            relay = original.path == RELAY_PATH
            if relay:
                origin = self.headers["x-relay-target"]
                provider = HOSTS[urlsplit(origin).hostname]
                host, path, _, _, _, _ = PROVIDERS[provider]
                assert origin == "https://" + host
                assert self.headers.get_all("x-relay-auth") == [TOKEN]
                assert self.headers.get_all("x-relay-path") == [path]
                lane = "relay"
            elif self.server.server_port == 52989:
                provider = original.path.split("/")[1]
                assert original.path == "/" + provider + PROVIDERS[provider][1]
                lane = "direct"
            else:
                provider = HOSTS[self.headers["Host"]]
                assert original.path == PROVIDERS[provider][1]
                lane = "tunnel"
            assert not original.query
            assert self.headers["Content-Type"] == "application/json"
            assert not any(self.headers[name] for name in ("Proxy-Authorization", "Cookie", "X-Internal-Fixture"))
            if not relay:
                assert not any(self.headers[name] for name in ("x-relay-target", "x-relay-path", "x-relay-auth"))
            actual = self.headers.get_all("Authorization")
            if provider == "elevenlabs":
                assert actual is None and self.headers.get_all("xi-api-key") == [KEYS[provider]]
                auth = {"xi-api-key": KEYS[provider]}
            else:
                scheme = "Basic" if provider == "inworld" else "Bearer"
                assert actual == [f"{scheme} {KEYS[provider]}"] and self.headers["xi-api-key"] is None
                auth = {"Authorization": actual[0]}
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            CALLS.append((provider, lane, json.loads(raw)))
            if MODE == "redirect":
                return self.reply(307, b"{}", location="http://127.0.0.1:52989/capture")
            if MODE == "error":
                message = f"Offline rejection {KEYS[provider]} {TOKEN}"
                if provider == "elevenlabs":
                    return self.reply(403, json.dumps({"detail": {"message": message}}).encode())
                if provider == "inworld":
                    return self.reply(503, json.dumps({"error": {"message": message}}).encode())
                return self.reply(200, json.dumps({"base_resp": {"status_code": 1041, "status_msg": message}}).encode())
            expected_path = "/" + provider + PROVIDERS[provider][1]
            with closing(http.client.HTTPConnection("127.0.0.1", 52988, timeout=4)) as mock:
                mock.request("POST", expected_path, raw, {"Content-Type": "application/json", **auth})
                response = mock.getresponse()
                answer = response.read()
                assert response.status == 200, answer
                self.reply(200, answer, response.getheader("Content-Type"))
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            ERRORS.append((urlsplit(self.path).path, repr(error)))
            self.reply(422, b"{}")


def account(db, provider, details, backup=False):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Offline speech',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider + ("-backup" if backup else "-primary"), provider, 2 if backup else 1,
                     json.dumps({"apiKey": "unused-backup" if backup else KEYS[provider], "providerSpecificData": details})))


def call(port, provider, fmt="json", model=None):
    _, _, suffix, text, _, _ = PROVIDERS[provider]
    payload = {"model": model or provider + "/" + suffix, "input": f"  {text}  ",
               "response_format": "ignored", "baseUrl": "http://127.0.0.1:52989/capture"}
    headers = {"Authorization": "Bearer offline-chat-key", "Content-Type": "application/json",
               "X-Internal-Fixture": "unsafe request header", "x-relay-auth": "override"}
    with closing(http.client.HTTPConnection("127.0.0.1", port, timeout=7)) as conn:
        conn.request("POST", "/v1/audio/speech?response_format=" + fmt, json.dumps(payload), headers)
        response = conn.getresponse()
        raw = response.read()
        assert all(secret.encode() not in raw for secret in shared.SECRETS)
        return response.status, response.getheader("Content-Type"), response.getheader("Content-Length"), raw


def expect_audio(port, provider):
    _, _, _, _, fmt, audio = PROVIDERS[provider]
    assert call(port, provider, fmt) == (200, "audio/" + fmt, str(len(audio)), audio)
    status, mime, _, raw = call(port, provider)
    assert (status, mime) == (200, "application/json")
    data = json.loads(raw)
    assert data["format"] == fmt
    if provider == "inworld":
        assert data["audio"] == " \n-_8"
    else:
        assert b64decode(data["audio"]) == audio


def verify(port, db, group, connect, http_proxy):
    global MODE
    for provider in group:
        account(db, provider, {})
        account(db, provider, {}, True)
        expect_audio(port, provider)
        origin_body = CALLS[-1][2]
        host, _, _, _, fmt, audio = PROVIDERS[provider]
        for flags, lane in ((http_proxy, "tunnel"), ({"proxyPoolId": "speech-socks"}, "tunnel"),
                            ({"proxyPoolId": "speech-managed"}, "relay"),
                            ({**http_proxy, "connectionNoProxy": host}, "direct")):
            account(db, provider, flags)
            expect_audio(port, provider)
            assert CALLS[-2:] == [(provider, lane, origin_body)] * 2
        assert shared.TUNNELS[-4:] == [("connect", host, 443)] * 2 + [("socks", host, 443)] * 2

        account(db, provider, {"proxyPoolId": "speech-managed"})
        other = group[0] if provider == group[1] else group[1]
        model = "protected-speech-" + provider
        models = [name + "/" + PROVIDERS[name][2] for name in (provider, other)]
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                VALUES (?,?,'tts',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""", (model, model, json.dumps(models)))
        for mode, message in (("redirect", b"redirect refused"), ("error", b"[redacted]")):
            MODE = mode
            before = len(CALLS)
            status, _, _, raw = call(port, provider, model=model)
            assert status == 502 and message in raw and CALLS[before:] == [(provider, "relay", origin_body)]
        MODE = None

        before = len(CALLS), len(shared.TUNNELS)
        if provider == "elevenlabs":
            blocked = call(port, provider, model="elevenlabs/eleven_turbo_v2_5/..")
            assert blocked[0] == 503 and b"does not support managed Relay" in blocked[-1]
        account(db, provider, {"proxyPoolId": "speech-unmanaged"})
        assert call(port, provider)[0] == 503
        account(db, provider, {"proxyPoolId": "speech-socks"})
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','*') WHERE id='speech-socks'")
        assert call(port, provider)[0] == 503
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_remove(data,'$.noProxy') WHERE id='speech-socks'")
        assert (len(CALLS), len(shared.TUNNELS)) == before

        account(db, provider, http_proxy)
        connect.reject = True
        try:
            assert call(port, provider, model=model)[0] == 502
        finally:
            connect.reject = False
        assert (len(CALLS), len(shared.TUNNELS)) == (before[0], before[1] + 1)
        assert shared.TUNNELS[-1] == ("connect", host, 443)
        with sqlite3.connect(db) as sql:
            stored, = sql.execute("SELECT data FROM providerConnections WHERE id=?", (provider + "-primary",)).fetchone()
            assert not json.loads(stored).get("lastError")
    assert not ERRORS and not shared.ERRORS, (ERRORS, shared.ERRORS)
    assert all(host in [PROVIDERS[name][0] for name in group] for host in shared.HELLOS)


with tempfile.TemporaryDirectory(prefix="9router-speech-vendor-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    shared.ROOT = Path(__file__).resolve().parents[2]
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), Vendor)))
    node = subprocess.Popen(["node", "--input-type=module", "-e", """
      import {createServer} from 'node:http';
      import {handleSpeechProviderMock} from './rust/reference/mock-speech-providers.mjs';
      createServer(handleSpeechProviderMock).listen(52988, '127.0.0.1', () => console.log('ready'));
    """], cwd=shared.ROOT, env={"HOME": temporary, "PATH": os.environ["PATH"], "NODE_NO_WARNINGS": "1"},
                            stdout=subprocess.PIPE, text=True)
    try:
        assert select.select([node.stdout], [], [], 5)[0] and node.stdout.readline().strip() == "ready"
        for group in (("elevenlabs", "inworld"), ("minimax", "minimax-cn")):
            with ExitStack() as services:
                shared.HELLOS.clear()
                shared.ZED_HOST, shared.CLINE_HOST = (PROVIDERS[name][0] for name in group)
                workspace = scratch / group[0]
                workspace.mkdir()
                ca, target, _ = shared.tls_services(services, workspace, Vendor)
                connect, socks = [shared.Proxy(("127.0.0.1", 0), shared.Tunnel) for _ in range(2)]
                for proxy, mode in ((connect, "connect"), (socks, "socks")):
                    proxy.mode, proxy.reject, proxy.target = mode, False, target
                    services.enter_context(shared.serving(proxy))
                http_proxy = {"connectionProxyEnabled": True,
                              "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
                socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
                pools = [("speech-socks", "socks5", socks_url), ("speech-managed", "cloudflare", RELAY),
                         ("speech-unmanaged", "cloudflare", RELAY)]
                with shared.gateway(workspace, "native", ca, pools) as (port, db):
                    with sqlite3.connect(db) as sql:
                        sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='speech-managed'",
                                    (json.dumps({"url": RELAY, "token": TOKEN}),))
                    verify(port, db, group, connect, http_proxy)
    finally:
        node.terminate()
        node.wait(5)
print("ElevenLabs, Inworld and regional MiniMax TTS preserved audio and wire across CONNECT, SOCKS5h, bypass and managed Relay.")
