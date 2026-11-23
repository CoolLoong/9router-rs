from contextlib import ExitStack
from http.server import ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import socket
import socketserver
import sqlite3
import sys
import tempfile


sys.dont_write_bytecode = True


def fixture(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


shared = fixture("codex_route_fixture", "zed-cline-models-proxy-contract.py")
codex = fixture("codex_stt_fixture", "codex-stt-fixture.py")
HOST = "chatgpt.com"
ORIGIN = "/backend-api/transcribe"
KEY, SECONDARY = "offline-codex-primary", "offline-codex-secondary"
WORKSPACE = "fixture-workspace-primary"
REFRESH = "synthetic-codex-renewal-must-stay-held"
RELAY = "https://codex-stt-fixture.offline-lane.workers.dev"
RELAY_KEY = "C" * 43
AUTH_ENDPOINT = "auth.openai.com"
shared.ZED_HOST, shared.CLINE_HOST = HOST, "unused.codex-stt-fixture.com"
shared.SECRETS = (KEY,SECONDARY,WORKSPACE,REFRESH,RELAY_KEY)
CALLS, ERRORS, OAUTH_DIALS = [], [], []


class Vendor(codex.Upstream):
    def expected_origin(self):
        if self.server.server_port == 52987:
            return super().expected_origin()
        return self.path == ORIGIN and self.headers["Host"] == HOST

    def do_POST(self):
        try:
            assert self.expected_origin()
            assert all(self.headers[name] is None for name in (
                "Proxy-Authorization", "X-Relay-Target", "X-Relay-Path", "X-Relay-Auth", "Cookie",
                "X-Internal-STT", "openai-beta", "originator", "chatgpt-account-user-id",
            ))
            CALLS.append(("direct" if self.server.server_port == 52987 else "tunnel", self.headers["Authorization"]))
            super().do_POST()
        except AssertionError as error:
            ERRORS.append((self.path,repr(error)))
            self.send(422,b'{"error":"fixture request mismatch"}')


class OAuthTrap(socketserver.BaseRequestHandler):
    def handle(self):
        OAUTH_DIALS.append(self.client_address)


def configure(db, details, backup=False):
    index, key = (2,SECONDARY) if backup else (1,KEY)
    workspace = "fixture-workspace-secondary" if backup else WORKSPACE
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,'codex','oauth','Offline Codex',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
            (f"stt-{index}",index,json.dumps({"accessToken":key,
              "providerSpecificData":{"workspaceId":workspace,**details}})))


def expired(db):
    with sqlite3.connect(db) as sql:
        sql.execute("""UPDATE providerConnections SET data=json_set(data,'$.expiresAt','2020-11-01T00:00:00Z',
            '$.refreshToken',?) WHERE id='stt-1'""",(REFRESH,))


def transcription(port, audio=b"fixture audio json", plain=False):
    form = [("model","cx/transcribe"),("file",audio)]
    if plain:
        form.append(("response_format","text"))
    body,mime = codex.multipart(form)
    return codex.request(port,"POST","/v1/audio/transcriptions",body,{**codex.inference,
                "Content-Type":mime,"Cookie":"not forwarded","X-Internal-STT":"not forwarded"})


def expect_good(port, lane, plain=False):
    before = len(CALLS)
    status,headers,body = transcription(port,b"fixture audio text" if plain else b"fixture audio json",plain)
    assert status == 200 and headers["Cache-Control"] == "no-store", (status,body,ERRORS)
    if plain:
        assert headers["Content-Type"] == "text/plain; charset=utf-8" and body.decode() == codex.transcript
    else:
        assert json.loads(body) == {"text":codex.transcript}
    assert CALLS[before:] == [(lane,"Bearer "+KEY)]


def expect_error(port, status, audio=b"fixture audio json"):
    code,_,body = transcription(port,audio)
    assert code == status and all(secret.encode() not in body for secret in shared.SECRETS),(code,body,ERRORS)
    return body


