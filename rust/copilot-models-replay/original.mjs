import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const fixture = JSON.parse(fs.readFileSync(0, "utf8"));
const [directory, base] = process.argv.slice(2);
process.env.DATA_DIR = directory;
const loopbackFetch = globalThis.fetch;

registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (specifier === "next/server") return next("next/server.js", context);
    if (context.parentURL?.endsWith("/open-sse/services/copilotModels.js") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch=(url,options)=>globalThis.discoverFixture(url,options);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry:false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`)
      : info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
function transport(url, options) {
  const paths = {
    "https://api.githubcopilot.com/models": "/models",
    "https://api.github.com/copilot_internal/v2/token": "/copilot",
    "https://github.com/login/oauth/access_token": "/github-token",
  };
  if (!paths[url]) throw new Error("Fixture declined a non-loopback request");
  return loopbackFetch(base+paths[url], options);
}
globalThis.fetch = transport;
globalThis.discoverFixture = transport;
const catalog = await import("../../open-sse/services/copilotModels.js");
const management = await import("../../src/app/api/providers/[id]/models/route.js");
const models = await import("../../src/app/api/v1/models/route.js");
const { getAdapter } = await import("../../src/lib/db/driver.js");
const db = await getAdapter();
globalThis.fetch = transport;

const { id, provider, authType, name, priority, isActive, createdAt, updatedAt, ...data } = fixture.account;
db.run("INSERT INTO providerConnections (id,provider,authType,name,priority,isActive,data,createdAt,updatedAt) VALUES (?,?,?,?,?,?,?,?,?)",
  [id,provider,authType,name,priority,isActive?1:0,JSON.stringify(data),createdAt,updatedAt]);

const results = [];
for (let turn=0; turn<(fixture.turns || 1); turn++) {
  if (fixture.mode === "management") {
    const response = await management.GET(new Request("http://127.0.0.1/api/providers/fixture/models"),{params:Promise.resolve({id})});
    const body = await response.json();
    results.push(response.ok ? {status:response.status,models:body.models} : {status:response.status,error:body.error});
  } else {
    if (turn > 0 && fixture.force) catalog.clearCopilotModelCache();
    const response = await models.GET(new Request("http://127.0.0.1/v1/models"));
    const body = await response.json();
    if (!response.ok) throw new Error("Pinned model catalog request failed");
    results.push(body.data.filter((row) => row.owned_by === "gh"));
  }
}
console.log(JSON.stringify(results));
process.exit(0);
