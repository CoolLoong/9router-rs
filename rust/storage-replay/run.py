import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

folder = Path(__file__).resolve().parent
repo = folder.parents[1]
stamp = "2026-11-19T00:00:00.000Z"
legacy = {
    "db.json": {
        "settings": {"testOnly": True},
        "providerConnections": [{"id": "account", "provider": "gemini", "name": "模型",
                                 "isActive": False, "apiKey": "offline-fixture", "priority": 4,
                                 "createdAt": stamp, "updatedAt": stamp}],
        "providerNodes": [{"id": "node", "type": "openai-compatible", "name": "Native",
                           "baseUrl": "http://127.0.0.1:9/v1", "createdAt": stamp, "updatedAt": stamp}],
        "proxyPools": [{"id": "proxy", "proxyUrl": "http://127.0.0.1:9", "createdAt": stamp,
                        "updatedAt": stamp}],
        "apiKeys": [{"id": "key", "key": "offline-api", "name": "read", "createdAt": stamp}],
        "combos": [{"id": "combo", "name": "fixture mix", "kind": "llm",
                    "models": ["gemini/alpha"], "createdAt": stamp, "updatedAt": stamp}],
        "modelAliases": {"known fixture": "gemini/alpha"},
        "customModels": [{"providerAlias": "gemini", "id": "alpha", "name": "测试"}],
        "mitmAlias": {"tool": None},
        "pricing": {"gemini": {"alpha": {"input": 0.2}}},
    },
    "usage.json": {
        "history": [{"timestamp": stamp, "provider": "gemini", "model": "alpha", "cost": 0.3,
                     "tokens": {"input_tokens": 5, "output_tokens": 7}}],
        "dailySummary": {"2026-11-19": {"totalRequests": 3}},
        "totalRequestsLifetime": 13,
    },
    "disabledModels.json": {"disabled": {"gemini": ["disabled"], "mock": None}},
    "request-details.json": {
        "records": [{"id": "request", "timestamp": stamp, "status": "success", "extra": "内容"}],
    },
}


def seed(root, scenario):
    if scenario == "upgrade":
        with sqlite3.connect(root / "db/data.sqlite") as db:
            db.executescript("""
                CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE apiKeys (id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, name TEXT,
                                      machineId TEXT, isActive INTEGER DEFAULT 1, createdAt TEXT NOT NULL);
                CREATE TABLE requestDetails (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL,
                    provider TEXT, model TEXT, connectionId TEXT, status TEXT, data TEXT NOT NULL);
                INSERT INTO _meta VALUES ('schemaVersion','1'), ('backupSchemaVersion','1');
                INSERT INTO apiKeys (id,key,createdAt) VALUES ('fixture','offline-before-migration','2026-11-19');
                INSERT INTO requestDetails (id,timestamp,data) VALUES ('detail','2026-11-19','{}');
            """)
    elif scenario == "marked":
        (root / "db/.migrated-from-json").write_text(stamp)
        (root / "db.json").write_text(json.dumps(legacy["db.json"]))
    elif scenario in ("legacy", "collision"):
        if scenario == "collision":
            legacy["db.json"]["apiKeys"] += legacy["db.json"]["apiKeys"].copy()
        for name, data in legacy.items():
            (root / name).write_text(json.dumps(data, ensure_ascii=False))


def snapshot(database):
    with sqlite3.connect(database) as db:
        flags = db.execute("PRAGMA integrity_check").fetchone()[0]
        assert flags == "ok"
        db.row_factory = sqlite3.Row
        indexes = [row[0] for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
        tables = {}
        for (table,) in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"):
            queries = [dict(row) for row in db.execute(f'SELECT * FROM "{table}"')]
            for row in queries:
                if table == "_meta" and row["key"] == "migratedAt":
                    row["value"] = bool(row["value"])
                for key in ("data", "models", "tokens", "meta"):
                    if isinstance(row.get(key), str):
                        row[key] = json.loads(row[key])
                if table == "kv":
                    row["value"] = json.loads(row["value"])
            tables[table] = {
                "columns": sorted(row["name"] for row in db.execute(f'PRAGMA table_info("{table}")')),
                "rows": sorted(queries, key=lambda value: json.dumps(value, sort_keys=True)),
            }
        return {"tables": tables, "indexes": indexes}


runner, scenario = sys.argv[1:]
with tempfile.TemporaryDirectory(prefix="9router-storage-replay-") as work:
    root = Path(work)
    (root / "db").mkdir()
    seed(root, scenario)
    argv = (["node", "--no-warnings", str(folder / "original.mjs")] if runner == "original"
            else [str(folder.parent / "app/target/debug/storage-replay")])
    states = []
    for _ in range(2):
        run = subprocess.run(argv, cwd=repo, env={**os.environ, "DATA_DIR": str(root)},
                             capture_output=True, text=True)
        if run.returncode:
            sys.stderr.write(run.stderr)
            sys.exit(run.returncode)
        states.append(snapshot(root / "db/data.sqlite"))

    backups = {}
    for path in sorted((root / "db/backups").iterdir()):
        if "-20" not in path.name:
            raise ValueError("expected versioned backup")
        name = path.name.rsplit("-", 2)[0]
        backup = {item.name: snapshot(item) if item.suffix == ".sqlite" else json.loads(item.read_text())
                  for item in path.iterdir()}
        backups[name] = backup
    print(json.dumps({
        "states": states, "marker": (root / "db/.migrated-from-json").exists(), "backups": backups,
    }, ensure_ascii=False, sort_keys=True, separators=(",", ":")))
