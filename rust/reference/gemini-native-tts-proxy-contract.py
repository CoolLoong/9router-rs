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
spec = importlib.util.spec_from_file_location("native_tts_proxy_helpers",Path(__file__).with_name("zed-cline-models-proxy-contract.py"))
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
HOST = "generativelanguage.googleapis.com"
MODEL = "gemini-2.5-pro-preview-tts"
PROVIDER_KEY = "offline-gemini-native-tts-proxy-key"
RELAY = "https://native-speech-fixture.vercel.app/api/relay"
RELAY_KEY = "r" * 43
BODY = {"contents":[{"role":"user","parts":[{"text":"Synthetic TTS"}]}],
        "generationConfig":{"responseModalities":["AUDIO"],"speechConfig":{"voiceConfig":{"prebuiltVoiceConfig":{"voiceName":"Puck"}}}}}
AUDIO = {"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"audio/L16;rate=24000","data":"AAECAw=="}}]},"finishReason":"STOP"}]}
ledger = []
errors = []
redirect = False
shared.ZED_HOST = HOST
shared.SECRETS = (PROVIDER_KEY,RELAY_KEY)


class Vendor(BaseHTTPRequestHandler):
    def log_message(self,*_):
        pass

    def answer(self,status,media,body):
        self.send_response(status)
        self.send_header("Content-Type",media)
        self.send_header("Content-Length",str(len(body)))
        if status == 307:
            self.send_header("Location","/unintended-target")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            mode,path = "tls",self.path
            if path == "/vercel-relay/native-speech-fixture.vercel.app":
                mode = "relay"
                assert self.headers["X-Relay-Auth"] == RELAY_KEY
                assert self.headers["X-Relay-Target"] == "https://" + HOST
                path = self.headers["X-Relay-Path"]
            elif path.startswith("/gemini/"):
                mode,path = "direct",path.removeprefix("/gemini")
            else:
                assert self.headers["Host"] == HOST
            assert self.headers["X-Goog-Api-Key"] == PROVIDER_KEY
            assert self.headers.get("Authorization") is None and self.headers.get("Cookie") is None
            assert self.headers.get("X-Injected") is None
            assert self.headers["Content-Type"] == "application/json"
            received = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert received == BODY
            url = urlsplit(path)
            stream = url.path.endswith(":streamGenerateContent")
            action = "streamGenerateContent" if stream else "generateContent"
            assert url.path == f"/v1beta/models/{MODEL}:{action}"
            assert parse_qs(url.query) == ({"alt":["sse"],"mode":["fixture"]} if stream else {"mode":["fixture"]})
            ledger.append((mode,action))
            data = json.dumps(AUDIO,separators=(",",":")).encode()
            if redirect:
                self.answer(307,"application/json",b"{}")
            elif stream:
                self.answer(200,"text/event-stream",b"data: "+data+b"\n\n")
            else:
                self.answer(200,"application/json",data)
        except (AssertionError,KeyError,ValueError,TypeError) as error:
            errors.append(str(error) or self.path)
            self.answer(422,"application/json",b"{}")


def configure(db,details,priority=1):
    key = PROVIDER_KEY if priority==1 else "offline-gemini-untouched-backup"
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                       VALUES (?,'gemini','apikey','Synthetic raw TTS',?,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')
                       ON CONFLICT(id) DO UPDATE SET data=excluded.data""",
                    (f"native-tts-{priority}",priority,json.dumps({"apiKey":key,"providerSpecificData":details})))


def call(port,stream=False,extra=""):
    action = "streamGenerateContent" if stream else "generateContent"
    path = f"/v1beta/models/{MODEL}:{action}?key=offline-active-key&mode=fixture"
    if stream:
        path += "&alt=sse"
    with closing(http.client.HTTPConnection("127.0.0.1",port,timeout=8)) as client:
        client.request("POST",path+extra,json.dumps(BODY),{
            "Content-Type":"application/json","Cookie":"offline-protected=not-forwarded","X-Injected":"offline-caller-header",
        })
        response = client.getresponse()
        return response.status,response.headers["Content-Type"],response.read()


with tempfile.TemporaryDirectory(prefix="9router-gemini-native-tts-proxy-") as temporary, ExitStack() as stack:
    scratch = Path(temporary)
    ca,tls,_ = shared.tls_services(stack,scratch,Vendor)
    stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",52989),Vendor)))
    connect,socks = [shared.Proxy(("127.0.0.1",0),shared.Tunnel) for _ in range(2)]
    for proxy,mode in ((connect,"connect"),(socks,"socks")):
        proxy.mode,proxy.reject,proxy.target = mode,False,tls
        stack.enter_context(shared.serving(proxy))
    http_proxy = f"http://127.0.0.1:{connect.server_address[1]}"
    socks_url = f"socks5h://127.0.0.1:{socks.server_address[1]}"
    with shared.gateway(scratch,"native",ca,[("socks","http",socks_url)]) as (port,db):
        configure(db,{})
        configure(db,{},2)
        baseline = [call(port),call(port,True)]
        assert all(status==200 for status,_,_ in baseline),baseline
        configure(db,{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy})
        assert call(port) == baseline[0]
        configure(db,{"proxyPoolId":"socks"})
        assert call(port,True) == baseline[1]
        assert shared.TUNNELS == [("connect",HOST,443),("socks",HOST,443)] and shared.HELLOS == [HOST,HOST]
        configure(db,{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy,"connectionNoProxy":HOST})
        assert call(port)==baseline[0] and ledger[-1][0]=="direct" and len(shared.TUNNELS)==2
        with sqlite3.connect(db) as sql:
            sql.execute("""INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
                         VALUES ('managed',1,'unknown',?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                         (json.dumps({"type":"vercel","proxyUrl":RELAY,"_managedVercel":{"url":RELAY,"token":RELAY_KEY}}),))
        configure(db,{"proxyPoolId":"managed"})
        assert call(port,True)==baseline[1] and ledger[-1][0]=="relay"
        before = len(ledger)
        code,_,raw = call(port,True,"&echo="+PROVIDER_KEY)
        assert code==503 and b"destination contains a credential" in raw and PROVIDER_KEY.encode() not in raw
        assert len(ledger)==before
        redirect = True
        assert call(port)[0]==502 and len(ledger)==before+1
        redirect = False
        configure(db,{"connectionProxyEnabled":True,"connectionProxyUrl":http_proxy})
        connect.reject = True
        before = len(ledger),len(shared.TUNNELS)
        assert call(port)[0]==502
        assert (len(ledger),len(shared.TUNNELS))==(before[0],before[1]+1)
        assert not errors and not shared.ERRORS,(errors,shared.ERRORS)
        print("Raw Gemini TTS matched direct JSON/SSE through CONNECT, SOCKS5h, managed Relay and explicit bypass.")
