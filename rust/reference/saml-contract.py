import base64
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qs, urlencode, urlsplit
import xml.etree.ElementTree as XML
import zlib

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]
admin = {"x-9r-cli-token": "68977b6fd9422fbb"}
expected_issuer = "urn:9router:saml-fixture"


def request(port, path, method="GET", body=None, headers=None):
    connection = HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        return response.status, response.headers, response.read()
    finally:
        connection.close()


def app_json(port, path, method, body, headers=None):
    return request(port, path, method, json.dumps(body).encode(),
                   {"content-type": "application/json", **(headers or {})})


def start(port):
    code, headers, _ = request(port, "/api/auth/saml/start")
    link = urlsplit(headers["location"])
    assert code == 307 and link.hostname == "idp.example.invalid"
    payload = parse_qs(link.query)
    assert payload["request-fixture"] == ["kept"]
    xml = zlib.decompress(base64.b64decode(payload["SAMLRequest"][0]), -15)
    document = XML.fromstring(xml)
    assert document.attrib["Destination"] == "https://idp.example.invalid/sign-in?request-fixture=kept"
    assert document.find("{urn:oasis:names:tc:SAML:2.0:assertion}Issuer").text == expected_issuer
    assert headers["set-cookie"].startswith("saml_state=" + document.attrib["ID"])
    return document.attrib["ID"], headers["set-cookie"].split(";", 1)[0]


with tempfile.TemporaryDirectory(prefix="9router-saml-contract-") as temp:
    work = Path(temp)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {**os.environ, "PORT": str(port), "DATA_DIR": str(work), "NINEROUTER_MIGRATION_DATA_DIR": str(work),
           "NINEROUTER_MIGRATION_FIXTURE": "1", "RUST_ANYTHING_FALLBACK_LOG": str(work / "fallback.log")}
    for key in ["BASE_URL", "NEXT_PUBLIC_BASE_URL", "JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE"]:
        env.pop(key, None)
    private = work / "fixture.pem"
    public = work / "fixture.crt"
    subprocess.run(["openssl", "req", "-newkey", "rsa:2048", "-x509", "-nodes", "-days", "1",
                    "-keyout", str(private), "-out", str(public), "-subj", "/CN=fixture-identity-provider"],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    with subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            for _ in range(150):
                if process.poll() is not None:
                    raise RuntimeError("fixture server quit")
                try:
                    if request(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    time.sleep(.04)
            else:
                raise RuntimeError("fixture server did not start")
            origin = f"http://127.0.0.1:{port}"
            acs_url = origin + "/api/auth/saml/acs"
            status, headers, _ = request(port, "/api/auth/saml/start")
            assert status == 307 and "saml_not_configured" in headers["location"]
            assert app_json(port, "/api/auth/saml/test", "POST", {})[0] == 401
            settings = {"authMode":"both", "ssoType":"saml", "samlEntryPoint":"https://idp.example.invalid/sign-in?request-fixture=kept",
                        "samlIssuer":expected_issuer, "samlCert":public.read_text(),
                        "samlAttributeEmail":"fixtureMail", "samlAttributeName":"fixtureName"}
            status, _, _ = app_json(port, "/api/settings", "PATCH", settings, admin)
            assert status == 200, status
            status, headers, body = request(port, "/api/auth/saml/metadata")
            assert status == 200 and headers["content-type"] == "application/xml"
            document = XML.fromstring(body)
            assert document.attrib["entityID"] == expected_issuer
            acs = document.find(".//{urn:oasis:names:tc:SAML:2.0:metadata}AssertionConsumerService")
            metadata_endpoint = urlsplit(acs.attrib["Location"])
            assert metadata_endpoint.hostname in ("127.0.0.1", "localhost")
            assert metadata_endpoint.port == port and metadata_endpoint.path == "/api/auth/saml/acs"
            status, session_headers, _ = app_json(port, "/api/auth/login", "POST", {"password":"123456"})
            assert status == 200
            auth = {"cookie": session_headers["set-cookie"].split(";", 1)[0]}
            status, _, body = app_json(port, "/api/auth/saml/test", "POST", {}, auth)
            assert status == 200 and json.loads(body)["certValid"] is True

            def issue(request_id, audience, mode="root-and-assertion"):
                return subprocess.check_output(["node", "rust/reference/saml-sign-fixture.cjs",
                    str(private), str(public), request_id, acs_url, audience, mode], cwd=root).decode()

            def submit(assertion, cookie):
                return request(port, "/api/auth/saml/acs", "POST", urlencode({"SAMLResponse": assertion}).encode(),
                               {"content-type": "application/x-www-form-urlencoded", **({"cookie":cookie} if cookie else {})})

            request_id, cookie = start(port)
            assertion = issue(request_id, expected_issuer)
            status, headers, _ = submit(assertion, cookie if server == "original" else None)
            assert status == 307 and urlsplit(headers["location"]).path == "/dashboard", headers["location"]
            cookies = dict(value.split(";", 1)[0].split("=", 1) for value in headers.get_all("set-cookie"))
            assert cookies["saml_state"] == "" and cookies["auth_token"]
            status, _, body = request(port, "/api/auth/status", headers={"cookie":f"auth_token={cookies['auth_token']}"})
            details = json.loads(body)
            assert status == 200 and details["samlLogin"] and details["samlEmail"] == "fixture+sso@example.invalid"
            assert details["samlName"] == "Fixture SAML User"

            if server != "original":
                replay = submit(assertion, cookie)
                assert replay[0] == 307 and urlsplit(replay[1]["location"]).path == "/login"
            for test_audience, mode in [("urn:fixture:other-audience", "root-and-assertion"), (expected_issuer, "assertion-only")]:
                request_id, cookie = start(port)
                status, headers, _ = submit(issue(request_id, test_audience, mode), cookie)
                assert status == 307 and urlsplit(headers["location"]).path == "/login"
                assert all(not value.startswith("auth_token=") for value in headers.get_all("set-cookie"))
            if server != "original":
                fallback = (work / "fallback.log").read_text()
                assert "/api/auth/saml" not in fallback
            print(f"{server}: validated SAML XML, certificate, audience and dashboard session")
        finally:
            process.terminate()
