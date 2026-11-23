import { createRequire } from "node:module";

const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (url.origin !== "https://cloud.zed.dev") return send(source, options);
  const target = "http://127.0.0.1:52989/zed-fixture" + url.pathname + url.search;
  return send(source instanceof Request ? new Request(target, source) : target, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
