import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

root = Path(__file__).resolve().parents[2]
cli = "68977b6fd9422fbb"
profile = sys.argv[2] if len(sys.argv) > 2 else "chat"
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]

launch = ["sh", "rust/reference/server.sh"] if sys.argv[1] == "reference" else ["rust/app/target/debug/server"]
env = dict(os.environ, PORT=str(port), NINEROUTER_MIGRATION_FIXTURE="1")
server = subprocess.Popen(launch, cwd=root, env=env, start_new_session=True,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def request(path, *, method="GET", body=None, headers=None):
    options = dict(headers or {})
    payload = None
    if body is not None:
        payload = json.dumps(body).encode()
        options["content-type"] = "application/json"
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=payload,
                                 method=method, headers=options)
    with urllib.request.urlopen(req, timeout=5) as res:
        return res.read()


def retry(operation):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            result = operation()
            if result:
                return result
        except (OSError, urllib.error.URLError):
            if server.poll() is not None:
                break
        time.sleep(0.025)
    raise RuntimeError("fixture service or persisted usage did not become ready")


def stats_at_least(count):
    payload = json.loads(request("/api/usage/stats", headers={"x-9r-cli-token": cli}))
    return payload if payload["totalRequests"] >= count else None


def chat(stream):
    result = request(
        "/v1/chat/completions", method="POST", headers={"authorization": "Bearer offline-chat-key"},
        body={"model":"fixture-live/gpt-5.6-sol", "stream":stream,
              "messages":[{"role":"user","content":"usage contract hello"}]},
    )
    if stream and b"data: [DONE]" not in result:
        raise RuntimeError("fixture SSE did not complete")
    if not stream and json.loads(result)["choices"][0]["message"]["content"] != "rust migration":
        raise RuntimeError("unexpected fixture completion")


def responses(stream):
    result = request(
        "/v1/responses", method="POST", headers={"authorization": "Bearer offline-chat-key"},
        body={"model": "fixture-responses/fixture-response-model", "stream": stream,
              "input": "direct native SSE" if stream else "direct genuine JSON"},
    )
    if stream and b"event: response.completed" not in result:
        raise RuntimeError("fixture Responses SSE did not complete")
    if not stream and json.loads(result)["output"][0]["content"][0]["text"] != "native responses migration":
        raise RuntimeError("unexpected Responses completion")


try:
    retry(lambda: request("/api/health"))
    connection, complete = ("fixture-responses-connection", responses) if profile == "responses" else (
        "fixture-live-connection", chat,
    )
    request(f"/api/providers/{connection}", method="PUT", body={"isActive": True},
            headers={"x-9r-cli-token": cli})
    complete(False)
    retry(lambda: stats_at_least(1))
    complete(True)
    stats = retry(lambda: stats_at_least(2))
    totals = ["totalRequests", "totalPromptTokens", "totalCompletionTokens", "totalCachedTokens", "totalCost"]
    counters = ("requests", "promptTokens", "completionTokens", "cachedTokens", "cost")
    result = {key: stats[key] for key in totals}
    for field in ["byProvider", "byModel", "byAccount", "byApiKey", "byEndpoint"]:
        result[field] = [{key: row.get(key, 0) for key in counters}
                         for _, row in sorted(stats[field].items())]
    print(json.dumps(result, separators=(",", ":"), sort_keys=True))
finally:
    try:
        os.killpg(server.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        server.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(server.pid, signal.SIGKILL)
        server.wait(timeout=2)
