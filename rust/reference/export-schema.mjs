import { TABLES, PRAGMA_SQL, SCHEMA_VERSION, buildCreateTableSql } from "../../src/lib/db/schema.js";
import { latestVersion } from "../../src/lib/db/migrations/index.js";

if (process.argv.includes("--sql")) {
  const statements = Object.entries(TABLES).flatMap(([name, table]) => [
    buildCreateTableSql(name, table),
    ...(table.indexes ?? []),
  ]);
  process.stdout.write(PRAGMA_SQL.trim() + "\n\n" + statements.map((statement) => statement + ";").join("\n") + "\n");
} else {
  process.stdout.write(JSON.stringify({
    schemaVersion: latestVersion(),
    backupSchemaVersion: SCHEMA_VERSION,
    pragmas: PRAGMA_SQL,
    tables: TABLES,
  }, null, 2) + "\n");
}
