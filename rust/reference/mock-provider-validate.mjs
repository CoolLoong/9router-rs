import { createRequire } from "node:module";

const hosts = new Set([
  "api.x.ai", "api.deepgram.com", "api.atria-asi.ai", "www.searchapi.io", "api.z.ai", "open.bigmodel.cn",
  "api.kimi.com", "api.minimax.io", "api.minimaxi.com", "ark.cn-beijing.volces.com", "ark.ap-southeast.bytepluses.com",
]);
const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (!hosts.has(url.hostname)) return send(source, options);
  const target = "http://127.0.0.1:52979/validate/" + url.hostname + url.pathname + url.search;
  return send(source instanceof Request ? new Request(target, source) : target, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
