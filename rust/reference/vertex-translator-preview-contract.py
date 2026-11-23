from contextlib import ExitStack, closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "vertex_preview_fixture", Path(__file__).with_name("zed-cline-models-proxy-contract.py")
)
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)

HOST = "https://aiplatform.googleapis.com"
RAW, TOKEN = "offline-preview-vertex-api-key", "offline-preview-saved-access-token"
SIGNER, ADC_SECRET, ADC_REFRESH = "offline-preview-private-key", "offline-preview-adc-secret", "offline-preview-adc-refresh"
EXPOSED_CONFIG = "https://untrusted.invalid/?token=ignored-saved-destination"
SECRETS = (RAW, TOKEN, SIGNER, ADC_SECRET, ADC_REFRESH, "ignored-saved-destination")
MODEL, PARTNER = "gemini-2.5-flash", "deepseek-ai/deepseek-v3.2-maas"
SA = json.dumps({"type": "service_account", "project_id": "approved-sa", "client_email": "fixture@example.invalid",
                 "private_key": SIGNER})
ADC = json.dumps({"type": "authorized_user", "quota_project_id": "approved-adc", "client_id": "offline-client",
                  "client_secret": ADC_SECRET, "refresh_token": ADC_REFRESH})
PARTNER_PROJECT_ERROR = "Vertex partner models require a project_id. Add it in providerSpecificData or use Service Account JSON."
CALLS = []


