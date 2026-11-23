from contextlib import ExitStack
from http.server import ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import sqlite3
import tempfile


shared = runpy.run_path(str(Path(__file__).with_name("zed-cline-models-proxy-contract.py")))
providers = ("zed", "cline", "clinepass")
auth = {"Authorization": "Bearer offline-chat-key"}
request_rows = shared["REQUESTS"]
tunnels = shared["TUNNELS"]
catalog_path = "/v1/models"
live_ids = {
    "zd/fixture-live-zed", "cl/fixture-live-cline", "cl/cline-pass/fixture",
    "cl/cline-free/fixture", "clinepass/cline-pass/fixture",
}


def add_account(db, identifier, provider, priority, auth_type, **data):
    with sqlite3.connect(db) as sql:
        sql.execute("""INSERT INTO providerConnections
            (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
            VALUES (?,?,?,?,?,1,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')""",
                    (identifier, provider, auth_type, "Offline catalog fixture", priority, json.dumps(data)))


def catalog(port, path=catalog_path, headers=auth):
    status, raw = shared["get"](port, path, headers)
    assert status == 200, (status, raw)
    assert all(secret.encode() not in raw for secret in (*shared["SECRETS"], *shared["LEASES"]))
    body = json.loads(raw)
    assert body["object"] == "list"
    return {model["id"]: model for model in body["data"]}


def live_catalog(port):
    models = catalog(port)
    assert live_ids <= models.keys(), set(models)
    assert "zd/disabled-zed" not in models
    assert models["zd/fixture-live-zed"]["capabilities"] == {"tools": True}
    assert "cl/clinepass/fixture" not in models
    return models


