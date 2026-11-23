from contextlib import ExitStack, closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
from urllib.parse import parse_qs, urlsplit


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("web_proxy_helpers", Path(__file__).with_name("zed-cline-models-proxy-contract.py"))
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
BRAVE, TAVILY = "api.search.brave.com", "api.tavily.com"
KEYS = {BRAVE:"offline-web-proxy-brave",TAVILY:"offline-web-proxy-tavily"}
RELAY = "https://web-proxy-fixture.vercel.app/api/relay"
RELAY_TOKEN = "r" * 43
DIRECT_URL = "http://93.184.216.34/fake-public-source"
QUERY = "offline web query"
delivered = []
errors = []
redirect = False
shared.ZED_HOST, shared.CLINE_HOST = BRAVE, TAVILY
shared.SECRETS = (*KEYS.values(), RELAY_TOKEN)


class Vendor(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def result(self, status, body, headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type","application/json")
        self.send_header("Content-Length",str(len(raw)))
        for name,value in (headers or {}).items():
            self.send_header(name,value)
        self.end_headers()
        self.wfile.write(raw)

    def handle_api(self):
        try:
            host = self.headers["Host"]
            mode = "tls"
            path = self.path
            if self.path == "/vercel-relay/web-proxy-fixture.vercel.app":
                mode = "relay"
                host = urlsplit(self.headers["X-Relay-Target"]).hostname
                assert self.headers["X-Relay-Auth"] == RELAY_TOKEN
                path = self.headers["X-Relay-Path"]
            elif self.path.startswith(("/brave/","/tavily/")):
                mode = "direct"
                _, prefix, suffix = self.path.split("/",2)
                host = BRAVE if prefix == "brave" else TAVILY
                path = "/" + suffix
            assert all(key not in path for key in (*KEYS.values(), RELAY_TOKEN))
            assert not self.headers.get("Cookie") and not self.headers.get("X-Should-Not-Forward")
            if host == BRAVE:
                assert self.command == "GET" and self.headers["X-Subscription-Token"] == KEYS[host]
                assert self.headers.get("Authorization") is None
                assert urlsplit(path).path == "/res/v1/web/search" and parse_qs(urlsplit(path).query)["q"] == [QUERY]
                value = {"web":{"results":[{"url":DIRECT_URL,"title":"Offline page","description":"Offline summary"}]}}
            else:
                assert host == TAVILY and self.command == "POST"
                assert self.headers["Authorization"] == "Bearer " + KEYS[host]
                data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if path == "/search":
                    assert data["query"] == QUERY and data["topic"] == "general"
                    value = {"results":[{"url":DIRECT_URL,"title":"Offline page","content":"Offline summary","score":1}]}
                else:
                    assert path == "/extract" and data == {"urls":[DIRECT_URL],"extract_depth":"basic","format":"markdown"}
                    value = {"results":[{"url":DIRECT_URL,"raw_content":"Offline document"}]}
            delivered.append((mode,host,path.split("?",1)[0]))
            if redirect:
                return self.result(307,{}, {"Location":"/unsafe-target"})
            return self.result(200,value)
        except (AssertionError, KeyError, TypeError, ValueError) as error:
            errors.append(str(error) or (self.command,self.path))
            self.result(422,{})

    do_GET = do_POST = handle_api


def configure(db, provider, details, priority=1):
    host = BRAVE if provider == "brave-search" else TAVILY
    key = KEYS[host] if priority == 1 else "offline-web-backup-never-selected"
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                    VALUES (?,?,'apikey','Fixture web proxy',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')
                    ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (f"{provider}-{priority}",provider,priority,json.dumps({"apiKey":key,"providerSpecificData":details})))


def call(port, provider, fetch=False):
    payload = {"provider":provider, **({"url":DIRECT_URL} if fetch else {"query":QUERY})}
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request("POST","/v1/web/fetch" if fetch else "/v1/search",json.dumps(payload), {
            "Content-Type":"application/json","Authorization":"Bearer offline-active-key",
            "Cookie":"offline-cookie=never-forward","X-Should-Not-Forward":"offline-canary",
        })
        response = client.getresponse()
        status, result = response.status,json.loads(response.read())
        if "metrics" in result:
            result["metrics"].pop("response_time_ms",None)
            result["metrics"].pop("upstream_latency_ms",None)
        for entry in result.get("results",[]):
            assert entry["citation"].pop("retrieved_at").endswith("Z")
        return status,result


with tempfile.TemporaryDirectory(prefix="9router-web-proxy-") as temp, ExitStack() as stack:
    scratch = Path(temp)
    ca, destination, _ = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect,socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in [(connect,"connect"),(socks,"socks")]:
        proxy.mode,proxy.reject,proxy.target = mode,False,destination
        stack.enter_context(shared.serving(proxy))
    http_proxy = f"http://127.0.0.1:{connect.server_address[1]}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    with shared.gateway(scratch,"native",ca,[("socks","http",socks_url)]) as (port,db):
        for provider in ("brave-search","tavily"):
            configure(db,provider,{})
            configure(db,provider,{},2)
        cases = [("brave-search",False),("tavily",False),("tavily",True)]
        direct = [call(port,*case) for case in cases]
        assert all(code==200 for code,_ in direct),direct
        configure(db,"brave-search",{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy})
        configure(db,"tavily",{"proxyPoolId":"socks"})
        through_proxies = [call(port,*case) for case in cases]
        assert through_proxies == direct, (through_proxies, direct, delivered, shared.TUNNELS, errors, shared.ERRORS)
        assert shared.TUNNELS == [("connect",BRAVE,443),("socks",TAVILY,443),("socks",TAVILY,443)]
        assert shared.HELLOS == [BRAVE,TAVILY,TAVILY]
        configure(db,"brave-search",{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy,"connectionNoProxy":"api.search.brave.com"})
        assert call(port,*cases[0]) == direct[0] and delivered[-1][0] == "direct" and len(shared.TUNNELS)==3

        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                         VALUES ('managed',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                        (json.dumps({"type":"vercel","proxyUrl":RELAY,"_managedVercel":{"url":RELAY,"token":RELAY_TOKEN}}),))
        configure(db,"brave-search",{"proxyPoolId":"managed"})
        assert call(port,*cases[0]) == direct[0] and delivered[-1][0] == "relay"
        before = (len(delivered),len(shared.TUNNELS))
        for provider in ("google-pse","searchapi"):
            configure(db,provider,{"proxyPoolId":"managed","cx":"offline-google-context"})
            code, problem = call(port,provider)
            assert code == 503 and "credentials in the URL" in problem["error"]["message"], (code,problem)
            assert (len(delivered),len(shared.TUNNELS)) == before
        redirect = True
        before = len(delivered)
        code, problem = call(port,*cases[0])
        assert code == 502 and "unexpected redirect" in problem["error"]["message"] and len(delivered)==before+1
        redirect = False
        configure(db,"brave-search",{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy})
        connect.reject = True
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO combos (id,name,kind,models,createdAt,updatedAt)
                         VALUES ('web-proxy-combo','web-proxy-combo','webSearch',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                         (json.dumps(["brave-search","tavily"]),))
        before = (len(delivered),len(shared.TUNNELS))
        assert call(port,"web-proxy-combo")[0] == 502
        assert (len(delivered),len(shared.TUNNELS)) == (before[0],before[1]+1)
        assert not errors and not shared.ERRORS,(errors,shared.ERRORS)
        print("Brave and Tavily direct, CONNECT, SOCKS5h, managed Relay and protected failure matched the test contract.")