class NoRuntimeFetch(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reject(self):
        CALLS.append("unexpected runtime request")
        self.send_response(418)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = do_POST = do_CONNECT = reject


@contextmanager
def original(scratch):
    home = scratch / "original"
    home.mkdir()
    data = home / "data"
    audit = home / "fetch.log"
    env = {"DATA_DIR": str(data), "HOSTNAME": "127.0.0.1", "HOME": str(home),
           "PATH": os.environ["PATH"], "ENABLE_TRANSLATOR": "true", "MODEL_CATALOG_SYNC": "off",
           "DISABLE_BACKGROUND_TOKEN_REFRESH": "1", "VERTEX_FIXTURE_AUDIT": str(audit)}
    subprocess.run([sys.executable, str(ROOT / "rust/reference/seed.py"), str(data)], env=env, check=True,
                   stderr=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        env["PORT"] = str(listener.getsockname()[1])
    db = data / "db/data.sqlite"
    with sqlite3.connect(db) as sql:
        sql.execute("UPDATE providerConnections SET isActive=0")
    rewrite = home / "deny-vertex-fetch.mjs"
    rewrite.write_text("""
import { appendFileSync } from "node:fs";
import { createRequire } from "node:module";
const guard = request => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (["aiplatform.googleapis.com","oauth2.googleapis.com","untrusted.invalid"].includes(url.hostname)) {
    appendFileSync(process.env.VERTEX_FIXTURE_AUDIT, "fetch");
    throw new Error("Preview attempted a runtime request");
  }
  return request(source, options);
};
globalThis.fetch = guard(globalThis.fetch);
const undici = createRequire(REFERENCE_SERVER)("undici");
undici.fetch = guard(undici.fetch);
""".replace("REFERENCE_SERVER", json.dumps(str(ROOT / ".next/standalone/server.js"))))
    with subprocess.Popen(["node", "--import", str(rewrite), ".next/standalone/custom-server.js"], cwd=ROOT, env=env,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
        try:
            port = int(env["PORT"])
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    if shared.get(port, "/api/health")[0] == 200:
                        break
                except OSError:
                    pass
                time.sleep(.03)
            else:
                raise RuntimeError("Pinned Vertex preview server did not start")
            yield port, db
            assert not audit.exists()
        finally:
            process.terminate()
            process.wait(timeout=7)


def save(db, provider, credentials, details=None):
    fields = {**credentials, "providerSpecificData": {"baseUrl": EXPOSED_CONFIG, **(details or {})}}
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,'apikey','Offline Vertex',1,1,?,'2026-11-20T00:00:00Z','2026-11-20T00:00:00Z')""",
                    (provider, provider, json.dumps(fields)))


def snapshot(db):
    with sqlite3.connect(db) as sql:
        return sql.execute("SELECT id,data,updatedAt FROM providerConnections ORDER BY id").fetchall()


def preview(port, provider="vertex", model=MODEL, body=None, code=200, admin=True, original_mode=False):
    if body is None:
        body = {"model": "model-from-intermediate", "messages": [{"role": "user", "content": "离线样本"}]}
    request = {"step": 3, "body": {"provider": provider, "model": model, "body": body}}
    headers = {"Content-Type": "application/json"}
    if admin:
        headers.update(shared.ADMIN)
    with closing(HTTPConnection("127.0.0.1", port, timeout=5)) as connection:
        connection.request("POST", "/api/translator/translate", json.dumps(request).encode(), headers)
        response = connection.getresponse()
        data = response.read()
        if not original_mode:
            assert not any(secret.encode() in data for secret in SECRETS)
        assert response.status == code, (response.status, json.loads(data) if not original_mode else "reference failed")
        value = json.loads(data)
    if code != 200:
        return value["error"]
    assert value["success"] is True
    result = value["result"]
    if original_mode:
        parts = urlsplit(result["url"])
        query = [(name, "[redacted]" if name == "key" else field) for name, field in parse_qsl(parts.query)]
        result["url"] = urlunsplit(parts._replace(query=urlencode(query)))
        if "Authorization" in result["headers"]:
            result["headers"]["Authorization"] = "[redacted]"
    return result


def cases(port, db, original_mode, trap):
    def call(provider="vertex", model=MODEL, body=None, code=200, admin=True):
        return preview(port, provider, model, body, code, admin, original_mode)

    def valid(name, expected_url, headers, *, provider="vertex", model=MODEL, body=None):
        before = snapshot(db)
        result = call(provider, model, body)
        assert result["url"] == HOST + expected_url, name
        assert result["headers"] == {"Content-Type": "application/json", **headers}, name
        assert snapshot(db) == before and not CALLS
        results[name] = result
        return result["body"]

    results = {}
    assert call(admin=False, code=401) == "Unauthorized"
    for provider in ("vertex", "vertex-partner"):
        results[provider + "-missing"] = call(provider, code=400)
        assert results[provider + "-missing"] == f"No active connection for provider: {provider}"
    save(db, "vertex", {"apiKey": RAW})
    content = valid("express-sse", f"/v1/publishers/google/models/{MODEL}:streamGenerateContent?alt=sse&key=%5Bredacted%5D",
                    {"Accept": "text/event-stream"})
    assert content["contents"] == [{"role": "user", "parts": [{"text": "离线样本"}]}] and content["model"] == MODEL

    save(db, "vertex", {"apiKey": RAW}, {"projectId": "approved-raw", "location": "eu"})
    body = {"model": "unchanged-by-discovery", "stream": False, "temperature": 0.7, "top_p": .4,
            "max_tokens": 17, "reasoning_effort": "low", "messages": [{"role": "user", "content": "think"}]}
    config = valid("scoped-gemini-4", "/v1/projects/approved-raw/locations/eu/publishers/google/models/gemini-4-argon"
                   ":generateContent?key=%5Bredacted%5D", {}, model="gemini-4-argon", body=body)["generationConfig"]
    assert "temperature" not in config and "topP" not in config and config["thinkingConfig"]["thinkingLevel"] == "low"

    save(db, "vertex", {"apiKey": RAW, "accessToken": TOKEN}, {"projectId": "approved-bearer"})
    tool = {"id": "call_vertex_preview", "type": "function", "function": {"name": "find", "arguments": '{"value":2}'}}
    body = {"stream": False, "messages": [{"role": "system", "content": "Use tools"},
             {"role": "user", "content": [{"type": "text", "text": "inspect"},
                  {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
             {"role": "assistant", "content": "", "reasoning_content": "Analyze", "tool_calls": [tool]},
             {"role": "tool", "tool_call_id": tool["id"], "content": '{"ok":true}'}],
             "tools": [{"type": "function", "function": {"name": "find", "description": "Lookup",
                         "parameters": {"type": "object", "properties": {"value": {"type": "number"}}}}}]}
    converted = valid("bearer-tools", f"/v1/projects/approved-bearer/locations/us-central1/publishers/google/models/"
                      f"{MODEL}:generateContent", {"Authorization": "[redacted]"}, body=body)
    parts = [part for turn in converted["contents"] for part in turn["parts"]]
    assert any(part.get("thoughtSignature") for part in parts)
    assert [part[key] for key in ("functionCall", "functionResponse") for part in parts if key in part] == [
        {"name": "find", "args": {"value": 2}},
        {"name": "find", "response": {"result": {"ok": True}}},
    ]
    assert any(part.get("inlineData", {}).get("data") == "AAAA" for part in parts)

    save(db, "vertex", {"apiKey": SA}, {"projectId": "ignored"})
    valid("service-account-metadata", f"/v1/projects/approved-sa/locations/us-central1/publishers/google/models/"
          f"{MODEL}:streamGenerateContent?alt=sse", {"Accept": "text/event-stream"})
    save(db, "vertex", {"apiKey": ADC, "accessToken": TOKEN}, {"location": "us-east1"})
    valid("adc-metadata-and-saved-auth", f"/v1/projects/approved-adc/locations/us-east1/publishers/google/models/"
          f"{MODEL}:streamGenerateContent?alt=sse", {"Accept": "text/event-stream", "Authorization": "[redacted]"})

    with sqlite3.connect(db) as sql:
        sql.execute("INSERT OR REPLACE INTO settings(id,data) VALUES(1,?)",
                    (json.dumps({"outboundProxyEnabled": True, "outboundProxyUrl": trap}),))
    save(db, "vertex-partner", {"apiKey": RAW}, {"connectionProxyEnabled": True, "connectionProxyUrl": trap})
    assert call("vertex-partner", PARTNER, code=500) == PARTNER_PROJECT_ERROR
    results["partner-missing-project"] = PARTNER_PROJECT_ERROR
    save(db, "vertex-partner", {"accessToken": TOKEN})
    assert call("vertex-partner", PARTNER, code=500) == PARTNER_PROJECT_ERROR
    save(db, "vertex-partner", {"apiKey": RAW}, {"projectId": "approved-partner"})
    body = {"stream": False, "stream_options": {"include_usage": True}, "model": "keep-intermediate",
            "temperature": 0.3, "reasoning_effort": "high",
            "messages": [{"role": "developer", "content": "Keep shape"}, {"role": "user", "content": "hello"}]}
    route = "/v1/projects/approved-partner/locations/global/endpoints/openapi/chat/completions"
    converted = valid("partner-key", route + "?key=%5Bredacted%5D", {}, provider="vertex-partner", model=PARTNER, body=body)
    assert converted["model"] == "keep-intermediate" and converted["messages"][0]["role"] == "system"
    assert converted["stream_options"] == {"include_usage": True} and converted["temperature"] == .3
    save(db, "vertex-partner", {"accessToken": TOKEN}, {"projectId": "approved-partner"})
    valid("partner-bearer", route, {"Accept": "text/event-stream", "Authorization": "[redacted]"},
          provider="vertex-partner", model=PARTNER)
    assert not CALLS
    return results


def safe_errors(port, db):
    save(db, "vertex", {"accessToken": TOKEN, "refreshToken": ADC_REFRESH}, {"projectId": "approved"})
    assert preview(port, code=500) == "Unsupported Vertex refresh configuration"
    save(db, "vertex", {"apiKey": "{" + ADC_SECRET})
    assert preview(port, code=500) == "Invalid Vertex credential JSON"
    save(db, "vertex", {"accessToken": TOKEN}, {"projectId": TOKEN})
    assert preview(port, code=500) == "Vertex preview destination contains a credential"
    save(db, "vertex", {"apiKey": RAW})
    assert preview(port, body={"contents": []}, code=400) == "Unsupported Vertex preview request body"
    assert not CALLS


with tempfile.TemporaryDirectory(prefix="9router-vertex-translator-preview-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    trap = stack.enter_context(shared.serving(ThreadingHTTPServer(("127.0.0.1", 52989), NoRuntimeFetch)))
    address = f"http://127.0.0.1:{trap.server_port}"
    original_mode = sys.argv[2] == "original"
    service = original(scratch) if original_mode else shared.gateway(scratch, "native", None, [])
    with service as (port, db):
        result = cases(port, db, original_mode, address)
        if not original_mode:
            safe_errors(port, db)
    print(json.dumps(result, sort_keys=True, ensure_ascii=False))
