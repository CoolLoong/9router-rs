import { createRequire } from "node:module";

const allowed = new Set([
  "api.bfl.ai", "api.eu.bfl.ai", "api.us.bfl.ai", "api.us1.bfl.ai", "api.us2.bfl.ai", "api.bfl.ml",
]);
const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (url.protocol !== "https:" || !allowed.has(url.hostname)) return send(source, options);
  const destination = "http://127.0.0.1:52989/bfl/" + url.hostname + url.pathname + url.search;
  return send(source instanceof Request ? new Request(destination, source) : destination, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
