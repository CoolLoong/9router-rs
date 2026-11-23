import http from "node:http";
import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const [root, base] = process.argv.slice(2);
process.env.DATA_DIR = root;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (specifier === "next/server") return next("next/server.js", context);
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry: false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`) : info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
const localFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const url = new URL(input);
  const [service, region, domain, com] = url.hostname.split(".");
  const endpoint = service === "q" ? "/ListAvailableModels" : service === "oidc" ? "/token" : null;
  if (url.protocol !== "https:" || url.pathname !== endpoint || domain !== "amazonaws" || com !== "com" ||
      url.hostname.split(".").length !== 4 || !/^[a-z]{2}-[a-z]+-[0-9]{1,2}$/.test(region)) {
    throw new Error("Fixture refused a non-loopback provider call");
  }
  return localFetch(`${base}/${service}/${region}${url.pathname}${url.search}`, { ...init, redirect: "manual" });
};
const apiKey = await import("../../src/app/api/oauth/kiro/api-key/route.js");
const idc = await import("../../src/app/api/oauth/kiro/import/route.js");
const { getAdapter } = await import("../../src/lib/db/driver.js");
await getAdapter();

const server = http.createServer(async (request, response) => {
  if (request.headers["x-replay-admin"] !== "fixture") {
    response.writeHead(401, { "content-type": "application/json" }).end('{"error":"Unauthorized"}');
    return;
  }
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const address = new URL(request.url, "http://127.0.0.1");
  const operation = address.pathname.endsWith("/api-key") ? apiKey : idc;
  const result = await operation.POST(new Request(address, {
    method: request.method, body: Buffer.concat(chunks),
    headers: { "content-type": "application/json" },
  }));
  response.writeHead(result.status, { "content-type": "application/json" }).end(await result.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
