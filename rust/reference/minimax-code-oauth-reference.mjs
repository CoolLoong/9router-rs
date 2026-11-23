import { registerHooks } from "node:module";
import { createHash } from "node:crypto";

const project = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.endsWith("/utils/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (...args) => fetch(...args);", context);
    }
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
const fetchLocal = globalThis.fetch;
const providers = new Map([
  ["https://account.minimax.cn", "minimax-code"],
  ["https://account.minimax.io", "minimax-code-global"],
]);
globalThis.fetch = (endpoint, options) => {
  const { origin, pathname } = new URL(endpoint);
  const provider = providers.get(origin);
  if (!provider || !["/oauth2/device/code", "/oauth2/token"].includes(pathname)) {
    throw new Error("Unexpected OAuth issuer endpoint");
  }
  return fetchLocal("http://127.0.0.1:52989/minimax-code-oauth/" + provider + pathname, { ...options, redirect: "manual" });
};
const providerId = process.argv[2];
const refresh = process.argv[3];
if (refresh) {
  const { refreshMiniMaxCodeToken } = await import("../../open-sse/services/tokenRefresh/providers.js");
  console.log(JSON.stringify(await refreshMiniMaxCodeToken(providerId, refresh)));
} else {
  const { default: provider } = providerId === "minimax-code"
    ? await import("../../src/lib/oauth/providers/minimax-code.js")
    : await import("../../src/lib/oauth/providers/minimax-code-global.js");
  const verifier = Buffer.alloc(32, 15).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  const begin = await provider.requestDeviceCode(provider.config, challenge);
  const first = await provider.pollToken(provider.config, begin.device_code, verifier, begin);
  const second = await provider.pollToken(provider.config, begin.device_code, verifier, begin);
  const result = {
    verification: begin.verification_uri, interval: begin.interval, expiresIn: begin.expires_in,
    userCode: begin.user_code, byUser: begin._minimaxPollByUser,
    pending: first.data.error, grant: second.ok && provider.mapTokens(second.data),
  };
  console.log(JSON.stringify(result));
}