def run_cases(port, db, http, connect):
    for provider in providers:
        shared["save"](db, provider)
    for provider in providers:
        add_account(db, provider + "-shadow", provider, 2, "oauth", accessToken="offline-shadow-never-read")

    models = live_catalog(port)
    assert not tunnels and len(request_rows) == 6 and all(row[0] for row in request_rows)

    for provider, details in zip(providers, (http, {"proxyPoolId": "fixture-socks"}, http)):
        shared["save"](db, provider, details)
    add_account(db, "qoder-legacy", "qoder", 1, "apikey", apiKey="offline-inaccessible-qoder")
    protected = live_catalog(port)
    assert all(protected[id_] == models[id_] for id_ in live_ids)
    assert any(model["owned_by"] == "qd" for model in protected.values())
    assert len(request_rows) == 12 and all(not row[0] for row in request_rows[6:])
    assert {kind for kind, _, _ in tunnels} == {"connect", "socks"}

    before = len(request_rows)
    assert live_ids <= catalog(port, "/api/v1/models").keys()
    assert [row[2] for row in request_rows[before:]] == [
        "/api/v1/models", "/api/v1/ai/cline/recommended-models", "/api/v1/models"]
    status, raw = shared["get"](port, "/v1/models/zd/fixture-live-zed", auth)
    assert status == 200 and json.loads(raw) == models["zd/fixture-live-zed"]

    before = (len(request_rows), len(tunnels))
    for provider, details in zip(providers, ({"proxyPoolId": "fixture-relay"}, {"proxyPoolId": "missing-proxy"},
                                            http | {"connectionNoProxy": ".cline.bot"})):
        shared["save"](db, provider, details)
    static = catalog(port)
    assert not live_ids.intersection(static)
    assert {"cl", "clinepass", "qd"} <= {model["owned_by"] for model in static.values()}
    assert all(model["owned_by"] != "zd" for model in static.values())
    assert (len(request_rows), len(tunnels)) == before

    shared["save"](db, "zed", http)
    shared["save"](db, "cline", http, expiresAt="2000-01-01T00:00:00Z")
    shared["save"](db, "clinepass", http, auth_type="oauth")
    connect.reject = True
    assert live_ids.intersection(catalog(port)) == {"zd/fixture-live-zed"}
    assert (len(request_rows), len(tunnels)) == before
    uncached = http["connectionProxyUrl"].replace("127.0.0.1", "offline-proxy@127.0.0.1")
    shared["save"](db, "zed", http | {"connectionProxyUrl": uncached})
    assert not live_ids.intersection(catalog(port))
    connect.reject = False
    assert len(request_rows) == before[0] and len(tunnels) == before[1] + 1

    shared["save"](db, "zed", {"proxyPoolId": "fixture-relay"})
    shared["save"](db, "cline", http)
    shared["MODE"]["cline"] = "redirect"
    before_redirect = len(request_rows)
    assert not live_ids.intersection(catalog(port))
    shared["MODE"].clear()
    assert [row[1:3] for row in request_rows[before_redirect:]] == [
        (shared["CLINE_HOST"], "/api/v1/models")]

    for provider in providers:
        shared["save"](db, provider)
    shared["enable_global_proxy"](db, http["connectionProxyUrl"])
    add_account(db, "kiro-legacy", "kiro", 1, "apikey", accessToken="offline-kiro-never-read",
                providerSpecificData={"authMethod": "api_key", "region": "us-east-1",
                                      "proxyPoolId": "fixture-relay"})
    add_account(db, "grok-legacy", "grok-cli", 1, "oauth", accessToken="offline-grok-never-read",
                providerSpecificData={"proxyPoolId": "fixture-relay"})
    add_account(db, "compatible-legacy", "openai-compatible-model-fixture", 1, "apikey",
                apiKey="offline-compatible-never-read",
                providerSpecificData={"baseUrl": "http://127.0.0.1:52989/trap",
                                      "connectionProxyEnabled": True,
                                      "connectionProxyUrl": "ftp://127.0.0.1:52989"})
    before = len(request_rows)
    live_catalog(port)
    assert [row[2] for row in request_rows[before:]] == [
        "/api/v1/models", "/api/v1/ai/cline/recommended-models", "/api/v1/models"]

    with sqlite3.connect(db) as sql:
        sql.execute("""UPDATE providerConnections SET isActive=0 WHERE id IN
            ('qoder-legacy','kiro-legacy','grok-legacy','compatible-legacy')""")
    before = (len(request_rows), len(tunnels))
    for provider in providers:
        shared["save"](db, provider, {"enabledModels": ["fixture-selected"]})
    selected = catalog(port)
    assert {"zd/fixture-selected", "cl/fixture-selected", "clinepass/fixture-selected"} <= selected.keys()
    assert not live_ids.intersection(selected)
    assert (len(request_rows), len(tunnels)) == before


with tempfile.TemporaryDirectory(prefix="9router-routed-catalog-") as directory, ExitStack() as stack:
    scratch = Path(directory)
    ca, target, _ = shared["tls_services"](stack, scratch)
    stack.enter_context(shared["serving"](ThreadingHTTPServer(("127.0.0.1", 52989), shared["Vendor"])))
    connect, socks, relay = [shared["Proxy"](("127.0.0.1", 0), shared["Tunnel"]) for _ in range(3)]
    for proxy, mode in ((connect, "connect"), (socks, "socks"), (relay, "connect")):
        proxy.mode, proxy.reject, proxy.target = mode, False, target
        stack.enter_context(shared["serving"](proxy))
    http = {"connectionProxyEnabled": True, "connectionProxyUrl": f"http://127.0.0.1:{connect.server_address[1]}"}
    pools = [("fixture-socks", "socks5", f"socks5h://127.0.0.1:{socks.server_address[1]}"),
             ("fixture-relay", "cloudflare", f"http://127.0.0.1:{relay.server_address[1]}")]
    with shared["gateway"](scratch, "public-catalog", ca, pools) as (port, db):
        run_cases(port, db, http, connect)
    assert not shared["ERRORS"], shared["ERRORS"]
print("Public catalog uses the first Zed/Cline/ClinePass account with pinned tunnels, static and cached fallback, and zero legacy egress.")
