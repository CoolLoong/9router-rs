from contextlib import ExitStack
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import sqlite3
import tempfile
from threading import Event
from urllib.parse import parse_qs


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
kimi = "auth.kimi.com"
portal = "portal.nousresearch.com"
inference = "inference-api.nousresearch.com"
hosts = (kimi, portal, inference)
relay_host = "nous-fixture.team.workers.dev"
lease = "N" * 43
old = "fixture-agent-access-old"
new = "fixture-agent-access-new"
refresh = "fixture-agent-refresh-old"
rotated = "fixture-agent-refresh-new"
key = "fixture-agent-key-old"
minted = "fixture-agent-key-new"
device = "11111111-1111-4111-8111-111111111111"
secrets = (old,new,refresh,rotated,key,minted,lease)
now, expired = "2099-01-01T00:00:00Z", "2000-01-01T00:00:00Z"
events, failures = [], []


class Vendor(BaseHTTPRequestHandler):
    hold = None
    response_status = 401
    mint_status = 200
    database = None

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            direct = self.server.server_port == 52989
            route = "direct" if direct else "tunnel"
            target, path = self.headers["Host"], self.path
            if path.startswith("/cloudflare-relay/"):
                assert path == "/cloudflare-relay/" + relay_host
                assert self.headers["X-Relay-Auth"] == lease
                target = self.headers["X-Relay-Target"].removeprefix("https://")
                path = self.headers["X-Relay-Path"]
                route = "relay"
            elif direct:
                assert path.startswith("/nous-device/")
                target, path = portal, path.removeprefix("/nous-device")
                path = "/api/oauth" + path
            assert self.headers["Cookie"] is None and self.headers["X-9r-Cli-Token"] is None
            assert self.headers["Proxy-Authorization"] is None
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            auth = self.headers["Authorization"]
            if target in (kimi, portal) and path == "/api/oauth/token":
                fields = {key:value[0] for key,value in parse_qs(raw.decode()).items()}
                assert fields.pop("client_id") == ("kimi-fixture-client" if target == kimi else "hermes-cli")
                assert fields == {"refresh_token":refresh,"grant_type":"refresh_token"} and auth is None
                if target == kimi:
                    assert self.headers["X-Msh-Device-Id"] == device
                    assert self.headers["User-Agent"].startswith("9Router/")
                else:
                    assert self.headers["X-Nous-Refresh-Token"] == refresh
                events.append((route,target,"grant"))
                self.reply(200,{"access_token":new,"refresh_token":rotated,"expires_in":3600})
            elif target == portal and path == "/api/oauth/agent-key":
                assert auth == "Bearer " + new and json.loads(raw) == {"min_ttl_seconds":7200}
                persisted = saved(self.database, "nous")
                assert persisted["accessToken"] == new and persisted["refreshToken"] == rotated
                assert persisted["nousRefreshAttempt"] is None
                events.append((route,target,"mint"))
                self.reply(self.mint_status,{"api_key":minted,"key_id":"fixture-key-id","expires_at":now})
            else:
                assert (target,path) == (inference,"/v1/chat/completions")
                assert json.loads(raw) == {"model":"poolside/laguna-xs-2.1:free",
                    "messages":[{"role":"user","content":"ping"}],"max_tokens":1,"stream":False}
                assert auth in ("Bearer " + key, "Bearer " + minted, "Bearer " + new)
                renewed = auth != "Bearer " + key
                events.append((route,target,"renewed" if renewed else "probe"))
                if self.hold and not renewed:
                    arrived, release = self.hold
                    arrived.set()
                    assert release.wait(5)
                self.reply(200 if renewed else self.response_status,{"notice":old})
        except (AssertionError, KeyError, ValueError) as error:
            failures.append((self.path,repr(error)))
            self.send_error(422)

    def reply(self, status, value):
        raw = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def save(db, provider, details=None, expiry=now):
    data = {"accessToken":old,"refreshToken":refresh,"expiresAt":expiry,
            "providerSpecificData":details or {}}
    if provider == "kimi":
        data["providerSpecificData"].update(deviceId=device,deviceClientId="kimi-fixture-client")
    else:
        data["apiKey"] = key
        data["providerSpecificData"]["agentKeyExpiresAt"] = expiry
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
          (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
          VALUES (?,?,'oauth','Synthetic OAuth fixture',1,1,?,'2026-11-20','2026-11-20')""",
                    (provider,provider,json.dumps(data)))


def saved(db, provider):
    with sqlite3.connect(db) as sql:
        return json.loads(sql.execute("SELECT data FROM providerConnections WHERE id=?",(provider,)).fetchone()[0])


def check(port, provider, refreshed=False, error=None):
    conn = HTTPConnection("127.0.0.1",port,timeout=8)
    try:
        conn.request("POST",f"/api/providers/{provider}/test",headers=shared["ADMIN"])
        response = conn.getresponse()
        raw = response.read()
        assert all(value.encode() not in raw for value in secrets), raw
        assert response.status == 200 and json.loads(raw) == {
            "valid":error is None,"error":error,"refreshed":refreshed,
        }, (response.status,raw,failures)
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix="9router-kimi-nous-test-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared["tls_services"](stack, scratch, Vendor, hosts)
    stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect, socks = [shared["Proxy"](("127.0.0.1",0),shared["Tunnel"]) for _ in range(2)]
    for proxy, mode in ((connect,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,target
        proxy.hosts,proxy.secrets = hosts,secrets
        stack.enter_context(shared["serving"](proxy))
    address = f"http://127.0.0.1:{connect.server_address[1]}"
    http = {"connectionProxyEnabled":True,"connectionProxyUrl":address}
    pools = [("shared-socks","socks5",f"socks5h://127.0.0.1:{socks.server_address[1]}")]
    with shared["gateway"](scratch,"provider-tests",ca,pools) as (port,db):
        Vendor.database = db
        save(db,"kimi",{"proxyPoolId":"shared-socks"},expired)
        check(port,"kimi",refreshed=True)
        assert events == [("tunnel",kimi,"grant")]
        assert shared["TUNNELS"] == [("socks",kimi,443)]
        assert saved(db,"kimi")["refreshToken"] == rotated
        check(port,"kimi")
        assert len(events) == 1

        save(db,"nous",http)
        check(port,"nous",refreshed=True)
        expected = [("tunnel",inference,"probe"),("tunnel",portal,"grant"),
                    ("tunnel",portal,"mint"),("tunnel",inference,"renewed")]
        assert events[1:] == expected
        assert saved(db,"nous")["apiKey"] == minted and saved(db,"nous")["refreshToken"] == rotated
        save(db,"nous",http)
        Vendor.mint_status = 503
        check(port,"nous",refreshed=True)
        assert events[-4:] == expected
        assert saved(db,"nous")["apiKey"] == new and saved(db,"nous")["refreshToken"] == rotated
        Vendor.mint_status = 200

        save(db,"nous",{**http,"connectionNoProxy":portal})
        check(port,"nous",refreshed=True)
        assert events[-4:] == [("tunnel",inference,"probe"),("direct",portal,"grant"),
                               ("direct",portal,"mint"),("tunnel",inference,"renewed")]

        with sqlite3.connect(db) as sql:
            relay = "https://" + relay_host
            data = {"type":"cloudflare","proxyUrl":relay,"strictProxy":True,
                    "_managedCloudflare":{"url":relay,"token":lease}}
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                VALUES ('test-relay',1,'unknown',?,'2026-11-20','2026-11-20')""",(json.dumps(data),))
        save(db,"nous",{"proxyPoolId":"test-relay"})
        check(port,"nous",refreshed=True)
        assert events[-4:] == [("relay",target,kind) for _,target,kind in expected]

        save(db,"nous",http)
        Vendor.response_status = 407
        start = len(events)
        check(port,"nous",error="Proxy authentication required")
        assert events[start:] == [expected[0]]
        Vendor.response_status = 401

        save(db,"nous")
        shared["enable_global_proxy"](db,address)
        arrived, release = Vendor.hold = Event(), Event()
        start = len(events)
        with ThreadPoolExecutor() as workers:
            pending = workers.submit(check,port,"nous",error="Token invalid or revoked")
            try:
                assert arrived.wait(5)
                with sqlite3.connect(db) as sql:
                    sql.execute("UPDATE settings SET data='{}' WHERE id=1")
            finally:
                release.set()
            pending.result()
        Vendor.hold = None
        assert events[start:] == [expected[0]] and saved(db,"nous")["refreshToken"] == refresh

        save(db,"kimi",http,expired)
        start = len(events), len(shared["TUNNELS"])
        connect.reject = True
        check(port,"kimi",error="Token expired and refresh failed")
        check(port,"kimi",error="Token expired and refresh failed")
        connect.reject = False
        assert len(events) == start[0] and len(shared["TUNNELS"]) == start[1]+1
        assert saved(db,"kimi")["kimiRefreshAttempt"] and saved(db,"kimi")["refreshToken"] == refresh
    assert not failures and not shared["ERRORS"], (failures,shared["ERRORS"])
print("Kimi and Nous OAuth Test verified protected renewal, provider minting and no direct fallback.")
