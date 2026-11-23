import fs from "node:fs";
import http from "node:http";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const [data] = process.argv.slice(2);
process.env.DATA_DIR = data;
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
    return next(info?.isDirectory() ? new URL(`${target.href}/index.js`).href :
      info ? target.href : `${target.href}.js`, context);
  },
});

const direct = globalThis.fetch;
const fetchFixture = (resource, options) => {
  const url = new URL(resource);
  if (url.protocol !== "https:" || !["zcode.z.ai", "api.z.ai"].includes(url.host) || url.search || url.hash) {
    throw new Error("Unexpected Z.ai fixture endpoint");
  }
  return direct(`http://127.0.0.1:52989/glm-consent/${url.host}${url.pathname}`, { ...options, redirect: "error" });
};
globalThis.fetch = fetchFixture;
const oauth = await import("../../src/app/api/oauth/[provider]/[action]/route.js");
globalThis.fetch = fetchFixture;

const server = http.createServer(async (incoming, outgoing) => {
  const url = new URL(incoming.url, "http://127.0.0.1");
  const [provider, action] = url.pathname.split("/").slice(-2);
  const payload = [];
  for await (const chunk of incoming) payload.push(chunk);
  const request = new Request(url, { method: incoming.method, headers: incoming.headers,
    body: incoming.method === "POST" ? Buffer.concat(payload) : undefined });
  const response = await oauth[incoming.method](request, { params: { provider, action } });
  outgoing.writeHead(response.status, { "content-type": "application/json" }).end(await response.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
