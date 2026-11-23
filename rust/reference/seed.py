import json
import pathlib
import sqlite3
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
database = root / "db" / "data.sqlite"
database.parent.mkdir(parents=True, exist_ok=True)
schema_export = pathlib.Path(__file__).resolve().with_name("export-schema.mjs")
ddl = subprocess.check_output(["node", str(schema_export), "--sql"], text=True)

with sqlite3.connect(database) as connection:
    connection.executescript(ddl)
    connection.executemany(
        """INSERT INTO kv (scope, key, value) VALUES ('mitmAlias', ?, ?)
           ON CONFLICT(scope,key) DO NOTHING""",
        [("antigravity", '{"offline-alias":"fixture-live/gpt-5.6-sol"}'),
         ("fixture tool", '{"offline-fallback":"fixture-responses/fixture-response-model"}')],
    )
    connection.executemany(
        """INSERT INTO apiKeys (id, key, name, machineId, isActive, createdAt, accessRestricted, accessAllow)
           VALUES (?, ?, ?, 'fixture-machine', ?, '2026-11-19T00:00:00.000Z', ?, ?)
           ON CONFLICT(id) DO NOTHING""",
        [
            ("fixture-active", "offline-active-key", "Active fixture", 1, 0, "[]"),
            ("fixture-chat-request", "offline-chat-key", "Chat fixture", 1, 0, "[]"),
            ("fixture-disabled", "offline-disabled-key", "Disabled fixture", 0, 0, "[]"),
            ("fixture-restricted", "offline-restricted-key", "Restricted fixture", 1, 1, '["gemini/gemini-4-argon"]'),
            ("fixture-capacity-restricted", "offline-capacity-primary", "Capacity fixture", 1, 1, '["fixture-live/fixture-combo-a"]'),
        ],
    )

    connection.execute(
        """INSERT INTO combos (id, name, kind, models, createdAt, updatedAt)
           VALUES ('fixture-seeded-combo', 'fixture-seeded-web', 'webFetch', '["openai"]',
                   '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING"""
    )
    connection.execute(
        """INSERT INTO proxyPools (id, isActive, testStatus, data, createdAt, updatedAt)
           VALUES ('fixture-proxy-pool', 1, 'unknown', '{}',
                   '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING"""
    )
    for id_, provider, name, priority, data in [
        ("fixture-gemini-one", "gemini", "Fixture Gemini 1", 5, {
            "apiKey": "offline-gemini-one", "testStatus": "error",
            "lastError": "synthetic failure", "errorCode": 429, "backoffLevel": 3,
            "modelLock_gemini-4-argon": "2030-01-01T00:00:00Z",
            "providerSpecificData": {"enabledModels": ["gemini-4-argon"]},
        }),
        ("fixture-gemini-two", "gemini", "Fixture Gemini 2", 10, {
            "apiKey": "offline-gemini-two", "testStatus": "unknown",
        }),
        ("fixture-bedrock", "bedrock", "Fixture AWS", 3, {
            "apiKey": "offline-aws", "accessToken": "offline-access", "refreshToken": "offline-refresh", "idToken": "offline-id",
            "providerSpecificData": {"region": "us-east-1", "sessionToken": "offline-aws-session"},
        }),
        ("fixture-cloudflare-image", "cloudflare-ai", "Fixture Cloudflare Images", 1, {
            "apiKey": "offline-cloudflare-image",
            "providerSpecificData": {"accountId":"0123456789abcdef0123456789abcdef"},
        }),
        ("fixture-fal-image", "fal-ai", "Fixture Fal Images", 1, {"apiKey":"offline-fal-image"}),
        ("fixture-fal-backup", "fal-ai", "Fixture Fal Backup", 2, {"apiKey":"offline-fal-backup"}),
        ("fixture-brave-proxy", "brave-search", "Fixture Brave Search", 1, {"apiKey":"offline-web-brave"}),
        ("fixture-tavily-proxy", "tavily", "Fixture Tavily", 1, {"apiKey":"offline-web-tavily"}),
        ("fixture-nous-valid", "nous", "Fixture Nous", 5, {"apiKey": "offline-nous-key"}),
        ("fixture-nous-reject", "nous", "Rejected Nous", 10, {"apiKey": "offline-nous-rejected"}),
        ("fixture-nous-missing", "nous", "Missing Nous key", 15, {}),
        ("fixture-xiaomi-plan", "xiaomi-tokenplan", "Fixture Token Plan", 1, {
            "apiKey":"tp-offline-tokenplan-fixture", "providerSpecificData":{"region":"sgp"},
        }),
    ]:
        connection.execute(
            """INSERT INTO providerConnections
                (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
               VALUES (?, ?, 'apikey', ?, ?, 0, ?,
                       '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (id_, provider, name, priority, json.dumps(data, separators=(",", ":"))),
        )

    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-kilo-chat', 'kilocode', 'oauth', 'Fixture Kilo', 1, 0, ?,
                    '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"offline-kilo-chat","providerSpecificData":{"orgId":"kilo-fixture-org"}}',),
    )
    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-glm-plan-auth', 'glm', 'oauth', 'Fixture GLM Plan', 1, 0, ?,
                    '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"fixture-glm-id.fixture-glm-secret","providerSpecificData":{"authMethod":"cli_poll"}}',),
    )
    for provider, credentials in [
        ("github", {"accessToken": "offline-probe-github"}),
        ("qoder", {"accessToken": "offline-probe-qoder"}),
        ("qoder-cn", {"accessToken": "offline-probe-qoder-cn"}),
        ("cursor", {"accessToken": "offline-probe-cursor"}),
        ("codebuddy-cn", {}),
        ("codebuddy-intl", {"accessToken": "offline-probe-codebuddy"}),
    ]:
        connection.execute(
            """INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,?,'oauth',?,10,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
                ON CONFLICT(id) DO NOTHING""",
            (f"fixture-oauth-probe-{provider}", provider, f"Fixture probe {provider}", json.dumps(credentials)),
        )
    for id_, auth, credentials in [
        ("fixture-kimi-code", "oauth", {
            "accessToken": "offline-kimi-code-token", "expiresAt": "2050-01-01T00:00:00.000Z",
            "providerSpecificData": {"deviceId": "0139b5c9-506c-48fb-bef1-956bfdc9880b", "authMethod": "device_code"},
        }),
        ("fixture-kimi-translator-oauth", "oauth", {
            "accessToken": "offline-kimi-translator-old", "refreshToken":"offline-kimi-translator-refresh",
            "expiresAt":"2050-01-01T00:00:00.000Z",
            "providerSpecificData":{"deviceId":"0139b5c9-506c-48fb-bef1-956bfdc9880b","authMethod":"device_code"},
        }),
        ("fixture-kimi-api", "apikey", {"apiKey": "offline-kimi-api"}),
    ]:
        connection.execute(
            """INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'kimi',?,?,1,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
                ON CONFLICT(id) DO NOTHING""",
            (id_, auth, id_, json.dumps(credentials)),
        )
    for id_, auth, credentials in [
        ("fixture-muse-oauth", "oauth", {"accessToken":"LLM|fixture-muse-seat",
            "providerSpecificData":{"authMethod":"device_code"}}),
        ("fixture-muse-api", "apikey", {"apiKey":"offline-muse-api"}),
    ]:
        connection.execute(
            """INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,'muse',?,?,1,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
                ON CONFLICT(id) DO NOTHING""",
            (id_, auth, id_, json.dumps(credentials)),
        )
    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-mimo-cloud', 'xiaomi-mimo', 'api_key', 'Fixture MiMo Cloud', 1, 0, ?,
                    '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"sk-offline-mimo-cloud","providerSpecificData":{"authMethod":"api_key","region":"ams"}}',),
    )
    for provider in ["alicode", "alicode-intl", "alims-intl", "sensenova", "sensenova-intl"]:
        connection.execute(
            """INSERT INTO providerConnections
                (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
                VALUES (?,?,'apikey',?,10,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
                ON CONFLICT(id) DO NOTHING""",
            (f"fixture-{provider}-management", provider, f"Fixture {provider} management",
             json.dumps({"apiKey":f"offline-static-{provider}"})),
        )
    for id_, name, prefix, port in [
        ("openai-compatible-chat-fixture-node", "Fixture node", "fixture-node", 9),
        ("openai-compatible-chat-fixture-live", "Fixture live discovery", "fixture-live", 52989),
    ]:
        detail = {"prefix": prefix, "apiType": "chat", "baseUrl": f"http://127.0.0.1:{port}/v1"}
        connection.execute(
            """INSERT INTO providerNodes (id, type, name, data, createdAt, updatedAt)
               VALUES (?, 'openai-compatible', ?, ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (id_, name, json.dumps(detail, separators=(",", ":"))),
        )
        connection.execute(
            """INSERT INTO providerConnections
                (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
                VALUES (?, ?, 'apikey', ?, 7, 0, ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
                ON CONFLICT(id) DO NOTHING""",
            ("fixture-node-connection" if port == 9 else "fixture-live-connection", id_, name, json.dumps({
                "apiKey": "offline-models-provider",
                "providerSpecificData": {**detail, "nodeName": name, "keep": "fixture"},
            }, separators=(",", ":"))),
        )
    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-live-second', 'openai-compatible-chat-fixture-live', 'apikey',
                    'Fixture backup', 8, 0, ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"apiKey":"offline-backup-provider","providerSpecificData":{"prefix":"fixture-live","apiType":"chat","baseUrl":"http://127.0.0.1:52989/v1","nodeName":"Fixture live discovery"}}',),
    )
    connection.execute(
        """INSERT INTO providerNodes (id, type, name, data, createdAt, updatedAt)
           VALUES ('openai-compatible-responses-fixture', 'openai-compatible',
                   'Fixture Responses', ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
        ('{"prefix":"fixture-responses","apiType":"responses","baseUrl":"http://127.0.0.1:52989/v1"}',),
    )
    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-responses-connection', 'openai-compatible-responses-fixture', 'apikey',
                    'Fixture Responses', 1, 0, ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"apiKey":"offline-responses-provider","providerSpecificData":{"prefix":"fixture-responses","apiType":"responses","baseUrl":"http://127.0.0.1:52989/v1","nodeName":"Fixture Responses"}}',),
    )
    connection.execute(
        """INSERT INTO providerNodes (id, type, name, data, createdAt, updatedAt)
           VALUES ('anthropic-compatible-fixture', 'anthropic-compatible',
                   'Fixture Anthropic', ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
        ('{"prefix":"fixture-anthropic","baseUrl":"http://127.0.0.1:52989/v1"}',),
    )
    for suffix, priority in [("primary", 5), ("secondary", 10)]:
        credential = {
            "accessToken": f"offline-codex-{suffix}",
            "expiresAt": "2035-01-01T00:00:00.000Z",
            "providerSpecificData": {"workspaceId": f"fixture-workspace-{suffix}"},
        }
        connection.execute(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,'codex','oauth',?,?,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (f"fixture-codex-{suffix}", f"Fixture Codex {suffix}", priority,
             json.dumps(credential, separators=(",", ":"))),
        )
    connection.execute(
        """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
           VALUES ('fixture-codex-raw','codex','access_token','Fixture Codex access token',1,0,?,
                   '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"offline-codex-raw","providerSpecificData":{"chatgptAccountId":"fixture-workspace-raw","authMethod":"access_token"}}',),
    )
    connection.execute(
        """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
           VALUES ('fixture-gemini-cli','gemini-cli','oauth','Fixture Gemini CLI',5,0,?,
                   '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"offline-gemini-cli","expiresAt":"2035-01-01T00:00:00.000Z","projectId":"fixture-code-assist"}',),
    )
    connection.execute(
        """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
           VALUES ('fixture-github','github','oauth','Fixture GitHub Copilot',5,0,?,
                   '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
        ('{"accessToken":"offline-github-oauth","expiresAt":"2100-01-01T00:00:00.000Z","providerSpecificData":{"copilotToken":"offline-github-copilot","copilotTokenExpiresAt":4102444800}}',),
    )
    connection.execute(
        """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
           VALUES ('fixture-ollama-local','ollama-local','apikey','Fixture Ollama Local',5,0,'{}',
                   '2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
           ON CONFLICT(id) DO NOTHING""",
    )
    for provider in ["elevenlabs", "inworld", "minimax", "minimax-cn"]:
        connection.execute(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey',?,5,0,'{}','2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (f"fixture-{provider}-speech", provider, f"Fixture {provider} speech"),
        )
    for provider in ["elevenlabs", "assemblyai"]:
        connection.execute(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey',?,5,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (f"fixture-{provider}-stt", provider, f"Fixture {provider} STT",
             json.dumps({"apiKey": f"offline-{provider}-stt"})),
        )
    connection.execute(
        """INSERT INTO providerConnections
            (id, provider, authType, name, priority, isActive, data, createdAt, updatedAt)
            VALUES ('fixture-anthropic-connection', 'anthropic-compatible-fixture', 'apikey',
                    'Fixture Anthropic', 1, 0, ?, '2026-11-19T00:00:00.000Z', '2026-11-19T00:00:00.000Z')
            ON CONFLICT(id) DO NOTHING""",
        ('{"apiKey":"offline-anthropic-provider","providerSpecificData":{"prefix":"fixture-anthropic","baseUrl":"http://127.0.0.1:52989/v1","nodeName":"Fixture Anthropic"}}',),
    )
    connection.execute(
        """INSERT INTO kv (scope, key, value)
           VALUES ('customModels', 'openai-compatible-chat-fixture-node|fixture-model|llm', ?)
           ON CONFLICT(scope,key) DO NOTHING""",
        ('{"providerAlias":"openai-compatible-chat-fixture-node","id":"fixture-model","type":"llm","name":"Fixture model"}',),
    )
    for kind, pool_id, proxy_url in [
        ("http", "fixture-forward-pool", "http://127.0.0.1:52987"),
        ("vercel", "fixture-relay-pool", "http://127.0.0.1:52989/relay-fixture"),
    ]:
        details = {"name": f"Fixture {kind} proxy", "proxyUrl": proxy_url,
                   "noProxy": "", "type": kind, "strictProxy": True}
        connection.execute(
            """INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
               VALUES (?,1,'unknown',?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (pool_id, json.dumps(details, separators=(",", ":"))),
        )
        provider = f"openai-compatible-chat-fixture-{kind}"
        prefix = f"fixture-via-{kind}"
        detail = {"prefix": prefix, "apiType": "chat", "baseUrl": "http://127.0.0.1:52985/v1",
                  "proxyPoolId": pool_id, "nodeName": f"Fixture {kind} chat"}
        connection.execute(
            """INSERT INTO providerNodes (id,type,name,data,createdAt,updatedAt)
               VALUES (?,'openai-compatible',?,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (provider, detail["nodeName"], json.dumps(detail, separators=(",", ":"))),
        )
        connection.execute(
            """INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt)
               VALUES (?,?,'apikey',?,1,0,?,'2026-11-19T00:00:00.000Z','2026-11-19T00:00:00.000Z')
               ON CONFLICT(id) DO NOTHING""",
            (f"{provider}-connection", provider, detail["nodeName"], json.dumps({
                "apiKey": "offline-models-provider", "providerSpecificData": detail,
            }, separators=(",", ":"))),
        )

(root / "auth").mkdir(exist_ok=True)
(root / "machine-id").write_text("migration-fixture-machine-id\n")
(root / "auth" / "cli-secret").write_text("migration-fixture-cli-secret\n")
(root / "jwt-secret").write_text("migration-fixture-jwt-secret\n")
