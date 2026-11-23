from contextlib import ExitStack, closing
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import parse_qs


sys.dont_write_bytecode = True
fixture = Path(__file__).with_name("zed-cline-models-proxy-contract.py")
spec = importlib.util.spec_from_file_location("profile_fixture", fixture)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
ROOT = shared.ROOT
SECRET = "offline-profile-secret"
REJECT = "offline-rejected-secret"
PASSWORD = "123456"
TOKEN_CHECKS = []


class Issuer(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, data):
        raw = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        assert self.path == "/issuer/.well-known/openid-configuration", self.path
        root = f"http://127.0.0.1:{self.server.server_port}/issuer"
        self.reply(200, {"issuer":root,"authorization_endpoint":root+"/authorize",
                         "token_endpoint":root+"/token","jwks_uri":root+"/jwks"})

    def do_POST(self):
        assert self.path == "/issuer/token", self.path
        form = parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
        assert form["client_id"] == ["fixture-client"] and form["grant_type"] == ["authorization_code"]
        secret, = form["client_secret"]
        assert secret in (SECRET,REJECT)
        TOKEN_CHECKS.append(secret)
        if secret == REJECT:
            self.reply(401, {"error":"invalid_client","error_description":"Rejected credential " + REJECT})
        else:
            self.reply(400, {"error":"invalid_grant","error_description":"Invalid fixture code"})


def request(port, method, path, body=None, headers=None):
    headers = dict(headers or {})
    if body is not None:
        headers["Content-Type"] = "application/json"
    with closing(HTTPConnection("127.0.0.1",port,timeout=8)) as connection:
        connection.request(method,path,json.dumps(body) if body is not None else None,headers)
        response=connection.getresponse()
        raw=response.read()
        media=response.headers.get("Content-Type","")
        return response.status, response.headers, (json.loads(raw) if media.startswith("application/json") else raw)


with tempfile.TemporaryDirectory(prefix="9router-profile-contract-") as temporary, ExitStack() as stack:
    scratch=Path(temporary)
    issuer=stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1",0),Issuer)))
    issuer_url=f"http://127.0.0.1:{issuer.server_port}/issuer"
    with shared.gateway(scratch,"profile",None,[]) as (port,_db):
        assert request(port,"GET","/dashboard/profile")[0]==307
        for path,method in (("/api/settings/database","GET"),("/api/settings/database","POST"),("/api/auth/oidc/test","POST"),
                            ("/api/auth/saml/test","POST")):
            assert request(port,method,path)[0]==401, path
        status,headers,body=request(port,"POST","/api/auth/login",{"password":PASSWORD})
        assert status==200 and body["success"]
        cookie=headers["Set-Cookie"].split(";",1)[0]
        assert cookie.startswith("auth_token=")
        headers={"Cookie":cookie}
        status,_,settings=request(port,"PATCH","/api/settings",{
            "authMode":"both","ssoType":"saml","oidcIssuerUrl":issuer_url,"oidcClientId":"fixture-client",
            "oidcClientSecret":SECRET,"samlIssuer":"urn:fixture:sp",
        },headers)
        assert status==200 and settings["oidcConfigured"] and "oidcClientSecret" not in settings
        status,_,body=request(port,"GET","/api/settings/database",headers=headers)
        assert status==401 and body["error"]=="Invalid password"
        subprocess.run(["node",str(ROOT/"rust/reference/profile-management-contract.mjs"),
                       f"http://127.0.0.1:{port}",cookie,issuer_url,PASSWORD],cwd=ROOT,
                       env={"PATH":os.environ["PATH"],"HOME":str(scratch)},check=True)
        status,_,settings=request(port,"GET","/api/settings",headers=headers)
        assert status==200 and settings["oidcConfigured"] and "oidcClientSecret" not in settings
        assert settings["requireApiKey"] is False and settings["authMode"]=="both" and settings["ssoType"]=="saml"
        assert TOKEN_CHECKS==[SECRET,REJECT]
print("Profile HTTP contract passed authenticated routes, OIDC/SAML tests, and synthetic database restore.")
