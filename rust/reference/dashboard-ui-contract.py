import http.client
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

root = Path(__file__).resolve().parents[2]
binary = str(Path(sys.argv[1]).resolve())


def request(port, path, method="GET", payload=None, **headers):
    body = None
    if payload is not None:
        body = json.dumps(payload).encode()
        headers["content-type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=12)
    try:
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        content = response.read().decode()
        if (response.headers.get("content-type") or "").startswith("application/json"):
            content = json.loads(content)
        return response.status, content, response.headers
    finally:
        connection.close()


with tempfile.TemporaryDirectory(prefix="9router-dashboard-ui-contract-") as directory:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    logs = Path(directory) / "fallbacks.txt"
    env = {
        **os.environ,
        "PORT": str(port),
        "DATA_DIR": directory,
        "TMPDIR": directory,
        "NINEROUTER_MIGRATION_DATA_DIR": directory,
        "NINEROUTER_MIGRATION_FIXTURE": "1",
        "RUST_ANYTHING_FALLBACK_LOG": str(logs),
    }
    for name in ["JWT_SECRET", "INITIAL_PASSWORD", "AUTH_COOKIE_SECURE", "TRUST_PROXY"]:
        env.pop(name, None)
    process = subprocess.Popen([binary], cwd=root, env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 15
        while True:
            if process.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError("dashboard fixture server did not start")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.2):
                    break
            except OSError:
                time.sleep(.04)

        status, body, headers = request(port, "/dashboard")
        assert (status, body, headers["location"]) == (307, "/login", "/login")
        status, body, headers = request(port, "/login?error=%3Cimg%20src%3Dx%3E")
        assert status == 200 and headers["cache-control"] == "no-store"
        status, callback, callback_headers = request(port, "/callback?code=synthetic-private-code&state=synthetic-state")
        assert status == 200 and callback_headers["cache-control"] == "no-store" and callback_headers["referrer-policy"] == "no-referrer"
        assert '/__9router-ui/oauth-callback.js' in callback and "synthetic-private-code" not in callback
        status, script, script_headers = request(port, "/__9router-ui/oauth-callback.js")
        assert status == 200 and script_headers["content-type"].startswith("text/javascript")
        assert "BroadcastChannel" in script and "localStorage" not in script
        assert headers["content-security-policy"].startswith("default-src 'none'")
        assert '<form id="login-form" method="post"' in body
        assert 'id="password"' in body and 'data-ready="false"' in body
        assert "&lt;img src=x&gt;" in body and "<img src=x>" not in body
        assert "/_next/" not in body
        assert request(port, "/api/keys")[0] == 401
        assert request(port, "/api/providers/web-login")[0] == 401
        status, body, headers = request(port, "/api/auth/login", "POST", {"password": "123456"})
        assert status == 200 and body["success"]
        cookie = headers["set-cookie"].split(";", 1)[0]
        assert cookie.startswith("auth_token=")
        profiles = Path(directory) / "browser-profiles"
        for name in ["fixture-z", "fixture-a", "web-login"]:
            (profiles / name).mkdir(parents=True)
        (profiles / "fixture-file").touch()
        (profiles / "fixture-link").symlink_to(profiles / "fixture-a", target_is_directory=True)
        web_login = "/api/providers/web-login"
        status, accounts, _ = request(port, web_login, cookie=cookie)
        assert status == 200 and accounts == {
            "providers": ["grok-web", "chatgpt-web", "gemini-web", "muse-web", "deepseek-web", "uc-web"],
            "profiles": ["fixture-a", "fixture-z"],
        }
        assert request(port, web_login + "?provider=fixture-missing", cookie=cookie)[0] == 404
        assert request(port, web_login + "?provider=fixture-missing", "DELETE", cookie=cookie)[:2] == (200, {"cancelled": False})
        assert request(port, web_login, "POST", {"provider":"fixture-missing"}, cookie=cookie)[0] == 400
        status, body, _ = request(port, "/login", cookie=cookie)
        assert status == 200 and 'data-ready="true"' in body

        for path in ["/dashboard", "/dashboard/endpoint"]:
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert all(marker in body for marker in (
                'id="server-endpoint"', 'id="new-key"', 'id="key-access-form"',
                'href="/dashboard/providers"', 'href="/dashboard/endpoint?panel=tunnel"',
                '/__9router-ui/dashboard.js', '/__9router-ui/dashboard.css',
            ))
            assert "offline-active-key" not in body and "/_next/" not in body

        for file, media, marker in [
            ("dashboard.js", "text/javascript", 'request("/api/keys"'),
            ("catalog.js", "text/javascript", 'const aliasPath = "/api/models/alias"'),
            ("management.js", "text/javascript", 'const providerApi = "/api/providers"'),
            ("pricing.js", "text/javascript", 'const endpoint = "/api/pricing"'),
            ("quota.js", "text/javascript", '/api/providers/client?'),
            ("quota-format.js", "text/javascript", 'export function parseQuotaData'),
            ("skills.js", "text/javascript", "navigator.clipboard.writeText"),
            ("console.js", "text/javascript", "new EventSource"),
            ("tunnel.js", "text/javascript", 'const base = "/api/tunnel"'),
            ("pxpipe.js", "text/javascript", 'request("/api/pxpipe/stats")'),
            ("dashboard.css", "text/css", ".workspace-content"),
            ("license.txt", "text/plain", "decolua and contributors"),
        ]:
            status, body, headers = request(port, f"/__9router-ui/{file}")
            assert status == 200 and headers["content-type"].startswith(media)
            assert headers["x-content-type-options"] == "nosniff" and marker in body

        status, body, _ = request(port, "/api/settings", "PATCH", {"requireApiKey": False}, cookie=cookie)
        assert status == 200 and body["requireApiKey"] is False
        assert 'id="require-key" checked' not in request(port, "/dashboard", cookie=cookie)[1]
        assert request(port, "/api/keys", cookie=cookie)[0] == 200
        status, created, _ = request(port, "/api/keys", "POST", {"name": "UI contract key"}, cookie=cookie)
        assert status == 201 and created["key"] and created["id"]
        path = "/api/keys/" + created["id"]
        access = {"restricted": True, "allow": [" gemini/fixture ", "gemini/fixture", "some-combo"]}
        assert request(port, path, "PUT", {"access": access}, cookie=cookie)[0] == 200
        assert request(port, path, "PUT", {"isActive": False}, cookie=cookie)[0] == 200
        status, key, _ = request(port, path, cookie=cookie)
        assert status == 200 and key["key"]["isActive"] is False
        assert key["key"]["access"] == {"restricted": True, "allow": ["gemini/fixture", "some-combo"]}
        assert request(port, path, "DELETE", cookie=cookie)[0] == 200
        assert request(port, path, cookie=cookie)[0] == 404

        pricing_page = "/dashboard/settings/pricing"
        assert request(port, pricing_page)[0] == 307
        status, body, headers = request(port, pricing_page, cookie=cookie)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert all(marker in body for marker in (
            'id="pricing-form"', 'id="pricing-provider"', 'id="pricing-rates"',
            '/__9router-ui/pricing.js', 'href="/dashboard/settings/pricing" aria-current="page"',
        ))
        assert request(port, "/api/pricing")[0] == 401
        prices = {"offline-pricing": {"fixture-model": {"input": 1.25, "output": 2}}}
        assert request(port, "/api/pricing", "PATCH", prices, cookie=cookie)[:2] == (200, prices)
        assert request(port, "/api/pricing", cookie=cookie)[1]["offline-pricing"] == prices["offline-pricing"]
        assert "offline-pricing" not in request(port, "/api/pricing", "DELETE", cookie=cookie)[1]
        assert request(port, "/dashboard/quota")[0] == 307
        status, body, headers = request(port, "/dashboard/quota", cookie=cookie)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert all(marker in body for marker in (
            'id="quota-accounts"', 'id="quota-provider"', 'id="quota-credits"', 'id="quota-auto-ping"',
            '/__9router-ui/quota.js', 'href="/dashboard/quota" aria-current="page"',
        ))
        assert request(port, "/api/providers/client")[0] == 401
        status, quota_accounts, _ = request(port, "/api/providers/client?pageSize=2", cookie=cookie)
        assert status == 200 and quota_accounts["pagination"]["pageSize"] == 2
        assert request(port, "/dashboard/pxpipe")[0] == 307
        status, body, _ = request(port, "/dashboard/pxpipe", cookie=cookie)
        assert status == 200 and all(marker in body for marker in (
            'id="pxpipe-metrics"', 'id="pxpipe-trend"', 'id="pxpipe-log"', '/__9router-ui/pxpipe.js',
        ))
        assert request(port, "/api/pxpipe/status", cookie=cookie)[1]["installed"] is False
        assert request(port, "/dashboard/skills")[0] == 307
        status, body, _ = request(port, "/dashboard/skills", cookie=cookie)
        assert status == 200 and body.count('data-skill-link=') == 8
        assert all(marker in body for marker in (
            'href="/dashboard/skills" aria-current="page"',
            '/__9router-ui/skills.js', '/9router/SKILL.md', '/v1/chat/completions',
        ))
        assert request(port, "/dashboard/console-log")[0] == 307
        status, body, _ = request(port, "/dashboard/console-log", cookie=cookie)
        assert status == 200 and 'id="console-lines"' in body
        log_path = "/api/translator/console-logs"
        assert request(port, log_path)[0] == 401
        status, body, _ = request(port, log_path, cookie=cookie)
        assert status == 200 and any("9Router listening" in line for line in body["logs"])
        stream = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
        try:
            stream.request("GET", log_path + "/stream", headers={"Cookie":cookie})
            live = stream.getresponse()
            assert live.status == 200 and live.headers["content-type"] == "text/event-stream"
            assert json.loads(live.readline().removeprefix(b"data: "))["type"] == "init"
            assert live.readline() == b"\n"
            assert request(port, log_path, "DELETE", cookie=cookie)[:2] == (200, {"success":True})
            assert json.loads(live.readline().removeprefix(b"data: ")) == {"type":"clear"}
        finally:
            stream.close()

        for path, markers in [
            ("/dashboard/combos", ['id="new-combo"', 'id="strategy-form"', 'id="adapter-pools"']),
            ("/dashboard/models?provider=offline-ui", ['id="alias-form"', 'id="custom-model-form"', 'id="disable-model-form"']),
        ]:
            assert request(port, path)[0] == 307
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert "/__9router-ui/catalog.js" in body and "/_next/" not in body
            assert all(marker in body for marker in markers)

        provider = "offline-ui"
        model = "fixture-model"
        full_model = f"{provider}/{model}"
        custom = {"providerAlias": provider, "id": model, "type": "llm", "name": "Fixture model",
                  "caps": {"vision": True, "reasoning": False}}
        assert request(port, "/api/models/custom", "POST", custom, cookie=cookie)[1] == {
            "success": True, "added": True,
        }
        custom["name"] = "Renamed fixture model"
        assert request(port, "/api/models/custom", "POST", custom, cookie=cookie)[1] == {
            "success": True, "added": False,
        }
        assert custom in request(port, "/api/models/custom", cookie=cookie)[1]["models"]
        assert request(port, "/api/models/alias", "PUT", {
            "alias": "offline-ui-model", "model": full_model,
        }, cookie=cookie)[0] == 200
        assert request(port, "/api/models/alias", cookie=cookie)[1]["aliases"]["offline-ui-model"] == full_model
        assert request(port, "/api/models/disabled", "POST", {
            "providerAlias": provider, "ids": [model],
        }, cookie=cookie)[0] == 200
        assert request(port, "/api/models/disabled", cookie=cookie)[1]["disabled"][provider] == [model]
        assert request(port, f"/api/models/disabled?providerAlias={provider}&id={model}", "DELETE", cookie=cookie)[0] == 200
        assert provider not in request(port, "/api/models/disabled", cookie=cookie)[1]["disabled"]
        assert request(port, "/api/models/alias?alias=offline-ui-model", "DELETE", cookie=cookie)[0] == 200
        assert request(port, f"/api/models/custom?providerAlias={provider}&id={model}&type=llm", "DELETE", cookie=cookie)[0] == 200
        assert custom not in request(port, "/api/models/custom", cookie=cookie)[1]["models"]

        members = [full_model, "gemini/fixture"]
        status, combo, _ = request(port, "/api/combos", "POST", {
            "name": "offline-ui-combo", "models": members,
        }, cookie=cookie)
        assert status == 201 and combo["models"] == members
        combo_route = f"/api/combos/{combo['id']}"
        members.reverse()
        assert request(port, combo_route, "PUT", {"models": members}, cookie=cookie)[0] == 200
        assert request(port, combo_route, cookie=cookie)[1]["models"] == members
        assert any(item["id"] == combo["id"] for item in request(port, "/api/combos", cookie=cookie)[1]["combos"])
        settings = request(port, "/api/settings", cookie=cookie)[1]
        pools = settings["capacityAdapter"]
        pools["vision"] = {"enabled": True, "roundRobin": False, "models": members}
        assert request(port, "/api/settings", "PATCH", {"capacityAdapter": pools}, cookie=cookie)[0] == 200
        assert request(port, "/api/settings", cookie=cookie)[1]["capacityAdapter"] == pools
        assert request(port, combo_route, "DELETE", cookie=cookie)[0] == 200
        assert request(port, combo_route, cookie=cookie)[0] == 404

        for path, markers in [
            ("/dashboard/providers", ['id="provider-list"', 'href="/dashboard/providers/gemini-cli"',
                                      'href="/dashboard/providers?panel=legacy"', '/__9router-ui/management.js']),
            ("/dashboard/providers/kilocode", ['data-provider="kilocode"', '/__9router-ui/provider-account.js']),
            ("/dashboard/providers/nous", ['data-provider="nous"', '/__9router-ui/provider-account.js']),
            ("/dashboard/providers/new", ['id="connection-fields"', 'id="connection-pool"',
                                          'type="password"', 'value="openai"', '/__9router-ui/management.js']),
            ("/dashboard/profile", ['id="theme-choice"', 'id="profile-password-form"',
                                    'id="outbound-form"', 'id="profile-sso-form"', 'id="profile-database-form"',
                                    '/__9router-ui/profile.js']),
        ]:
            assert request(port, path)[0] == 307
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert "/_next/" not in body
            assert all(marker in body for marker in markers)

        assert request(port, "/dashboard/basic-chat")[0] == 307
        status, body, headers = request(port, "/dashboard/basic-chat", cookie=cookie)
        assert status == 200 and headers["cache-control"] == "no-store"
        assert all(token in body for token in ['id="chat-key"', 'id="chat-model"', 'id="chat-form"',
                                               'id="chat-stream"', '/__9router-ui/basic-chat.js'])
        assert "offline-chat-key" not in body and "offline-models-provider" not in body
        status, script, headers = request(port, "/__9router-ui/basic-chat.js")
        assert status == 200 and headers["content-type"].startswith("text/javascript")
        assert 'gateway("/v1/chat/completions"' in script and 'request("/api/keys")' in script
        status, script, _ = request(port, "/__9router-ui/chat-stream.mjs")
        assert status == 200 and "export async function readCompletion" in script
        fixture = "/api/providers/fixture-live-connection"
        assert request(port, fixture, "PUT", {"isActive": True}, cookie=cookie)[0] == 200
        assert request(port, "/api/settings", "PATCH", {"requireApiKey": True}, cookie=cookie)[0] == 200
        chat = {
            "model": "fixture-live/gpt-5.6-sol",
            "messages": [{"role": "user", "content": "offline basic chat test"}],
            "stream": False,
        }
        endpoint = "/v1/chat/completions"
        active = {"authorization": "Bearer offline-chat-key"}
        assert request(port, endpoint, "POST", chat)[0] == 401
        assert request(port, endpoint, "POST", chat, authorization="Bearer offline-disabled-key")[0] == 401
        assert request(port, endpoint, "POST", chat, authorization="Bearer offline-restricted-key")[0] == 403
        assert request(port, "/v1/models", **active)[0] == 200
        status, body, _ = request(port, endpoint, "POST", chat, **active)
        assert status == 200 and body["choices"][0]["message"]["content"] == "rust migration"
        chat["stream"] = True
        status, body, headers = request(port, endpoint, "POST", chat, accept="text/event-stream", **active)
        assert status == 200 and headers["content-type"].startswith("text/event-stream")
        deltas = [json.loads(line[6:]) for line in body.splitlines()
                  if line.startswith("data: ") and line != "data: [DONE]"]
        assert "".join(item["choices"][0]["delta"].get("content", "")
                       for item in deltas if item.get("choices")) == "rust migration"
        assert "data: [DONE]" in body

        status, embedding, _ = request(port, "/v1/embeddings", "POST", {
            "model": "fixture-live/fixture-embedding", "input": "fixture embedding", "dimensions": 3,
        }, **active)
        assert status == 200 and embedding["data"][0]["embedding"] == [0.125, -0.25, 0.5]
        assert request(port, fixture, "PUT", {"isActive": False}, cookie=cookie)[0] == 200

        for path in ["/dashboard/usage", "/dashboard/usage?tab=details", "/dashboard/usage?tab=logs"]:
            assert request(port, path)[0] == 307
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert all(marker in body for marker in ('id="usage-period"', 'id="usage-filters"',
                                                      'id="usage-log"', '/__9router-ui/usage.js'))
        status, script, _ = request(port, "/__9router-ui/usage.js")
        assert status == 200 and 'new EventSource(`${api}/stream`)' in script
        deadline = time.monotonic() + 5
        while True:
            status, stats, _ = request(port, "/api/usage/stats?period=all", cookie=cookie)
            assert status == 200
            if stats["totalRequests"] >= 2:
                break
            assert time.monotonic() < deadline
            time.sleep(.05)
        assert stats["totalPromptTokens"] >= 8 and stats["totalCompletionTokens"] >= 4
        assert "offline-chat-key" not in json.dumps(stats)
        for endpoint, expected in [
            ("chart?period=all", list), ("providers", dict), ("logs", list),
            ("request-details?page=1&pageSize=20", dict),
        ]:
            status, results, _ = request(port, "/api/usage/" + endpoint, cookie=cookie)
            assert status == 200 and isinstance(results, expected)
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            connection.request("GET", "/api/usage/stream", headers={"cookie": cookie})
            response = connection.getresponse()
            assert response.status == 200 and response.headers["content-type"].startswith("text/event-stream")
            line = response.readline().decode()
            assert line.startswith("data: ")
            activity = json.loads(line[6:])
            assert len(activity["recentRequests"]) >= 2 and "activeRequests" in activity
        finally:
            connection.close()

        pool = next(item["id"] for item in request(port, "/api/proxy-pools", cookie=cookie)[1]["proxyPools"]
                    if item["id"] == "fixture-proxy-pool")
        first_key = "offline-management-key-one"
        rotated_key = "offline-management-key-two"
        status, created, _ = request(port, "/api/providers", "POST", {
            "provider": "openai", "name": "Dashboard provider fixture", "apiKey": first_key,
            "proxyPoolId": pool, "priority": 1,
        }, cookie=cookie)
        assert status == 201 and "apiKey" not in created["connection"]
        connection_id = created["connection"]["id"]
        path = f"/api/providers/{connection_id}"
        assert created["connection"]["providerSpecificData"]["proxyPoolId"] == pool
        for route in [path, "/api/providers"]:
            response = request(port, route, cookie=cookie)
            assert response[0] == 200 and first_key not in json.dumps(response[1])
        route = f"/dashboard/providers/connections/{connection_id}"
        status, body, _ = request(port, route, cookie=cookie)
        assert status == 200 and f'data-connection-id="{connection_id}"' in body
        assert 'id="connection-models"' in body and 'id="connection-headers-form"' in body and first_key not in body
        status, updated, _ = request(port, path, "PUT", {
            "name": "Dashboard fixture (updated)", "defaultModel": "fixture-model",
            "proxyPoolId": None, "isActive": False, "apiKey": rotated_key,
        }, cookie=cookie)
        assert status == 200 and rotated_key not in json.dumps(updated)
        connection = request(port, path, cookie=cookie)[1]["connection"]
        assert connection["defaultModel"] == "fixture-model" and connection["isActive"] is False
        assert "proxyPoolId" not in connection["providerSpecificData"]
        with sqlite3.connect(Path(directory) / "db" / "data.sqlite") as database:
            row = database.execute("SELECT data FROM providerConnections WHERE id = ?", (connection_id,)).fetchone()
            assert json.loads(row[0])["apiKey"] == rotated_key
        assert request(port, path, "DELETE", cookie=cookie)[0] == 200
        assert request(port, path, cookie=cookie)[0] == 404

        for path, markers in [
            ("/dashboard/proxy-pools", ['id="pool-list"', 'id="pool-type"', 'value="cloudflare"',
                                        'id="pool-import-form"', 'id="pool-health"',
                                        'id="pool-cloudflare-form"', 'id="pool-cloudflare-token" type="password"',
                                        'id="pool-deno-form"', 'id="pool-deno-token" type="password"',
                                        'id="pool-netlify-form"', 'id="pool-netlify-token" type="password"',
                                        'id="pool-vercel-form"', 'id="pool-vercel-token" type="password"']),
            ("/dashboard/token-saver", ['data-setting="rtkEnabled"', 'data-setting="headroomTimeoutMs"',
                                         'data-setting="cavemanLevel"', 'data-setting="ponytailLevel"',
                                         'id="headroom-extras-refresh"', 'id="headroom-install-copy"',
                                         'href="/api/headroom/proxy/dashboard"']),
        ]:
            assert request(port, path)[0] == 307
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert "/__9router-ui/optimization.js" in body and "/_next/" not in body
            assert all(marker in body for marker in markers)
        status, script, _ = request(port, "/__9router-ui/optimization.js")
        assert status == 200 and '?includeUsage=true' in script and '"/api/headroom/status"' in script
        assert '${api}/${kind}-deploy' in script and 'token.value = ""' in script

        payload = {
            "name": "Offline HTTP pool", "proxyUrl": "http://127.0.0.1:9",
            "type": "http", "noProxy": "localhost,offline.example.invalid",
            "strictProxy": True, "isActive": True,
        }
        status, body, _ = request(port, "/api/proxy-pools", "POST", payload, cookie=cookie)
        assert status == 201
        pool_id = body["proxyPool"]["id"]
        pool_route = f"/api/proxy-pools/{pool_id}"
        assert all(body["proxyPool"][field] == value for field, value in payload.items())
        provider = "/api/providers/fixture-node-connection"
        assert request(port, provider, "PUT", {"proxyPoolId": pool_id}, cookie=cookie)[0] == 200
        pool_entries = request(port, "/api/proxy-pools?includeUsage=true", cookie=cookie)[1]["proxyPools"]
        assert next(item for item in pool_entries if item["id"] == pool_id)["boundConnectionCount"] == 1
        status, blocked, _ = request(port, pool_route, "DELETE", cookie=cookie)
        assert status == 409 and blocked["boundConnectionCount"] == 1
        for kind in ("vercel", "cloudflare", "deno", "netlify"):
            status, body, _ = request(port, pool_route, "PUT", {"type": kind, "isActive": False}, cookie=cookie)
            assert status == 200 and body["proxyPool"]["type"] == kind and body["proxyPool"]["isActive"] is False
        status, body, _ = request(port, pool_route, cookie=cookie)
        assert status == 200 and body["proxyPool"]["proxyUrl"] == payload["proxyUrl"]
        assert body["proxyPool"]["noProxy"] == payload["noProxy"] and body["proxyPool"]["strictProxy"] is True
        assert request(port, provider, "PUT", {"proxyPoolId": None}, cookie=cookie)[0] == 200
        assert request(port, pool_route, "DELETE", cookie=cookie)[0] == 200
        assert request(port, pool_route, cookie=cookie)[0] == 404

        saver_settings = {
            "rtkEnabled": False, "headroomEnabled": True, "headroomUrl": "http://127.0.0.1:9",
            "headroomTimeoutMs": 1800, "headroomCodeAware": True, "headroomKompress": False,
            "headroomCompressUserMessages": True, "cavemanEnabled": True, "cavemanLevel": "wenyan-ultra",
            "ponytailEnabled": True, "ponytailLevel": "lite", "customPromptEnabled": True,
            "customPrompt": "offline  sample\nline two", "pxpipeEnabled": True, "pxpipeMinChars": 14000,
        }
        status, saved, _ = request(port, "/api/settings", "PATCH", saver_settings, cookie=cookie)
        assert status == 200 and all(saved[field] == value for field, value in saver_settings.items())
        status, settings, _ = request(port, "/api/settings", cookie=cookie)
        assert status == 200 and all(settings[field] == value for field, value in saver_settings.items())
        with sqlite3.connect(Path(directory) / "db" / "data.sqlite") as database:
            record = json.loads(database.execute("SELECT data FROM settings WHERE id = 1").fetchone()[0])
            assert all(record[field] == value for field, value in saver_settings.items())

        for path, markers in [
            ("/dashboard/media-providers/embedding", ['id="new-embedding"', 'href="/dashboard/media-providers/embedding/openai"']),
            ("/dashboard/media-providers/embedding/openai", ['data-endpoint="/v1/embeddings"', 'id="media-preview-form"']),
            ("/dashboard/media-providers/tts/openai", ['data-endpoint="/v1/audio/speech"']),
            ("/dashboard/media-providers/stt/openai", ['data-endpoint="/v1/audio/transcriptions"']),
            ("/dashboard/media-providers/image/openai", ['data-endpoint="/v1/images/generations"']),
            ("/dashboard/media-providers/web", ['id="media-new-combo"', 'value="webFetch"']),
            ("/dashboard/cli-tools", ['id="cli-refresh"', 'href="/dashboard/cli-tools/forge"']),
            ("/dashboard/cli-tools/codex", ['id="cli-codex-profile-form"', 'id="cli-host-actions"']),
            ("/dashboard/cli-tools/droid", ['id="cli-extra-models"', 'id="cli-host-actions"']),
            ("/dashboard/cli-tools/claude", ['id="cli-claude-compact"', 'id="cli-claude-exa"']),
            ("/dashboard/cli-tools/opencode", ['id="cli-opencode-list"', 'id="cli-opencode-clear"']),
            ("/dashboard/cli-tools/cline", ['data-tool="cline"', 'id="cli-host-actions"']),
            ("/dashboard/cli-tools/openclaw", ['data-tool="openclaw"', 'id="cli-host-actions"']),
            ("/dashboard/cli-tools/pi", ['data-tool="pi"', 'id="cli-host-actions"']),
            ("/dashboard/cli-tools/forge", ['id="cli-config-form"', 'Apply to host', 'id="cli-preview"']),
        ]:
            assert request(port, path)[0] == 307
            status, body, headers = request(port, path, cookie=cookie)
            assert status == 200 and headers["cache-control"] == "no-store"
            assert "/__9router-ui/integrations.js" in body and "/_next/" not in body
            assert all(marker in body for marker in markers)
            assert "offline-chat-key" not in body and "offline-models-provider" not in body
        assert request(port, "/__9router-ui/cli-status")[0] == 307
        status, script, _ = request(port, "/__9router-ui/integrations.js")
        assert status == 200 and "/api/cli-tools/" in script and 'new FormData()' in script

        endpoint = {"name": "Offline embedding node", "prefix": "offline-media", "baseUrl": "http://127.0.0.1:9/v1"}
        status, body, _ = request(port, "/api/provider-nodes", "POST", {
            "type": "custom-embedding", **endpoint,
        }, cookie=cookie)
        assert status == 201 and body["node"]["type"] == "custom-embedding"
        node_id = body["node"]["id"]
        status, body, _ = request(port, f"/dashboard/media-providers/embedding/{node_id}", cookie=cookie)
        assert status == 200 and f'data-provider="{node_id}"' in body and "endpoint</h2>" in body
        endpoint["name"] = "Offline embedding renamed"
        node_path = f"/api/provider-nodes/{node_id}"
        assert request(port, node_path, "PUT", endpoint, cookie=cookie)[0] == 200
        nodes = request(port, "/api/provider-nodes", cookie=cookie)[1]["nodes"]
        assert next(entry for entry in nodes if entry["id"] == node_id)["name"] == endpoint["name"]
        assert request(port, node_path, "DELETE", cookie=cookie)[0] == 200

        status, combo, _ = request(port, "/api/combos", "POST", {
            "kind": "webFetch", "name": "offline-media-combo", "models": [],
        }, cookie=cookie)
        assert status == 201
        route = f"/dashboard/media-providers/combo/{combo['id']}"
        status, body, _ = request(port, route, cookie=cookie)
        assert status == 200 and f'data-combo="{combo["id"]}"' in body
        combo_path = f"/api/combos/{combo['id']}"
        result = request(port, combo_path, "PUT", {"models": ["openai"]}, cookie=cookie)
        assert result[0] == 200 and result[1]["kind"] == "webFetch" and result[1]["models"] == ["openai"]
        assert request(port, combo_path, "DELETE", cookie=cookie)[0] == 200

        assert request(port, "/api/settings", "PATCH", {"requireLogin": False}, cookie=cookie)[0] == 200
        assert request(port, "/dashboard/profile")[0] == 200
        assert request(port, "/api/settings", "PATCH", {"requireLogin": True}, cookie=cookie)[0] == 200
        assert request(port, "/dashboard/profile")[0] == 307
        updated_password = "offline-profile-test-password"
        assert request(port, "/api/settings", "PATCH", {
            "currentPassword": "wrong-password", "newPassword": updated_password,
        }, cookie=cookie)[0] == 401
        status, body, _ = request(port, "/api/settings", "PATCH", {
            "newPassword": updated_password,
        }, cookie=cookie)
        assert status == 200 and "password" not in body and "newPassword" not in body
        assert request(port, "/api/auth/login", "POST", {"password": "123456"})[0] == 401
        assert request(port, "/api/auth/login", "POST", {"password": updated_password})[0] == 200
        status, body, _ = request(port, "/api/settings", cookie=cookie)
        assert status == 200 and body["hasPassword"] is True and "password" not in body
        assert not logs.exists() or not logs.read_text().strip()

        status, body, _ = request(port, "/api/cli-tools/forge-settings", "POST", {}, cookie=cookie)
        assert status == 400 and body["error"]["message"] == "baseUrl is required"

        status, body, _ = request(port, "/api/settings", "PATCH", {
            "outboundProxyEnabled": False,
            "outboundProxyUrl": "http://127.0.0.1:9",
            "outboundNoProxy": "localhost,127.0.0.1",
        }, cookie=cookie)
        assert status == 200 and body["outboundProxyEnabled"] is False
        status, body, _ = request(port, "/api/settings", cookie=cookie)
        assert status == 200 and body["outboundNoProxy"] == "localhost,127.0.0.1"

        for path in ["/dashboard/providers?panel=legacy", "/dashboard/providers/gemini-cli?panel=legacy",
                     "/dashboard/profile?panel=legacy", "/dashboard/proxy-pools?panel=legacy",
                     "/dashboard/token-saver?panel=legacy"]:
            status, body, _ = request(port, path, cookie=cookie)
            assert status == 200 and "/_next/" in body
        status, body, _ = request(port, "/dashboard/endpoint?panel=tunnel", cookie=cookie)
        assert status == 200 and "/_next/" not in body
        assert all(marker in body for marker in (
            'id="cloudflare-enable"', 'id="tailscale-check"', 'id="tunnel-dashboard-access"',
            'id="tailscale-install-form"', 'type="password" id="tailscale-sudo-password"',
            '/__9router-ui/tunnel.js',
        ))
        status, tunnel_status, _ = request(port, "/api/tunnel/status", cookie=cookie)
        assert status == 200 and "tunnel" in tunnel_status and "tailscale" in tunnel_status
        status, preview, _ = request(port, "/api/combos/presets?source=claude", cookie=cookie)
        assert status == 200 and preview["source"] == "claude" and preview["toCreate"] > 0
        status, preset, _ = request(port, "/api/combos/presets", "POST", {"source": "claude"}, cookie=cookie)
        assert status == 200 and preset["createdCount"] == preview["toCreate"]
        saved_combos = {entry["name"] for entry in request(port, "/api/combos", cookie=cookie)[1]["combos"]}
        assert {entry["name"] for entry in preset["created"]} <= saved_combos
        strategy = {"offline-ui-combo": {"fallbackStrategy": "round-robin"}}
        assert request(port, "/api/settings", "PATCH", {"comboStrategies": strategy}, cookie=cookie)[0] == 200
        settings = request(port, "/api/settings", cookie=cookie)[1]
        assert settings["comboStrategies"] == strategy and settings["capacityAdapter"] == pools

        status, body, _ = request(port, "/api/settings", "PATCH", {
            "tunnelDashboardAccess": False,
            "tunnelUrl": "https://offline.example.invalid",
            "authMode": "oidc",
            "oidcIssuerUrl": "https://issuer.example.invalid",
            "oidcClientId": "offline",
            "oidcClientSecret": "offline",
            "oidcLoginLabel": "<i>Fixture login</i>",
        }, cookie=cookie)
        assert status == 200 and body["tunnelDashboardAccess"] is False
        tunnel = {"host": "offline.example.invalid"}
        assert request(port, "/dashboard", cookie=cookie, **tunnel)[0] == 307
        status, body, _ = request(port, "/login", cookie=cookie, **tunnel)
        assert status == 200 and 'data-ready="false"' in body and "Dashboard access via this tunnel is disabled." in body

        status, body, headers = request(port, "/api/auth/logout", "POST", cookie=cookie)
        assert status == 200 and body["success"]
        assert any(entry.startswith("auth_token=;") for entry in headers.get_all("set-cookie"))
        assert request(port, "/dashboard/endpoint")[0] == 307
        status, body, _ = request(port, "/login")
        assert status == 200 and 'href="/api/auth/oidc/start"' in body
        assert "&lt;i&gt;Fixture login&lt;/i&gt;" in body and "<i>Fixture login</i>" not in body
        assert 'action="/api/auth/login" hidden' in body
        assert logs.read_text().splitlines() == [
            "PATCH /api/settings",
            "GET /dashboard/providers",
            "GET /dashboard/providers/gemini-cli",
            "GET /dashboard/profile",
            "GET /dashboard/proxy-pools",
            "GET /dashboard/token-saver",
            "PATCH /api/settings",
        ]
        print("Dashboard UI contract passed, including media previews, provider nodes, web combos and CLI validation")
    finally:
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
