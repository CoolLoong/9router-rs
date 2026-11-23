import { createRequire } from "node:module";

const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (url.origin !== "https://api.dev.runwayml.com") return send(source, options);
  const destination = "http://127.0.0.1:52989/runway" + url.pathname + url.search;
  return send(source instanceof Request ? new Request(destination, source) : destination, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
