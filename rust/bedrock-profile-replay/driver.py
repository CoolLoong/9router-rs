from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import parse_qs, quote, urlsplit
from xml.sax.saxutils import escape as xml_escape
import zlib


ROOT = Path(__file__).resolve().parents[2]
MODE, *ARGS = sys.argv[1:]
REFERENCE = MODE == "reference"
BINARY = None if REFERENCE else str(Path(ARGS.pop(0)).resolve())
CHECK = ARGS[0]
MOCK = "http://127.0.0.1:52989/bedrock-profile"
MODEL = "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
SDK_ACCESS = ("AKIA_STATIC_FIXTURE_SHARED", "offline-static-secret-config", None)
DEFAULT = ("AKIA_STATIC_FIXTURE_DEFAULT", "offline-default-secret-local", None)
SPACES = ("ASIA_STATIC_FIXTURE_SPACED", "offline-spaced-secret-local", "offline-spaced-session")
BOOTSTRAP = ("AKIA_FIXTURE_BOOTSTRAP", "offline-bootstrap-secret-local", None)
INNER = ("ASIA_FIXTURE_INNER", "offline-inner-secret-local", "offline-inner-session-local")
OUTER = ("ASIA_FIXTURE_OUTER", "offline-outer-secret-local", "offline-outer-session-local")
COLD = ("ASIA_FIXTURE_COLD", "offline-cold-secret-local", "offline-cold-session-local")
REDIRECT = ("ASIA_FIXTURE_REDIRECT", "offline-redirect-secret-local", "offline-redirect-session-local")
SSO = ("ASIA_FIXTURE_NAMED_SSO", "offline-named-sso-secret", "offline-named-sso-session")
SSO_LEGACY = ("ASIA_FIXTURE_LEGACY_SSO", "offline-legacy-sso-secret", "offline-legacy-sso-session")
SSO_RENEW = ("ASIA_FIXTURE_RENEWED_SSO", "offline-renewed-sso-secret", "offline-renewed-sso-session")
SSO_TOKEN = "offline-sso-oidc-access-token"
LEGACY_TOKEN = "offline-sso-legacy-access-token"
NEW_TOKEN = "offline-new-sso-access-token"
NEW_REFRESH = "offline-rotated-sso-refresh-token"
EXTERNAL_TOKEN = "offline-external-sso-access-token"
SSO_START = "https://fixture-office.awsapps.com/start"
LEGACY_START = "https://fixture-legacy.awsapps.com/start"
EXPIRED_START = "https://fixture-expired.awsapps.com/start"
SSO_ROLES = {
    "FixtureAccess": ("eu-west-1", SSO_TOKEN, SSO),
    "LegacyAccess": ("us-east-2", LEGACY_TOKEN, SSO_LEGACY),
    "FixtureRenewal": ("eu-west-1", NEW_TOKEN, SSO_RENEW),
    "AlternateRenewal": ("eu-west-1", NEW_TOKEN, SSO_RENEW),
    "ExternalRenewal": ("eu-west-1", EXTERNAL_TOKEN, SSO_RENEW),
    "UnrotatedRenewal": ("eu-west-1", NEW_TOKEN, SSO_RENEW),
}
DEFINITIONS = {
    "inner": (BOOTSTRAP, INNER, "inner-session", None, "1900", "eu-west-2"),
    "outer": (INNER, OUTER, "outer-session", "offline-outer-id", "2100", "eu-west-2"),
    "cold": (BOOTSTRAP, COLD, "cold-session", None, "3600", "ca-central-1"),
    "redirect": (BOOTSTRAP, REDIRECT, "redirect-session", None, "3600", "us-west-1"),
}
CASE = [
    ("mixed", "us-west-2", SDK_ACCESS),
    ("default", "us-east-2", DEFAULT),
    ("with three spaces", "us-west-1", SPACES),
    ("outer", "us-east-1", OUTER),
    ("sso", "us-east-2", SSO),
    ("sso-legacy", "ap-northeast-1", SSO_LEGACY),
]
if CHECK == "renew":
    CASE = [("sso-refresh", "us-east-2", SSO_RENEW)]
LEDGER = []
PROBLEMS = []
SSO_FILES = {}


def timestamp(seconds):
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat().replace("+00:00", "Z")


