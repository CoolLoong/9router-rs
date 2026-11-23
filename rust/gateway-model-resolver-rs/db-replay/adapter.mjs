
import { DatabaseSync } from "node:sqlite";
import { readFileSync } from "node:fs";
const connection = new DatabaseSync(":memory:");
connection.exec(readFileSync(new URL("./src/fixture.sql", import.meta.url), "utf8"));
if (process.argv[3] === "anthropic") {
  connection.exec("INSERT INTO providerConnections VALUES ('anthropic', 1)");
}
const adapter = {
  all: (sql, params = []) => connection.prepare(sql).all(...params),
  get: (sql, params = []) => connection.prepare(sql).get(...params),
};
export const getAdapter = async () => adapter;
