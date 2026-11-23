import datetime as dt
import hashlib
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
import urllib.error
import urllib.request

root = Path(__file__).resolve().parents[2]
fixture = sys.argv[1]
cli = {"x-9r-cli-token": "68977b6fd9422fbb"}
local_key = "local-no-key"
keys = ("abcdefgh-fixture-a-wxyz", "abcdefgh-fixture-b-wxyz")
now = dt.datetime.fromtimestamp(int(sys.argv[2]), dt.timezone.utc).replace(second=0, microsecond=0)
anchor = "synthetic-redaction-comparison"


def iso(timepoint):
    return timepoint.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def counter():
    return dict(requests=0, promptTokens=0, completionTokens=0, cachedTokens=0, cost=0)


def add(item, prompt, completion, cached, cost):
    item["requests"] += 1
    item["promptTokens"] += prompt
    item["completionTokens"] += completion
    item["cachedTokens"] += cached
    item["cost"] += cost


def seed(data):
    subprocess.check_call(["python3", str(root / "rust/reference/seed.py"), str(data)], cwd=root,
                          env=dict(os.environ, NODE_NO_WARNINGS="1"))
    rows = [
        (now-dt.timedelta(days=9,hours=4), "dg", "nova-3", None, None, None, 3, 1, 0, .125, None),
        (now-dt.timedelta(days=3), "gemini", "gemini-4-argon", "fixture-gemini-two", keys[0], "/v1/chat/completions", 9, 3, 2, .25, "success"),
        (now-dt.timedelta(hours=25), "openai-compatible-chat-fixture-live", "fixture", "fixture-live-connection", keys[0], "/v1/responses", 12, 3, 1, .25, None),
        (now-dt.timedelta(hours=2,seconds=9), "openai-compatible-chat-fixture-live", "fixture", "fixture-live-connection", keys[0], "/v1/responses", 4, 2, 1, .125, "ok"),
        (now-dt.timedelta(hours=2,seconds=3), "openai-compatible-chat-fixture-live", "fixture", "fixture-live-connection", keys[1], "/v1/responses", 4, 2, 1, .125, "ok"),
        (now-dt.timedelta(minutes=4), "gemini", "gemini-4-argon", "fixture-gemini-one", None, "/v1/messages", 11, 4, 3, .5, "ok"),
        (now-dt.timedelta(minutes=1), None, "zero", None, None, "", 0, 0, 0, 0, "ignored"),
    ]
    aggregate = {}
    with sqlite3.connect(data / "db/data.sqlite") as db:
        for n, key in enumerate(keys, 1):
            db.execute(
                """INSERT INTO apiKeys(id,key,name,machineId,isActive,createdAt)
                   VALUES(?,?,?,'fixture-machine',1,'2026-11-19T00:00:00.000Z')""",
                (f"usage-collision-{n}", key, f"Fixture key {n}"))
        for (timepoint, provider, model, account, key, endpoint, prompt, completion, cached, cost, status) in rows:
            db.execute(
                """INSERT INTO usageHistory(timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,
                                            completionTokens,cost,status,tokens,meta) VALUES(?,?,?,?,?,?,?,?,?,?,?,'{}')""",
                (iso(timepoint), provider, model, account, key, endpoint, prompt, completion, cost, status,
                 json.dumps(dict(prompt_tokens=prompt, completion_tokens=completion, cached_tokens=cached))))
            day = aggregate.setdefault(timepoint.astimezone().date().isoformat(), counter() | {
                k:{} for k in ("byProvider","byModel","byAccount","byApiKey","byEndpoint")
            })
            add(day, prompt, completion, cached, cost)
            def item(section, title, **metadata):
                row = day[section].setdefault(title, counter())
                add(row, prompt, completion, cached, cost)
                row.update(metadata)
            if provider:
                item("byProvider", provider)
            item("byModel", f"{model}|{provider}" if provider else model, rawModel=model, provider=provider)
            if account:
                item("byAccount", account, rawModel=model, provider=provider)
            item("byApiKey", f"{key or local_key}|{model}|{provider or 'unknown'}", rawModel=model, provider=provider, apiKey=key)
            endpoint = endpoint or "Unknown"
            item("byEndpoint", f"{endpoint}|{model}|{provider or 'unknown'}", rawModel=model, provider=provider, endpoint=endpoint)
        db.executemany("INSERT INTO usageDaily(dateKey,data) VALUES(?,?)",
                       [(day,json.dumps(data)) for day,data in aggregate.items()])
        for n, (provider, model, status) in enumerate([
            ("openai-compatible-chat-fixture-live", "fixture", "success"),
            ("dg", "nova-3", "failed"),
            ("gemini", "gemini-4-argon", "success"),
            ("gemini", "gemini-4-argon", "failed"),
            (None, "legacy", "failed"),
        ], 1):
            stamp = iso(now-dt.timedelta(days=n))
            record = dict(id=f"fixture-usage-detail-{n}", timestamp=stamp, provider=provider,
                          model=model, connectionId="fixture-live-connection", status=status,
                          latency={"ttft": 2, "total": 9}, tokens={"prompt_tokens":4})
            for key in ("request", "providerRequest", "providerResponse", "response"):
                record[key] = {"payload":anchor,"token":"never emit synthetic fixture payload"}
            db.execute(
                "INSERT INTO requestDetails(id,timestamp,provider,model,connectionId,status,data) VALUES(?,?,?,?,?,?,?)",
                (record["id"],stamp,provider,model,record["connectionId"],status,json.dumps(record)))

