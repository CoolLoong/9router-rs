import { createRequire } from "node:module";

const target = new Map([
  ["https://api.stability.ai", "stability"],
  ["https://router.huggingface.co", "hf-hosted"],
  ["http://local-hf.fixture:8080", "hf-self-hosted"],
]);
const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  const lane = target.get(url.origin);
  if (!lane) return send(source, options);
  const destination = "http://127.0.0.1:52989/" + lane + url.pathname + url.search;
  return send(source instanceof Request ? new Request(destination, source) : destination, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