def cache_file(directory, key, contents):
    filename = hashlib.sha1(key.encode()).hexdigest() + ".json"
    path = directory / filename
    path.write_text(json.dumps(contents))
    path.chmod(0o600)
    return path


def registration(label, **fields):
    return {"clientId": "offline-registered-" + label, "clientSecret": "offline-client-secret-" + label,
            "refreshToken": "offline-refresh-" + label, "registrationExpiresAt": timestamp(3600), **fields}


def renewal_profiles(config, directory):
    with config.open("a") as aws_config:
        for kind, role in (("refresh", "FixtureRenewal"), ("deny", "FixtureRenewal"), ("redirect", "FixtureRenewal"),
                           ("malformed", "FixtureRenewal"), ("old-registration", "FixtureRenewal"),
                           ("region", "FixtureRenewal"), ("issuer", "FixtureRenewal"),
                           ("no-registration", "FixtureRenewal"), ("symlink", "FixtureRenewal"),
                           ("external", "ExternalRenewal"), ("unrotated", "UnrotatedRenewal")):
            session = "local-" + kind
            start = f"https://fixture-{kind}.awsapps.com/start"
            aws_config.write(f"\n[profile sso-{kind}]\nsso_session = {session}\nsso_account_id = 123456789012\n"
                             f"sso_role_name = {role}\n[sso-session {session}]\nsso_region = eu-west-1\nsso_start_url = {start}\n")
            token = registration(kind, startUrl=start, region="eu-west-1", accessToken="offline-stale-access-" + kind,
                                 expiresAt=timestamp(-600), originalField="preserve")
            if kind == "old-registration":
                token["registrationExpiresAt"] = timestamp(-10)
            if kind == "region":
                token["region"] = "us-east-1"
            if kind == "issuer":
                token["startUrl"] = "https://other-fixture.awsapps.com/start"
            if kind == "no-registration":
                del token["clientSecret"]
            SSO_FILES[kind] = cache_file(directory, session, token)
            if kind == "symlink":
                original = directory / "symlink-target.json"
                SSO_FILES[kind].rename(original)
                SSO_FILES[kind].symlink_to(original)
        aws_config.write("\n[profile sso-refresh-alt]\nsso_session = local-refresh\nsso_account_id = 123456789012\n"
                         "sso_role_name = AlternateRenewal\n")


def role(name):
    return f"arn:aws:iam::123456789012:role/path/{name}"


def setup(scratch):
    home = scratch / "offline-home"
    home.mkdir()
    credentials = scratch / "offline-credentials"
    config = home / "offline-config"
    process_marker = scratch / "credential-process-ran"
    config.write_text(f"""\
[default]
aws_access_key_id = {DEFAULT[0]}
aws_secret_access_key = {DEFAULT[1]}
region = us-west-1
[profile mixed]
aws_access_key_id = AKIA_IGNORED_CONFIG_KEY
aws_secret_access_key = {SDK_ACCESS[1]}  # preserved key
region = ap-northeast-1
[profile inner]
role_arn = {role('inner')}
source_profile = bootstrap
role_session_name = inner-session
region = ap-south-1
duration_seconds = 1900
[profile outer]
role_arn = {role('outer')}
source_profile = inner
role_session_name = outer-session
region = eu-west-2
external_id = offline-outer-id
duration_seconds = 2100
[profile cold]
role_arn = {role('cold')}
source_profile = bootstrap
role_session_name = cold-session
region = ca-central-1
[profile redirect]
role_arn = {role('redirect')}
source_profile = bootstrap
role_session_name = redirect-session
region = us-west-1
[profile process]
credential_process = /usr/bin/touch {process_marker}
[profile cyclic-a]
role_arn = {role('cyclic-a')}
source_profile = cyclic-b
[profile cyclic-b]
role_arn = {role('cyclic-b')}
source_profile = cyclic-a
[profile metadata]
role_arn = {role('metadata')}
credential_source = Ec2InstanceMetadata
[profile sso]
sso_session = local-office
sso_account_id = 123456789012
sso_role_name = FixtureAccess
[sso-session local-office]
sso_region = eu-west-1
sso_start_url = {SSO_START}
sso_registration_scopes = sso:account:access
[profile sso-legacy]
sso_region = us-east-2
sso_start_url = {LEGACY_START}
sso_account_id = 123456789012
sso_role_name = LegacyAccess
[profile sso-expired]
sso_start_url = {EXPIRED_START}
sso_region = us-east-2
sso_account_id = 123456789012
sso_role_name = LegacyAccess
[profile sso-missing]
sso_session = missing-office
sso_account_id = 123456789012
sso_role_name = FixtureAccess
[sso-session missing-office]
sso_start_url = https://fixture-missing.awsapps.com/start
sso_region = us-east-2
""")
    credentials.write_text(f"""\
[mixed]
aws_access_key_id = {SDK_ACCESS[0]}
[bootstrap]
aws_access_key_id = {BOOTSTRAP[0]}
aws_secret_access_key = {BOOTSTRAP[1]}
[with three spaces]
aws_access_key_id = {SPACES[0]}
aws_secret_access_key = {SPACES[1]}
aws_session_token = {SPACES[2]}
""")
    token_cache = home / ".aws/sso/cache"
    token_cache.mkdir(parents=True)
    for name, start, region, token, ttl in [
        ("local-office", SSO_START, "eu-west-1", SSO_TOKEN, 7200),
        (LEGACY_START, LEGACY_START, "us-east-2", LEGACY_TOKEN, 7200),
        (EXPIRED_START, EXPIRED_START, "us-east-2", "offline-sso-expired", -600),
    ]:
        token = {"startUrl": start, "region": region, "accessToken": token, "expiresAt": timestamp(ttl)}
        cache_file(token_cache, name, registration("legacy", **token) if ttl < 0 else token)
    if CHECK.startswith("renew"):
        renewal_profiles(config, token_cache)
    return home, credentials, process_marker