def normalized_key(bare, entry):
    if bare == local_key or (entry["apiKeyMasked"] is None and bare.startswith(local_key + "|")):
        return bare
    return "key-" + hashlib.sha256(bare.encode()).hexdigest()


def probe(base, path, authenticated=True):
    request = urllib.request.Request(base+path, headers=cli if authenticated else {})
    try:
        response = urllib.request.urlopen(request, timeout=5)
    except urllib.error.HTTPError as failure:
        response = failure
    with response:
        contents = response.read()
        if fixture == "rust" and path.startswith(("/api/usage/stats", "/api/usage/history")):
            assert all(key.encode() not in contents for key in keys), "raw API key leaked from native stats"
        if path.startswith("/api/usage/request-details"):
            assert anchor.encode() not in contents, "request details were not redacted"
        value = json.loads(contents)
        if "byApiKey" in value:
            if fixture == "reference":
                value["byApiKey"] = {normalized_key(key,item):item for key,item in value["byApiKey"].items()}
            old = value["last10Minutes"]
            assert len(old) == 10, old
            value["last10Minutes"] = [item for item in old if item["requests"] > 0]
            assert sum(item["requests"] for item in value["last10Minutes"]) == 2
        if path.endswith("/chart?period=24h"):
            assert len(value) == 24
            value = [{k:v for k,v in bucket.items() if k!="label"} for bucket in value]
        return {"status":response.status,"body":value}


with tempfile.TemporaryDirectory(prefix="9router-usage-dashboard-") as directory:
    data = Path(directory)
    seed(data)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    launch = ["sh", "rust/reference/server.sh"] if fixture == "reference" else [str(root / "rust/app/target/debug/server")]
    env = dict(os.environ, PORT=str(port), DATA_DIR=str(data),
               NINEROUTER_MIGRATION_DATA_DIR=str(data), NINEROUTER_MIGRATION_FIXTURE="1")
    server = subprocess.Popen(launch, cwd=root, env=env, start_new_session=True,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{port}"
    try:
        deadline = time.monotonic() + 6
        while True:
            try:
                probe(base, "/api/health")
                break
            except OSError:
                if time.monotonic() > deadline or server.poll() is not None:
                    raise RuntimeError("fixture did not start")
                time.sleep(0.025)
        paths = [
            "/api/usage/history", "/api/usage/stats?period=today", "/api/usage/stats?period=24h",
            "/api/usage/stats?period=7d", "/api/usage/stats?period=all", "/api/usage/stats?period=bad",
            "/api/usage/chart?period=today", "/api/usage/chart?period=24h", "/api/usage/chart?period=7d",
            "/api/usage/chart?period=all", "/api/usage/logs", "/api/usage/request-logs",
            "/api/usage/providers", "/api/usage/request-details?page=2&pagesize=2&pageSize=0x2notes",
            "/api/usage/request-details?pageSize=2&provider=gemini&status=failed",
            "/api/usage/request-details?page=0", "/api/usage/request-details?pageSize=101",
            "/api/usage/request-details?startDate=not-a-date",
            "/api/usage/request-details?startDate=2000-01-01&endDate=2000-12-31",
        ]
        snapshots = {path: probe(base,path) for path in paths}
        snapshots["/api/usage/history#unauthorized"] = probe(base, paths[0], authenticated=False)
        assert snapshots["/api/usage/history#unauthorized"]["status"] == 401
        assert all(actor["body"].get("providers") for key,actor in snapshots.items() if key.endswith("/usage/providers"))
        assert len(snapshots["/api/usage/stats?period=all"]["body"]["byApiKey"]) > 3
        print(json.dumps(snapshots, sort_keys=True, separators=(",",":")))
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
