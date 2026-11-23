import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qs, urlencode, urlsplit

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def ready(proc, portno):
    deadline = time.monotonic() + 20
    while proc.poll() is None and time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", portno), timeout=.2):
                return
        except OSError:
            time.sleep(.05)
    raise RuntimeError("fixture process did not become ready")


def request(portno, path, method="GET", data=None, headers=None):
    headers = {} if headers is None else headers.copy()
    if data is not None:
        data = json.dumps(data).encode()
        headers["content-type"] = "application/json"
    conn = http.client.HTTPConnection("127.0.0.1", portno, timeout=12)
    try:
        conn.request(method, path, body=data, headers=headers)
        response = conn.getresponse()
        body = response.read()
        return response.status, response.headers, body
    finally:
        conn.close()


def cookie_map(headers):
    cookies = headers.get_all("set-cookie") or []
    return dict(cookie.split(";", 1)[0].split("=", 1) for cookie in cookies)


def fresh_login(gateway, issuer, code=None, state_override=None):
    status, headers, _ = request(gateway, "/api/auth/oidc/start")
    assert status == 307 and len(headers.get_all("set-cookie")) == 3, status
    cookies = cookie_map(headers)
    state = cookies["oidc_state"]
    nonce = cookies["oidc_nonce"]
    verifier = cookies["oidc_code_verifier"]
    location = urlsplit(headers["location"])
    params = parse_qs(location.query)
    assert location.hostname == "127.0.0.1" and location.port == issuer
    assert len(state) == len(nonce) == 22 and len(verifier) == 43
    digest = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
    assert params["state"] == [state] and params["nonce"] == [nonce]
    assert params["code_challenge"] == [digest]
    assert all("HttpOnly" in value and "SameSite=lax" in value and "Max-Age=600" in value
               for value in headers.get_all("set-cookie"))
    response = request(issuer, location.path + "?" + location.query)
    assert response[0] == 200
    if code is None:
        code = "valid-code"
    query = urlencode({"code": code, "state": state_override or state})
    return request(gateway, "/api/auth/oidc/callback?" + query,
                   headers={"cookie": "; ".join(f"{name}={value}" for name, value in cookies.items())})


with tempfile.TemporaryDirectory(prefix="9router-oidc-contract-") as work:
    gateway_port, issuer_port = port(), port()
    env = {
        **os.environ,
        "PORT": str(gateway_port),
        "DATA_DIR": work,
        "NINEROUTER_MIGRATION_DATA_DIR": work,
        "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(Path(work) / "fallback.log"),
    }
    for name in ["BASE_URL", "NEXT_PUBLIC_BASE_URL", "JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE",
                 "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "all_proxy", "no_proxy"]:
        env.pop(name, None)
    with subprocess.Popen(["node", "rust/reference/mock-oidc.mjs", str(issuer_port)], cwd=root,
                          env=env, stdout=subprocess.DEVNULL) as issuer:
        with subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL) as gateway:
            try:
                ready(issuer, issuer_port)
                ready(gateway, gateway_port)
                start = request(gateway_port, "/api/auth/oidc/start")
                assert start[0] == 307 and "oidc_not_configured" in start[1]["location"]
                assert request(gateway_port, "/api/auth/oidc/test", "POST", {})[0] == 401
                login = request(gateway_port, "/api/auth/login", "POST", {"password": "123456"})
                assert login[0] == 200, login[0]
                cookie = login[1]["set-cookie"].split(";", 1)[0]
                oidc_settings = {
                    "authMode": "both",
                    "oidcIssuerUrl": f"http://127.0.0.1:{issuer_port}",
                    "oidcClientId": "fixture-client",
                    "oidcClientSecret": "offline-fixture-secret",
                }
                config = request(gateway_port, "/api/settings", "PATCH", oidc_settings, {"cookie": cookie})
                assert config[0] == 200
                tested = json.loads(request(gateway_port, "/api/auth/oidc/test", "POST", {}, {"cookie": cookie})[2])
                assert tested["ok"] and tested["clientSecretTested"] and tested["clientSecretValid"]
                rejected = json.loads(request(gateway_port, "/api/auth/oidc/test", "POST",
                                  {"clientSecret": "incorrect-offline-secret"}, {"cookie": cookie})[2])
                assert not rejected["ok"] and rejected["clientSecretValid"] is False
                status, headers, _ = fresh_login(gateway_port, issuer_port)
                assert status == 307 and urlsplit(headers["location"]).path == "/dashboard", headers["location"]
                cookies = cookie_map(headers)
                assert set(cookies) == {"auth_token", "oidc_state", "oidc_nonce", "oidc_code_verifier"}
                status, _, data = request(gateway_port, "/api/auth/status", headers={
                    "cookie": f"auth_token={cookies['auth_token']}"})
                claims = json.loads(data)
                assert status == 200 and claims["authenticated"] and claims["oidcLogin"]
                assert claims["oidcName"] == "Fixture Auth" and claims["oidcEmail"] == "fixture@example.invalid"
                for code in ["wrong-audience", "wrong-signature"] + ([] if server == "original" else ["wrong-nonce"]):
                    invalid = fresh_login(gateway_port, issuer_port, code)
                    assert invalid[0] == 307 and urlsplit(invalid[1]["location"]).path == "/login"
                    assert "auth_token" not in cookie_map(invalid[1])
                invalid = fresh_login(gateway_port, issuer_port, state_override="wrong-state")
                assert invalid[0] == 307 and "oidc_invalid_state" in invalid[1]["location"]
                if server != "original":
                    fallback = Path(work, "fallback.log").read_text()
                    assert "/api/auth/oidc" not in fallback
                print(f"{server}: OIDC discovery, PKCE, cookie/JWT login and reject cases passed")
            finally:
                gateway.terminate()
                issuer.terminate()
