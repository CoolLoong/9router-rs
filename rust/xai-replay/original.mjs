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
    if (context.parentURL?.includes("/open-sse/executors/") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (url, options) => globalThis.fixtureXaiFetch(url, options);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry: false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`) :
      info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});

const directFetch = globalThis.fetch;
const upstream = new Map([
  ["https://api.x.ai/v1/chat/completions", "/static-chat/xai"],
  ["https://auth.x.ai/.well-known/openid-configuration", "/xai-oauth/.well-known/openid-configuration"],
  ["https://auth.x.ai/oauth2/token", "/xai-oauth/oauth2/token"],
  ["https://fixture.x.ai/alternate/token", "/xai-oauth/alternate/token"],
]);
const fixtureFetch = (input, options) => {
  const url = upstream.get(String(input));
  if (!url) throw new Error(`Fixture refused a non-xAI upstream ${input}`);
  return directFetch(`http://127.0.0.1:52989${url}`, { ...options, redirect: "error" });
};
globalThis.fetch = fixtureFetch;
globalThis.fixtureXaiFetch = fixtureFetch;
const chat = await import("../../src/app/api/v1/chat/completions/route.js");
const responses = await import("../../src/app/api/v1/responses/route.js");
const oauth = await import("../../src/app/api/oauth/[provider]/[action]/route.js");
globalThis.fetch = fixtureFetch;

const server = http.createServer(async (incoming, outgoing) => {
  try {
    const url = new URL(incoming.url, "http://127.0.0.1");
    const parts = [];
    for await (const part of incoming) parts.push(part);
    const request = new Request(url, {
      method: incoming.method,
      headers: incoming.headers,
      body: incoming.method === "POST" ? Buffer.concat(parts) : undefined,
    });
    const action = url.pathname.split("/").at(-1);
    const reply = url.pathname.startsWith("/api/oauth/xai/") ?
      await oauth[incoming.method](request, { params: { provider: "xai", action } }) :
      await (action === "responses" ? responses : chat).POST(request);
    outgoing.writeHead(reply.status, { "content-type": reply.headers.get("content-type") });
    outgoing.end(await reply.text());
  } catch (error) {
    console.error(error);
    outgoing.writeHead(500, { "content-type": "application/json" }).end('{"error":"Fixture route failed"}');
  }
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