def signed(handler, body, expected, service, region, path):
    auth = handler.headers["Authorization"] or ""
    parsed = re.fullmatch(r"AWS4-HMAC-SHA256 Credential=([^/]+)/([^,]+),\s*SignedHeaders=([^,]+),\s*Signature=([0-9a-f]{64})", auth)
    assert parsed, "Missing AWS signature"
    key, scope, allowed, signature = parsed.groups()
    assert key == expected[0], "Wrong access key precedence"
    assert handler.headers["X-Amz-Security-Token"] == expected[2], "Wrong session token"
    day, scope_region, scope_service, suffix = scope.split("/")
    assert (scope_region, scope_service, suffix) == (region, service, "aws4_request"), "Wrong signing scope"
    headers = "".join(name + ":" + " ".join(handler.headers[name].split()) + "\n" for name in allowed.split(";"))
    canonical = "POST\n" + path + "\n\n" + headers + "\n" + allowed + "\n" + hashlib.sha256(body).hexdigest()
    signable = "AWS4-HMAC-SHA256\n" + handler.headers["x-amz-date"] + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    hashkey = ("AWS4" + expected[1]).encode()
    for field in (day, region, service, suffix):
        hashkey = hmac.new(hashkey, field.encode(), hashlib.sha256).digest()
    assert hmac.compare_digest(hmac.new(hashkey, signable.encode(), hashlib.sha256).hexdigest(), signature), "AWS signature differs"


def event(kind, fields):
    header = b""
    for name, value in {":message-type":"exception", ":exception-type":kind}.items():
        name, value = name.encode(), value.encode()
        header += bytes([len(name)]) + name + b"\x07" + struct.pack(">H", len(value)) + value
    data = json.dumps(fields).encode()
    before = struct.pack(">II", 16 + len(header) + len(data), len(header))
    packet = before + struct.pack(">I", zlib.crc32(before)) + header + data
    return packet + struct.pack(">I", zlib.crc32(packet))


