import http.server
import json
import pathlib
import subprocess
import sys
import threading

MODE, *ARGS, CASE = sys.argv[1:]
PROJECT = pathlib.Path(__file__).resolve().parents[2]
VERSION = json.loads((PROJECT / "package.json").read_text())["version"]
ORDINARY_KEY = "ksk_fixture_router_only"
PROFILE_TOKEN = "fixture-kiro-profile-token"
PROFILE_ARN = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE"
OTHER_PROFILE_ARN = "arn:aws:codewhisperer:us-east-1:123456789012:profile/DIFFERENT"
SHARED_PROFILE_ARN = "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX"
OBSERVED = []
MODELS = [
    {"modelId": "auto", "modelName": "Auto"},
    {"modelId": "sonnet-thinking-agentic", "modelName": "  Sonnet  ", "rateMultiplier": "1.25",
     "tokenLimits": {"maxInputTokens": "300000"}, "description": "Fixture context"},
    {"id": "pass-through-v1", "modelName": "", "rateMultiplier": 2.15, "tokenLimits": {"maxInputTokens": 0}},
    {"id": "explicit-one", "rateMultiplier": 1.0000000001},
    {"id": "null-rate", "rateMultiplier": None},
    {"id": "non-finite-rate", "rateMultiplier": "Infinity"},
    {"modelName": "Skip unnamed entry"},
]


def key(region=None, field="accessToken", token=ORDINARY_KEY):
    details = {"authMethod": "api_key"}
    if region is not None:
        details["region"] = region
    return {"provider": "kiro", "authType": "api_key", field: token, "providerSpecificData": details}


def profile(method="idc", arn=PROFILE_ARN):
    return {"provider": "kiro", "authType": "oauth", "accessToken": PROFILE_TOKEN,
            "providerSpecificData": {"authMethod": method, "profileArn": arn}}


def accounts():
    if CASE == "key-eu":
        return [key("eu-west-2")]
    if CASE == "key-fallback-field":
        return [key(field="apiKey")]
    if CASE == "builder-oauth":
        return [profile("builder-id")]
    if CASE == "profile-cache":
        return [profile(), profile()]
    if CASE == "key-cache":
        return [key(), key()]
    if CASE == "regional-cache-isolation":
        return [key(), key("eu-west-2")]
    if CASE == "profile-cache-isolation":
        return [profile(), profile(arn=OTHER_PROFILE_ARN)]
    if CASE == "unsupported-modes":
        return [
            key("us-east-1.invalid"), key("us-gov-west-1"), key("eu-west-2/path"),
            key("eu-west-2", token="bad\r\ncredential"),
            key() | {"refreshToken": "fixture-unsupported"},
            {**key(), "providerSpecificData": {"authMethod": "api_key", "connectionProxyEnabled": True}},
            {**key(), "providerSpecificData": {"authMethod": "api_key", "proxyPoolId": "fixture-pool"}},
            profile(arn=SHARED_PROFILE_ARN), profile("external_idp"), profile("imported"),
            profile(arn="arn:aws:codewhisperer:eu-west-2:123456789012:profile/FIXTURE"),
            profile() | {"refreshToken": "fixture-unsupported"},
        ]
    if CASE == "idc-oauth":
        return [profile()]
    return [key()]


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def respond(self, valid):
        wire_path = self.path
        OBSERVED.append({"method": self.command, "path": wire_path, "valid": valid})
        if not valid:
            return self.send(422, {"error": "Invalid fixture request"})
        if CASE == "unauthorized":
            return self.send(401, {"message": "Credentials rejected"})
        if CASE == "upstream-error":
            return self.send(502, {})
        if CASE == "wrong-json":
            return self.send(200, b"non-json")
        if CASE == "redirect":
            return self.send(302, {"models": MODELS}, {"Location": "http://127.0.0.1:1/incorrect"})
        self.send(200, {"models": [] if CASE == "empty-models" else MODELS})

    def send(self, status, value, headers=None):
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Content-Type", "application/json")
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def common(self):
        if self.headers.get("Accept") != "application/json":
            return False
        if MODE == "reference":
            return True
        return self.headers.get("User-Agent") == "9Router/" + VERSION and all(
            not self.headers.get(header) for header in [
                "X-Amz-User-Agent", "X-Amzn-Kiro-Agent-Mode", "X-Amzn-Codewhisperer-Optout", "Amz-Sdk-Invocation-Id",
            ])

    def do_GET(self):
        expected = set(["/q/us-east-1/ListAvailableModels?origin=AI_EDITOR"])
        if CASE == "key-eu":
            expected = {"/q/eu-west-2/ListAvailableModels?origin=AI_EDITOR"}
        if CASE == "regional-cache-isolation":
            expected.add("/q/eu-west-2/ListAvailableModels?origin=AI_EDITOR")
        self.respond(
            self.common() and self.path in expected
            and self.headers.get("Authorization") == "Bearer " + ORDINARY_KEY
            and self.headers.get("TokenType") == "API_KEY"
            and self.headers.get("X-Amz-Target") is None
        )

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        arns = {PROFILE_ARN, OTHER_PROFILE_ARN} if CASE == "profile-cache-isolation" else {PROFILE_ARN}
        self.respond(
            self.common() and self.path == "/codewhisperer/us-east-1"
            and self.headers.get("Authorization") == "Bearer " + PROFILE_TOKEN
            and self.headers.get("Content-Type") == "application/x-amz-json-1.0"
            and self.headers.get("X-Amz-Target") == "AmazonCodeWhispererService.ListAvailableModels"
            and self.headers.get("TokenType") is None
            and set(body) == {"origin", "profileArn"}
            and body["origin"] == "AI_EDITOR" and body["profileArn"] in arns
        )


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream) as upstream:
    thread = threading.Thread(target=upstream.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{upstream.server_port}"
    original = ["node", "--no-warnings", str(pathlib.Path(__file__).with_name("original.mjs"))]
    command = original if MODE == "reference" else [ARGS[0]]
    process = subprocess.run([*command, base], check=True, input=json.dumps(accounts()),
                             capture_output=True, text=True, timeout=10)
    result = json.loads(process.stdout)
    upstream.shutdown()
    thread.join()

assert all(entry["valid"] for entry in OBSERVED), OBSERVED
if CASE == "unsupported-modes":
    assert not OBSERVED and all(entry == {"eligible": False, "models": None} for entry in result)
elif CASE.endswith("-cache-isolation"):
    assert len(OBSERVED) == 2 and len(result) == 2 and all(entry["models"] for entry in result)
else:
    assert len(OBSERVED) == 1
    assert all(entry["eligible"] for entry in result)
    models = result[0]["models"]
    if CASE == "empty-models":
        assert models == []
    elif CASE in {"unauthorized", "upstream-error", "wrong-json", "redirect"}:
        assert models is None
    else:
        assert [model["id"] for model in models[:6]] == [
            "auto", "auto-thinking", "sonnet", "sonnet-thinking", "sonnet-agentic", "sonnet-thinking-agentic",
        ]
        assert models[2]["name"] == "Kiro Sonnet (1.3x credit)" and models[2]["contextLength"] == 300000
print(json.dumps({"results": result, "requests": OBSERVED}, sort_keys=True, separators=(",", ":")))
