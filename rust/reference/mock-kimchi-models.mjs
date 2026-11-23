import { createRequire } from "node:module";

const resolver = createRequire(new URL("../../.next/standalone/server.js", import.meta.url));
const local = "http://127.0.0.1:52989";
const path = "/v1/models/metadata?include_in_cli=true";
const origin = "https://llm.kimchi.dev";
const providerFetch = fetch => (input, options) => {
  const url = typeof input === "string" ? input : input.url ?? input.href;
  if (url === `${origin}${path}`) return fetch(`${local}/kimchi${path}`, options);
  if (url === `${origin}/openai/v1/chat/completions`) return fetch(`${local}/static-chat/kimchi`, options);
  if (url === `${local}/tenant%2Falpha${path}`) return fetch(url, options);
  return Promise.reject(new Error("Unexpected Kimchi catalog fixture destination"));
};
globalThis.fetch = providerFetch(globalThis.fetch);
const undici = resolver("undici");
undici.fetch = providerFetch(undici.fetch);
