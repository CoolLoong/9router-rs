import { registerHooks } from "node:module";

const project = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (url, options) => {
  const paths = {
    "https://api.cast.ai/v1/llm/openai/supported-providers": "supported-providers",
    "https://app.kimchi.dev/api/v1/me": "me",
  };
  if (!paths[url]) throw new Error("Unexpected Kimchi endpoint");
  return fetchLoopback(`http://127.0.0.1:52989/kimchi-oauth/${paths[url]}`, { ...options, redirect: "manual" });
};
const { default: provider } = await import("../../src/lib/oauth/providers/kimchi.js");
const input = JSON.parse(process.argv[2]);
const authorize = provider.buildAuthUrl(provider.config, input.redirect, input.state);
let result;
try {
  const verified = await provider.exchangeToken(provider.config, input.token);
  result = { success: true, fields: provider.mapTokens(verified) };
} catch (error) {
  result = { success: false, error: error.message };
}
console.log(JSON.stringify({ authorize, flow: provider.flowType, result }));
