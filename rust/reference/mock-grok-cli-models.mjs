import { createRequire } from "node:module";

const resolver = createRequire(new URL("../../.next/standalone/server.js", import.meta.url));
const endpoints = new Map([
  ["https://cli-chat-proxy.grok.com/v1/models", "/grok-cli/v1/models"],
  ["https://cli-chat-proxy.grok.com/v1/user", "/grok-cli/v1/user"],
  ["https://auth.x.ai/.well-known/openid-configuration", "/xai-oauth/auth.x.ai/.well-known/openid-configuration"],
  ["https://login.x.ai/oauth2/native-token?region=global", "/xai-oauth/login.x.ai/oauth2/native-token?region=global"],
  ["https://auth.x.ai/oauth2/token", "/xai-oauth/auth.x.ai/oauth2/token"],
]);
const fixtureFetch = fetch => (input, options) => {
  const url = typeof input === "string" ? input : input.url ?? input.href;
  const local = endpoints.get(url);
  if (!local) return Promise.reject(new Error("Unexpected Grok catalog fixture destination"));
  return fetch(`http://127.0.0.1:52989${local}`, options);
};
globalThis.fetch = fixtureFetch(globalThis.fetch);
const undici = resolver("undici");
undici.fetch = fixtureFetch(undici.fetch);
