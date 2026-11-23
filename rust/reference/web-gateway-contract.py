import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

root = Path(__file__).resolve().parents[2]
implementation = sys.argv[1]
search = "/v1/search"
fetch = "/v1/web/fetch"
target = "https://192.0.2.141/fixture?lang=zh"
auth = {"Authorization": "Bearer offline-chat-key"}
restricted = {"Authorization": "Bearer offline-restricted-key"}


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)])
    providers = {
        "serper": "serper", "brave-search": "brave", "exa": "exa", "tavily": "tavily", "firecrawl": "firecrawl",
        "jina-reader": "jina", "ollama": "ollama", "tinyfish": "tinyfish",
    }
    ids = [(f"web-{name}", name, 1, f"offline-web-{suffix}") for name, suffix in providers.items()]
    ids.append(("web-exa-backup", "exa", 2, "offline-web-exa-backup"))
    with sqlite3.connect(data / "db/data.sqlite") as db:
        db.executemany(
            """INSERT INTO providerConnections
               (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey','Fixture web',?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
            [(id_, provider, priority, json.dumps({"apiKey": key})) for id_, provider, priority, key in ids],
        )
        db.execute("UPDATE apiKeys SET accessAllow = ? WHERE id = 'fixture-restricted'", (json.dumps(["brave", "exa"]),))


def request(base, path, payload=None, headers=auth):
    headers = dict(headers)
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    else:
        data = None
    try:
        reply = urlopen(Request(base + path, data=data, headers=headers), timeout=8)
    except HTTPError as error:
        reply = error
    with reply:
        result = json.load(reply)
        if "metrics" in result:
            metrics = result["metrics"]
            for field in ("response_time_ms", "upstream_latency_ms"):
                assert isinstance(metrics.pop(field), int)
        for item in result.get("results", []):
            generated = item["citation"].pop("retrieved_at")
            assert generated.endswith("Z") and len(generated) == 24
        return reply.status, result


def run_cases(base, database):
    snapshots = {}

    def send(name, path, payload, headers=auth, status=200):
        received, response = request(base, path, payload, headers)
        assert received == status, (name, received, response)
        assert "offline-web-" not in json.dumps(response), name
        snapshots[name] = response
        return response

    serper = send("serper-web", search, {"provider": "serper", "query": "  ｆｉｘｔｕｒｅ\tquery　news  ",
                                         "country": "US", "language": "en", "max_results": 1})
    assert serper["query"] == "fixture query news" and serper["results"][0]["display_url"] == "example.invalid/page"
    assert len(serper["results"]) == 1 and serper["metrics"]["total_results_available"] == 17
    no_url = send("serper-news", search, {"model": "serper", "query": "fixture news", "search_type": "news"})
    assert "display_url" not in no_url["results"][0] and no_url["results"][0]["snippet"] == "Fallback excerpt"
    brave = send("brave-restricted-alias", "/api/v1/search", {"model": "brave-search", "query": "fixture query news",
                 "country": "US", "language": "en", "max_results": 99, "search_type": "news"}, restricted)
    assert brave["results"][0]["published_at"] == "2026-01-01" and brave["metrics"]["total_results_available"] == 21
    exa = send("exa-search", search, {"model": "exa", "query": "fixture exa", "search_type": "news", "max_results": 1,
                                     "domain_filter": ["example.invalid", "-excluded.invalid"]})
    assert exa["results"][0]["score"] == 1 and exa["results"][0]["metadata"]["author"] == "Tester"
    assert exa["results"][0]["content"]["length"] == 12
    tavily = send("tavily-search", "/v1/v1/search", {"provider": "tavily", "query": "fixture tavily", "country": "taiwan"})
    assert tavily["results"][0]["score"] == 0 and tavily["usage"]["search_cost_usd"] == .008
    rotation = send("search-rotation", search, {"model": "exa", "query": "rotate fixture"})
    assert rotation["results"][0]["title"] == "Backup account"
    with sqlite3.connect(database) as db:
        [raw] = db.execute("SELECT data FROM providerConnections WHERE id='web-exa'").fetchone()
        account = json.loads(raw)
        assert account.get("modelLock_websearch:exa") and not account.get("modelLock___all")

    exa_page = send("exa-fetch-unlocked", fetch, {"provider": "exa", "url": target, "max_characters": 8}, restricted)
    assert exa_page["content"] == {"format": "markdown", "text": "Document", "length": 8}
    assert exa_page["title"] == "Exa page" and exa_page["usage"]["fetch_cost_usd"] == .001
    crawl = send("firecrawl-html", "/api/v1/web/fetch", {"provider": "firecrawl", "url": target, "format": "html"})
    assert crawl["content"]["text"] == "<p>Fixture page extract</p>" and crawl["title"] == "Firecrawl page"
    jina = send("jina-title", fetch, {"provider": "jina-reader", "url": target, "max_characters": 9})
    assert jina["title"] == "Fixture Reader" and jina["content"]["text"] == "# Heading"
    extract = send("tavily-extract", "/v1/v1/web/fetch", {"model": "tavily", "url": target, "format": "text"})
    assert extract["title"] is None and extract["content"]["text"] == "Tavily extracted text"
    ollama = send("ollama-fetch", fetch, {"model": "ollama", "url": target})
    assert ollama["links"] == ["https://example.invalid/link"] and ollama["usage"]["fetch_cost_usd"] is None
    tinyfish = send("tinyfish-fetch", fetch, {"model": "tinyfish", "url": target, "format": "html"})
    assert tinyfish["links"] == ["https://example.invalid/link"] and tinyfish["metadata"]["language"] == "en"

    send("fetch-key-denied", fetch, {"provider": "firecrawl", "url": target}, restricted, status=403)
    send("search-key-denied", search, {"provider": "firecrawl", "query": "q"}, restricted, status=403)
    invalid = send("search-query-controls", search, {"model": "serper", "query": "bad\u0001query"}, status=400)
    assert invalid["error"] == {"message": "Query contains invalid control characters", "code": 400}
    fmt = send("fetch-format", fetch, {"model": "ollama", "url": target, "format": "html"}, status=400)
    assert fmt["error"]["message"] == "Unsupported format for ollama. Supported: markdown"
    if implementation == "rust":
        status, blocked = request(base, search, {"provider": "exa", "query": "scope check",
                                                  "provider_options": {"baseUrl": "https://example.invalid/"}})
        assert status == 400 and blocked["error"]["message"] == "Search base URL must be configured on the provider connection"
        status, blocked = request(base, fetch, {"provider": "exa", "url": "file:///fixture.txt"})
        assert status == 400 and blocked["error"]["message"] == "Blocked URL: unsupported protocol"
        status, redacted = request(base, search, {"provider": "serper", "query": "redacted fixture"})
        assert status == 422 and redacted["error"]["message"] == "serper returned 422: credential [redacted] and Bearer [redacted]"
    return snapshots


with tempfile.TemporaryDirectory(prefix="9router-web-contract-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    command = ["sh", "rust/reference/server.sh"] if implementation == "reference" else [str(root / "rust/app/target/debug/server")]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data), NINEROUTER_MIGRATION_DATA_DIR=str(data),
               NINEROUTER_MIGRATION_FIXTURE="1", RUST_ANYTHING_FALLBACK_LOG=str(data / "fallback.log"))
    server = subprocess.Popen(command, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 8
        while True:
            try:
                request(base, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        snapshots = run_cases(base, data / "db/data.sqlite")
        if implementation == "rust":
            assert (data / "fallback.log").read_text() == ""
        print(json.dumps(snapshots, separators=(",", ":"), sort_keys=True))
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
