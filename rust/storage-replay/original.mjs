import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "@/lib/dataDir.js") {
      return next(new URL("../../src/lib/dataDir.js", import.meta.url).href, context);
    }
    return next(specifier, context);
  },
});
const [{ runMigrationOnce }, { createNodeSqliteAdapter }, { DATA_FILE, ensureDirs }] = await Promise.all([
  import("../../src/lib/db/migrate.js"),
  import("../../src/lib/db/adapters/nodeSqliteAdapter.js"),
  import("../../src/lib/db/paths.js"),
]);
ensureDirs();
const db = await createNodeSqliteAdapter(DATA_FILE);
try {
  await runMigrationOnce(db);
} finally {
  db.close();
}
