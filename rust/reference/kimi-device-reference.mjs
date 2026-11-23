import { registerHooks } from "node:module";

const project = new URL("../../", import.meta.url);
const local = "http://127.0.0.1:52989/kimi-device";
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.endsWith("/utils/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (...args) => fetch(...args);", context);
    }
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (endpoint, options) => {
  const url = new URL(endpoint);
  if (url.origin !== "https://auth.kimi.com" || !["/api/oauth/device_authorization", "/api/oauth/token"].includes(url.pathname)) {
    throw new Error("Unexpected issuer endpoint");
  }
  return fetchLoopback(local + url.pathname.slice(10), { ...options, redirect: "manual" });
};
const input = JSON.parse(process.argv[2]);
if (input.refresh) {
  const { refreshKimiToken } = await import("../../open-sse/services/tokenRefresh/providers.js");
  const tokens = await refreshKimiToken(input.refresh, input.account);
  console.log(JSON.stringify(tokens));
} else {
  const { default: provider } = await import("../../src/lib/oauth/providers/kimi.js");
  const code = await provider.requestDeviceCode(provider.config);
  const polls = [];
  for (let index = 0; index < input.polls; ++index) {
    const result = await provider.pollToken(provider.config, code.device_code, null, code);
    if (result.data.access_token && result.ok) {
      polls.push({ success: true, fields: provider.mapTokens(result.data) });
    } else {
      const error = result.data.error || "no_access_token";
      polls.push({ success: false, error, pending: ["authorization_pending", "slow_down"].includes(error) });
    }
  }
  console.log(JSON.stringify({ code, polls }));
}
