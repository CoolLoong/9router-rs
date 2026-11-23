import base64
import email.utils
import hashlib
import hmac
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

root = Path(__file__).resolve().parents[2]
server = sys.argv[1]
command = ["sh", "rust/reference/server.sh"] if server == "original" else [server]


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(port, path, method="GET", body=None, cookie=None, headers=None):
    headers = {} if headers is None else headers.copy()
    if cookie is not None:
        headers["cookie"] = cookie
    if body is not None:
        headers["content-type"] = "application/json"
        body = json.dumps(body).encode()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read()), response.headers
    finally:
        connection.close()


def unpack(segment):
    return base64.urlsafe_b64decode(segment + "=" * (-len(segment) % 4))


def login(port, password, secure=False):
    headers = {"x-forwarded-proto": "https"} if secure else {}
    code, payload, response = request(port, "/api/auth/login", "POST", {"password": password}, headers=headers)
    assert code == 200 and payload == {"success": True, "mustChangePassword": False}, code
    cookies = response.get_all("set-cookie")
    assert response["cache-control"] == "no-store" and len(cookies) == 1
    cookie, *parts = cookies[0].split("; ")
    assert cookie.startswith("auth_token=")
    assert all(part in parts for part in ["Path=/", "Max-Age=86400", "HttpOnly", "SameSite=lax"])
    assert ("Secure" in parts) == secure
    expires = next(part.removeprefix("Expires=") for part in parts if part.startswith("Expires="))
    assert 86397 < (email.utils.parsedate_to_datetime(expires) - datetime.now(timezone.utc)).total_seconds() <= 86400
    token = cookie.split("=", 1)[1]
    header, payload, signature = token.split(".")
    signed = (header + "." + payload).encode()
    expected = hmac.new(b"migration-fixture-jwt-secret", signed, hashlib.sha256).digest()
    assert hmac.compare_digest(unpack(signature), expected)
    assert json.loads(unpack(header)) == {"alg": "HS256"}
    claims = json.loads(unpack(payload))
    assert claims["authenticated"] is True and claims["exp"] - claims["iat"] == 86400
    return cookie


def update(port, cookie, values, expected=200):
    code, payload, response = request(port, "/api/settings", "PATCH", values, cookie=cookie)
    assert code == expected, code
    if expected == 200:
        assert response["cache-control"] == "no-store"
        assert "password" not in payload and "oidcClientSecret" not in payload
    return payload


with tempfile.TemporaryDirectory(prefix="9router-dashboard-contract-") as folder:
    port = free_port()
    env = {
        **os.environ,
        "PORT": str(port),
        "TMPDIR": folder,
        "NINEROUTER_MIGRATION_FIXTURE": "1",
        "NINEROUTER_MIGRATION_DATA_DIR": folder,
    }
    for key in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "TRUST_PROXY",
                "RUST_ANYTHING_FALLBACK_LOG"]:
        env.pop(key, None)
    process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)
        code, payload, response = request(port, "/api/auth/login", "POST", {"password": "123456"},
                                          headers={"x-forwarded-for": "192.0.2.51"})
        assert code == 403 and payload["mustChangePassword"] and not response.get_all("set-cookie")
        cookie = login(port, "123456")
        secure_cookie = login(port, "123456", secure=True)
        assert request(port, "/api/auth/status", cookie=secure_cookie)[1]["authenticated"] is True
        assert request(port, "/api/settings", cookie=cookie)[1]["hasPassword"] is False
        assert request(port, "/api/auth/oidc/test", "POST", {}, cookie=cookie)[:2] == (
            400, {"error": "Issuer URL is required"}
        )

        patched = update(port, cookie, {
            "newPassword": "offline-dashboard-one", "password": "ignored",
            "mitmSudoEncrypted": "ignored",
        })
        assert "newPassword" not in patched and "mitmSudoEncrypted" not in patched
        assert request(port, "/api/settings", cookie=cookie)[1]["hasPassword"] is True
        assert request(port, "/api/auth/login", "POST", {"password": "123456"})[0] == 401
        update(port, cookie, {"newPassword": "offline-dashboard-invalid"}, expected=400)
        cookie = login(port, "offline-dashboard-one")

        update(port, cookie, {"authMode": "both", "samlEntryPoint": "https://offline.example.invalid",
                              "samlCert": "offline-fixture-certificate"})
        cookie = login(port, "offline-dashboard-one")
        assert request(port, "/api/auth/status", cookie=cookie)[1]["authenticated"] is True
        update(port, cookie, {"authMode": "password", "samlEntryPoint": "", "samlCert": ""})

        update(port, cookie, {"currentPassword": "offline-dashboard-one",
                              "newPassword": "offline-dashboard-two", "comboStrategies": {}})
        cookie = login(port, "offline-dashboard-two")
        code, payload, response = request(port, "/api/auth/logout", "POST", cookie=cookie)
        assert code == 200 and payload == {"success": True} and response["cache-control"] == "no-store"
        cookies = response.get_all("set-cookie")
        assert cookies and [entry.split("=", 1)[0] for entry in cookies] == [
            "auth_token", "oidc_state", "oidc_nonce", "oidc_code_verifier"
        ]
        assert all("Expires=Thu, 01 Jan 1970 00:00:00 GMT" in entry for entry in cookies)
        assert request(port, "/api/auth/status")[1]["authenticated"] is False
        print(f"{server}: dashboard cookie, shared JWT verification, bidirectional bcrypt and logout passed")
    finally:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