def cases(port, db, http, socks, wrong_host, connect):
    configure(db,{"baseUrl":"http://127.0.0.1:52989/capture"})
    configure(db,{},backup=True)
    expect_good(port,"direct")
    expect_good(port,"direct",plain=True)
    assert not shared.TUNNELS
    configure(db,http | {"baseUrl":"http://127.0.0.1:52989/capture"})
    expired(db)
    expect_good(port,"tunnel")
    assert shared.TUNNELS == [("connect",HOST,443)]
    before=len(CALLS)
    expect_error(port,401,b"fixture retry")
    assert CALLS[before:] == [("tunnel","Bearer "+KEY)]
    assert not OAUTH_DIALS
    configure(db,{"proxyPoolId":"fixture-socks"})
    expect_good(port,"tunnel",plain=True)
    assert shared.TUNNELS[-1] == ("socks",HOST,443)

    start=len(shared.TUNNELS)
    configure(db,http | {"connectionNoProxy":".chatgpt.com"})
    expired(db)
    expect_good(port,"direct")
    before=len(CALLS)
    expect_error(port,401,b"fixture retry")
    assert CALLS[before:] == [("direct","Bearer "+KEY)]
    configure(db,{})
    shared.enable_global_proxy(db,socks)
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data=json_set(data,'$.outboundNoProxy','.chatgpt.com') WHERE id=1")
    expect_good(port,"direct",plain=True)
    assert len(shared.TUNNELS) == start and not OAUTH_DIALS
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE settings SET data='{}' WHERE id=1")
        sql.execute("UPDATE proxyPools SET data=json_set(data,'$.noProxy','.chatgpt.com') WHERE id='fixture-socks'")

    before=(len(CALLS),len(shared.TUNNELS))
    for details in ({"proxyPoolId":"fixture-socks"},{"proxyPoolId":"missing"},{"strictProxy":True},
                    {"proxyPoolId":"managed-relay"},{"proxyPoolId":"unmanaged-relay"},{"vercelRelayUrl":RELAY}):
        configure(db,details)
        assert b"Codex transcription proxy is unavailable" in expect_error(port,503)
    assert (len(CALLS),len(shared.TUNNELS)) == before

    configure(db,http)
    connect.reject=True
    expect_error(port,502)
    assert (len(CALLS),len(shared.TUNNELS)) == (before[0],before[1]+1)
    connect.reject=False
    previous_target=connect.target
    connect.target=wrong_host
    expect_error(port,502)
    assert len(CALLS)==before[0] and shared.HELLOS[-1]==HOST
    connect.target=previous_target
    for audio,code in ((b"fixture error body",422),(b"fixture redirect",502),(b"fixture oversized response",502),
                       (b"fixture long transcript",502),(b"fixture malformed response",502)):
        before=(len(CALLS),len(shared.TUNNELS))
        expect_error(port,code,audio)
        assert CALLS[before[0]:] == [("tunnel","Bearer "+KEY)] and len(shared.TUNNELS)==before[1]+1


with tempfile.TemporaryDirectory(prefix="9router-codex-stt-proxy-") as folder, ExitStack() as stack:
    scratch=Path(folder)
    fake=scratch/"ffmpeg"
    fake.write_text("#!"+sys.executable+"\n"+codex.fake_ffmpeg)
    fake.chmod(0o700)
    wrapper=scratch/"gateway.sh"
    wrapper.write_text(f'#!/bin/sh\nexport PATH="{scratch}"\nexec "{shared.BINARY}" "$@"\n')
    wrapper.chmod(0o700)
    shared.BINARY=wrapper
    ca,target,wrong_host=shared.tls_services(stack,scratch,Vendor)
    for port in (52987,52988,52989):
        stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",port),Vendor)))
    address="127.0.0.2"
    if "--check-renewal" in sys.argv[2:]:
        addresses = socket.getaddrinfo(AUTH_ENDPOINT,443,type=socket.SOCK_STREAM)
        assert {record[4][0] for record in addresses} == {address}, addresses
        stack.enter_context(shared.serving(shared.Proxy((address,443),OAuthTrap)))
    connect,socks=[shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((connect,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target=mode,False,target
        stack.enter_context(shared.serving(proxy))
    http={"connectionProxyEnabled":True,"connectionProxyUrl":f"http://127.0.0.1:{connect.server_address[1]}"}
    socks_url=f"socks5h://127.0.0.1:{socks.server_address[1]}"
    pools=[("fixture-socks","socks5",socks_url),("managed-relay","cloudflare",RELAY),("unmanaged-relay","cloudflare",RELAY)]
    with shared.gateway(scratch,"codex-stt",ca,pools) as (port,db):
        with sqlite3.connect(db) as sql:
            sql.execute("UPDATE proxyPools SET data=json_set(data,'$._managedCloudflare',json(?)) WHERE id='managed-relay'",
                        (json.dumps({"url":RELAY,"token":RELAY_KEY}),))
        cases(port,db,http,socks_url,wrong_host,connect)
    assert all(name==HOST for name in shared.HELLOS)
    assert not ERRORS and not shared.ERRORS and not codex.unauthorized_origins and not OAUTH_DIALS, (ERRORS,shared.ERRORS)
print("Codex STT reused exact workspace, multipart WAV and JSON/text over CONNECT/SOCKS5h and explicit bypass; no failover.")
if "--check-renewal" in sys.argv[2:]:
    print("The isolated OAuth authority received no connections from protected transcription.")
