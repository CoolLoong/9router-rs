import base64
import hashlib
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading

MODE,*ARGS=sys.argv[1:]
HERE=pathlib.Path(__file__).resolve().parent
if MODE in {"native","strict"}:
    binary,CASE=ARGS; command=[binary]
else:
    CASE,=ARGS; command=["node","--no-warnings",str(HERE/"original.mjs")]
provider=CASE.split("-")[0]
provider={"eleven":"elevenlabs","minicn":"minimax-cn"}.get(provider,provider)
CONFIG={
    "elevenlabs":("https://api.elevenlabs.io","/v1/text-to-speech","FixtureVoice"),
    "deepgram":("https://api.deepgram.com","/v1/speak","aura-test-en"),
    "inworld":("https://api.inworld.ai","/tts/v1/voice","inworld-tts-1.5-max/FixtureVoice"),
    "minimax":("https://api.minimax.io","/v1/t2a_v2","speech-2.8-hd"),
    "minimax-cn":("https://api.minimaxi.com","/v1/t2a_v2","speech-2.8-turbo"),
}
vendor,path,default_model=CONFIG[provider]
API="fixture-component-key"
ITEM={"provider":provider,"model":default_model,"apiKey":API,"input":" \tFixture TTS\r\n",
      "vendor":vendor,"responseFormat":"json" if CASE in {"inworld-raw","minicn-json","deepgram-pcm","eleven-model"} else "mp3",
      "language":"fixture language","style":"fixture style"}
if CASE=="deepgram-disabled":
    ITEM["mode"]="registry"
if CASE.endswith("-key"):
    ITEM["apiKey"]=None
if CASE=="eleven-model":
    ITEM["model"]="eleven_turbo_v2_5/ExplicitVoice/ignored"
if CASE=="deepgram-default":
    ITEM["model"]=""
if CASE=="inworld-bare":
    ITEM["model"]="inworld-tts-1.5-mini"
if CASE=="inworld-default-voice":
    ITEM["model"]="inworld-tts-1.5-mini/"
if CASE=="minimax-voice":
    ITEM["model"]="speech-02-turbo/Region/Narrator"
if CASE=="minimax-bare-voice":
    ITEM["model"]="RegionNarrator"
WIRES=[]
EL_AUDIO=bytes(range(256))*5
DG_AUDIO=bytes(range(120))
HEX_AUDIO="2066CAFEfe01"
RAW_BASE64=" \n-_8"

class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self,*_args): pass

    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        expected = {
            "elevenlabs":("xi-api-key",API),"deepgram":("Authorization","Token "+API),
            "inworld":("Authorization","Basic "+API),"minimax":("Authorization","Bearer "+API),
            "minimax-cn":("Authorization","Bearer "+API),
        }[provider]
        assert self.headers[expected[0]]==expected[1]
        assert self.headers["Content-Type"]=="application/json"
        WIRES.append({"path":self.path,"body":body,"verifiedAuth":True})
        status,mime,blob=response()
        self.send_response(status)
        self.send_header("Content-Type",mime)
        self.send_header("Content-Length",str(len(blob)))
        self.end_headers()
        self.wfile.write(blob)

def response():
    def obj(value, status=200):
        return status,"application/json",json.dumps(value).encode()
    if provider=="elevenlabs":
        if CASE=="eleven-small": return 200,"audio/mpeg",EL_AUDIO[:1023]
        if CASE=="eleven-error": return obj({"detail":{"message":"ElevenLabs rejected the fixture"}},403)
        if CASE=="eleven-error-default": return 429,"text/plain",b"fixture rejected"
        if CASE=="eleven-redacted": return obj({"detail":{"message":"credential "+API}},401)
        return 200,"audio/wav" if CASE=="eleven-wav-header" else "audio/mpeg", EL_AUDIO
    if provider=="deepgram":
        if CASE=="deepgram-small": return 200,"audio/mpeg",DG_AUDIO[:99]
        if CASE=="deepgram-error": return obj({"error":{"message":"Deepgram fixture rejection"}},429)
        if CASE=="deepgram-text-error": return 403,"text/plain",b"Deepgram text fixture rejection"
        mime={"deepgram-wav":"audio/wav","deepgram-ogg":"audio/ogg","deepgram-pcm":"audio/L16;pcm"}.get(CASE,"audio/mpeg")
        return 200,mime,DG_AUDIO
    if provider=="inworld":
        if CASE=="inworld-no-audio": return obj({})
        if CASE=="inworld-error": return obj({"detail":"Inworld rejected fixture"},400)
        if CASE=="inworld-raw": return obj({"audioContent":RAW_BASE64})
        return obj({"audioContent":"AAECAwQ="})
    if CASE=="minimax-invalid-hex": return obj({"data":{"audio":"0x00x"}})
    if CASE=="minimax-no-audio": return obj({"data":{"audio":""}})
    if CASE=="minimax-status":
        return obj({"base_resp":{"status_code":"7","status_msg":"MiniMax application rejected fixture"}})
    if CASE=="minimax-status-unknown":
        return obj({"base_resp":{"status_code":"invalid"},"data":{"audio":HEX_AUDIO}})
    if CASE=="minimax-http-error":
        return obj({"baseResp":{"statusCode":8,"statusMsg":"MiniMax HTTP fixture rejection"}},422)
    if CASE=="minimax-text-error":
        return 500,"text/plain",b"MiniMax raw fixture rejection"
    if CASE=="minicn-json":
        return obj({"data":{"audio":" "+HEX_AUDIO+" "},"baseResp":{"statusCode":"0"},"extraInfo":{"audioFormat":"wav"}})
    return obj({"data":{"audio":HEX_AUDIO},"base_resp":{"status_code":0},"extra_info":{"audio_format":"ogg"}})

with tempfile.TemporaryDirectory(prefix="9router-speech-providers-") as directory:
    server=http.server.ThreadingHTTPServer(("127.0.0.1",0),Mock)
    threading.Thread(target=lambda:server.serve_forever(poll_interval=.02),daemon=True).start()
    ITEM["loopback"]=f"http://127.0.0.1:{server.server_port}"
    ITEM["endpoint"]=ITEM["loopback"]+path
    env=dict(os.environ,HOME=directory,DATA_DIR=directory,NO_PROXY="127.0.0.1,localhost")
    try:
        run=subprocess.run(command,cwd=directory,env=env,text=True,input=json.dumps(ITEM),capture_output=True,timeout=8)
    finally:
        server.shutdown();server.server_close()
    assert run.returncode==0,"isolated speech fixture failed"
    output=json.loads(run.stdout.strip().splitlines()[-1])
    if output.get("status")==200:
        audio=base64.b64decode(output.pop("bytes"))
        output["length"]=len(audio)
        output["digest"]=hashlib.sha256(audio).hexdigest()
        if "encoded" in output:
            encoded=output.pop("encoded")
            output["originalEncoding"]=(encoded==RAW_BASE64)
            if CASE=="inworld-raw":
                assert encoded==RAW_BASE64 and audio==b"\xfb\xff"
    if CASE=="eleven-redacted":
        assert API not in output["error"] and "[redacted]" in output["error"]
    print(json.dumps({"result":output,"wire":WIRES},sort_keys=True))
