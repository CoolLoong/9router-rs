import { registerHooks } from "node:module";

const project = new URL("../../", import.meta.url);
const stored = [];
globalThis.windsurfFixtureDb = {
  transaction(action) { action(); },
  all(_sql, [provider]) { return stored.filter((row) => row.provider === provider); },
  get(_sql, [provider]) {
    return { m: Math.max(0, ...stored.filter((row) => row.provider === provider).map((row) => row.priority)) };
  },
  run(_sql, [id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt]) {
    stored.push({ id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt });
  },
};
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "../driver.js" && context.parentURL?.endsWith("/connectionsRepo.js")) {
      return next("data:text/javascript,export const getAdapter = async () => globalThis.windsurfFixtureDb;", context);
    }
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
globalThis.fetch = () => { throw new Error("Pasting a Windsurf API key must not contact a provider"); };
const { default: windsurf } = await import("../../src/lib/oauth/providers/windsurf.js");
const { createProviderConnection } = await import("../../src/lib/db/repos/connectionsRepo.js");
const inputs = JSON.parse(process.argv[2]);
for (const { code } of inputs) {
  const tokens = await windsurf.exchangeToken(windsurf.config, code.trim());
  const profile = await windsurf.postExchange(tokens);
  const record = windsurf.mapTokens(tokens, profile);
  await createProviderConnection({ provider: "windsurf", authType: "api_key", ...record,
    expiresAt: record.expiresIn ? new Date(Date.now() + record.expiresIn * 1000).toISOString() : null,
    testStatus: "active" });
}
console.log(JSON.stringify(stored.map(({ id, createdAt, updatedAt, data, ...row }) => ({ ...row, data: JSON.parse(data) }))));