class MockAWS(ThreadingHTTPServer):
    allow_reuse_address = True


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, payload, mime="application/json", extra=None):
        if isinstance(payload, dict):
            payload = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(payload)))
        for name, value in (extra or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        try:
            if self.path.startswith("/bedrock-profile/sts/") or self.path.rstrip("/") == "/bedrock-profile/sdk-sts":
                self.sts(raw)
            elif self.path.startswith(("/bedrock-profile/oidc/", "/bedrock-profile/sdk-oidc/")):
                self.oidc(raw)
            elif self.path.startswith("/bedrock/model/"):
                self.infer(raw)
            else:
                raise AssertionError("Unexpected fixture network call " + self.path)
        except (AssertionError, ValueError, KeyError, TypeError) as error:
            PROBLEMS.append(str(error) or "AWS fixture invariant failed")
            self.send(422, {"message": "Mock AWS invariant failed"})

    def oidc(self, raw):
        path = "/bedrock-profile/sdk-oidc/token" if REFERENCE else "/bedrock-profile/oidc/eu-west-1/token"
        assert self.path == path and self.headers["Content-Type"] == "application/json"
        assert self.headers["Authorization"] is None and self.headers["x-amz-sso_bearer_token"] is None
        data = json.loads(raw)
        kind = data["clientId"].removeprefix("offline-registered-")
        assert kind in {"refresh", "deny", "redirect", "malformed", "external", "unrotated"}
        assert data == {"clientId": "offline-registered-" + kind, "clientSecret": "offline-client-secret-" + kind,
                        "grantType": "refresh_token", "refreshToken": "offline-refresh-" + kind}
        LEDGER.append("refresh:" + kind)
        time.sleep(.15)
        if kind == "deny":
            self.send(400, {"error": "invalid_grant", "error_description": " ".join(data.values())})
        elif kind == "redirect":
            self.send(307, {}, extra={"Location": MOCK + "/never"})
        elif kind == "malformed":
            self.send(200, {"accessToken": NEW_TOKEN, "expiresIn": 0, "refreshToken": NEW_REFRESH})
        else:
            if kind == "external":
                path = SSO_FILES[kind]
                token = json.loads(path.read_text())
                staged = path.with_suffix(".external")
                staged.write_text(json.dumps(token | {"accessToken": EXTERNAL_TOKEN, "expiresAt": timestamp(7200)}))
                staged.chmod(0o600)
                staged.replace(path)
            outcome = {"accessToken": NEW_TOKEN, "expiresIn": 3600, "tokenType": "Bearer"}
            if kind != "unrotated":
                outcome["refreshToken"] = NEW_REFRESH
            self.send(200, outcome)

    def do_GET(self):
        try:
            url = urlsplit(self.path)
            args = parse_qs(url.query)
            role_name, = args["role_name"]
            expected_region, token, credential = SSO_ROLES[role_name]
            assert args["account_id"] == ["123456789012"]
            if REFERENCE:
                assert url.path == "/bedrock-profile/sdk-sso/federation/credentials"
            else:
                assert url.path == f"/bedrock-profile/sso/{expected_region}/federation/credentials"
            assert self.headers["x-amz-sso_bearer_token"] == token
            assert self.headers["Authorization"] is None
            LEDGER.append(role_name)
            current = datetime.now(timezone.utc) + timedelta(hours=1)
            self.send(200, {"roleCredentials":{
                "accessKeyId":credential[0],"secretAccessKey":credential[1],"sessionToken":credential[2],
                "expiration":int(current.timestamp() * 1000),
            }})
        except (AssertionError, ValueError, KeyError, TypeError) as error:
            PROBLEMS.append(str(error) or "AWS SSO fixture invariant failed")
            self.send(422, {"message":"Mock AWS SSO invariant failed"})

    def sts(self, raw):
        form = parse_qs(raw.decode())
        assert form["Action"] == ["AssumeRole"] and form["Version"] == ["2011-06-15"]
        name = form["RoleArn"][0].rsplit("/", 1)[-1]
        base, selected, session, external_id, duration, region = DEFINITIONS[name]
        assert form["RoleArn"] == [role(name)]
        assert form["RoleSessionName"] == [session]
        assert form.get("ExternalId") == ([external_id] if external_id else None)
        assert form["DurationSeconds"] == [duration]
        expected_path = self.path if REFERENCE else "/"
        if not REFERENCE:
            assert self.path == "/bedrock-profile/sts/" + region
            assert self.headers["Host"] == f"sts.{region}.amazonaws.com"
        signed(self, raw, base, "sts", region, expected_path)
        LEDGER.append(name)
        if name == "redirect":
            self.send(307, {}, extra={"Location": MOCK + "/never"})
            return
        if name == "cold":
            time.sleep(.15)
        expires = (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat().replace("+00:00", "Z")
        access, key, token = map(xml_escape, selected)
        xml = f"""<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
<AssumeRoleResult><Credentials><AccessKeyId>{access}</AccessKeyId><SecretAccessKey>{key}</SecretAccessKey>
<SessionToken>{token}</SessionToken><Expiration>{expires}</Expiration></Credentials></AssumeRoleResult>
<ResponseMetadata><RequestId>fixture</RequestId></ResponseMetadata></AssumeRoleResponse>"""
        self.send(200, xml.encode(), "text/xml")

    def infer(self, raw):
        data = json.loads(raw)
        text = data["messages"][0]["content"]
        label = text if isinstance(text, str) else text[0]["text"]
        key_name = label.split(":", 1)[0]
        selected = EXPECTED[key_name]
        region, expected = selected[0], selected[1]
        assert self.headers["Host"] == f"bedrock-runtime.{region}.amazonaws.com"
        assert "model" not in data and data["anthropic_version"] == "bedrock-2023-05-31"
        path = "/".join(quote(part, safe="-_.~") for part in urlsplit(self.path).path.removeprefix("/bedrock").split("/"))
        signed(self, raw, expected, "bedrock", region, path)
        if label.endswith(":error"):
            self.send(422, {"message": "redact " + " ".join(credential for credential in expected if credential)})
            return
        if label.endswith(":sse-error"):
            msg = "redact " + " ".join(credential for credential in expected if credential)
            self.send(200, event("throttlingException", {"message":msg}), "application/vnd.amazon.eventstream")
            return
        self.send(200, {
            "id":"msg_local_bedrock_profile", "model":MODEL, "type":"message",
            "role":"assistant", "content":[{"type":"text","text":"Local profile accepted"}],
            "stop_reason":"end_turn", "stop_sequence":None, "usage":{"input_tokens":3,"output_tokens":2},
        })


EXPECTED = {name: (region, credential) for name, region, credential in CASE} | {
    "outer-other-region": ("us-east-2", OUTER), "cold": ("us-east-2", COLD),
    **{name: ("us-east-2", SSO_RENEW) for name in (
        "sso-refresh", "sso-refresh-alt", "sso-external", "sso-unrotated"
    )},
}


def select(database, name, region):
    account = {
        "apiKey": "stray-static-not-selected", "apiKeyOptionalWith": "profile",
        "providerSpecificData": {"profile":name, "region":region, "accessKeyId":"STRAYFIXTURE"},
    }
    with sqlite3.connect(database) as sql:
        sql.execute("UPDATE providerConnections SET data=? WHERE id='bedrock-profile-fixture'", (json.dumps(account),))
    return account


def request(port, payload=None):
    body = None if payload is None else json.dumps(payload)
    method = "GET" if payload is None else "POST"
    endpoint = "/api/health" if payload is None else "/v1/messages"
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        client.request(method, endpoint, body, {"Authorization":"Bearer offline-chat-key", "Content-Type":"application/json"})
        response = client.getresponse()
        return response.status, response.read().decode()
    finally:
        client.close()


def invoke(port, label, stream=False):
    return request(port, {"model":"br/" + MODEL, "max_tokens":20, "stream":stream,
                         "messages":[{"role":"user","content":label}]})


def boot(binary, data, env):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    app = subprocess.Popen([binary, "--no-browser"], cwd=ROOT, env=env | {"PORT":str(port)},
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 10
    while app.poll() is None and time.monotonic() < deadline:
        try:
            if request(port)[0] == 200:
                return app, port
        except OSError:
            time.sleep(.04)
    raise RuntimeError("Native Bedrock profile fixture did not start")


def run_compare(port, database):
    results = []
    for name, region, _ in CASE:
        select(database, name, region)
        status, raw = invoke(port, name)
        assert status == 200, (name, status, raw, PROBLEMS)
        assert json.loads(raw)["content"][0]["text"] == "Local profile accepted"
        results.append({"name":name,"mode":"profile","region":region,"accepted":True})
    if CHECK == "compare":
        select(database, "outer", "us-east-2")
        assert invoke(port, "outer-other-region")[0] == 200
    return results


def run_strict(port, database, process_marker):
    for name, message in [("process","credential_process"), ("cyclic-a","cyclic"),
        ("metadata","credential_source"), ("../outside","Invalid AWS profile"), ("missing","named AWS profile"),
        ("sso-expired","session has expired"), ("sso-missing","session is missing or invalid")]:
        select(database, name, "us-east-2")
        status, body = invoke(port, name)
        assert status in (502,503) and message in body, (name,status,body)
    assert process_marker.exists() and not LEDGER
    select(database, "redirect", "us-east-2")
    status, body = invoke(port, "redirect")
    assert status in (502,503) and "AWS STS AssumeRole failed" in body
    assert LEDGER == ["redirect"]
    select(database, "cold", "us-east-2")
    with ThreadPoolExecutor(max_workers=5) as workers:
        jobs = [workers.submit(invoke, port, "cold") for _ in range(5)]
        assert all(status == 200 for status, _ in (job.result() for job in jobs))
    assert LEDGER == ["redirect","cold"]
    for label, stream in [("cold:error",False), ("cold:sse-error",True)]:
        status, body = invoke(port, label, stream)
        assert status == (200 if stream else 422)
        assert "[redacted]" in body and all(value not in body for value in COLD), body
    print("Bedrock named profiles reused one STS grant, rejected invalid process credentials and blocked role cycles and redirects without Node")


def assert_renewed(kind, access, refresh):
    path = SSO_FILES[kind]
    token = json.loads(path.read_text())
    assert token["accessToken"] == access and token["refreshToken"] == refresh
    assert token["clientId"] == "offline-registered-" + kind and token["originalField"] == "preserve"
    assert token["region"] == "eu-west-1" and token["startUrl"] == f"https://fixture-{kind}.awsapps.com/start"
    assert (datetime.fromisoformat(token["expiresAt"]) - datetime.now(timezone.utc)).total_seconds() > 3500
    assert path.stat().st_mode & 0o777 == 0o600


def run_renew_strict(port, db, alternate, alternate_db):
    select(db, "sso-refresh", "us-east-2")
    select(alternate_db, "sso-refresh-alt", "us-east-2")
    with ThreadPoolExecutor(max_workers=8) as workers:
        jobs = [workers.submit(invoke, port if i % 2 else alternate, "sso-refresh" if i % 2 else "sso-refresh-alt")
                for i in range(8)]
        assert all(status == 200 for status, _ in (job.result() for job in jobs)), (LEDGER, PROBLEMS)
    assert LEDGER[0] == "refresh:refresh" and sorted(LEDGER[1:]) == ["AlternateRenewal", "FixtureRenewal"]
    assert_renewed("refresh", NEW_TOKEN, NEW_REFRESH)
    LEDGER.clear()

    cold = {kind: path.read_bytes() for kind, path in SSO_FILES.items()}
    for kind, message in (("old-registration", "session has expired"), ("no-registration", "session has expired"),
                          ("region", "session is missing or invalid"), ("issuer", "session is missing or invalid"),
                          ("symlink", "Could not renew"), ("redirect", "Could not renew"), ("malformed", "Could not renew")):
        select(db, "sso-" + kind, "us-east-2")
        status, body = invoke(port, "sso-" + kind)
        assert status in (502, 503) and message in body, (kind, status, body)
        assert SSO_FILES[kind].read_bytes() == cold[kind]
    assert LEDGER == ["refresh:redirect", "refresh:malformed"]

    select(db, "sso-deny", "us-east-2")
    with ThreadPoolExecutor(max_workers=6) as workers:
        jobs = [workers.submit(invoke, port, "sso-deny") for _ in range(6)]
        for job in jobs:
            status, body = job.result()
            assert status in (502, 503) and "Could not renew" in body
            assert not any(secret in body for secret in ("offline-stale-access-deny", "offline-refresh-deny",
                                                         "offline-client-secret-deny", "offline-registered-deny"))
    assert LEDGER[-1:] == ["refresh:deny"] and LEDGER.count("refresh:deny") == 1
    assert SSO_FILES["deny"].read_bytes() == cold["deny"]
    LEDGER.clear()

    for kind, access, refresh, role in (("external", EXTERNAL_TOKEN, "offline-refresh-external", "ExternalRenewal"),
                                        ("unrotated", NEW_TOKEN, "offline-refresh-unrotated", "UnrotatedRenewal")):
        select(db, "sso-" + kind, "us-east-2")
        assert invoke(port, "sso-" + kind)[0] == 200, (LEDGER, PROBLEMS)
        assert LEDGER[-2:] == ["refresh:" + kind, role]
        assert_renewed(kind, access, refresh)
    print("Bedrock refreshed one shared named SSO cache across two native gateways and signed both role requests.")
    print("Invalid registrations, redirects and rejected grants stayed local; external cache updates took precedence.")


def seed_database(data, home):
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], cwd=ROOT,
                   env={"PATH":os.environ["PATH"], "HOME":str(home)}, check=True, stderr=subprocess.DEVNULL)
    path = data / "db/data.sqlite"
    with sqlite3.connect(path) as sql:
        sql.execute("""INSERT INTO providerConnections(id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
             VALUES ('bedrock-profile-fixture','bedrock','apikey','Profile Fixture',1,1,'{}','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')""")
    return path


with tempfile.TemporaryDirectory(prefix="9router-bedrock-profiles-") as temp:
    scratch = Path(temp)
    home, credentials, process_marker = setup(scratch)
    data = scratch / "data"
    empty_path = scratch / "empty-path"
    empty_path.mkdir()
    env = {
        "HOME":str(home), "USERPROFILE":str(home),
        "PATH":os.environ["PATH"] if REFERENCE else str(empty_path),
        "AWS_CONFIG_FILE":"~/offline-config","AWS_SHARED_CREDENTIALS_FILE":str(credentials),
        "AWS_ACCESS_KEY_ID":"IGNORED_ENV_ACCESS", "AWS_SECRET_ACCESS_KEY":"IGNORED_ENV_SECRET",
        "AWS_REGION":"us-west-2","AWS_EC2_METADATA_DISABLED":"true",
        "DATA_DIR":str(data),"NINEROUTER_MIGRATION_FIXTURE":"1",
        "RUST_ANYTHING_FALLBACK_LOG":str(scratch / "fallbacks"),
    }
    mock = MockAWS(("127.0.0.1", 52989), Provider)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    apps = []
    try:
        if REFERENCE:
            cases = [{"name":name,"account":{"providerSpecificData":{"profile":name,"region":region}},
                      "expected":chosen} for name, region, chosen in CASE]
            env["AWS_ENDPOINT_URL_STS"] = MOCK + "/sdk-sts"
            env["AWS_ENDPOINT_URL_SSO"] = MOCK + "/sdk-sso"
            env["AWS_ENDPOINT_URL_SSO_OIDC"] = MOCK + "/sdk-oidc"
            env["AWS_CONFIG_FILE"] = str(home / "offline-config")
            result = subprocess.run(["node","--no-warnings",str(Path(__file__).with_name("original.mjs"))],
                                    input=json.dumps(cases), cwd=ROOT, env=env, text=True, capture_output=True, timeout=14)
            assert result.returncode == 0, (result.stderr, PROBLEMS)
            summaries = json.loads(result.stdout)
        else:
            database = seed_database(data, home)
            app, port = boot(BINARY, data, env)
            apps.append(app)
            if CHECK == "strict":
                run_strict(port, database, process_marker)
            elif CHECK == "renew-strict":
                alternate_data = scratch / "alternate-data"
                alternate_db = seed_database(alternate_data, home)
                alternate_app, alternate = boot(BINARY, alternate_data, env | {"DATA_DIR": str(alternate_data),
                                                "RUST_ANYTHING_FALLBACK_LOG": str(scratch / "alternate-fallbacks")})
                apps.append(alternate_app)
                run_renew_strict(port, database, alternate, alternate_db)
                assert not (scratch / "alternate-fallbacks").read_bytes()
            else:
                summaries = run_compare(port, database)
            assert (scratch / "fallbacks").read_bytes() == b""
        assert not PROBLEMS, PROBLEMS
        if CHECK == "compare":
            assert LEDGER == ["inner","outer","FixtureAccess","LegacyAccess"], LEDGER
            print(json.dumps({"profiles":summaries,"aws":LEDGER}, separators=(",", ":")))
        if CHECK == "renew":
            assert LEDGER == ["refresh:refresh", "FixtureRenewal"], (LEDGER, PROBLEMS)
            assert_renewed("refresh", NEW_TOKEN, NEW_REFRESH)
            print(json.dumps({"profiles":summaries,"aws":LEDGER}, separators=(",", ":")))
    finally:
        for app in apps:
            app.terminate()
            try:
                app.wait(timeout=4)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait(timeout=2)
        mock.shutdown()
        mock.server_close()
