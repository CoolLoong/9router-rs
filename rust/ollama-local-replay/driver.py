import http.server
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import threading
import urllib.parse

MODE,*ARGS=sys.argv[1:]
ROOT=pathlib.Path(__file__).resolve().parent
if MODE in {"native","strict"}:
    binary,CASE=ARGS
    COMMAND=[binary]
else:
    CASE,=ARGS
    COMMAND=["node","--no-warnings",str(ROOT/"original.mjs")]

ITEM={
    "model":"fixture-ollama:latest",
    "body":{"model":"incoming-alias","messages":[{"role":"system","content":"Local instructions"},{"role":"user","content":"A local question"}]},
    "account":{"providerSpecificData":{}},
    "settings":{},
    "stream":False,
    "expectedUrl":"http://localhost:11434/api/chat",
}
EXPECTED_AUTH={"default":"Bearer undefined"}
WIRES=[]

if CASE.startswith("url-"):
    ITEM["mode"]="url"
if CASE=="url-ipv6":
    ITEM["account"]["providerSpecificData"]["baseUrl"]="  http://[::1]:22333/nested/ "
    ITEM["expectedUrl"]="http://[::1]:22333/nested/api/chat"
if CASE=="url-trailing":
    ITEM["account"]["providerSpecificData"]["baseUrl"]="https://ollama.example.invalid/gateway//"
    ITEM["expectedUrl"]="https://ollama.example.invalid/gateway//api/chat"
if CASE=="url-blank":
    ITEM["account"]["providerSpecificData"]["baseUrl"]=" \u00a0  "
if CASE in {"stored-key","stored-oauth","stored-null","settings"}:
    ITEM["account"]["providerSpecificData"]["baseUrl"]="https://ollama.example.invalid/prefix/"
    ITEM["expectedUrl"]="https://ollama.example.invalid/prefix/api/chat"
    if CASE in {"stored-key","settings"}:
        ITEM["account"].update(apiKey="fixture-local-key",accessToken="fixture-oauth-key")
    elif CASE=="stored-oauth":
        ITEM["account"].update(apiKey="",accessToken="fixture-oauth-key")
    else:
        ITEM["account"].update(apiKey="",accessToken=None)
if CASE=="settings":
    ITEM["settings"]={"providerOverrides":{"ollama-local":{"headers":{"Accept":"application/x-ndjson","x-local-fixture":"metadata"}}}}
if CASE=="options":
    ITEM["body"].update(temperature=0,max_tokens=4,top_p=.1,max_completion_tokens=40,
                        response_format={"type":"json_object"},seed=9)
if CASE in {"multimodal","image-only"}:
    parts = [
        {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,UmF3"}},
        {"type":"image_url","image_url":{"url":"https://example.invalid/discard.png"}},
        {"type":"image_url","image_url":"data:image/png;base64,RW5k"},
    ]
    if CASE=="multimodal":
        parts.insert(0,{"type":"text","text":"Photo"})
        parts.append({"type":"text","text":"Next"})
    ITEM["body"]["messages"]=[{"role":"user","content":parts}]
if CASE=="tools":
    ITEM["body"]["messages"]=[
        {"role":"user","content":"Inspect"},
        {"role":"assistant","content":None,"tool_calls":[
            {"index":3,"id":"call-1","type":"function","function":{"name":"inspect","arguments":"{\"size\": 5}"}},
            {"id":"call-2","type":"function","function":{"name":"describe","arguments":"invalid json"}},
        ]},
        {"role":"tool","tool_call_id":"call-1","content":[{"type":"text","text":"Exact result"}]},
        {"role":"tool","tool_call_id":"call-2","content":"Second result"},
    ]
    ITEM["body"]["tools"]=[{"type":"function","function":{"name":"inspect","parameters":{"type":"object"}}}]
    ITEM["body"]["tool_choice"]="auto"
if CASE in {"stream-tools","stream-empty-tools","stream-length","ndjson-standard","bad-stream-media"}:
    ITEM["stream"]=True

CREDENTIALS={
    "Bearer undefined":"omitted",
    "Bearer null":"explicit-null",
    "Bearer fixture-local-key":"saved-key",
    "Bearer fixture-oauth-key":"saved-access",
}

class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self,*_args): pass

    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert self.path==urllib.parse.urlsplit(ITEM["expectedUrl"]).path
        auth = CREDENTIALS[self.headers["Authorization"]]
        WIRES.append({"path":self.path,"authorization":auth,"accept":self.headers.get("Accept"),
                      "contentType":self.headers.get("Content-Type"),"body":body,
                      "metadata":self.headers.get("x-local-fixture")})
        status,media,data=content()
        self.send_response(status)
        self.send_header("Content-Type",media)
        self.send_header("Content-Length",str(len(data)))
        self.end_headers()
        if ITEM["stream"] and len(data)>30:
            midpoint=len(data)//2
            self.wfile.write(data[:midpoint]);self.wfile.flush()
            self.wfile.write(data[midpoint:]);self.wfile.flush()
        else:
            self.wfile.write(data)

