import { createRequire } from "node:module";

const approved = new Set(["oidc.us-east-1.amazonaws.com", "oidc.eu-central-1.amazonaws.com",
  "prod.us-east-1.auth.desktop.kiro.dev"]);
const remap = send => (source, options) => {
  const url = new URL(source instanceof Request ? source.url : source);
  if (url.protocol !== "https:" || !approved.has(url.hostname)) return send(source, options);
  const fixture = "http://127.0.0.1:52989/kiro-refresh/" + url.hostname + url.pathname + url.search;
  return send(source instanceof Request ? new Request(fixture, source) : fixture, options);
};
globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);