def content():
    if CASE=="bad-status":
        return 400,"application/json",b'{"error":"Fixture Ollama rejected request"}'
    if CASE=="bad-stream-media":
        return 200,"text/html",b"<title>Fixture relay failure</title>"
    if not ITEM["stream"]:
        record={"model":"fixture-upstream-model","done":True,"done_reason":"stop",
                "message":{"role":"assistant","content":"Local answer","thinking":"Local reasoning"},
                "prompt_eval_count":11,"eval_count":3,"prompt_eval_cached_count":4}
        if CASE=="tools":
            record["message"]["content"]=""
            record["message"]["tool_calls"]=[
                {"function":{"index":4,"name":"inspect","arguments":{"20":"b","1":"a","size":5.0}}},
                {"id":"provider-call","function":{"name":"describe","arguments":"{\"style\":\"short\"}"}},
            ]
        return 200,"application/json",json.dumps(record).encode()
    chunks=[
        {"message":{"role":"assistant","content":""},"done":False},
        {"model":"updated-model-should-not-replace-initial","message":{"content":"你好","thinking":"Local thinking"},"done":False},
    ]
    if CASE=="stream-tools":
        chunks.extend([
            {"message":{"tool_calls":[{"id":"fixed","function":{"index":3,"name":"tool","arguments":{"x":1}}}]},"done":False},
            {"message":{"tool_calls":[{"function":{"name":"another","arguments":"{}"}}]},"done":False},
        ])
    if CASE=="stream-empty-tools":
        chunks.append({"message":{"tool_calls":[]},"done":False})
    chunks.append({"done":True,"done_reason":"max_tokens" if CASE=="stream-length" else "stop",
                   "prompt_eval_count":13,"eval_count":8,"prompt_eval_cached_count":5})
    lines=[json.dumps(chunk,ensure_ascii=False) for chunk in chunks]
    lines.insert(1,"{bad json")
    return 200,("application/x-ndjson" if CASE=="ndjson-standard" else "application/json"),"\n".join(lines).encode()

def normalized(result):
    if "body" not in result or result["status"]>=400 or CASE=="bad-stream-media":
        return result
    raw=result["body"]
    if ITEM["stream"]:
        result["body"]=[line[6:] if line[6:]=="[DONE]" else json.loads(line[6:]) for line in raw.splitlines() if line.startswith("data: ")]
    else:
        result["body"]=json.loads(raw)
    def walk(x):
        if isinstance(x,list):
            return [walk(v) for v in x]
        if not isinstance(x,dict):
            return x
        rewritten={key:walk(val) for key,val in x.items()}
        identity=rewritten.get("id")
        if isinstance(identity,str) and re.fullmatch(r"chatcmpl-\d+",identity):
            rewritten["id"]="chatcmpl-fixture"
            assert isinstance(rewritten["created"],int)
            rewritten["created"]=1
        if isinstance(identity,str) and re.fullmatch(r"call_\d+_\d+",identity):
            rewritten["id"]=identity.rsplit("_",1)[0]+"_fixture"
        return rewritten
    return walk(result)

with tempfile.TemporaryDirectory(prefix="9router-ollama-fixture-") as directory:
    mock=http.server.ThreadingHTTPServer(("127.0.0.1",0),Mock)
    threading.Thread(target=lambda:mock.serve_forever(poll_interval=.02),daemon=True).start()
    suffix=urllib.parse.urlsplit(ITEM["expectedUrl"]).path
    ITEM["endpoint"]=f"http://127.0.0.1:{mock.server_port}{suffix}"
    env=dict(os.environ,HOME=directory,DATA_DIR=directory,NO_PROXY="localhost,127.0.0.1")
    try:
        result=subprocess.run(COMMAND,input=json.dumps(ITEM),text=True,capture_output=True,env=env,cwd=directory,timeout=7)
    finally:
        mock.shutdown();mock.server_close()
    assert result.returncode==0,"isolated Ollama fixture failed"
    value=json.loads(result.stdout.strip().splitlines()[-1])
    result=normalized(value)
    if CASE=="ndjson-standard":
        assert result["body"][-1]=="[DONE]" and result["body"][-2]["usage"]["prompt_tokens_details"]["cached_tokens"]==5
    if CASE=="bad-stream-media":
        assert result["body"]=="<title>Fixture relay failure</title>"
    print(json.dumps({"wire":WIRES,"response":result},sort_keys=True))
